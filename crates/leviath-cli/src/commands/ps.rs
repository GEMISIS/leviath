//! `lev ps` - list the agents running in the shared-world daemon.
//!
//! Queries the daemon over its control socket and prints one line per run. The
//! query + formatting cores are tested here; the socket-path resolution + connect
//! live in the binary behind [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_core::run_meta::{RunMeta, RunStatus};
use leviath_runtime::components::AgentStatus;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::host::{DaemonHealth, RunListEntry};
use serde::{Deserialize, Serialize};

use crate::runstate;

/// `lev ps --help`. Every status an operator can see, and what to do about it.
pub const PS_LONG_ABOUT: &str = "\
List agents running in the shared-world daemon.

Columns: RUN, STATUS, STAGE (with position when the blueprint has several),
ITER (iterations in the current stage), TOOLS (tool calls so far), and AGE.

READS appears only when some listed run's blueprint declares [read_paths], and
reads granted/declared. A blueprint declaring paths outside its workdir is not
the same as being allowed to read them: your config.toml has to grant them too,
so `0/2` means the run is up and every such read will be refused. `lev validate
<agent>` names the entries and prints the stanza to add.

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

A finished run marked `(no output)` changed no files, though its agent had a
tool to change them with. Usually the work went through the shell, which the
framework cannot see: edits made with `sed -i`, `tee` or a redirect are not
recorded, so re-apply them with `edit_file` or `write_file`. Agents that never
had a file-writing tool - a router, a researcher - are never marked this way.

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
approve automatically, including for sub-agents and fan-out workers.

A run stays listed for a few minutes after it finishes, so a script polling on
an interval learns how a run ended rather than finding it gone. Set
`[limits] finished_retention_secs` to change the window, or 0 to drop a run the
moment it finishes. The record is held in memory, so a daemon restart clears it;
`meta.json` and the REST API keep the durable copy.

An `out of service` block under the table lists providers the daemon has stopped
sending work to, because each failed several times in a row for something only
you can fix: an account out of credits, or a key that was rejected. Runs move to
the next provider a stage lists (or one from `[providers] fallback_order`); a run
with none left is failed rather than left waiting. Each entry says how long until
that provider is tried again, and topping up the account needs no restart.

A `lanes:` line under the table means the daemon itself is worth a look. It
shows the tool lane's occupancy - batches running, parked on a wait, and queued
behind them - and, if the daemon has stopped getting anywhere, how many re-drive
cycles it has gone without a single run moving. A run parked on a wait costs the
lane nothing, so `parked` is not a problem on its own; `queued` with no progress
is.

--json prints {\"runs\": [...], \"finished\": [...], \"health\": {...}}, keeping
finished runs apart from the ones the daemon is still hosting.

--all adds a NOT RUNNING block, read from the runs dir rather than the daemon's
memory. The retention window above covers the minutes after a run ends; this
covers the rest of time, and survives a daemon restart. A row marked
`(abandoned)` claims on disk to be running, is not held by the daemon, and has
not moved in five minutes - clear it with `lev cancel --force <run-id>`.

With --all the daemon being down is reported rather than fatal, and nothing is
marked abandoned in that case, because an unreachable daemon looks exactly like
every run dying at once. --all --json adds \"daemon_reachable\" and
\"not_running\"; without --all the JSON is unchanged. Reading the runs dir costs
a file per run and nothing prunes it, so poll --all less often than plain ps.";

/// Arguments for `lev ps`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct PsArgs {
    /// Print the raw listing as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Also list runs on disk that the daemon is not hosting, including
    /// finished ones. For reconciling an external queue against Leviath.
    #[arg(long)]
    pub all: bool,
}

/// How many `NOT RUNNING` rows the table shows before it summarizes the rest.
///
/// The table is for a person, and a long-lived runs dir holds thousands. `--json`
/// is uncapped, because that is what a reconciler reads.
const OFFLINE_TABLE_LIMIT: usize = 20;

/// One run that exists on disk but which the daemon is not currently hosting.
///
/// Deliberately not a [`RunListEntry`]. That type describes a live agent, and
/// there is no honest way to turn a persisted [`RunStatus`] back into an
/// `AgentStatus`: `Starting` and `CompleteInteractive` have no counterpart, and
/// `Idle`/`Active` both collapse to `Running` on the way out. Inventing a live
/// status for a run nobody is running is the exact kind of convenient lie that
/// made issue #202 hard to diagnose, so the two sources stay in two arrays, each
/// honest about where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfflineRun {
    /// The run id.
    pub run_id: String,
    /// The status recorded on disk, verbatim.
    pub status: RunStatus,
    /// The recorded error, for a run that ended badly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix seconds when the run started.
    pub started_at: i64,
    /// Unix seconds of the last snapshot, heartbeat included. Not progress.
    pub updated_at: i64,
    /// Unix seconds when the run last actually moved, when it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<i64>,
    /// Whether the run finished having modified nothing, when it could have.
    #[serde(default)]
    pub empty_output: bool,
    /// Disk says this run is still going, and the daemon is not hosting it, and
    /// it has not moved in a long time. See [`runstate::looks_abandoned`].
    ///
    /// Never true when the daemon did not answer: an unreachable daemon looks
    /// exactly like every run dying at once.
    pub abandoned: bool,
}

/// The runs on disk that `live` does not account for, newest first.
///
/// `live` is `None` when the daemon gave no answer, in which case every run on
/// disk is reported (there is no live set to subtract) and none is judged.
pub fn offline_runs(
    on_disk: Vec<RunMeta>,
    live: Option<&std::collections::HashSet<String>>,
    now: i64,
) -> Vec<OfflineRun> {
    on_disk
        .into_iter()
        .filter(|m| !live.is_some_and(|l| l.contains(&m.run_id)))
        .map(|m| OfflineRun {
            abandoned: runstate::looks_abandoned(&m, live, now),
            run_id: m.run_id,
            status: m.status,
            error: m.error,
            started_at: m.started_at,
            updated_at: m.updated_at,
            last_progress_at: m.last_progress_at,
            empty_output: m.flags.empty_output,
        })
        .collect()
}

/// The status cell for a run the daemon is not hosting: the persisted status,
/// plus why it is worth looking at.
fn offline_status_cell(run: &OfflineRun) -> String {
    let status = run.status.to_string().to_lowercase();
    if run.abandoned {
        return format!("{status} (abandoned)");
    }
    match run.empty_output {
        true => format!("{status} (no output)"),
        false => status,
    }
}

/// Render the `NOT RUNNING` block. `None` when there is nothing to show.
pub fn format_offline(runs: &[OfflineRun], now: i64) -> Option<String> {
    if runs.is_empty() {
        return None;
    }
    let shown = runs.len().min(OFFLINE_TABLE_LIMIT);
    let headers = ["RUN", "STATUS", "LAST MOVED"];
    let rows: Vec<[String; 3]> = runs[..shown]
        .iter()
        .map(|r| {
            [
                r.run_id.clone(),
                offline_status_cell(r),
                humanize_age(now.saturating_sub(r.last_progress_at.unwrap_or(r.updated_at))),
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let render = |cells: &[String; 3]| {
        let mut line = String::new();
        for (i, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            match i == cells.len() - 1 {
                true => line.push_str(cell),
                false => line.push_str(&format!("{cell:<width$}")),
            }
        }
        line
    };

    let header_row = headers.map(str::to_string);
    let mut out = std::iter::once("NOT RUNNING".to_string())
        .chain(std::iter::once(render(&header_row)))
        .chain(rows.iter().map(render))
        .collect::<Vec<_>>()
        .join("\n");
    if runs.len() > shown {
        out.push_str(&format!("\n+{} older", runs.len() - shown));
    }
    Some(out)
}

/// The status cell for a run: the status word, plus what it is waiting on when
/// that is the difference between "leave it alone" and "go answer it", or a
/// note that a finished run has nothing to show for itself.
///
/// A run that ends having changed nothing looks identical to a successful one
/// from the outside, which is how a whole batch of them can go unnoticed - the
/// failure that produced issue #107 in the first place.
fn status_cell(entry: &RunListEntry) -> String {
    match (&entry.status, &entry.wait_reason) {
        (AgentStatus::Waiting, Some(reason)) => format!("waiting: {reason}"),
        (status, _) if entry.empty_output => format!("{status} (no output)"),
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

/// The providers currently out of service, with why and when each is retried.
///
/// This is the line that would have answered issue #201 on sight. Ten runs
/// dying in a row produced ten identical error rows and nothing that said "the
/// OpenRouter account is empty", so the shape of the problem was invisible from
/// the listing.
fn providers_footer(health: &DaemonHealth) -> Option<String> {
    if health.providers_down.is_empty() {
        return None;
    }
    let each = health
        .providers_down
        .iter()
        .map(|c| {
            format!(
                "  {} ({}, {} failures) - retrying in {}",
                c.provider,
                c.reason.label(),
                c.consecutive_failures,
                humanize_age(c.retry_in_secs as i64)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let noun = match health.providers_down.len() {
        1 => "provider is",
        _ => "providers are",
    };
    Some(format!(
        "{} {noun} out of service:\n{each}",
        health.providers_down.len()
    ))
}

/// The READS cell: how many of the blueprint's `[read_paths]` entries the
/// config granted, over how many it declared. `-` for a run that declared none,
/// which is what nearly every agent does.
///
/// `0/2` is the shape worth spotting: the run is up and looks healthy, and
/// every read it was designed to make outside its workdir will be refused.
fn reads_cell(entry: &RunListEntry) -> String {
    match entry.read_paths {
        Some(counts) => format!("{}/{}", counts.granted, counts.declared),
        None => "-".to_string(),
    }
}

/// The daemon-wide footer: what the tool lane is holding, and whether the daemon
/// as a whole has stopped getting anywhere.
///
/// Absent while everything is healthy, so an ordinary listing stays a table and
/// nothing else. A lane at capacity is worth mentioning; a dead-cycle streak is
/// worth mentioning loudly, because every row above it can look busy while the
/// factory as a whole has not moved in hours (issue #191).
fn health_footer(health: &DaemonHealth) -> Option<String> {
    let saturated = health.tools_busy >= health.tools_workers && health.tools_queued > 0;
    if !saturated && health.dead_cycles == 0 {
        return None;
    }
    let mut line = format!(
        "lanes: tools {}/{} busy",
        health.tools_busy, health.tools_workers
    );
    if health.tools_parked > 0 {
        line.push_str(&format!(", {} parked", health.tools_parked));
    }
    if health.tools_queued > 0 {
        line.push_str(&format!(", {} queued", health.tools_queued));
    }
    if health.dead_cycles > 0 {
        let seconds = health.dead_cycles as i64 * health.redrive_secs as i64;
        line.push_str(&format!(
            "  ·  no progress for {} cycles ({})",
            health.dead_cycles,
            humanize_age(seconds)
        ));
    }
    Some(line)
}

/// Render a run listing as an aligned table (or a friendly note when empty),
/// with the daemon's own health underneath when it has something to say.
///
/// `finished` are runs the daemon has unloaded but still remembers. They are
/// listed after the live ones rather than left out, because "the run I started
/// died on its first inference" and "there is no such run" are the two answers
/// issue #205's scheduler could not tell apart, and an empty table said the
/// second when it meant the first.
///
/// `now` is unix seconds, passed in rather than read here so the output is
/// deterministic under test.
pub fn format_runs(
    runs: &[RunListEntry],
    finished: &[RunListEntry],
    health: &DaemonHealth,
    now: i64,
) -> String {
    if runs.is_empty() && finished.is_empty() {
        // "no agents running" on its own is the most misleading thing this
        // command can say while a provider is down: it is what an operator sees
        // once even the finished records have aged out, and it reads as an idle
        // daemon rather than a factory that cannot start anything (issue #201).
        // Say why the list is empty.
        return match providers_footer(health) {
            Some(footer) => format!("no agents running\n\n{footer}"),
            None => "no agents running".to_string(),
        };
    }
    // READS only appears when some run has `[read_paths]` to report, which is
    // nearly never: an extra column of dashes on every ordinary listing would
    // cost every reader something to buy the rare reader nothing.
    let show_reads = runs.iter().chain(finished).any(|e| e.read_paths.is_some());
    let mut headers = vec!["RUN", "STATUS", "STAGE", "ITER", "TOOLS", "AGE"];
    if show_reads {
        headers.push("READS");
    }
    let rows: Vec<Vec<String>> = runs
        .iter()
        .chain(finished)
        .map(|e| {
            let mut cells = vec![
                e.run_id.clone(),
                status_cell(e),
                stage_cell(e),
                e.iteration.to_string(),
                e.tool_calls.to_string(),
                age_cell(e, now),
            ];
            if show_reads {
                cells.push(reads_cell(e));
            }
            cells
        })
        .collect();

    // Column widths from the header and every cell, so nothing wraps under a
    // long run id or a long wait reason.
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }

    let render = |cells: &Vec<String>| {
        let mut line = String::new();
        for (i, (cell, width)) in cells.iter().zip(&widths).enumerate() {
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

    let header_row: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
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
    let mut out = match blocked {
        0 => table,
        1 => format!("{table}\n\n1 run needs an answer: lev respond"),
        n => format!("{table}\n\n{n} runs need an answer: lev respond"),
    };
    if let Some(footer) = providers_footer(health) {
        out.push_str(&format!("\n\n{footer}"));
    }
    if let Some(footer) = health_footer(health) {
        out.push_str(&format!("\n\n{footer}"));
    }
    out
}

/// Print the live listing, optionally followed by the runs on disk the daemon is
/// not hosting. Pure formatting/serialization, so the shape is testable without
/// a daemon.
fn print_listing(
    runs: &[RunListEntry],
    finished: &[RunListEntry],
    health: &DaemonHealth,
    offline: Option<&[OfflineRun]>,
    daemon_reachable: bool,
    args: &PsArgs,
    now: i64,
) {
    if args.json {
        let mut body = serde_json::json!({ "runs": runs, "finished": finished, "health": health });
        if let Some(offline) = offline {
            // Only `--all` adds keys, so a plain `--json` keeps the exact shape
            // it had before this flag existed.
            body["daemon_reachable"] = serde_json::json!(daemon_reachable);
            body["not_running"] = serde_json::json!(offline);
        }
        // Plain data with no map keys to reject, so serializing cannot fail.
        println!(
            "{}",
            serde_json::to_string_pretty(&body).expect("a run listing serializes")
        );
        return;
    }
    if daemon_reachable {
        println!("{}", format_runs(runs, finished, health, now));
    } else {
        println!("the leviath daemon is not reachable; showing the runs dir only");
    }
    if let Some(block) = offline.and_then(|o| format_offline(o, now)) {
        println!("\n{block}");
    }
}

/// Query the daemon for its runs and print the listing.
///
/// With `--all`, an unreachable daemon is reported rather than fatal. A harness
/// polling on an interval will eventually catch the daemon restarting, and the
/// whole point of the flag is to be the thing it reconciles against: failing
/// there, or reporting an empty live set, would tell it every run had died at
/// once. Without `--all` the old behavior stands, because a listing of live runs
/// with no daemon to list them is simply an error.
pub async fn send_list(client: &ControlClient, args: &PsArgs) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    match (client.list().await, args.all) {
        (
            Ok(ControlResponse::List {
                runs,
                finished,
                health,
            }),
            all,
        ) => {
            // Both halves of the daemon's answer are already on screen, so the
            // disk block subtracts both rather than listing them twice. A run in
            // `finished` is terminal on disk anyway, so this cannot change an
            // abandoned verdict, only avoid a duplicate row.
            let shown: std::collections::HashSet<String> = runs
                .iter()
                .chain(finished.iter())
                .map(|r| r.run_id.clone())
                .collect();
            let offline = all.then(|| offline_runs(runstate::list_runs(), Some(&shown), now));
            print_listing(
                &runs,
                &finished,
                &health,
                offline.as_deref(),
                true,
                args,
                now,
            );
            Ok(())
        }
        (Ok(other), _) => bail!("unexpected daemon response: {other:?}"),
        (Err(_), true) => {
            let offline = offline_runs(runstate::list_runs(), None, now);
            print_listing(
                &[],
                &[],
                &DaemonHealth::default(),
                Some(&offline),
                false,
                args,
                now,
            );
            Ok(())
        }
        (Err(e), false) => {
            bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`")
        }
    }
}

#[cfg(test)]
mod tests;
