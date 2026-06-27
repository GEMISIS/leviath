//! `lev stop` - Stop a background agent run.

use clap::Args;
use crate::runstate::{read_meta, write_meta, RunStatus};

#[derive(Args)]
pub struct StopArgs {
    /// Run ID to stop
    pub run_id: String,
}

pub async fn execute(args: StopArgs) -> anyhow::Result<()> {
    let mut meta = match read_meta(&args.run_id) {
        Ok(m) => m,
        Err(_) => anyhow::bail!(
            "Run '{}' not found. Check the run ID with: lev ps",
            args.run_id
        ),
    };

    match meta.status {
        RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled => {
            println!("Run '{}' is already finished (status: {}).", args.run_id, meta.status);
            return Ok(());
        }
        _ => {}
    }

    // Send SIGTERM to the worker process (Unix)
    #[cfg(unix)]
    {
        if meta.pid > 0 {
            let result = unsafe { libc::kill(meta.pid as libc::pid_t, libc::SIGTERM) };
            if result == 0 {
                println!("Sent SIGTERM to process {} (run {}).", meta.pid, args.run_id);
            } else {
                println!(
                    "Warning: kill({}, SIGTERM) failed — process may have already exited.",
                    meta.pid
                );
            }
        } else {
            println!("Warning: no PID recorded for run '{}'.", args.run_id);
        }
    }

    #[cfg(not(unix))]
    {
        println!("Note: process termination is not supported on this platform.");
    }

    // Update status to Cancelled in metadata
    meta.status = RunStatus::Cancelled;
    meta.touch();
    let _ = write_meta(&meta);

    println!("Run '{}' marked as Cancelled.", args.run_id);
    Ok(())
}
