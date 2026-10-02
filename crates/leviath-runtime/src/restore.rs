//! Resuming a run from its run file.
//!
//! A run's file holds the spec it was resolved to, the code that spec names,
//! and the state it was last in. Resuming it is [`read_for_resume`], then the
//! host binds the spec against this machine (a binding that fails is a typed
//! refusal naming what changed, never a quiet fall back), then [`resume`]
//! places it with [`insert`](crate::insert::insert). Nothing is resolved
//! again: the spec is the run's, as it was decided when it started.
//!
//! What the run was doing comes back with its state. A run that was waiting on
//! a model reply asks again; a tool batch in flight is dispatched again with
//! the results that had come back carried over, so a call that finished is
//! never run twice, and a question one of its calls put to a person is asked
//! again; a run stopped at a checkpoint has it asked again over the same
//! document; a run choosing its next stage is asked again among the same
//! edges; a fan-out picks its workers back up by run id, and a worker that
//! finished while the daemon was down is read from its own file. See
//! [`insert::place`](crate::insert::place) for each.
//!
//! When the daemon restarts, [`triage`] puts the runs it finds in the order
//! they come back in.

use std::cmp::Reverse;

use bevy_ecs::prelude::*;

use crate::state::{PipelinePhase, RunStatus};

/// A run read back from its run file, ready to bind and insert.
#[derive(Debug)]
pub struct Resumable {
    /// The run's spec.
    pub spec: std::sync::Arc<crate::spec::run_spec::RunSpec>,
    /// Its state as of its last step.
    pub state: crate::state::RunState,
    /// The code its spec names, for binding.
    pub code: crate::spec::env::CodeFiles,
    /// The files it was given or made, by digest, for the store its tools
    /// read from. A file whose bytes do not read is left out.
    pub blobs: std::collections::BTreeMap<crate::spec::names::Digest, Vec<u8>>,
    /// How many questions it has put to a person, answered or still open,
    /// so a question it asks again gets an id of its own.
    pub asked: u64,
}

/// Read the run in `run_dir` back from its run file.
///
/// `Ok(None)` when the directory holds no run file. A run file that is there
/// and cannot be read is an error naming the file and what is wrong with it.
pub fn read_for_resume(
    run_dir: &std::path::Path,
) -> Result<Option<Resumable>, crate::runfile::RunFileError> {
    let path = run_dir.join(leviath_core::files::RUN_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let reader = crate::runfile::RunFileReader::open(&path)?;
    let mut state = reader.latest_state()?;
    if let (Some(fan_out), Some(runs_dir)) = (state.fan_out.as_mut(), run_dir.parent()) {
        settle_finished_workers(fan_out, runs_dir);
    }
    let blobs = reader
        .blob_digests()
        .map(|d| (d.clone(), reader.blob(d).ok().flatten().unwrap_or_default()))
        .collect();
    // Every step decoded on the way to the state, so these read too.
    let answered = reader
        .deltas(1, reader.last_seq())
        .unwrap_or_default()
        .iter()
        .flat_map(|d| &d.events)
        .filter(|e| matches!(e, crate::state::RunEvent::Answered { .. }))
        .count();
    let open = state.interactions.len() + state.pending.as_ref().map_or(0, |b| b.calls.len());
    Ok(Some(Resumable {
        asked: u64::try_from(answered + open).unwrap_or(u64::MAX),
        code: reader.code_files()?,
        blobs,
        state,
        spec: std::sync::Arc::new(reader.spec().clone()),
    }))
}

/// Take each fan-out worker that finished out of the running ones, with what
/// it finished with, read from its own run file under `runs_dir`. A worker's
/// end is recorded on its own file before its parent records reaping it, so a
/// parent that stopped in between still lists it as running; a finished run
/// is not brought back, so this is where its result is found. A worker whose
/// file does not read stays running, and placing the parent counts it failed
/// when it is not in the world either.
fn settle_finished_workers(fan_out: &mut crate::state::FanOutState, runs_dir: &std::path::Path) {
    let active = std::mem::take(&mut fan_out.active);
    for (item, run) in active {
        let reader = crate::runfile::RunFileReader::open(
            &runs_dir
                .join(run.as_str())
                .join(leviath_core::files::RUN_FILE),
        )
        .ok();
        let finished = reader
            .and_then(|reader| reader.latest_state().ok().map(|state| (state, reader)))
            .and_then(|(state, reader)| worker_result(reader.spec(), &state));
        match finished {
            Some(Ok(summary)) => fan_out.done.push((item, summary)),
            Some(Err(why)) => fan_out.failed.push((item, why)),
            None => fan_out.active.push((item, run)),
        }
    }
}

/// What a worker finished with, or `None` while it is still going: as the
/// parent reads a worker it reaps live (its final output, else its last
/// reply, unless its stage requires an output it did not give).
fn worker_result(
    spec: &crate::spec::run_spec::RunSpec,
    state: &crate::state::RunState,
) -> Option<Result<String, String>> {
    match &state.status {
        RunStatus::Complete => Some(match &state.final_output {
            Some(out) => Ok(out.content.clone()),
            None if spec
                .graph
                .stage(state.cursor.stage.as_str())
                .is_some_and(|s| s.require_output) =>
            {
                Err("worker finished without the final output its stage requires".to_string())
            }
            None => Ok(last_reply(state)),
        }),
        RunStatus::Error(message) => Some(Err(message.clone())),
        RunStatus::Cancelled => Some(Err("worker cancelled".to_string())),
        _ => None,
    }
}

/// The text of the last model turn in a run's context.
fn last_reply(state: &crate::state::RunState) -> String {
    state
        .context
        .regions
        .iter()
        .flat_map(|r| &r.entries)
        .filter(|e| matches!(e.kind, crate::state::EntryKind::AssistantTurn(_)))
        .max_by_key(|e| e.timestamp)
        .map(|e| e.text.clone())
        .unwrap_or_default()
}

/// The result a call gets when the daemon died while it was running. Its
/// effect may or may not have landed (the command may even still be running),
/// so it is not run again: the model is told to check before it runs
/// side-effecting work again.
pub const INTERRUPTED_TOOL_RESULT: &str = "[error] interrupted: the daemon restarted while this tool call was executing and its \
     result was lost. Verify whether it took effect before re-running side-effecting work.";

/// The file a daemon keeps in its runs directory while it runs, and removes
/// when it stops cleanly. Finding it when starting means the last daemon died.
const SESSION_MARK: &str = ".daemon-running";

/// Start a daemon's session over `runs_dir`, and say whether the last one
/// died without stopping cleanly (it left its mark behind).
pub fn begin_session(runs_dir: &std::path::Path) -> bool {
    let mark = runs_dir.join(SESSION_MARK);
    let crashed = mark.exists();
    let _ = std::fs::create_dir_all(runs_dir);
    let _ = std::fs::write(&mark, b"");
    crashed
}

/// End a daemon's session over `runs_dir` cleanly: every call it had running
/// was stopped, so a restart may run them again.
pub fn end_session(runs_dir: &std::path::Path) {
    let _ = std::fs::remove_file(runs_dir.join(SESSION_MARK));
}

/// Settle the calls a run had running when the daemon died: each call of its
/// batch that had not finished gets [`INTERRUPTED_TOOL_RESULT`] (naming the
/// run's children for a call that starts one), so it is not run again. A
/// batch stopped on a question to a person is left to ask it again: the
/// question is asked before anything else in a batch runs, so nothing in it
/// was running.
pub fn interrupt_in_flight(state: &mut crate::state::RunState) {
    let children: Vec<String> = state.children.iter().map(ToString::to_string).collect();
    let Some(batch) = state.pending.as_mut() else {
        return;
    };
    let asking = batch.calls.iter().any(|call| {
        !batch.done.contains_key(&call.id)
            && crate::dynamic_interaction::BLOCKING_INTERACTION_TOOLS
                .contains(&leviath_tools::canonical_tool_name(&call.name))
    });
    if asking {
        return;
    }
    for call in &batch.calls {
        batch
            .done
            .entry(call.id.clone())
            .or_insert_with(|| crate::state::ToolResultState {
                text: interrupted_result(&call.name, &children),
                is_error: true,
            });
    }
}

/// The stand-in for one interrupted call: the base text, and for a call that
/// starts sub-agents in a run that has some, the child runs to check before
/// starting more.
fn interrupted_result(tool_name: &str, children: &[String]) -> String {
    match leviath_tools::is_subagent_tool(tool_name) && !children.is_empty() {
        true => format!(
            "{INTERRUPTED_TOOL_RESULT} This run already has child agent runs: {}; check them \
             with check_agent before spawning again.",
            children.join(", ")
        ),
        false => INTERRUPTED_TOOL_RESULT.to_string(),
    }
}

/// Place a run read back by [`read_for_resume`] into the world, with the live
/// handles binding its spec produced. A run that was held is held no longer.
pub fn resume(
    world: &mut World,
    mut run: Resumable,
    bindings: crate::spec::env::Bindings,
) -> Entity {
    run.state.held = None;
    crate::insert::insert(world, run.spec, bindings, &run.state)
}

/// Why a run is held, as the listings say it: the machine changed, and what
/// to put back, one problem after another.
pub fn held_reason(
    issues: &crate::spec::issues::SpawnIssues,
) -> leviath_core::run_meta::WaitReason {
    let problems: Vec<String> = issues.iter().map(ToString::to_string).collect();
    leviath_core::run_meta::WaitReason::NeedsSetup {
        blocker: leviath_core::run_meta::SetupBlocker::MachineChanged,
        remedy: format!(
            "{}; put that back, then `lev resume` this run or restart the daemon",
            problems.join("; ")
        ),
    }
}

/// The listing row of a run held because this machine cannot take it back
/// as it stands: paused, with [`held_reason`] as its wait reason, and the rest
/// as its state last recorded.
pub fn held_entry(
    spec: &crate::spec::run_spec::RunSpec,
    state: &crate::state::RunState,
    issues: &crate::spec::issues::SpawnIssues,
) -> crate::host::RunListEntry {
    use crate::insert::place;
    let md = place::run_metadata(spec, state);
    let flags = place::outcome_flags(state).0;
    crate::host::RunListEntry {
        run_id: md.run_id,
        title: md.title,
        status: crate::components::AgentStatus::Paused,
        wait_reason: Some(held_reason(issues)),
        stage: state.cursor.stage.to_string(),
        stage_index: Some(place::stage_index(spec, state)),
        num_stages: Some(md.num_stages),
        iteration: state.cursor.iteration as usize,
        tool_calls: state.totals.tool_calls as usize,
        last_progress_at: None,
        started_at: Some(md.started_at),
        active: Some(place::run_clock(state).0),
        unattended: md.unattended,
        yolo_profile: md.yolo_profile,
        empty_output: false,
        splits_degraded: flags.splits_degraded,
        broken_scripts: flags.broken_scripts,
        read_paths: md.read_paths,
        has_final_output: state.final_output.is_some(),
        may_never_finish: spec.warnings().iter().map(ToString::to_string).collect(),
    }
}

/// How urgently a run should come back on restart. Ordered so a higher value
/// comes back first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RestorePriority {
    /// Can make no progress the moment it is back: waiting on a person,
    /// paused, or a parent waiting on its fan-out workers.
    Blocked,
    /// Has work to do the moment it is back: a model call to make again or
    /// tool calls to dispatch.
    Active,
}

/// How a run should come back on restart, or `None` for one that has
/// finished. A run that was cancelled stays stopped: somebody stopped it on
/// purpose, and restarting the daemon is not them changing their mind.
pub(crate) fn classify(state: &crate::state::RunState) -> Option<RestorePriority> {
    match &state.status {
        RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled => None,
        RunStatus::Paused | RunStatus::Waiting => Some(RestorePriority::Blocked),
        RunStatus::Idle | RunStatus::Active => match state.phase {
            PipelinePhase::FanOut
            | PipelinePhase::AwaitingPerson
            | PipelinePhase::Paused
            | PipelinePhase::WaitingForChildren => Some(RestorePriority::Blocked),
            _ => Some(RestorePriority::Active),
        },
    }
}

/// The runs to bring back on restart, in the order to bring them back in.
///
/// Finished and cancelled runs are left out. A child comes back before the
/// run that started it, since a parent waiting on fan-out workers picks them
/// up by run id as it is placed; among runs at the same depth, the ones with
/// work to do come first, then the most recently started.
///
/// `of` reads the run out of each item, so a caller can carry its own
/// details along with each run.
pub fn triage<T>(runs: Vec<T>, of: impl Fn(&T) -> &Resumable) -> Vec<T> {
    let mut ranked: Vec<(RestorePriority, T)> = runs
        .into_iter()
        .filter_map(|item| classify(&of(&item).state).map(|p| (p, item)))
        .collect();
    ranked.sort_by_key(|(priority, item)| {
        let run = of(item);
        (
            Reverse(run.spec.placement.depth),
            Reverse(*priority),
            Reverse(run.spec.created_at),
        )
    });
    ranked.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod resume_tests;
