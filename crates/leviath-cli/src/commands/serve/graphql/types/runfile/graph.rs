//! `RunGraph`: a run's stages and edges, with what it did on them.
//!
//! The nodes and edges are the spec's own graph. What this adds is the run's
//! record of walking it: how often each stage was entered, which one it is in,
//! and how often each edge was taken, counted from the moves its run file
//! records rather than inferred from the order stages were visited in.

use async_graphql::{Enum, SimpleObject};
use leviath_graphql_derive::mirror;
use leviath_runtime::spec::graph::EdgeCondition as CoreCondition;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::{Change, RunState, StateDelta, TransitionRecord};

use super::saturating;

/// When an edge is followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum EdgeCondition {
    /// Whenever the stage ends.
    Always,
    /// When the stage fails.
    Error,
    /// When the stage runs out of iterations.
    MaxIterations,
    /// When the model chooses it.
    LlmChoice,
    /// When the stage can go nowhere else.
    DeadEnd,
    /// When the stage is stuck.
    Stuck,
}

impl From<CoreCondition> for EdgeCondition {
    fn from(condition: CoreCondition) -> Self {
        match condition {
            CoreCondition::Always => Self::Always,
            CoreCondition::Error => Self::Error,
            CoreCondition::MaxIterations => Self::MaxIterations,
            CoreCondition::LlmChoice => Self::LlmChoice,
            CoreCondition::DeadEnd => Self::DeadEnd,
            CoreCondition::Stuck => Self::Stuck,
        }
    }
}

/// One stage of a run's graph, and the run's time in it.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunGraphNode {
    /// The stage.
    pub(crate) stage: String,
    /// How many times the run entered it.
    pub(crate) visits: i32,
    /// Whether the run is in it now, or, for a finished run, ended in it.
    pub(crate) current: bool,
}

/// One edge of a run's graph, and how often the run took it.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunGraphEdge {
    /// The stage it leaves.
    pub(crate) from: String,
    /// The stage it enters.
    pub(crate) to: String,
    /// Its name, unique among the edges leaving `from`.
    pub(crate) name: String,
    /// When it is followed.
    pub(crate) condition: EdgeCondition,
    /// How many times the run took it, counted from the moves its run file
    /// records.
    pub(crate) taken: i32,
}

/// A run's graph: its stages and edges, with how the run walked them.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunGraph {
    /// The stages, in the graph's order.
    pub(crate) nodes: Vec<RunGraphNode>,
    /// The edges, in the graph's order.
    pub(crate) edges: Vec<RunGraphEdge>,
}

/// Every move between stages a run's steps record, in order.
fn transitions(deltas: &[StateDelta]) -> Vec<&TransitionRecord> {
    deltas
        .iter()
        .flat_map(|delta| delta.changes.iter())
        .filter_map(|change| match change {
            Change::LastTransition(Some(record)) => Some(record),
            _ => None,
        })
        .collect()
}

impl RunGraph {
    /// The graph of `spec`, walked as far as `state`, with each edge's count
    /// taken from `deltas`.
    ///
    /// A move names the edge it took when it took a named one. One that names
    /// none (a forced move, a router's choice) counts against the first edge
    /// between the same two stages, and a move no edge joins is not counted.
    pub(crate) fn of(spec: &RunSpec, state: &RunState, deltas: &[StateDelta]) -> Self {
        let moves = transitions(deltas);
        let graph = &spec.graph;
        let mut taken = vec![0u32; graph.edges.len()];
        for record in moves {
            let found = graph.edges.iter().position(|edge| {
                edge.from == record.from
                    && match &record.edge {
                        Some(name) => &edge.name == name,
                        None => edge.to == record.to,
                    }
            });
            if let Some(at) = found {
                taken[at] += 1;
            }
        }
        Self {
            nodes: graph
                .stages
                .iter()
                .map(|stage| RunGraphNode {
                    stage: stage.name.to_string(),
                    visits: saturating(state.visits.get(&stage.name).copied().unwrap_or(0)),
                    current: state.cursor.stage == stage.name,
                })
                .collect(),
            edges: graph
                .edges
                .iter()
                .zip(taken)
                .map(|(edge, count)| RunGraphEdge {
                    from: edge.from.to_string(),
                    to: edge.to.to_string(),
                    name: edge.name.to_string(),
                    condition: EdgeCondition::from(edge.when),
                    taken: saturating(count),
                })
                .collect(),
        }
    }
}
