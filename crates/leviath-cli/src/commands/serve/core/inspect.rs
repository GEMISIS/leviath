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
use leviath_runtime::spec::names::Digest;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::{RunState, StateDelta, TransitionReason, TransitionRecord};
use serde::Serialize;

use super::super::types::AppState;
use super::error::ServeError;
use super::run_file;

/// The spec a run was resolved to: the graph it runs, its inputs, the models
/// and tools each stage got, and its launch policy.
///
/// The webhook's signing secret is replaced with a marker. The run file keeps
/// it so a resumed run can still sign, and nothing that reads a spec back has
/// any use for it.
pub(crate) fn spec(run_id: &str) -> Result<RunSpec, ServeError> {
    let spec = run_file::require(run_id)?.spec().clone();
    Ok(crate::runstate::run_file::redacted_spec(spec))
}

/// A run's state at step `at`, or as it is now.
///
/// Now is the daemon's answer for a run it holds, read from the run's live
/// components; for one it does not hold, or when the daemon cannot be reached,
/// it is the run file's last step.
pub(crate) async fn state(
    app: &AppState,
    run_id: &str,
    at: Option<u64>,
) -> Result<RunState, ServeError> {
    if let Some(seq) = at {
        return state_at(&run_file::require(run_id)?, run_id, seq);
    }
    if let Ok(ControlResponse::State { state }) = app.control.inspect(run_id).await {
        return Ok(*state);
    }
    let reader = run_file::require(run_id)?;
    state_at(&reader, run_id, reader.last_seq())
}

/// The state at step `seq` of the run in `reader`. A step past the run's last
/// is a window the file does not hold.
fn state_at(reader: &RunFileReader, run_id: &str, seq: u64) -> Result<RunState, ServeError> {
    reader.state_at(seq).map_err(|e| match e.kind {
        RunFileErrorKind::NoSuchStep { seq, last } => ServeError::RangeNotSatisfiable(format!(
            "Run '{run_id}' has no step {seq}; its last step is {last}"
        )),
        _ => run_file::unreadable(run_id, &e),
    })
}

/// The run's steps from `from` to `to`, both included. `from` defaults to the
/// first step and `to` to the last.
pub(crate) fn deltas(
    run_id: &str,
    from: Option<u64>,
    to: Option<u64>,
) -> Result<Vec<StateDelta>, ServeError> {
    let reader = run_file::require(run_id)?;
    let from = from.unwrap_or(1);
    let to = to.unwrap_or_else(|| reader.last_seq());
    if from > to {
        return Err(ServeError::BadRequest(format!(
            "`from` ({from}) is after `to` ({to})"
        )));
    }
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
pub(crate) fn context(run_id: &str) -> Result<ContextSnapshot, ServeError> {
    let reader = run_file::require(run_id)?;
    let state = state_at(&reader, run_id, reader.last_seq())?;
    Ok(leviath_runtime::runfile::context_snapshot(
        reader.spec(),
        &state,
    ))
}

/// The run's per-stage ledger as of its last step: one record per stage it
/// entered. Empty for a run that has not reached its first stage.
pub(crate) fn stages(run_id: &str) -> Result<Vec<StageRecord>, ServeError> {
    let reader = run_file::require(run_id)?;
    let state = state_at(&reader, run_id, reader.last_seq())?;
    Ok(leviath_runtime::runfile::stage_records(&state))
}

/// The stored parts the run's context holds as of its last step, by hash,
/// first appearance first. A part counts as stored when the run file or the
/// run's blob directory holds its bytes.
pub(crate) fn blobs(run_id: &str) -> Result<Vec<crate::blobs::BlobEntry>, ServeError> {
    let reader = run_file::require(run_id)?;
    let state = state_at(&reader, run_id, reader.last_seq())?;
    let snapshot = leviath_runtime::runfile::context_snapshot(reader.spec(), &state);
    let held: Vec<&str> = reader.blob_digests().map(|d| d.as_str()).collect();
    let mut entries = crate::blobs::list_from(run_id, &snapshot);
    for entry in &mut entries {
        entry.stored |= held.contains(&entry.sha256.as_str());
    }
    Ok(entries)
}

/// The bytes of the part hashed `sha256`: from the run file, or from the run's
/// blob directory. `None` when neither holds them.
pub(crate) fn blob(run_id: &str, sha256: &str) -> Result<Option<Vec<u8>>, ServeError> {
    let from_file = match (run_file::open(run_id)?, Digest::new(sha256)) {
        (Some(reader), Ok(digest)) => reader
            .blob(&digest)
            .map_err(|e| run_file::unreadable(run_id, &e))?,
        _ => None,
    };
    Ok(from_file.or_else(|| crate::blobs::read(run_id, sha256).ok()))
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod tests;
