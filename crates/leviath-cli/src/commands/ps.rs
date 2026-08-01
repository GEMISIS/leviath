//! `lev ps` - list the agents running in the shared-world daemon.
//!
//! Queries the daemon over its control socket and prints one line per run. The
//! query + formatting cores are tested here; the socket-path resolution + connect
//! live in the binary behind [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_runtime::components::AgentStatus;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::host::RunListEntry;

/// `lev ps --help`. Every status an operator can see, and what to do about it.
pub const PS_LONG_ABOUT: &str = "\
List agents running in the shared-world daemon.

Columns: RUN, STATUS, STAGE (with position when the blueprint has several),
ITER (iterations in the current stage), TOOLS (tool calls so far), and AGE.

AGE is how long since the run last actually moved - a new iteration, a new
stage, or a change of status. It is not the `updated_at` in meta.json, which
also advances on a 30-second heartbeat and so stays fresh on a wedged run.

Statuses:
  active     running a turn, or waiting on the model or a tool
  idle       spawned, not yet started
  paused     paused with `lev pause`; resume with `lev resume`
  waiting    blocked - see the reason after the colon
  complete   finished
  cancelled  cancelled with `lev kill`
  error      ended with the error shown

A `waiting` run says what it is blocked on. These need a person:
  tool approval  a tool call needs approving; answer with `lev respond`
  user prompt    the agent asked a question (ask_user_*); answer it
  taint gate     a call needs clearance for the data it touches
  checkpoint     a blueprint stage-boundary review

These do not - the run is parked on other work and resumes by itself:
  workers(n)     a fan-out parent, n workers still to finish
  children(n)    a stage holding for n spawned sub-agents

So `waiting: children(3)` alongside busy children is a healthy factory, while
`waiting: tool approval` is stopped until someone answers. Run with `--yolo` to
approve automatically, including for sub-agents and fan-out workers.";

/// Arguments for `lev ps`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct PsArgs {
    /// Print the raw listing as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// The status cell for a run: the status word, plus what it is waiting on when
/// that is the difference between "leave it alone" and "go answer it".
fn status_cell(entry: &RunListEntry) -> String {
    match (&entry.status, &entry.wait_reason) {
        (AgentStatus::Waiting, Some(reason)) => format!("waiting: {reason}"),
        (status, _) => status.to_string(),
    }
}

/// A compact age, in the largest unit that keeps the number small: `12s`, `4m`,
/// `3h`, `2d`. Negative deltas (a clock that moved backwards) read as `0s`.
fn humanize_age(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// The AGE cell: how long since the run last actually moved. A run that has not
/// persisted a snapshot yet has nothing to measure from and reads `-`.
fn age_cell(entry: &RunListEntry, now: i64) -> String {
    match entry.last_progress_at {
        Some(at) => humanize_age(now.saturating_sub(at)),
        None => "-".to_string(),
    }
}

/// The STAGE cell: the stage name, with its position when the blueprint has more
/// than one stage (`implement 2/4`).
fn stage_cell(entry: &RunListEntry) -> String {
    match (entry.stage_index, entry.num_stages) {
        (Some(i), Some(n)) if n > 1 => format!("{} {}/{}", entry.stage, i + 1, n),
        _ => entry.stage.clone(),
    }
}

/// Render a run listing as an aligned table (or a friendly note when empty).
///
/// `now` is unix seconds, passed in rather than read here so the output is
/// deterministic under test.
pub fn format_runs(runs: &[RunListEntry], now: i64) -> String {
    if runs.is_empty() {
        return "no agents running".to_string();
    }
    let headers = ["RUN", "STATUS", "STAGE", "ITER", "TOOLS", "AGE"];
    let rows: Vec<[String; 6]> = runs
        .iter()
        .map(|e| {
            [
                e.run_id.clone(),
                status_cell(e),
                stage_cell(e),
                e.iteration.to_string(),
                e.tool_calls.to_string(),
                age_cell(e, now),
            ]
        })
        .collect();

    // Column widths from the header and every cell, so nothing wraps under a
    // long run id or a long wait reason.
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }

    let render = |cells: &[String; 6]| {
        let mut line = String::new();
        for (i, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            // The last column is never padded, so lines have no trailing blanks.
            match i == cells.len() - 1 {
                true => line.push_str(cell),
                false => line.push_str(&format!("{cell:<width$}")),
            }
        }
        line
    };

    let header_row = headers.map(str::to_string);
    let table = std::iter::once(render(&header_row))
        .chain(rows.iter().map(render))
        .collect::<Vec<_>>()
        .join("\n");

    // The rows that will not move until somebody acts. Worth calling out under
    // the table: on a wide listing they are easy to miss among the healthy
    // `waiting: children(n)` rows they used to be indistinguishable from.
    let blocked = runs
        .iter()
        .filter(|e| e.wait_reason.as_ref().is_some_and(|r| r.needs_a_person()))
        .count();
    match blocked {
        0 => table,
        1 => format!("{table}\n\n1 run needs an answer: lev respond"),
        n => format!("{table}\n\n{n} runs need an answer: lev respond"),
    }
}

/// Query the daemon for its runs and print the listing.
pub async fn send_list(client: &ControlClient, args: &PsArgs) -> anyhow::Result<()> {
    match client.list().await {
        Ok(ControlResponse::List { runs }) => {
            match args.json {
                // `RunListEntry` is plain data with no map keys to reject, so
                // serializing it cannot fail.
                true => println!(
                    "{}",
                    serde_json::to_string_pretty(&runs).expect("a run listing serializes")
                ),
                false => println!("{}", format_runs(&runs, chrono::Utc::now().timestamp())),
            }
            Ok(())
        }
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

#[cfg(test)]
mod tests;
