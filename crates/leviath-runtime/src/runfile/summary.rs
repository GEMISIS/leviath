//! A run file read as the [`RunMeta`] the listings show: `lev list`, the
//! dashboard, and the HTTP API's run views.
//!
//! The record is built the way the world builds it for a live run, from the
//! components the run would be placed with, so a run read off disk and the same
//! run in the world list the same.

use leviath_core::run_meta::RunMeta;

use super::error::RunFileError;
use super::reader::RunFileReader;
use crate::insert::place;
use crate::persistence::{RunMetaSources, RunPosition, build_run_meta};

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
    let ledger = place::stage_ledger(&state);
    let final_output = place::final_output(&state);
    Ok(build_run_meta(
        RunMetaSources {
            md: &place::run_metadata(spec, &state),
            state: &place::agent_state(spec, &state),
            totals: &place::token_totals(&state),
            flags: &place::outcome_flags(&state),
            final_output: final_output.as_ref(),
            stage_models: leviath_core::run_meta::stage_models_of(&ledger.0),
            parked: Default::default(),
        },
        RunPosition {
            stage_index: place::stage_index(spec, &state),
            now_secs: updated_at,
            last_progress_at: Some(updated_at),
            depth: usize::from(spec.placement.depth),
            max_child_depth: usize::from(spec.launch.max_depth),
            active: Some(place::run_clock(&state).0),
        },
    ))
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
