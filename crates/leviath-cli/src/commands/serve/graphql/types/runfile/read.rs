//! Reading a run's file for `Run.spec`, `Run.state`, `Run.deltas` and
//! `Run.graph`.
//!
//! These are the REST routes' own reads (`core::inspect`), so a run answers
//! the same on both surfaces. What this adds is GraphQL's shape: a run with
//! no run file reads as null rather than as a miss, a step a client names is
//! a GraphQL `Int` checked here, and one `deltas` call answers at most
//! [`MAX_STEPS`] steps.

use super::super::super::super::blocking::blocking;
use super::super::super::super::core::error::ServeError;
use super::super::super::super::core::inspect;
use super::super::super::super::types::AppState;
use super::super::run::counted;
use super::delta::StateDelta;
use super::graph::RunGraph;
use super::spec::RunSpec;
use super::state::RunState;

/// The most steps one `deltas` call answers with.
pub(crate) const MAX_STEPS: u64 = 200;

/// `read`'s answer, with a run that has no run file read as `None`, and a
/// step the run does not have as the caller's mistake.
fn present<T>(read: Result<T, ServeError>) -> Result<Option<T>, ServeError> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(ServeError::NotFound(_)) => Ok(None),
        Err(ServeError::RangeNotSatisfiable(message)) => Err(ServeError::BadRequest(message)),
        Err(e) => Err(e),
    }
}

/// A step a client named, as the run file counts them.
fn step(name: &str, n: i32) -> Result<u64, ServeError> {
    u64::try_from(n).map_err(|_| ServeError::BadRequest(format!("`{name}` is never negative")))
}

/// The run as it was resolved. Null for a run with no run file.
pub(crate) async fn spec(run_id: &str) -> Result<Option<RunSpec>, ServeError> {
    counted(run_id);
    let id = run_id.to_string();
    let spec = present(blocking(move || inspect::spec(&id)).await)?;
    Ok(spec.as_ref().map(RunSpec::from))
}

/// The run's state at step `at`, or now: the daemon's answer while it holds
/// the run, so a live run reads at this tick rather than at its last write.
pub(crate) async fn state(
    app: &AppState,
    run_id: &str,
    at: Option<i32>,
) -> Result<Option<RunState>, ServeError> {
    let at = at.map(|n| step("at", n)).transpose()?;
    counted(run_id);
    let state = present(inspect::state(app, run_id, at).await)?;
    Ok(state.as_ref().map(RunState::from))
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
    counted(run_id);
    let id = run_id.to_string();
    let to = to.unwrap_or(from + MAX_STEPS - 1);
    let deltas = present(blocking(move || inspect::deltas(&id, Some(from), Some(to))).await)?;
    Ok(deltas.iter().flatten().map(StateDelta::from).collect())
}

/// The run's graph, with each edge's count. Null for a run with no run file.
pub(crate) async fn graph(run_id: &str) -> Result<Option<RunGraph>, ServeError> {
    counted(run_id);
    let id = run_id.to_string();
    let view = present(blocking(move || inspect::graph(&id)).await)?;
    Ok(view.as_ref().map(RunGraph::from))
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
