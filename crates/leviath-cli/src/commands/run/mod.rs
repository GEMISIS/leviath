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

pub async fn execute(args: RunArgs) -> anyhow::Result<()> {
    if args.foreground {
        if args.count > 1 {
            anyhow::bail!("--count is not supported with --foreground");
        }
        return foreground::run_foreground(args).await;
    }

    // Background mode: create run state, spawn detached worker process(es)
    let path = args.path.as_deref().unwrap_or(".").to_string();
    let manifest_path = manifest::find_manifest(&path)?;

    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = manifest::parse_manifest(&manifest_content)?;

    // Resolve the task once (may launch an interactive editor) before spawning workers.
    let description = Some(blueprint.description.as_str());
    let task = session::resolve_task(&args.task, &blueprint.name, description)?;

    let workdir = std::env::current_dir()?;
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
        runstate::create_run(&meta)?;

        // Redirect the worker's stdout + stderr to the run's output.log
        let log_path = runstate::run_dir(&run_id).join("output.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let log_file2 = log_file.try_clone()?;

        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("Failed to locate current executable: {}", e))?;

        let mut cmd = std::process::Command::new(&exe);
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
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
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
        assert!(
            err.contains("count") || err.contains("foreground"),
            "Expected error about --count with --foreground, got: {}",
            err
        );
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

    /// Removes every run directory whose id starts with `agent_name-`,
    /// wherever `execute()` actually wrote them (respects `LEVIATH_RUNS_DIR`
    /// if set, same as `runstate::run_dir` itself).
    fn cleanup_runs_with_prefix(agent_name: &str) {
        let dir = runstate::runs_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
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

    #[tokio::test]
    async fn execute_background_happy_path_spawns_single_worker() {
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

        let result = execute(args).await;
        assert!(
            result.is_ok(),
            "expected background execute to succeed, got: {:?}",
            result.err()
        );

        let runs = runstate::list_runs();
        assert!(
            runs.iter().any(|m| m.agent_name == agent_name),
            "expected a run to have been created for {agent_name}"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn execute_background_happy_path_spawns_multiple_workers() {
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

        let result = execute(args).await;
        assert!(
            result.is_ok(),
            "expected multi-count background execute to succeed, got: {:?}",
            result.err()
        );

        let runs = runstate::list_runs();
        let matching = runs.iter().filter(|m| m.agent_name == agent_name).count();
        assert_eq!(matching, 3, "expected 3 runs to have been created");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn execute_worker_thin_wrapper_delegates_to_worker_module() {
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
}
