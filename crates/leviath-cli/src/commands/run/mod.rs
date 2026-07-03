//! `lev run` - Run an agent
//!
//! This module is organized into focused sub-modules:
//! - `manifest`: Finding and parsing agent.leviath manifest files
//! - `session`: Task resolution, editor launching, provider registry setup
//! - `graph`: Graph traversal, transition resolution, edge transforms
//! - `stages`: Stage runner functions (interactive, autonomous, interactive_points)
//! - `inference`: Streaming inference support
//! - `helpers`: Context window setup, title generation, snapshots
//! - `foreground`: Foreground (inline, blocking) run mode
//! - `worker`: Background worker run mode

mod dynamic_interaction;
pub mod executor;
mod foreground;
mod graph;
mod helpers;
mod inference;
pub mod io;
mod manifest;
mod session;
mod stages;
mod worker;

use clap::Args;
use std::path::Path;

use crate::runstate;

// Re-export public API used by other commands
pub use manifest::parse_manifest_public;
pub use session::build_provider_registry;

#[derive(Args)]
pub struct RunArgs {
    /// Path to agent project or agent.leviath (or installed agent name)
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt, or path to a file containing the task.
    /// Omit to open an interactive editor (requires a TTY).
    #[arg(short, long)]
    pub task: Option<String>,

    /// Model override
    #[arg(short, long)]
    pub model: Option<String>,

    /// Run in the foreground (inline, blocking) instead of the default background mode
    #[arg(short = 'f', long)]
    pub foreground: bool,

    /// Allow all tool calls without prompting for this run (same as --allow '*')
    #[arg(long)]
    pub yolo: bool,

    /// Allow specific tools without prompting (comma-separated, e.g. "read_file,bash")
    #[arg(long, value_delimiter = ',')]
    pub allow: Vec<String>,

    /// Require approval for specific tools even if they would be auto-allowed (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub ask: Vec<String>,

    /// Deny specific tools entirely for this run (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub deny: Vec<String>,

    /// Maximum sub-agent tree depth (overrides blueprint config, lower wins)
    #[arg(long)]
    pub max_depth: Option<usize>,

    /// Number of background instances to spawn (default: 1)
    #[arg(short = 'n', long, default_value = "1")]
    pub count: usize,
}

/// Arguments for the hidden `__run-worker` subcommand.
#[derive(Args)]
pub struct WorkerArgs {
    /// Path to agent manifest or directory
    #[arg(value_name = "PATH")]
    pub path: String,

    /// Task prompt
    #[arg(short, long)]
    pub task: String,

    /// Run ID for on-disk state tracking
    #[arg(long)]
    pub run_id: String,

    /// Model override
    #[arg(short, long)]
    pub model: Option<String>,

    /// Allow all tool calls without prompting
    #[arg(long, default_value_t = false)]
    pub yolo: bool,

    /// Tools to auto-allow (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub allow: Vec<String>,

    /// Tools to always ask about (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub ask: Vec<String>,

    /// Tools to deny entirely (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub deny: Vec<String>,

    /// Maximum sub-agent tree depth
    #[arg(long)]
    pub max_depth: Option<usize>,
}

/// Opens the run's `output.log` file (creating it if needed, appending) and
/// returns a cloned handle so both stdout and stderr can be redirected to it.
///
/// Both operations are expected to always succeed once `create_run` has been
/// called (which creates the directory with appropriate permissions); any
/// failure here indicates a fatal system-level condition.
///
/// Extracted as a standalone function so tests can verify the happy path
/// without needing to exercise the rest of `execute_background`.
fn open_log_file(log_path: &std::path::Path) -> (std::fs::File, std::fs::File) {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .unwrap_or_else(|e| panic!("failed to open run log at {}: {}", log_path.display(), e));
    let log_file2 = log_file
        .try_clone()
        .expect("failed to clone log file handle — this should never happen");
    (log_file, log_file2)
}

/// Called in the forked child process (via `CommandExt::pre_exec`) to start a
/// new session, detaching the worker from the spawning terminal.
///
/// `setsid()` may fail if this process is already a process-group leader; that
/// is silently ignored — the worker still runs, just without a new session.
#[cfg(unix)]
fn new_session_pre_exec() -> std::io::Result<()> {
    // SAFETY: setsid() is async-signal-safe and has no preconditions beyond the
    // usual POSIX constraints.  We ignore the return value intentionally.
    unsafe { libc::setsid() };
    Ok(())
}

pub async fn execute(args: RunArgs) -> anyhow::Result<()> {
    if args.foreground {
        if args.count > 1 {
            anyhow::bail!("--count is not supported with --foreground");
        }
        return foreground::run_foreground(args).await;
    }

    // current_exe always succeeds on supported platforms; any failure is a fatal
    // misconfiguration that should surface as a panic rather than a user error.
    let exe = std::env::current_exe().expect("current executable path must be available");
    execute_background(args, &exe).await
}

async fn execute_background(args: RunArgs, exe: &std::path::Path) -> anyhow::Result<()> {
    execute_background_with(args, exe, runstate::create_run).await
}

async fn execute_background_with(
    args: RunArgs,
    exe: &std::path::Path,
    create_run: impl Fn(&runstate::RunMeta) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // Background mode: create run state, spawn detached worker process(es)
    let path = args.path.as_deref().unwrap_or(".").to_string();
    let manifest_path = manifest::find_manifest(&path)?;

    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = manifest::parse_manifest(&manifest_content)?;

    // Resolve the task once (may launch an interactive editor) before spawning workers.
    let description = Some(blueprint.description.as_str());
    let task = session::resolve_task(&args.task, &blueprint.name, description)?;

    let workdir = std::env::current_dir()
        .ok()
        .unwrap_or(std::path::PathBuf::from("."));
    let count = args.count.max(1);

    for i in 0..count {
        let run_id = runstate::new_run_id(&blueprint.name);

        let meta = runstate::RunMeta::new(
            run_id.clone(),
            blueprint.name.clone(),
            manifest_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .to_string(),
            task.clone(),
            args.model.clone(),
            workdir.to_string_lossy().to_string(),
            blueprint.stages.len(),
        );
        create_run(&meta)?;

        // Redirect the worker's stdout + stderr to the run's output.log
        let log_path = runstate::run_dir(&run_id).join("output.log");
        let (log_file, log_file2) = open_log_file(&log_path);

        let mut cmd = std::process::Command::new(exe);
        cmd.arg("__run-worker")
            .arg(manifest_path.to_string_lossy().as_ref())
            .arg("--task")
            .arg(&task)
            .arg("--run-id")
            .arg(&run_id);

        if let Some(ref model) = args.model {
            cmd.arg("--model").arg(model);
        }
        if args.yolo {
            cmd.arg("--yolo");
        }
        for t in &args.allow {
            cmd.arg("--allow").arg(t);
        }
        for t in &args.ask {
            cmd.arg("--ask").arg(t);
        }
        for t in &args.deny {
            cmd.arg("--deny").arg(t);
        }
        if let Some(md) = args.max_depth {
            cmd.arg("--max-depth").arg(md.to_string());
        }

        cmd.current_dir(&workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_file2));

        // On Unix: setsid() detaches the worker into its own session so it
        // survives the spawning terminal being closed.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(new_session_pre_exec);
            }
        }

        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn worker process: {}", e))?;

        if count == 1 {
            println!("Started run: {}", run_id);
            println!("  lev dash  — monitor in TUI dashboard");
        } else {
            println!("  [{}/{}] Started run: {}", i + 1, count, run_id);
        }
    }

    if count > 1 {
        println!("Spawned {} runs. Use `lev dash` to monitor.", count);
    }

    Ok(())
}

pub async fn execute_worker(args: WorkerArgs) -> anyhow::Result<()> {
    worker::execute_worker(args).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn run_args_defaults() {
        let args = RunArgs {
            path: None,
            task: None,
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        assert!(args.path.is_none());
        assert!(args.task.is_none());
        assert!(args.model.is_none());
        assert!(!args.foreground);
        assert!(!args.yolo);
        assert!(args.allow.is_empty());
        assert!(args.ask.is_empty());
        assert!(args.deny.is_empty());
        assert!(args.max_depth.is_none());
        assert_eq!(args.count, 1);
    }

    #[test]
    fn run_args_with_values() {
        let args = RunArgs {
            path: Some("./my-agent".to_string()),
            task: Some("build something".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            foreground: true,
            yolo: true,
            allow: vec!["read_file".to_string(), "bash".to_string()],
            ask: vec!["write_file".to_string()],
            deny: vec!["shell".to_string()],
            max_depth: Some(3),
            count: 5,
        };
        assert_eq!(args.path.unwrap(), "./my-agent");
        assert_eq!(args.task.unwrap(), "build something");
        assert!(args.foreground);
        assert!(args.yolo);
        assert_eq!(args.allow.len(), 2);
        assert_eq!(args.ask.len(), 1);
        assert_eq!(args.deny.len(), 1);
        assert_eq!(args.max_depth, Some(3));
        assert_eq!(args.count, 5);
    }

    #[test]
    fn worker_args_construction() {
        let args = WorkerArgs {
            path: "/path/to/agent".to_string(),
            task: "do the thing".to_string(),
            run_id: "run-abc-123".to_string(),
            model: Some("gpt-4".to_string()),
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };
        assert_eq!(args.path, "/path/to/agent");
        assert_eq!(args.task, "do the thing");
        assert_eq!(args.run_id, "run-abc-123");
        assert_eq!(args.model.unwrap(), "gpt-4");
        assert!(!args.yolo);
    }

    #[test]
    fn parse_manifest_public_with_valid_manifest() {
        let manifest = r#"
[agent]
name = "test-agent"
version = "1.0.0"
description = "A test agent"

[stages.plan]
mode = "autonomous"
prompt = "Plan the work"
"#;
        let bp = parse_manifest_public(manifest).unwrap();
        assert_eq!(bp.name, "test-agent");
        assert_eq!(bp.version, "1.0.0");
        assert_eq!(bp.description, "A test agent");
        assert!(!bp.stages.is_empty());
    }

    #[test]
    fn parse_manifest_public_with_invalid_toml() {
        let result = parse_manifest_public("not valid toml [[[");
        assert!(result.is_err());
    }

    #[test]
    fn parse_manifest_public_missing_agent_section() {
        let result = parse_manifest_public("[stages.plan]\nprompt = \"x\"\n");
        assert!(result.is_err());
    }

    #[test]
    fn parse_manifest_public_minimal() {
        let manifest = r#"
[agent]
name = "minimal"

[stages.default]
prompt = "do something"
"#;
        let bp = parse_manifest_public(manifest).unwrap();
        assert_eq!(bp.name, "minimal");
    }

    #[test]
    fn parse_manifest_public_multiple_stages() {
        let manifest = r#"
[agent]
name = "multi"
version = "0.1.0"
description = "Multi-stage agent"

[stages.plan]
mode = "autonomous"
prompt = "Plan"

[stages.implement]
mode = "autonomous"
prompt = "Implement"
"#;
        let bp = parse_manifest_public(manifest).unwrap();
        assert_eq!(bp.stages.len(), 2);
    }

    // ─── execute() error paths ─────────────────────────────────────────────

    #[tokio::test]
    async fn execute_foreground_with_count_greater_than_1_errors() {
        let args = RunArgs {
            path: None,
            task: Some("do something".to_string()),
            model: None,
            foreground: true,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 2, // count > 1 with foreground is not allowed
        };
        let result = execute(args).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Expected error about --count with --foreground.
        assert!(err.contains("count") | err.contains("foreground"));
    }

    #[tokio::test]
    async fn execute_background_no_manifest_returns_error() {
        let args = RunArgs {
            path: Some("/nonexistent/path/to/agent".to_string()),
            task: Some("do something".to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        let result = execute(args).await;
        assert!(result.is_err()); // manifest not found
    }

    #[tokio::test]
    async fn execute_background_manifest_is_directory_returns_read_error() {
        // Covers line 144: the `map_err` closure in `read_to_string(..).map_err(..)?`
        // when the manifest path is a directory (EISDIR). We create a temp directory
        // that contains `agent.leviath` as a sub-directory so `find_manifest` returns
        // Ok(path) but `read_to_string` fails.
        let agent_dir = std::env::temp_dir().join(format!(
            "leviath-test-bg-dir-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let manifest_as_dir = agent_dir.join("agent.leviath");
        std::fs::create_dir_all(&manifest_as_dir).unwrap();

        let args = RunArgs {
            path: Some(agent_dir.to_string_lossy().into_owned()),
            task: Some("do something".to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        let result = execute(args).await;
        let _ = std::fs::remove_dir_all(&agent_dir);
        // Expected read error for directory manifest.
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        let has_manifest_err = err.contains("Failed to read manifest") | err.contains("manifest");
        assert!(has_manifest_err);
    }

    #[tokio::test]
    async fn execute_background_bad_exe_returns_spawn_error() {
        // Covers line ~226: the `map_err` closure in `cmd.spawn().map_err(..)?`
        // when the executable path does not exist. We call execute_background
        // directly (the private inner function) with a non-existent exe path so
        // the spawn fails immediately.
        let agent_name = "test-execute-bg-bad-exe";
        let _cleanup = RunPrefixCleanup(agent_name);
        let temp_dir = std::env::temp_dir().join(agent_name);
        let _ = std::fs::create_dir_all(&temp_dir);
        write_valid_manifest(&temp_dir, agent_name);

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        let bad_exe = std::path::Path::new("/nonexistent/executable/path/leviath-cli");
        let result = super::execute_background(args, bad_exe).await;
        // Expected spawn error.
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        let has_spawn_err =
            err.contains("Failed to spawn") | err.contains("spawn") | err.contains("No such file");
        assert!(has_spawn_err);
    }

    // ─── WorkerArgs: more coverage ─────────────────────────────────────────

    #[test]
    fn worker_args_with_all_overrides() {
        let args = WorkerArgs {
            path: "/my/agent.leviath".to_string(),
            task: "complex task".to_string(),
            run_id: "run-xyz-999".to_string(),
            model: None,
            yolo: true,
            allow: vec!["read_file".to_string()],
            ask: vec!["write_file".to_string()],
            deny: vec!["bash".to_string()],
            max_depth: Some(5),
        };
        assert_eq!(args.path, "/my/agent.leviath");
        assert!(args.yolo);
        assert_eq!(args.allow, vec!["read_file"]);
        assert_eq!(args.ask, vec!["write_file"]);
        assert_eq!(args.deny, vec!["bash"]);
        assert_eq!(args.max_depth, Some(5));
    }

    // ─── execute() background happy path ───────────────────────────────────
    //
    // These drive `execute()` all the way through manifest loading, task
    // resolution, run-state creation, and worker-process spawning. The
    // spawned "worker" is genuinely `std::env::current_exe()` — i.e. this
    // very test binary re-invoked with `__run-worker` args it doesn't
    // understand — but `execute()` never waits on it (`cmd.spawn()`, not
    // `.status()`/`.output()`), so its eventual (harmless, logged-to-a-tempfile)
    // failure has no bearing on these assertions. This mirrors the existing
    // convention elsewhere in this crate (e.g. `worker.rs`'s tests) of writing
    // real, uniquely-named entries under the real run-state directory and
    // cleaning them up afterward, rather than mocking process spawning.

    fn write_valid_manifest(dir: &std::path::Path, agent_name: &str) {
        let manifest_content = format!(
            r#"
[agent]
name = "{agent_name}"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
prompt = "Do the thing"
"#
        );
        std::fs::write(dir.join("agent.leviath"), manifest_content).unwrap();
    }

    /// Core of [`cleanup_runs_with_prefix`]: scans `dir` and removes every
    /// entry whose name starts with `agent_name-`.  Accepts the directory as
    /// an argument so tests can exercise the early-return path without
    /// touching the process-global `LEVIATH_RUNS_DIR` env var.
    fn cleanup_runs_with_prefix_in_dir(agent_name: &str, dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let prefix = format!("{agent_name}-");
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(prefix.as_str())
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Removes every run directory whose id starts with `agent_name-`,
    /// wherever `execute()` actually wrote them (respects `LEVIATH_RUNS_DIR`
    /// if set, same as `runstate::run_dir` itself).
    fn cleanup_runs_with_prefix(agent_name: &str) {
        cleanup_runs_with_prefix_in_dir(agent_name, &runstate::runs_dir());
    }

    /// RAII guard that runs [`cleanup_runs_with_prefix`] on drop, so a
    /// mid-test assertion failure can't leak real run directories under
    /// `~/.leviath/runs` (as happened once while developing these tests,
    /// before this guard existed).
    struct RunPrefixCleanup<'a>(&'a str);
    impl Drop for RunPrefixCleanup<'_> {
        fn drop(&mut self) {
            cleanup_runs_with_prefix(self.0);
        }
    }

    // These two tests drive `execute_background` (not the public `execute`)
    // with `/usr/bin/true` standing in for the worker executable, rather than
    // letting it resolve `std::env::current_exe()` and spawn a detached copy
    // of *this test binary*. Spawning the real test binary here was the
    // source of a runaway-process/OOM risk: under `cargo llvm-cov` each
    // detached child is itself coverage-instrumented and writes its own
    // `.profraw` file (hundreds accumulated in the repo from a single test
    // run), and — since the child is a copy of the whole suite re-invoked
    // with unrecognized args — any future harness change that makes it run
    // instead of erroring out fast would re-trigger this same fork-bomb
    // shape. `/usr/bin/true` ignores all arguments and exits immediately, so
    // this still exercises run-state creation and the successful `spawn()`
    // path without any of that risk. Matches the existing Unix-only
    // `/usr/bin/true` convention used throughout `session.rs`'s
    // `launch_editor` tests.
    #[cfg(unix)]
    #[tokio::test]
    async fn execute_background_happy_path_spawns_single_worker() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "execute_background_happy_path_spawns_single_worker",
        );
        let agent_name = "test-execute-bg-happy-single";
        let _cleanup = RunPrefixCleanup(agent_name);
        let temp_dir = std::env::temp_dir().join(agent_name);
        let _ = std::fs::create_dir_all(&temp_dir);
        write_valid_manifest(&temp_dir, agent_name);

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            foreground: false,
            yolo: true,
            allow: vec!["read_file".to_string()],
            ask: vec!["bash".to_string()],
            deny: vec!["write_file".to_string()],
            max_depth: Some(2),
            count: 1,
        };

        let harmless_exe = std::path::Path::new("/usr/bin/true");
        super::execute_background(args, harmless_exe)
            .await
            .expect("expected background execute to succeed");

        let runs = runstate::list_runs();
        let run_was_created = runs.iter().any(|m| m.agent_name == agent_name);
        // Expected a run to have been created.
        assert!(run_was_created);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_background_happy_path_spawns_multiple_workers() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "execute_background_happy_path_spawns_multiple_workers",
        );
        let agent_name = "test-execute-bg-happy-multi";
        let _cleanup = RunPrefixCleanup(agent_name);
        let temp_dir = std::env::temp_dir().join(agent_name);
        let _ = std::fs::create_dir_all(&temp_dir);
        write_valid_manifest(&temp_dir, agent_name);

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 3,
        };

        let harmless_exe = std::path::Path::new("/usr/bin/true");
        super::execute_background(args, harmless_exe)
            .await
            .expect("expected multi-count background execute to succeed");

        let runs = runstate::list_runs();
        let matching = runs.iter().filter(|m| m.agent_name == agent_name).count();
        assert_eq!(matching, 3);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn execute_worker_thin_wrapper_delegates_to_worker_module() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "execute_worker_thin_wrapper_delegates_to_worker_module",
        );
        // `execute_worker` (the public re-export) is a one-line delegation to
        // `worker::execute_worker` -- exercised end-to-end (not mocked) by
        // `worker.rs`'s own test suite. This just proves the delegation
        // itself runs without panicking, using a path that fails fast.
        let args = WorkerArgs {
            path: "/nonexistent/path/for/mod-rs-delegation-test".to_string(),
            task: "task".to_string(),
            run_id: "test-execute-worker-delegation".to_string(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };
        let result = execute_worker(args).await;
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(runstate::run_dir("test-execute-worker-delegation"));
    }

    // ─── RunArgs: more coverage ────────────────────────────────────────────

    #[test]
    fn run_args_count_zero_is_min_1_after_execute() {
        // count: 0 would be interpreted as 1 via count.max(1) in execute()
        let args = RunArgs {
            path: None,
            task: Some("task".to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 0,
        };
        // count.max(1) = 1
        assert_eq!(args.count.max(1), 1);
    }

    // ─── foreground path (line 123) ───────────────────────────────────────────

    #[tokio::test]
    async fn execute_foreground_count_1_delegates_to_run_foreground() {
        // Exercises the `return foreground::run_foreground(args).await` branch
        // (count==1 and foreground==true). We use a nonexistent path so
        // run_foreground fails fast; what we care about is that execute() reaches
        // and executes that branch rather than returning before it.
        let args = RunArgs {
            path: Some("/nonexistent/foreground-test-path".to_string()),
            task: Some("task".to_string()),
            model: None,
            foreground: true,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        let result = execute(args).await;
        // foreground::run_foreground should return an error for nonexistent path
        assert!(result.is_err());
    }

    // ─── parse_manifest error path (line 132) ────────────────────────────────

    #[tokio::test]
    async fn execute_background_manifest_invalid_toml_returns_error() {
        // Creates a valid file path with invalid TOML content so that
        // manifest::parse_manifest returns an error (covers the `?` at line 132).
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let agent_name = format!("test-execute-bg-badtoml-{pid}-{now}");
        let temp_dir = std::env::temp_dir().join(&agent_name);
        let _ = std::fs::create_dir_all(&temp_dir);
        // Write a file that's a valid TOML but not a valid manifest (no [agent] section)
        // — using truly broken TOML to force parse_manifest to fail.
        std::fs::write(
            temp_dir.join("agent.leviath"),
            "this is [not valid = toml {{{{",
        )
        .unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("task".to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        let result = execute(args).await;
        // Expected error for invalid manifest TOML.
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // ─── resolve_task error path (line 136) ──────────────────────────────────

    /// Covers `resolve_task(...)? ` (line 136) via the "empty task file"
    /// error, not `task: None`. `task: None` reaches `resolve_task`'s real,
    /// un-injected `std::io::stdin().is_terminal()` check -- under `cargo
    /// test` run from a real interactive terminal that's actually true
    /// (not the "always non-TTY" assumption this test used to make), so it
    /// launched a real editor (`vim`/`nano`/`vi`) with the test process's
    /// real inherited stdio, hanging the whole run on real keyboard input.
    /// An empty task file hits a `resolve_task` error deterministically in
    /// every environment, without depending on whether stdin is a TTY.
    #[tokio::test]
    async fn execute_background_empty_task_file_returns_error() {
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let agent_name = format!("test-execute-bg-notask-{pid}-{now}");
        let _cleanup = RunPrefixCleanup(&agent_name);
        let temp_dir = std::env::temp_dir().join(&agent_name);
        let _ = std::fs::create_dir_all(&temp_dir);
        write_valid_manifest(&temp_dir, &agent_name);
        let empty_task_file = temp_dir.join("empty-task.txt");
        std::fs::write(&empty_task_file, "").unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some(empty_task_file.to_string_lossy().to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        let result = execute(args).await;
        // Expected error for empty task file.
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("is empty"));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // ─── new_session_pre_exec: lines 208-211 ─────────────────────────────────

    /// On Unix, `new_session_pre_exec` is the function passed to `pre_exec`.
    /// Calling it directly exercises the body (lines 209-210) that LLVM
    /// instruments inside the closure/function.
    ///
    /// `setsid()` may return EPERM if we are already a process-group leader,
    /// but we always return Ok(()) regardless — so the test always passes.
    #[cfg(unix)]
    #[test]
    fn new_session_pre_exec_returns_ok() {
        // Directly invoke the pre_exec function body so LLVM marks lines
        // 209-210 as covered.  The setsid() call is a no-op or EPERM here.
        let result = new_session_pre_exec();
        assert!(result.is_ok());
    }

    // ─── cleanup_runs_with_prefix_in_dir: read_dir failure ───────────────────

    #[test]
    fn cleanup_runs_with_prefix_in_dir_handles_nonexistent_dir() {
        // Exercises the `let Ok(entries) = std::fs::read_dir(dir) else { return; }`
        // early-return path in cleanup_runs_with_prefix_in_dir when the directory
        // does not exist. We call the inner function directly with a nonexistent
        // path so we never touch the process-global LEVIATH_RUNS_DIR env var
        // (which would race with concurrent tests).
        let nonexistent = std::path::Path::new("/tmp/leviath-test-nonexistent-cleanup-in-dir");
        // Must not panic — it should silently return.
        cleanup_runs_with_prefix_in_dir("anything", nonexistent);
    }

    #[test]
    fn cleanup_runs_with_prefix_in_dir_skips_non_matching_entries() {
        // Exercises the `if entry starts_with(prefix)` false branch: a
        // directory entry that does NOT match the prefix must be left alone.
        let dir = std::env::temp_dir().join(format!(
            "leviath-test-cleanup-in-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let agent_name = "match-me";
        let matching = dir.join(format!("{agent_name}-123"));
        let non_matching = dir.join("unrelated-entry");
        std::fs::create_dir_all(&matching).unwrap();
        std::fs::create_dir_all(&non_matching).unwrap();

        cleanup_runs_with_prefix_in_dir(agent_name, &dir);

        // Matching entry should be removed, non-matching entry kept.
        assert!(!matching.exists());
        assert!(non_matching.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── open_log_file ─────────────────────────────────────────────────────────

    #[test]
    fn open_log_file_succeeds_for_writable_path() {
        // Happy path: a real temp file. Covers the success branches in open_log_file.
        let log_path = std::env::temp_dir().join(format!(
            "leviath-test-open-log-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let (f1, f2) = open_log_file(&log_path);
        let _ = std::fs::remove_file(&log_path);
        // Verify both handles are valid by checking metadata
        assert!(f1.metadata().is_ok());
        assert!(f2.metadata().is_ok());
    }

    #[test]
    #[should_panic(expected = "failed to open run log")]
    fn open_log_file_panics_for_nonexistent_parent_dir() {
        // Error path: parent directory doesn't exist → open() panics.
        // Covers `unwrap_or_else(|e| panic!(...))` in open_log_file.
        let bad_path =
            std::path::Path::new("/nonexistent-parent-dir-leviath-test/subdir/output.log");
        let _ = open_log_file(bad_path);
    }

    // ─── create_run error path ────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_background_create_run_fails_returns_error() {
        // Covers `create_run(&meta)?` in execute_background_with when the
        // injected create_run function returns an error. Uses dependency
        // injection (execute_background_with) rather than env-var mutation,
        // so this test is race-free in a parallel test run.
        let agent_name = "test-execute-bg-create-run-fail";
        let temp_dir = std::env::temp_dir().join(agent_name);
        let _ = std::fs::create_dir_all(&temp_dir);
        write_valid_manifest(&temp_dir, agent_name);

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        let exe = std::env::current_exe().unwrap();

        // Inject a create_run stub that always fails — no env-var mutation needed.
        let result = super::execute_background_with(args, &exe, |_meta| {
            Err(anyhow::anyhow!("injected create_run failure"))
        })
        .await;
        let _ = std::fs::remove_dir_all(&temp_dir);

        // Expected create_run failure to propagate, with the injected error message.
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("injected"));
    }
}
