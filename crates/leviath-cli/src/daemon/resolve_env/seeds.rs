//! Running a spawn-time seed on the daemon.
//!
//! Each kind runs under the machinery a spawn has always used: files and
//! globs read inside the workdir, or outside it where the run's
//! `[read_paths]` are granted, or (written `blueprint:<path>`) inside the
//! blueprint's own directory and never outside it; code runs on the Rhai
//! engine with the task and workdir in hand, a shell command runs under
//! `[security] allow_seed_commands`, the safe-command list (the operator's and
//! the graph's) and the entry stage's sandbox, and a tool call answers to the
//! same permission layers and yolo profile the run's tools do (see
//! [`layers`](super::layers)).

use std::time::Duration;

use leviath_runtime::spec::graph::SeedToolCall;
use leviath_runtime::spec::inputs::InputValue;

use super::layers::{self, Layers};
use super::*;
use crate::daemon::seed_tool;

/// Run one seed and return the text it produced.
pub(super) fn run(env: &DaemonEnv, seed: &Seed, cx: SeedCx<'_>) -> Result<String, String> {
    match seed {
        Seed::Literal(text) => Ok(text.clone()),
        Seed::Files(paths) => {
            let places = Places::of(env, &cx)?;
            let paths = paths
                .iter()
                .map(|p| {
                    let (root, rel) = places.root(p.as_str())?;
                    places.fence(root, &root.join(rel))
                })
                .collect::<Result<Vec<_>, _>>()?;
            read(paths)
        }
        Seed::Glob(pattern) => {
            let places = Places::of(env, &cx)?;
            let (root, rel) = places.root(pattern)?;
            let full = root.join(rel);
            let matches = glob::glob(&full.to_string_lossy())
                .map_err(|e| format!("bad glob '{pattern}': {e}"))?;
            // Each match is checked, not the pattern: `../*.toml` cannot be
            // judged before it is expanded.
            let paths = matches
                .filter_map(Result::ok)
                .map(|p| places.fence(root, &p))
                .collect::<Result<Vec<_>, _>>()?;
            read(paths)
        }
        Seed::Code(code) => run_code(code, cx),
        Seed::Command(command) => run_command(env, command, cx),
        Seed::Tools { calls, .. } => run_tools(env, calls, cx),
    }
}

/// The prefix that reads a seed path from the blueprint's own directory.
const BLUEPRINT_PREFIX: &str = "blueprint:";

/// Where a files or glob seed may read: the workdir, with what the run's
/// `[read_paths]` grant beyond it, and the blueprint's own directory.
struct Places<'a> {
    workdir: &'a Path,
    blueprint_dir: Option<&'a Path>,
    read_paths: leviath_core::ReadPathPolicy,
}

impl<'a> Places<'a> {
    /// The places for the run `cx` describes. A `[read_paths]` entry that
    /// will not compile is the seed's failure, as it is the run's.
    fn of(env: &DaemonEnv, cx: &SeedCx<'a>) -> Result<Self, String> {
        let (read_paths, _warning) = crate::daemon::spawn::compile_read_path_policy(
            cx.agent,
            &cx.graph.read_paths,
            &env.config,
            cx.workdir,
        )?;
        Ok(Self {
            workdir: cx.workdir,
            blueprint_dir: cx.blueprint_dir,
            read_paths,
        })
    }

    /// The directory `declared` is read against, and the rest of it: the
    /// blueprint's for a `blueprint:` path, the workdir's for any other.
    fn root<'p>(&self, declared: &'p str) -> Result<(&'a Path, &'p str), String> {
        match declared.strip_prefix(BLUEPRINT_PREFIX) {
            None => Ok((self.workdir, declared)),
            Some(rel) => self.blueprint_dir.map(|dir| (dir, rel)).ok_or_else(|| {
                format!(
                    "seed path '{declared}' reads from the blueprint's directory, and a graph \
                     its caller wrote has none"
                )
            }),
        }
    }

    /// `path`, when the run may read it from `root`: a blueprint's files stay
    /// inside its directory whatever is granted (a grant widens what the
    /// agent may read on this machine, not what a package ships), and a
    /// workdir path may leave the workdir only where `[read_paths]` grant it.
    fn fence(&self, root: &Path, path: &Path) -> Result<PathBuf, String> {
        if leviath_core::resolves_within(path, root) {
            return Ok(path.to_path_buf());
        }
        let granted = root == self.workdir
            && std::fs::canonicalize(path).is_ok_and(|real| {
                self.read_paths.decide(&real) == leviath_core::ReadPathDecision::Allowed
            });
        match (granted, root == self.workdir) {
            (true, _) => Ok(path.to_path_buf()),
            (false, true) => Err(format!(
                "seed path '{}' resolves outside the working directory ({}), and no \
                 [read_paths] grant covers it",
                path.display(),
                root.display()
            )),
            (false, false) => Err(format!(
                "seed path '{}' resolves outside the blueprint's directory ({}); a \
                 'blueprint:' path reads only files the blueprint ships",
                path.display(),
                root.display()
            )),
        }
    }
}

/// Read each file under a `--- <path> ---` heading, the way a files seed has
/// always read.
fn read(paths: Vec<PathBuf>) -> Result<String, String> {
    crate::daemon::spawn::read_and_concat("seed", paths.into_iter()).map(Option::unwrap_or_default)
}

/// Run a code seed, from the run's own copy of its code however the graph
/// named it: the text its script returns.
fn run_code(code: &CodeRef, cx: SeedCx<'_>) -> Result<String, String> {
    let source = cx
        .code_of(code)
        .ok_or("the run holds no code for this seed")?;
    let source =
        std::str::from_utf8(source).map_err(|e| format!("the seed is not UTF-8 text: {e}"))?;
    let task = match cx.inputs.get("task") {
        Some(InputValue::Text(text)) => text.clone(),
        _ => String::new(),
    };
    let mut input = rhai::Map::new();
    input.insert("task".into(), rhai::Dynamic::from(task));
    input.insert(
        "workdir".into(),
        rhai::Dynamic::from(cx.workdir.display().to_string()),
    );
    leviath_scripting::ScriptEngine::new()
        .transform(source, input)
        .map(crate::daemon::script_host::cap_script_io)
        .map_err(|e| format!("code seed failed: {e}"))
}

/// The sandbox the entry stage's shell runs in, for a seed.
fn sandbox(
    env: &DaemonEnv,
    cx: &SeedCx<'_>,
    layers: &Layers,
) -> Result<Option<Arc<crate::daemon::sandbox_manager::SandboxManager>>, String> {
    let built = layers::sandbox(
        &env.config,
        cx.graph,
        cx.run_id.as_str(),
        cx.workdir,
        layers.entry_index,
    )?;
    Ok(built.map(Arc::new))
}

/// Run a shell-command seed, when seed commands may run at all: pre-approved
/// only by the operator's and the graph's safe-command lists (a seed runs
/// before any prompt exists), inside the entry stage's sandbox.
fn run_command(env: &DaemonEnv, command: &str, cx: SeedCx<'_>) -> Result<String, String> {
    if !cx.commands_allowed {
        return Err(
            "command seeds are disabled (`[security] allow_seed_commands = false` or \
             --no-seed-commands)"
                .to_string(),
        );
    }
    let config = &env.config;
    let layers = Layers::new(config, cx.graph, cx.launch, cx.agent);
    let safe = layers::blueprint_safe(cx.graph);
    let policy = crate::daemon::seed_command::SeedCommandPolicy::new(
        true,
        Duration::from_secs(config.limits.script_shell_timeout_secs),
        Arc::new(
            config
                .safe_keys_for_agent(cx.agent, safe.as_ref())
                .into_keys()
                .collect(),
        ),
        sandbox(env, &cx, &layers)?,
        crate::daemon::spawn::shell_env_policy(config),
    );
    policy.run(command, cx.workdir)
}

/// Run a tool seed's calls under the run's permission layers and yolo
/// profile, one block per call that produced something. A call that fails is
/// left out; the seed fails only when nothing produced anything and something
/// failed.
fn run_tools(env: &DaemonEnv, calls: &[SeedToolCall], cx: SeedCx<'_>) -> Result<String, String> {
    let config = &env.config;
    let workdir = cx.workdir.to_path_buf();
    let layers = Layers::new(config, cx.graph, cx.launch, cx.agent);
    let profile = layers::profile(&cx.launch.unattended).map_err(|e| e.to_string())?;
    let sandbox = sandbox(env, &cx, &layers)?;
    let builtins = sandbox.iter().fold(
        leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(workdir.clone())
                .with_shell_env(crate::daemon::spawn::shell_env_policy(config)),
        ),
        |tools, mgr| {
            tools.with_shell_executor(mgr.clone() as Arc<dyn leviath_tools::ShellExecutor>)
        },
    );
    let builtins = Arc::new(builtins);
    let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
    let writes = Arc::new(crate::daemon::tool_service::WriteBudget::new(
        config.limits.write_limits(),
    ));
    let allow = layers::script_allow(config, cx.graph, &layers, profile.as_deref(), &workdir);
    let script_host = Arc::new(
        crate::daemon::script_host::DaemonScriptHost::new(allow, workdir.clone())
            .with_write_budget(writes.clone()),
    );
    let (script_tools, script_names) = layers::code_tools(cx.code);
    let known_builtins = builtin_names.clone();
    let resolve: seed_tool::SeedPolicyResolver = Arc::new(
        move |name: &str, is_builtin: bool, args: &serde_json::Value| {
            let kind = crate::yolo::ToolKind::classify(
                name,
                known_builtins.contains(name),
                script_names.contains(name),
            );
            layers.decide(profile.as_deref(), name, args, kind, is_builtin, &workdir)
        },
    );
    let runner = seed_tool::production_runner(
        seed_tool::SeedToolContext {
            builtins,
            builtin_names,
            script_tools,
            script_host,
            mcp: env.shared_mcp.clone(),
            writes,
            protected: crate::tools::permission_files(config),
        },
        resolve,
    );
    let mut blocks = Vec::new();
    let mut failures = Vec::new();
    for call in calls {
        match runner(call.tool.as_str(), call.args.value()) {
            Ok(text) => blocks.extend(
                (!text.trim().is_empty()).then(|| seed_tool::seed_block(call.tool.as_str(), &text)),
            ),
            Err(e) => failures.push(format!("{}: {e}", call.tool)),
        }
    }
    match (seed_tool::join_blocks(blocks), failures.is_empty()) {
        (Some(text), _) => Ok(text),
        (None, true) => Ok(String::new()),
        (None, false) => Err(failures.join("; ")),
    }
}

#[cfg(test)]
#[path = "seeds_tests.rs"]
mod tests;
