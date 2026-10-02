//! A run file read as the [`RunMeta`] the listings show: `lev list`, the
//! dashboard, and the HTTP API's run views.
//!
//! The record is built the way the world builds it for a live run, from the
//! components the run would be placed with, so a run read off disk and the same
//! run in the world list the same. The context window and the stage ledger are
//! read the same way, for the readers that show those.

use leviath_core::run_meta::{ContextSnapshot, RunMeta, StageRecord};

use super::error::RunFileError;
use super::reader::RunFileReader;
use crate::insert::place;
use crate::persistence::{RunMetaSources, RunPosition, build_context_snapshot, build_run_meta};
use crate::spec::run_spec::RunSpec;
use crate::state::RunState;

/// The run in `reader`, as of its last step.
pub fn summary(reader: &RunFileReader) -> Result<RunMeta, RunFileError> {
    let spec = reader.spec();
    let state = reader.latest_state()?;
    // When the run last moved: its last step, or when it was resolved for one
    // that has taken none. The step decoded a moment ago, as part of the state.
    let updated_at = reader
        .deltas(state.seq, state.seq)
        .ok()
        .and_then(|deltas| deltas.last().map(|delta| delta.at))
        .unwrap_or(spec.created_at);
    Ok(summary_of(spec, &state, updated_at))
}

/// The run `spec` describes, as it stood in `state`, last moving at
/// `updated_at` (unix seconds). What [`summary`] reads for the last step, for
/// a reader that walks a run's history and wants the record at each step.
pub fn summary_of(spec: &RunSpec, state: &RunState, updated_at: i64) -> RunMeta {
    let ledger = place::stage_ledger(state);
    let final_output = place::final_output(state);
    let mut meta = build_run_meta(
        RunMetaSources {
            md: &place::run_metadata(spec, state),
            state: &place::agent_state(spec, state),
            totals: &place::token_totals(state),
            flags: &place::outcome_flags(state),
            final_output: final_output.as_ref(),
            stage_models: leviath_core::run_meta::stage_models_of(&ledger.0),
            parked: Default::default(),
        },
        RunPosition {
            stage_index: place::stage_index(spec, state),
            now_secs: updated_at,
            last_progress_at: Some(updated_at),
            depth: usize::from(spec.placement.depth),
            max_child_depth: usize::from(spec.launch.max_depth),
            active: Some(place::run_clock(state).0),
        },
    );
    // Why it is parked is what the state recorded when it last moved, unless
    // the machine it was last brought back on could not take it.
    meta.waiting_on = match &state.held {
        Some(issues) => Some(crate::restore::held_reason(issues)),
        None => state
            .wait_reason
            .as_ref()
            .map(leviath_core::run_meta::WaitReason::from),
    };
    meta
}

/// The run's context window in `state`, as a snapshot: each region shaped as
/// its graph declares it, with the state's entries in it.
pub fn context_snapshot(spec: &RunSpec, state: &RunState) -> ContextSnapshot {
    build_context_snapshot(
        &place::context_window(spec, state),
        state.cursor.stage.as_str(),
    )
}

/// The run's per-stage ledger in `state`: one record per stage it entered,
/// with its spend and visits.
pub fn stage_records(state: &RunState) -> Vec<StageRecord> {
    place::stage_ledger(state).0
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
