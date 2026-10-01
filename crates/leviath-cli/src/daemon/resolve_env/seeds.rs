//! Running a spawn-time seed on the daemon.
//!
//! Each kind runs under the machinery a spawn has always used: files and
//! globs read inside the workdir and never outside it, code runs on the Rhai
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
        Seed::Files(paths) => read(
            cx.workdir,
            paths.iter().map(|p| cx.workdir.join(p.as_str())).collect(),
        ),
        Seed::Glob(pattern) => {
            let full = cx.workdir.join(pattern);
            let matches = glob::glob(&full.to_string_lossy())
                .map_err(|e| format!("bad glob '{pattern}': {e}"))?;
            read(cx.workdir, matches.filter_map(Result::ok).collect())
        }
        Seed::Code(code) => run_code(code, cx),
        Seed::Command(command) => run_command(env, command, cx),
        Seed::Tools { calls, .. } => run_tools(env, calls, cx),
    }
}

/// Read each file (every one inside the workdir) under a `--- <path> ---`
/// heading, the way a files seed has always read.
fn read(workdir: &Path, paths: Vec<PathBuf>) -> Result<String, String> {
    let inside = paths
        .into_iter()
        .map(|p| match leviath_core::resolves_within(&p, workdir) {
            true => Ok(p),
            false => Err(format!(
                "seed path '{}' resolves outside the working directory ({})",
                p.display(),
                workdir.display()
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    crate::daemon::spawn::read_and_concat("seed", inside.into_iter(), true)
        .map(Option::unwrap_or_default)
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
    let (profile, _) = layers::profile(&cx.launch.unattended).map_err(|e| e.to_string())?;
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
