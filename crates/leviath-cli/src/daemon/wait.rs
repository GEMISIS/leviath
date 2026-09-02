//! Waiting for a run in the shared-world daemon to finish.
//!
//! `lev run` hands a run to the daemon and returns; this is the other half, for
//! the callers that want the answer: `lev run --wait`, and the MCP server's
//! `run` and `wait` tools. It follows the daemon's [`WorldEvent`] stream for
//! the run and reads the run's `meta.json` as a backstop, because the stream
//! alone is not enough: `CompleteInteractive` emits no `Completed` event, and a
//! slow consumer can have a frame dropped.
//!
//! The caller subscribes *before* spawning and passes the stream in, so no
//! event between the spawn and the subscription is lost. Events for the
//! watched run's descendants count too: a fan-out worker parked on an approval
//! has its own run id, and a caller that only watched the parent would see a
//! silent hang instead of the question.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use leviath_core::interaction::InteractionRequest;
use leviath_core::output::FinalOutput;
use leviath_core::run_meta::RunStatus;
use leviath_runtime::control_socket::{ControlClient, WorldEventStream};
use leviath_runtime::host::WorldEvent;
use leviath_runtime::persistence::run_status_for_label;

use crate::runstate::{is_terminal_status, read_final_output_in, read_meta_from, run_dir_in};

/// How many times in a row the daemon may drop the event stream without
/// delivering an event before the wait gives up. Every event resets the count,
/// so a run that lives through several restarts is followed through all of
/// them; only a daemon that comes back and drops the stream again at once,
/// repeatedly, ends the wait.
pub(crate) const MAX_SILENT_DROPS: u32 = 3;

/// How the wait ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WaitOutcome {
    /// The run reached a terminal status.
    Finished {
        /// The terminal status.
        status: RunStatus,
        /// What the run handed back, when it submitted anything.
        final_output: Option<FinalOutput>,
        /// Why it failed, from its record, when it did.
        error: Option<String>,
        /// Global Rhai tools the run (or a descendant) installed, by name.
        tools_installed: Vec<String>,
    },
    /// The run, or one of its descendants, is waiting for an answer.
    Interaction {
        /// The run that asked - a fan-out worker's id, not the parent's, when
        /// the worker asked - so an answer is addressed to the right run.
        run_id: String,
        /// The question.
        request: InteractionRequest,
    },
    /// The caller's deadline passed first. The run continues.
    TimedOut {
        /// Where the run stood on disk at the deadline.
        status: Option<RunStatus>,
    },
    /// The daemon's stream could not be followed any further.
    Lost {
        /// What happened.
        reason: String,
    },
}

/// The clocks in the loop, injectable so tests do not wait on them.
#[derive(Debug, Clone)]
pub(crate) struct WaitTiming {
    /// How often, absent an event, the run's `meta.json` status is re-read.
    pub tick: Duration,
    /// The pause before re-subscribing after a dropped stream.
    pub resubscribe_pause: Duration,
    /// The pause between attempts to read a finished run's answer off disk.
    pub output_retry: Duration,
    /// How many times to retry that read.
    pub output_retries: u32,
}

impl Default for WaitTiming {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            resubscribe_pause: Duration::from_millis(200),
            output_retry: Duration::from_millis(100),
            output_retries: 5,
        }
    }
}

/// Follow `run_id` on `stream` until it finishes, asks a question, or
/// `timeout` passes. `on_event` sees every event accepted for the run or its
/// descendants, in order, before the loop acts on it.
///
/// `+ Send` on the callback because the callers await this from spawned tasks.
pub(crate) async fn wait_for_run(
    control: &ControlClient,
    stream: WorldEventStream,
    run_id: &str,
    runs_dir: &Path,
    timeout: Option<Duration>,
    on_event: &mut (dyn FnMut(&WorldEvent) + Send),
) -> WaitOutcome {
    wait_for_run_with(
        control,
        stream,
        run_id,
        runs_dir,
        timeout,
        on_event,
        &WaitTiming::default(),
    )
    .await
}

/// [`wait_for_run`] with its clocks injected.
pub(crate) async fn wait_for_run_with(
    control: &ControlClient,
    mut stream: WorldEventStream,
    run_id: &str,
    runs_dir: &Path,
    timeout: Option<Duration>,
    on_event: &mut (dyn FnMut(&WorldEvent) + Send),
    timing: &WaitTiming,
) -> WaitOutcome {
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let mut lineage = Lineage::new(runs_dir, run_id);
    let mut tools_installed: Vec<String> = Vec::new();
    let mut silent_drops = 0u32;
    // A run that finished before the caller started waiting is answered from
    // disk at once rather than after the first tick.
    if let Some(done) = finished_on_disk(runs_dir, run_id, &tools_installed, timing).await {
        return done;
    }
    loop {
        tokio::select! {
            biased;
            event = stream.next() => {
                let Some(event) = event else {
                    // The daemon closed the stream: it is restarting, or it is
                    // gone. Follow the run onto the new daemon by subscribing
                    // again; the control client waits a restart out.
                    silent_drops += 1;
                    if silent_drops > MAX_SILENT_DROPS {
                        return WaitOutcome::Lost {
                            reason: "the daemon kept dropping its event stream without \
                                     delivering an event"
                                .to_string(),
                        };
                    }
                    tokio::time::sleep(timing.resubscribe_pause).await;
                    match control.subscribe().await {
                        Ok(fresh) => stream = fresh,
                        Err(e) => {
                            return WaitOutcome::Lost {
                                reason: format!("the daemon went away and did not come back ({e})"),
                            };
                        }
                    }
                    // The run may have finished while the stream was down.
                    if let Some(done) = finished_on_disk(runs_dir, run_id, &tools_installed, timing).await {
                        return done;
                    }
                    continue;
                };
                silent_drops = 0;
                if !lineage.accepts(event.run_id()) {
                    continue; // another run in the shared world
                }
                on_event(&event);
                match event {
                    WorldEvent::Completed { run_id: id, status, final_output, .. } if id == run_id => {
                        let status = run_status_for_label(&status)
                            .or_else(|| read_status(runs_dir, run_id))
                            .unwrap_or(RunStatus::Error);
                        return finished(runs_dir, run_id, status, final_output, tools_installed, timing).await;
                    }
                    WorldEvent::Interaction { run_id: id, request, .. } => {
                        return WaitOutcome::Interaction { run_id: id, request };
                    }
                    WorldEvent::ToolCallFinished { tool, ok, summary, .. }
                        if ok && tool == "install_tool" =>
                    {
                        if let Some(name) = installed_tool_name(&summary) {
                            tools_installed.push(name);
                        }
                    }
                    _ => {}
                }
                // `CompleteInteractive` emits no `Completed`, so the record is
                // consulted after every event as well as on the tick.
                if let Some(done) = finished_on_disk(runs_dir, run_id, &tools_installed, timing).await {
                    return done;
                }
            }
            _ = tokio::time::sleep(timing.tick) => {
                if let Some(done) = finished_on_disk(runs_dir, run_id, &tools_installed, timing).await {
                    return done;
                }
            }
            _ = until(deadline) => {
                return WaitOutcome::TimedOut { status: read_status(runs_dir, run_id) };
            }
        }
    }
}

/// Resolves at `deadline`, or never when there is none.
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The name in an `install_tool` result's `Installed tool '<name>'` prefix.
pub(crate) fn installed_tool_name(summary: &str) -> Option<String> {
    let rest = summary.strip_prefix("Installed tool '")?;
    let (name, _) = rest.split_once('\'')?;
    Some(name.to_string())
}

/// The run's status off disk, `None` while nothing has been persisted.
fn read_status(runs_dir: &Path, run_id: &str) -> Option<RunStatus> {
    read_meta_from(&run_dir_in(runs_dir, run_id))
        .ok()
        .map(|m| m.status)
}

/// `Finished` when the run's record says it is over, else `None`.
async fn finished_on_disk(
    runs_dir: &Path,
    run_id: &str,
    tools_installed: &[String],
    timing: &WaitTiming,
) -> Option<WaitOutcome> {
    let status = read_status(runs_dir, run_id).filter(is_terminal_status)?;
    Some(
        finished(
            runs_dir,
            run_id,
            status,
            None,
            tools_installed.to_vec(),
            timing,
        )
        .await,
    )
}

/// Build the `Finished` outcome, reading the answer off disk when the event
/// did not carry it.
async fn finished(
    runs_dir: &Path,
    run_id: &str,
    status: RunStatus,
    final_output: Option<FinalOutput>,
    tools_installed: Vec<String>,
    timing: &WaitTiming,
) -> WaitOutcome {
    let dir = run_dir_in(runs_dir, run_id);
    let final_output = match final_output {
        Some(output) => Some(output),
        None => read_output_retrying(&dir, timing).await,
    };
    let error = read_meta_from(&dir).ok().and_then(|m| m.error);
    WaitOutcome::Finished {
        status,
        final_output,
        error,
        tools_installed,
    }
}

/// The answer off disk, with a few short retries.
///
/// `Completed` fires the moment the run goes terminal, and the persist tick
/// that writes `meta.json` and the answer's sidecar has not necessarily run
/// yet. A record that is terminal and carries no answer descriptor ends the
/// retries early: the run said all it is going to.
async fn read_output_retrying(dir: &Path, timing: &WaitTiming) -> Option<FinalOutput> {
    for attempt in 0..=timing.output_retries {
        if let Ok(meta) = read_meta_from(dir) {
            if let Some(output) = read_final_output_in(dir, &meta) {
                return Some(output);
            }
            if is_terminal_status(&meta.status) && meta.final_output.is_none() {
                return None;
            }
        }
        if attempt < timing.output_retries {
            tokio::time::sleep(timing.output_retry).await;
        }
    }
    None
}

/// Which runs' events count as the watched run's: itself, and every run whose
/// `parent_run_id` chain reaches it.
///
/// Resolved off each run's `meta.json`, cached per id. A negative answer is
/// cached only when every record on the way was readable: a child whose
/// placeholder record is not on disk yet must be asked about again.
struct Lineage<'a> {
    runs_dir: &'a Path,
    watched: &'a str,
    cache: HashMap<String, bool>,
}

impl<'a> Lineage<'a> {
    fn new(runs_dir: &'a Path, watched: &'a str) -> Self {
        Self {
            runs_dir,
            watched,
            cache: HashMap::new(),
        }
    }

    fn accepts(&mut self, id: &str) -> bool {
        if id == self.watched {
            return true;
        }
        if let Some(&known) = self.cache.get(id) {
            return known;
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut current = id.to_string();
        let (descends, certain) = loop {
            if !seen.insert(current.clone()) {
                break (false, true); // a cycle in the records; not ours
            }
            match read_meta_from(&run_dir_in(self.runs_dir, &current)) {
                Ok(meta) => match meta.parent_run_id {
                    Some(parent) if parent == self.watched => break (true, true),
                    Some(parent) => current = parent,
                    None => break (false, true),
                },
                Err(_) => break (false, false),
            }
        };
        if certain {
            self.cache.insert(id.to_string(), descends);
        }
        descends
    }
}

#[cfg(test)]
#[path = "wait_tests.rs"]
pub(crate) mod tests;
