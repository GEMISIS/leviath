//! Running a spawn-time seed on the daemon.
//!
//! Each kind runs under the machinery a spawn has always used: files and
//! globs read inside the workdir and never outside it, code runs on the Rhai
//! engine with the task and workdir in hand, a shell command runs under
//! `[security] allow_seed_commands` and the safe-command list, and a tool
//! call answers to `[tool_permissions]` through the seed tool runner.
//!
//! A seed sees only its [`SeedCx`], so the policies here are the operator's
//! alone: no blueprint safe-command list, no launch `--allow`, and no stage
//! sandbox.

use std::collections::HashMap;
use std::time::Duration;

use leviath_runtime::spec::graph::SeedToolCall;
use leviath_runtime::spec::inputs::InputValue;

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

/// Run a code seed: the text its script returns.
fn run_code(code: &CodeRef, cx: SeedCx<'_>) -> Result<String, String> {
    let CodeRef::Inline(source) = code else {
        return Err(
            "a code seed runs the code it carries; name the script by file and the run's own \
             copy is used"
                .to_string(),
        );
    };
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

/// Run a shell-command seed, when seed commands may run at all.
fn run_command(env: &DaemonEnv, command: &str, cx: SeedCx<'_>) -> Result<String, String> {
    if !cx.commands_allowed {
        return Err(
            "command seeds are disabled (`[security] allow_seed_commands = false` or \
             --no-seed-commands)"
                .to_string(),
        );
    }
    let config = &env.config;
    let policy = crate::daemon::seed_command::SeedCommandPolicy::new(
        true,
        Duration::from_secs(config.limits.script_shell_timeout_secs),
        Arc::new(config.safe_keys_for_agent("", None).into_keys().collect()),
        None,
        crate::daemon::spawn::shell_env_policy(config),
    );
    policy.run(command, cx.workdir)
}

/// Run a tool seed's calls under `[tool_permissions]`, one block per call
/// that produced something. A call that fails is left out; the seed fails
/// only when nothing produced anything and something failed.
fn run_tools(env: &DaemonEnv, calls: &[SeedToolCall], cx: SeedCx<'_>) -> Result<String, String> {
    let config = &env.config;
    let workdir = cx.workdir.to_path_buf();
    let builtins = Arc::new(leviath_tools::BuiltinTools::new(
        leviath_tools::ToolContext::new(workdir.clone())
            .with_shell_env(crate::daemon::spawn::shell_env_policy(config)),
    ));
    let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
    let writes = Arc::new(crate::daemon::tool_service::WriteBudget::new(
        config.limits.write_limits(),
    ));
    let nothing = crate::daemon::script_host::ScriptAllow {
        http_get: false,
        http_post: false,
        shell: false,
        read_file: false,
        write_file: false,
        env_var: false,
    };
    let script_host = Arc::new(
        crate::daemon::script_host::DaemonScriptHost::new(nothing, workdir)
            .with_write_budget(writes.clone()),
    );
    let global = config.permissions_for_agent("");
    let may_loosen = config.security.allow_blueprint_permissions;
    let none = HashMap::new();
    let untouched = HashMap::new();
    let resolve: seed_tool::SeedPolicyResolver = Arc::new(
        move |name: &str, is_builtin: bool, _args: &serde_json::Value| {
            seed_tool::SeedToolPermissions {
                launch: &none,
                stage: &untouched,
                agent: &untouched,
                global: &global,
                may_loosen,
            }
            .resolve(name, is_builtin)
        },
    );
    let runner = seed_tool::production_runner(
        seed_tool::SeedToolContext {
            builtins,
            builtin_names,
            script_tools: leviath_scripting::ScriptToolSet::default(),
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
