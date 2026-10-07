//! `RunGraph`: a run's stages and edges, with what it did on them.
//!
//! The nodes and edges are the spec's own graph. What this adds is the run's
//! record of walking it: how often each stage was entered, which one it is in,
//! and how often each edge was taken, counted from the moves its run file
//! records rather than inferred from the order stages were visited in. The
//! counting is the REST route's own (`core::inspect::graph_of`).

use async_graphql::{Enum, SimpleObject};
use leviath_graphql_derive::mirror;
use leviath_runtime::spec::graph::EdgeCondition as CoreCondition;

use super::super::super::super::core::inspect::RunGraphView;
use super::saturating;
use super::state::TransitionReason;

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

/// One edge of a run's graph, and how often the run took it: a declared
/// edge, or a move the run made that no declared edge joins (a fan-out stage
/// sent to its merge stage, a person moving the run).
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunGraphEdge {
    /// The stage it leaves.
    pub(crate) from: String,
    /// The stage it enters.
    pub(crate) to: String,
    /// Its name, unique among the edges leaving `from`. Null for a move no
    /// edge joins.
    pub(crate) name: Option<String>,
    /// When it is followed. Null for a move no edge joins.
    pub(crate) condition: Option<EdgeCondition>,
    /// Why the run made a move no edge joins. Null for a declared edge.
    pub(crate) reason: Option<TransitionReason>,
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
    /// The edges, in the graph's order, then each move the run made that no
    /// edge joins, in the order it first made it.
    pub(crate) edges: Vec<RunGraphEdge>,
}

impl From<&RunGraphView> for RunGraph {
    fn from(view: &RunGraphView) -> Self {
        Self {
            nodes: view
                .nodes
                .iter()
                .map(|node| RunGraphNode {
                    stage: node.stage.clone(),
                    visits: saturating(node.visits),
                    current: node.current,
                })
                .collect(),
            edges: view
                .edges
                .iter()
                .map(|edge| RunGraphEdge {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    name: edge.name.clone(),
                    condition: edge.condition.map(EdgeCondition::from),
                    reason: edge.reason.map(TransitionReason::from),
                    taken: saturating(edge.taken),
                })
                .collect(),
        }
    }
}
