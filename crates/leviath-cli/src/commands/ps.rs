//! `lev ps` - List background agent runs.

use clap::Args;
use crate::runstate::{list_runs, RunStatus};

#[derive(Args)]
pub struct PsArgs {
    /// Show only runs with this status (e.g. running, complete, error)
    #[arg(short, long)]
    pub status: Option<String>,
}

pub async fn execute(args: PsArgs) -> anyhow::Result<()> {
    let runs = list_runs();

    if runs.is_empty() {
        println!("No runs found. Start one with: lev run <agent> --task <task>");
        return Ok(());
    }

    // Header
    println!(
        "{:<40} {:<15} {:<20} {:<12} {:>6} {:>10} STARTED",
        "RUN ID", "AGENT", "STAGE", "STATUS", "ITER", "TOKENS"
    );
    println!("{}", "-".repeat(120));

    for run in runs {
        // Filter by status if requested
        if let Some(ref filter) = args.status {
            let status_str = run.status.to_string().to_lowercase();
            if !status_str.contains(&filter.to_lowercase()) {
                continue;
            }
        }

        let tokens = run.prompt_tokens + run.completion_tokens;
        let started = format_timestamp(run.started_at);
        let status_display = format_status(&run.status);

        println!(
            "{:<40} {:<15} {:<20} {:<12} {:>6} {:>10} {}",
            truncate(&run.run_id, 39),
            truncate(&run.agent_name, 14),
            truncate(&run.current_stage, 19),
            status_display,
            run.iteration,
            tokens,
            started,
        );
    }

    Ok(())
}

fn format_status(status: &RunStatus) -> String {
    match status {
        RunStatus::Starting => "Starting".to_string(),
        RunStatus::Running => "Running".to_string(),
        RunStatus::WaitingInput => "Waiting".to_string(),
        RunStatus::Complete => "Complete".to_string(),
        RunStatus::Error => "Error".to_string(),
        RunStatus::Cancelled => "Cancelled".to_string(),
    }
}

fn format_timestamp(ts: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let system_time = match UNIX_EPOCH.checked_add(Duration::from_secs(ts as u64)) {
        Some(t) => t,
        None => return "unknown".to_string(),
    };
    let elapsed: Duration = match system_time.elapsed() {
        Ok(e) => e,
        Err(_) => return "unknown".to_string(),
    };

    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
