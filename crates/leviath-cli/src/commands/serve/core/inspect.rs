//! One run, read off its run file: the spec it was resolved to, its state at
//! any step, the steps themselves, its graph with the edges it took, and the
//! context window and stage ledger as of its last step.
//!
//! The state of a run the daemon holds is asked of the daemon, which reads its
//! live components, so it is never a step behind. Everything else is the file,
//! which a finished run, or one the daemon has not loaded, reads the same as a
//! live one.

use leviath_core::run_meta::{ContextSnapshot, StageRecord};
use leviath_runtime::control_socket::ControlResponse;
use leviath_runtime::runfile::{RunFileErrorKind, RunFileReader};
use leviath_runtime::spec::graph::EdgeCondition;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::{RunState, StateDelta, TransitionReason, TransitionRecord};
use serde::Serialize;

use super::super::types::AppState;
use super::error::ServeError;
use super::run_file;

/// The spec a run was resolved to: the graph it runs, its inputs, the models
/// and tools each stage got, and its launch policy. The webhook's signing
/// secret is named by where the machine keeps it, never held in the spec.
pub(crate) fn spec(run_id: &str) -> Result<RunSpec, ServeError> {
    run_file::require_spec(run_id)
}

/// A run's state at step `at`, or as it is now.
///
/// Now is the daemon's answer for a run it holds, read from the run's live
/// components; for one it does not hold, or when the daemon cannot be reached,
/// it is the run file's last step. A held run reads paused, as it is listed.
pub(crate) async fn state(
    app: &AppState,
    run_id: &str,
    at: Option<u64>,
) -> Result<RunState, ServeError> {
    if at.is_none()
        && let Ok(ControlResponse::State { state }) = app.control.inspect(run_id).await
    {
        return Ok(leviath_runtime::runfile::as_it_stands(*state));
    }
    // The file is replayed up to the step, which is work for the blocking pool.
    let id = run_id.to_string();
    super::super::blocking::blocking(move || {
        let reader = run_file::require(&id)?;
        let seq = at.unwrap_or_else(|| reader.last_seq());
        state_at(&reader, &id, seq)
    })
    .await
}

/// The state at step `seq` of the run in `reader`. A step past the run's last
/// is a window the file does not hold.
fn state_at(reader: &RunFileReader, run_id: &str, seq: u64) -> Result<RunState, ServeError> {
    reader
        .state_at(seq)
        .map(leviath_runtime::runfile::as_it_stands)
        .map_err(|e| match e.kind {
            RunFileErrorKind::NoSuchStep { seq, last } => ServeError::RangeNotSatisfiable(format!(
                "Run '{run_id}' has no step {seq}; its last step is {last}"
            )),
            _ => run_file::unreadable(run_id, &e),
        })
}

/// The most steps one read of a run's steps answers with.
pub(crate) const MAX_STEPS: u64 = 200;

/// The run's steps from `from` to `to`, both included, one page at most.
///
/// `from` defaults to the first step and `to` to as far as one page goes;
/// either way the answer stops at the run's last step, so a run with no
/// steps yet, or a `from` past its last, answers with none. A `to` before
/// `from`, or a range wider than [`MAX_STEPS`], is refused rather than cut
/// short, so a client never mistakes a page for the whole.
pub(crate) fn deltas(
    run_id: &str,
    from: Option<u64>,
    to: Option<u64>,
) -> Result<Vec<StateDelta>, ServeError> {
    let from = from.unwrap_or(1).max(1);
    if let Some(to) = to {
        if to < from {
            return Err(ServeError::BadRequest(format!(
                "`to` ({to}) is before `from` ({from})"
            )));
        }
        if to - from >= MAX_STEPS {
            return Err(ServeError::BadRequest(format!(
                "one read answers with at most {MAX_STEPS} steps; {from} to {to} is {}",
                to - from + 1
            )));
        }
    }
    let to = to.unwrap_or_else(|| from.saturating_add(MAX_STEPS - 1));
    let reader = run_file::require(run_id)?;
    reader
        .deltas(from, to)
        .map_err(|e| run_file::unreadable(run_id, &e))
}

/// A run's graph as it ran: every stage with how often the run entered it, and
/// every edge with how often the run took it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct RunGraphView {
    /// The stages, in the graph's order.
    pub(crate) nodes: Vec<GraphNode>,
    /// The edges, in the graph's order, then each move the run made that no
    /// edge joins, in the order it first made it.
    pub(crate) edges: Vec<GraphEdge>,
}

/// One stage of a run's graph.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct GraphNode {
    /// The stage.
    pub(crate) stage: String,
    /// How many times the run entered it.
    pub(crate) visits: u32,
    /// Whether the run is in it: the stage it is working in, or the one it
    /// ended in.
    pub(crate) current: bool,
}

/// One edge of a run's graph: a declared one, or a move the run made that no
/// declared edge joins (a fan-out stage sent to its merge stage, a person
/// moving the run).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct GraphEdge {
    /// The stage it leaves.
    pub(crate) from: String,
    /// The stage it enters.
    pub(crate) to: String,
    /// Its name, unique among the edges leaving `from`. Absent for a move no
    /// edge joins.
    pub(crate) name: Option<String>,
    /// When the run takes it. Absent for a move no edge joins.
    pub(crate) condition: Option<EdgeCondition>,
    /// Why the run made a move no edge joins. Absent for a declared edge.
    pub(crate) reason: Option<TransitionReason>,
    /// How many times the run took it, counted from the transitions its run
    /// file records.
    pub(crate) taken: u32,
}

/// A run's graph, with the visits of its last step and every edge it took.
pub(crate) fn graph(run_id: &str) -> Result<RunGraphView, ServeError> {
    let reader = run_file::require(run_id)?;
    let state = state_at(&reader, run_id, reader.last_seq())?;
    let deltas = reader
        .deltas(1, reader.last_seq())
        .map_err(|e| run_file::unreadable(run_id, &e))?;
    Ok(graph_of(reader.spec(), &state, &deltas))
}

/// The graph of `spec`, walked as far as `state`, with each edge's count
/// taken from the moves `deltas` record.
///
/// A move names the edge it took when it took a named one. One that names
/// none (a forced move, a router's choice) counts against the first edge
/// between the same two stages, and a move no edge joins is an edge of its
/// own after the declared ones, one per pair of stages and reason.
pub(crate) fn graph_of(spec: &RunSpec, state: &RunState, deltas: &[StateDelta]) -> RunGraphView {
    let graph = &spec.graph;
    let mut taken = vec![0u32; graph.edges.len()];
    let mut unjoined: Vec<GraphEdge> = Vec::new();
    let moves = deltas.iter().flat_map(StateDelta::transitions);
    for record in moves {
        let found = graph.edges.iter().position(|edge| {
            edge.from == record.from
                && match &record.edge {
                    Some(name) => &edge.name == name,
                    None => edge.to == record.to,
                }
        });
        match found {
            Some(at) => taken[at] += 1,
            None => count_unjoined(&mut unjoined, record),
        }
    }
    RunGraphView {
        nodes: graph
            .stages
            .iter()
            .map(|stage| GraphNode {
                stage: stage.name.to_string(),
                visits: state.visits.get(&stage.name).copied().unwrap_or(0),
                current: state.cursor.stage == stage.name,
            })
            .collect(),
        edges: graph
            .edges
            .iter()
            .zip(taken)
            .map(|(edge, taken)| GraphEdge {
                from: edge.from.to_string(),
                to: edge.to.to_string(),
                name: Some(edge.name.to_string()),
                condition: Some(edge.when),
                reason: None,
                taken,
            })
            .chain(unjoined)
            .collect(),
    }
}

/// Count `record`, a move no declared edge joins, on the edge already shown
/// for its two stages and reason, or as a new one.
fn count_unjoined(unjoined: &mut Vec<GraphEdge>, record: &TransitionRecord) {
    let (from, to) = (record.from.as_str(), record.to.as_str());
    let reason = Some(record.reason);
    match unjoined
        .iter_mut()
        .find(|e| e.from == from && e.to == to && e.reason == reason)
    {
        Some(edge) => edge.taken += 1,
        None => unjoined.push(GraphEdge {
            from: from.to_string(),
            to: to.to_string(),
            name: None,
            condition: None,
            reason,
            taken: 1,
        }),
    }
}

/// The run's context window as of its last step.
///
/// Not found for a run that never held a window: one converted from an
/// earlier release whose record kept none, which that release had no window
/// to show for either.
pub(crate) fn context(run_id: &str) -> Result<ContextSnapshot, ServeError> {
    let (spec, state) = latest(run_id)?;
    if state.context.regions.is_empty() {
        return Err(ServeError::NotFound(format!(
            "No context snapshot for run '{run_id}'"
        )));
    }
    Ok(leviath_runtime::runfile::context_snapshot(&spec, &state))
}

/// The run's spec and its state as of its last step, read from the two ends
/// of its file rather than replayed from the start.
fn latest(run_id: &str) -> Result<(RunSpec, RunState), ServeError> {
    let tail = run_file::require_tail(run_id)?;
    Ok((
        tail.spec,
        leviath_runtime::runfile::as_it_stands(tail.state),
    ))
}

/// The run's per-stage ledger as of its last step: one record per stage it
/// entered. Empty for a run that has not reached its first stage.
pub(crate) fn stages(run_id: &str) -> Result<Vec<StageRecord>, ServeError> {
    let (spec, state) = latest(run_id)?;
    Ok(leviath_runtime::runfile::stage_records(&spec, &state))
}

/// The stored parts the run's context holds as of its last step, by hash,
/// first appearance first, each marked stored when its file is in the run's
/// blob directory.
pub(crate) fn blobs(run_id: &str) -> Result<Vec<crate::blobs::BlobEntry>, ServeError> {
    let (spec, state) = latest(run_id)?;
    let snapshot = leviath_runtime::runfile::context_snapshot(&spec, &state);
    Ok(crate::blobs::list_from(run_id, &snapshot))
}

/// Where the part hashed `sha256` is kept, in the run's blob directory.
/// `sha256` is one the caller has checked is 64 lowercase hex digits, so it
/// is a file name and nothing else.
pub(crate) fn blob_path(run_id: &str, sha256: &str) -> std::path::PathBuf {
    crate::runstate::run_dir(run_id)
        .join(leviath_core::files::BLOBS_DIR)
        .join(sha256)
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod tests;
