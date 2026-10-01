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
//! never run twice; a run choosing its next stage is asked again among the
//! same edges; a fan-out picks its workers back up by run id. See
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
    let state = reader.latest_state()?;
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

/// Place a run read back by [`read_for_resume`] into the world, with the live
/// handles binding its spec produced.
pub fn resume(world: &mut World, run: Resumable, bindings: crate::spec::env::Bindings) -> Entity {
    crate::insert::insert(world, run.spec, bindings, &run.state)
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
