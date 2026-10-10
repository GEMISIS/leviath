//! A run's file, as every per-run read on this server opens it.
//!
//! A run is one file, `run.lvr`: its spec, the state it started in, a delta
//! per step and a checkpoint every so often. Every read that answers about one
//! run (its context, its stages, its history, what it asked and what it ran)
//! starts here, so a run reads the same on every route and on both surfaces.
//!
//! The file is read from bytes and never opened for writing: a run the daemon
//! is still appending to can end in a half-written frame, and the reader drops
//! that tail from what it holds without cutting it off the file underneath the
//! writer.
//!
//! A run directory in the older many-file layout has no run file of its own
//! until the daemon converts it, which it does for every such directory when it
//! starts. This server reads only run files: an unconverted directory answers
//! as a run whose file cannot be read, and the message says that a daemon
//! start converts it.

use std::ops::ControlFlow;
use std::path::PathBuf;

use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::state::{Cursor, RunState, StateDelta};

use super::error::ServeError;
use crate::runstate;

/// Where a run's file is.
pub(crate) fn path(run_id: &str) -> PathBuf {
    runstate::run_dir(run_id).join(leviath_core::files::RUN_FILE)
}

/// The run's file, or `None` when the run has none.
///
/// A file that is there and will not read is an error rather than a miss: the
/// run exists, and saying it does not would send a client looking elsewhere.
pub(crate) fn open(run_id: &str) -> Result<Option<RunFileReader>, ServeError> {
    let path = path(run_id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(run_id, &e)),
    };
    RunFileReader::from_bytes(&path, bytes)
        .map(Some)
        .map_err(|e| unreadable(run_id, &e))
}

/// The run's file, or a miss naming the run.
pub(crate) fn require(run_id: &str) -> Result<RunFileReader, ServeError> {
    open(run_id)?.ok_or_else(|| ServeError::NotFound(format!("Run '{run_id}' not found")))
}

/// A run file that would not read, said so that whoever reads it knows what
/// to do about one in the older layout.
pub(crate) fn unreadable(run_id: &str, e: &dyn std::fmt::Display) -> ServeError {
    ServeError::Internal(format!(
        "Run '{run_id}' has a run file this server cannot read ({e}). A run directory \
         from an older release is converted when the daemon starts."
    ))
}

/// One step of a run, with where the run was before it and the state after.
pub(crate) struct Step<'a> {
    /// The step itself.
    pub(crate) delta: &'a StateDelta,
    /// Where the run was in its graph before it: the stage, visit and
    /// iteration the step happened in.
    pub(crate) cursor: &'a Cursor,
    /// The run's state after it.
    pub(crate) after: &'a RunState,
}

/// One step of a run, with the whole state on each side of it.
pub(crate) struct Pair<'a> {
    /// The step itself.
    pub(crate) delta: &'a StateDelta,
    /// The run's state before it.
    pub(crate) before: &'a RunState,
    /// The run's state after it.
    pub(crate) after: &'a RunState,
}

/// The state the run started in.
pub(crate) fn initial(run_id: &str, reader: &RunFileReader) -> Result<RunState, ServeError> {
    reader.state_at(0).map_err(|e| unreadable(run_id, &e))
}

/// The state the run started in, and every step after it.
fn steps(run_id: &str, reader: &RunFileReader) -> Result<(RunState, Vec<StateDelta>), ServeError> {
    let state = initial(run_id, reader)?;
    let deltas = reader
        .deltas(state.seq + 1, reader.last_seq())
        .map_err(|e| unreadable(run_id, &e))?;
    Ok((state, deltas))
}

/// Replay the run from the state it started in, handing `visit` each step in
/// order. Stops early when `visit` breaks.
pub(crate) fn walk(
    run_id: &str,
    reader: &RunFileReader,
    visit: &mut dyn FnMut(Step<'_>) -> ControlFlow<()>,
) -> Result<(), ServeError> {
    let (mut state, deltas) = steps(run_id, reader)?;
    for delta in &deltas {
        let cursor = state.cursor.clone();
        delta.apply(&mut state);
        let step = Step {
            delta,
            cursor: &cursor,
            after: &state,
        };
        if visit(step).is_break() {
            break;
        }
    }
    Ok(())
}

/// Replay the run from the state it started in, handing `visit` each state a
/// step adds to the run's history, with the step that added it: none for a
/// step that left the window alone, and two for one that wrote to the window
/// on its way to another stage (see [`step_points`]). Stops early when
/// `visit` breaks.
///
/// [`step_points`]: leviath_runtime::runfile::history::step_points
pub(crate) fn walk_points(
    run_id: &str,
    reader: &RunFileReader,
    visit: &mut dyn FnMut(&StateDelta, &RunState) -> ControlFlow<()>,
) -> Result<(), ServeError> {
    let (mut state, deltas) = steps(run_id, reader)?;
    for delta in &deltas {
        let flow = leviath_runtime::runfile::history::step_points(delta, &mut state, &mut |at| {
            visit(delta, at)
        });
        if flow.is_break() {
            break;
        }
    }
    Ok(())
}

/// [`walk`], handing `visit` the whole state before each step too. Each step
/// costs a copy of the state, so this is for a reader that compares the two.
pub(crate) fn walk_pairs(
    run_id: &str,
    reader: &RunFileReader,
    visit: &mut dyn FnMut(Pair<'_>) -> ControlFlow<()>,
) -> Result<(), ServeError> {
    let (mut state, deltas) = steps(run_id, reader)?;
    for delta in &deltas {
        let before = state.clone();
        delta.apply(&mut state);
        let pair = Pair {
            delta,
            before: &before,
            after: &state,
        };
        if visit(pair).is_break() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "run_file_tests.rs"]
pub(crate) mod tests;
