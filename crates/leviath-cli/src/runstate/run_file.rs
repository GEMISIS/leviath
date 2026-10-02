//! A run directory's run file, as the CLI and the dashboard read it.
//!
//! A run is one file, `run.lvr`: its spec, the state it started in, a step per
//! change and a checkpoint every so often. The listings, the context and stage
//! views, the history the dashboard steps through and the edges its explorer
//! lights all start here.
//!
//! The file is read from its bytes and never opened for writing. A run the
//! daemon is still appending to can end in a half-written frame; the reader
//! drops that tail from what it holds, and opening the file the way the daemon
//! does would cut it off the file underneath the writer.

use std::path::{Path, PathBuf};

use leviath_core::run_meta::{ContextSnapshot, StageRecord};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::history::RunPoint;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::{Change, RunState};

/// Where the run file of the run in `dir` is.
pub(crate) fn path_in(dir: &Path) -> PathBuf {
    dir.join(leviath_core::files::RUN_FILE)
}

/// The run file in `dir`, read from its bytes.
pub(crate) fn open_in(dir: &Path) -> anyhow::Result<RunFileReader> {
    let path = path_in(dir);
    let bytes = std::fs::read(&path)?;
    Ok(RunFileReader::from_bytes(&path, bytes)?)
}

/// The run file in `dir` and the state of its last step, or `None` when the
/// directory has no run file this build reads.
pub(crate) fn latest_in(dir: &Path) -> Option<(RunFileReader, RunState)> {
    let reader = open_in(dir).ok()?;
    let state = reader.latest_state().ok()?;
    Some((reader, state))
}

/// The run's context window as of its last step.
pub(crate) fn context_in(dir: &Path) -> Option<ContextSnapshot> {
    let (reader, state) = latest_in(dir)?;
    Some(leviath_runtime::runfile::context_snapshot(
        reader.spec(),
        &state,
    ))
}

/// The run's per-stage ledger as of its last step.
pub(crate) fn stages_in(dir: &Path) -> Option<Vec<StageRecord>> {
    let (_, state) = latest_in(dir)?;
    Some(leviath_runtime::runfile::stage_records(&state))
}

/// What a run's file says about how it got where it is: the window at every
/// step that changed it, and every edge it took.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RunHistory {
    /// The window as the run started, then after each step that changed it,
    /// oldest first. Every record is redacted: a run's record names its
    /// webhook's secret, and nothing that shows a history has a use for it.
    pub(crate) points: Vec<RunPoint>,
    /// Each edge the run took, as `(from, to)`, in the order it took them.
    /// `None` when nothing says which edge a move followed.
    pub(crate) transitions: Option<Vec<(String, String)>>,
}

/// One point of a run's history.
fn point(spec: &RunSpec, state: &RunState, at: i64) -> RunPoint {
    RunPoint {
        meta: leviath_runtime::runfile::summary_of(spec, state, at).redacted(),
        context: leviath_runtime::runfile::context_snapshot(spec, state),
        at,
    }
}

/// The history of the run in `dir`, read off its run file: the run replayed
/// from the state it started in, one step at a time. `None` when the
/// directory has no run file this build reads, or one of its steps does not
/// decode.
pub(crate) fn history_in(dir: &Path) -> Option<RunHistory> {
    let reader = open_in(dir).ok()?;
    let spec = reader.spec();
    let mut state = reader.state_at(0).ok()?;
    let mut points = vec![point(spec, &state, spec.created_at)];
    let mut transitions = Vec::new();
    for delta in reader.deltas(state.seq + 1, reader.last_seq()).ok()? {
        delta.apply(&mut state);
        for taken in delta.transitions() {
            transitions.push((taken.from.to_string(), taken.to.to_string()));
        }
        if delta
            .changes
            .iter()
            .any(|c| matches!(c, Change::Context(_)))
        {
            points.push(point(spec, &state, delta.at));
        }
    }
    Some(RunHistory {
        points,
        transitions: Some(transitions),
    })
}

#[cfg(test)]
#[path = "run_file_reads_tests.rs"]
pub(crate) mod tests;
