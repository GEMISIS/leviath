//! Reading a run's file for `Run.spec`, `Run.state`, `Run.deltas` and
//! `Run.graph`.
//!
//! The file is read whole into memory and indexed there, never opened for
//! writing: the daemon may be appending to it as this reads, and a reader
//! that cut a torn tail off a live run's file would be cutting off the step
//! being written. A torn tail is dropped from what is read and left on disk
//! for the writer.

use leviath_runtime::control_socket::ControlResponse;
use leviath_runtime::runfile::{RunFileError, RunFileErrorKind, RunFileReader};

use super::super::super::super::blocking::blocking;
use super::super::super::super::core::error::ServeError;
use super::super::super::super::types::AppState;
use super::super::run::counted;
use super::delta::StateDelta;
use super::graph::RunGraph;
use super::spec::RunSpec;
use super::state::RunState;

/// The most steps one `deltas` call answers with.
pub(crate) const MAX_STEPS: u64 = 200;

/// A run file that would not read, as the failure a client sees.
///
/// Asking for a step the file does not have is the caller's mistake; anything
/// else is a file this server cannot make sense of.
fn file_error(e: &RunFileError) -> ServeError {
    match e.kind {
        RunFileErrorKind::NoSuchStep { .. } => ServeError::BadRequest(e.to_string()),
        _ => ServeError::Unprocessable(e.to_string()),
    }
}

/// Read the run file at `path`. `None` when there is none.
fn read_at(path: &std::path::Path) -> Result<Option<RunFileReader>, ServeError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ServeError::Internal(format!(
                "{} could not be read: {e}",
                path.display()
            )));
        }
    };
    RunFileReader::from_bytes(path, bytes)
        .map(Some)
        .map_err(|e| file_error(&e))
}

/// Open a run's file. `None` for a run that has none.
pub(crate) async fn open(run_id: &str) -> Result<Option<RunFileReader>, ServeError> {
    counted(run_id);
    let path = crate::runstate::run_dir(run_id).join(leviath_core::files::RUN_FILE);
    blocking(move || read_at(&path)).await
}

/// A step a client named, as the run file counts them.
fn step(name: &str, n: i32) -> Result<u64, ServeError> {
    u64::try_from(n).map_err(|_| ServeError::BadRequest(format!("`{name}` is never negative")))
}

/// The run as it was resolved. Null for a run with no run file.
pub(crate) async fn spec(run_id: &str) -> Result<Option<RunSpec>, ServeError> {
    Ok(open(run_id)
        .await?
        .map(|reader| RunSpec::from(reader.spec())))
}

/// The run's state as the daemon holds it now, when the daemon answers with
/// one.
async fn live(app: &AppState, run_id: &str) -> Option<leviath_runtime::state::RunState> {
    match app.control.inspect(run_id).await {
        Ok(ControlResponse::State { state }) => Some(*state),
        _ => None,
    }
}

/// The run's state at step `at`, or now.
///
/// Now is the daemon's answer while it holds the run, so a live run reads at
/// this tick rather than at its last write. A run the daemon does not hold,
/// or a daemon that is not there, is read from the file.
pub(crate) async fn state(
    app: &AppState,
    run_id: &str,
    at: Option<i32>,
) -> Result<Option<RunState>, ServeError> {
    let at = at.map(|n| step("at", n)).transpose()?;
    if at.is_none()
        && let Some(state) = live(app, run_id).await
    {
        return Ok(Some(RunState::from(&state)));
    }
    let Some(reader) = open(run_id).await? else {
        return Ok(None);
    };
    let seq = at.unwrap_or_else(|| reader.last_seq());
    let state = reader.state_at(seq).map_err(|e| file_error(&e))?;
    Ok(Some(RunState::from(&state)))
}

/// The steps from `from` to `to`, both included.
///
/// `from` defaults to the first step and `to` to as far as one call answers,
/// or the last step, whichever comes first. A range wider than [`MAX_STEPS`]
/// is refused rather than cut short, so a client never mistakes a page for
/// the whole.
pub(crate) async fn deltas(
    run_id: &str,
    from: Option<i32>,
    to: Option<i32>,
) -> Result<Vec<StateDelta>, ServeError> {
    let from = step("from", from.unwrap_or(1))?.max(1);
    let to = to.map(|n| step("to", n)).transpose()?;
    if let Some(to) = to
        && to < from
    {
        return Err(ServeError::BadRequest(format!(
            "`to` ({to}) is before `from` ({from})"
        )));
    }
    if let Some(to) = to
        && to - from >= MAX_STEPS
    {
        return Err(ServeError::BadRequest(format!(
            "one call reads at most {MAX_STEPS} steps; {from} to {to} is {}",
            to - from + 1
        )));
    }
    let Some(reader) = open(run_id).await? else {
        return Ok(Vec::new());
    };
    let to = to.unwrap_or_else(|| reader.last_seq().min(from + MAX_STEPS - 1));
    let deltas = reader.deltas(from, to).map_err(|e| file_error(&e))?;
    Ok(deltas.iter().map(StateDelta::from).collect())
}

/// The run's graph, with each edge's count. Null for a run with no run file.
pub(crate) async fn graph(run_id: &str) -> Result<Option<RunGraph>, ServeError> {
    let Some(reader) = open(run_id).await? else {
        return Ok(None);
    };
    let state = reader.latest_state().map_err(|e| file_error(&e))?;
    let deltas = reader
        .deltas(1, reader.last_seq())
        .map_err(|e| file_error(&e))?;
    Ok(Some(RunGraph::of(reader.spec(), &state, &deltas)))
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
