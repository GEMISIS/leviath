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
