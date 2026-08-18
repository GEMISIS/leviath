//! Pure layered layout for a [`StageGraph`].
//!
//! Turns the graph into `(layer, slot)` cells, then into canvas positions.
//! Deliberately free of ratatui and rataflow: layer assignment and ordering
//! are plain data transforms, testable without a terminal, and deterministic
//! (same graph, same picture), which is what lets rendered-buffer tests assert
//! on where things land.
//!
//! Approach: layers are longest-path depth from the entry over the
//! layout-shaping edges with the back-edges removed (the model classified
//! those, so the relaxation terminates); within a layer, nodes keep a
//! deterministic order seeded by definition order and refined by one
//! median-of-predecessors sweep so edges cross less. Escape edges never shape
//! the layout: nearly every stage has one to the same hub, and letting them in
//! flattens every graph into "everything points at error_recovery".
//!
//! rataflow ships a Sugiyama layout behind a feature flag. It is not used
//! here: it lays out over every edge including hidden ones, applies each
//! disconnected component's coordinates without offsetting them, and asserts
//! acyclicity after reversing a feedback arc set. This one is smaller and does
//! what the stage graphs need.

use std::collections::HashMap;

use super::model::StageGraph;

/// One node's cell: which layer (column, since layers run left to right) it
/// sits in and its order within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeSlot {
    pub(crate) name: String,
    pub(crate) layer: usize,
    /// Position within the layer, top to bottom.
    pub(crate) slot: usize,
}

/// The laid-out graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GraphLayout {
    /// Every node in the graph appears exactly once (unreachable nodes get
    /// trailing layers).
    pub(crate) nodes: Vec<NodeSlot>,
    /// Highest layer index in `nodes`.
    pub(crate) max_layer: usize,
}

impl GraphLayout {
    /// The cell of `name`.
    pub(crate) fn slot_of(&self, name: &str) -> Option<&NodeSlot> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// The layer `name` sits in.
    pub(crate) fn layer_of(&self, name: &str) -> Option<usize> {
        self.slot_of(name).map(|n| n.layer)
    }

    /// Node names on `layer`, ordered by slot.
    pub(crate) fn layer_nodes(&self, layer: usize) -> Vec<&NodeSlot> {
        let mut nodes: Vec<&NodeSlot> = self.nodes.iter().filter(|n| n.layer == layer).collect();
        nodes.sort_by_key(|n| n.slot);
        nodes
    }

    /// Canvas positions. Layers advance along the flow axis (x when the graph
    /// runs left to right, y when it runs top to bottom), slots along the
    /// other one, each by the box size plus its gap. Layers with fewer nodes
    /// are centred on the widest one so a diamond reads as a diamond.
    pub(crate) fn positions(
        &self,
        direction: Direction,
        node_w: f64,
        node_h: f64,
        gap_x: f64,
        gap_y: f64,
    ) -> Vec<(String, (f64, f64))> {
        let widest = (0..=self.max_layer)
            .map(|l| self.layer_nodes(l).len())
            .fold(0, usize::max);
        let mut out = Vec::with_capacity(self.nodes.len());
        for l in 0..=self.max_layer {
            let column = self.layer_nodes(l);
            let offset = (widest.saturating_sub(column.len())) as f64 / 2.0;
            for node in column {
                let along = l as f64;
                let across = node.slot as f64 + offset;
                let (x, y) = match direction {
                    Direction::LeftToRight => (along * (node_w + gap_x), across * (node_h + gap_y)),
                    Direction::TopToBottom => (across * (node_w + gap_x), along * (node_h + gap_y)),
                };
                out.push((node.name.clone(), (x, y)));
            }
        }
        out
    }

    /// The far corner of the laid-out graph in world units.
    pub(crate) fn extent(
        &self,
        direction: Direction,
        node_w: f64,
        node_h: f64,
        gap_x: f64,
        gap_y: f64,
    ) -> (f64, f64) {
        self.positions(direction, node_w, node_h, gap_x, gap_y)
            .iter()
            .fold((0.0_f64, 0.0_f64), |(w, h), (_, (x, y))| {
                (w.max(x + node_w), h.max(y + node_h))
            })
    }
}

/// Which way the layers run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// Layers are columns, left to right: the natural fit for a wide
    /// terminal.
    LeftToRight,
    /// Layers are rows, top to bottom: for a tall one, or a long chain.
    TopToBottom,
}

impl Direction {
    /// The other way round.
    pub(crate) fn rotated(self) -> Self {
        match self {
            Direction::LeftToRight => Direction::TopToBottom,
            Direction::TopToBottom => Direction::LeftToRight,
        }
    }
}

/// Lay out `graph`. Deterministic: same input, same layout.
pub(crate) fn layout(graph: &StageGraph) -> GraphLayout {
    let names: Vec<&str> = graph.ids().collect();
    let forward = |name: &str| -> Vec<&str> {
        graph
            .outgoing(name)
            .filter(|e| e.class.shapes_layout() && !e.back_edge)
            .map(|e| e.to.as_str())
            .collect()
    };

    // ── 1. Layers: longest path from the entry over the forward edges ──────
    let mut layer: HashMap<&str, usize> = HashMap::new();
    if names.contains(&graph.entry.as_str()) {
        layer.insert(graph.entry.as_str(), 0);
        // Relax repeatedly; bounded by node count since back-edges are gone.
        for _ in 0..names.len() {
            let mut changed = false;
            for name in &names {
                let Some(&from_layer) = layer.get(name) else {
                    continue;
                };
                for target in forward(name) {
                    let entry = layer.entry(target).or_insert(0);
                    if *entry < from_layer + 1 {
                        *entry = from_layer + 1;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    // Unreached nodes (no forward path from the entry) get layers after the
    // last reached one, in definition order - shown rather than silently
    // dropped.
    let mut max_layer = layer.values().copied().max().unwrap_or(0);
    for name in &names {
        if !layer.contains_key(name) {
            max_layer += 1;
            layer.insert(name, max_layer);
        }
    }

    // ── 2. Order within layers ──────────────────────────────────────────────
    // Seed by definition order, then one median-of-predecessor-slots sweep.
    let mut slots: HashMap<&str, usize> = HashMap::new();
    for l in 0..=max_layer {
        let mut column: Vec<&str> = names
            .iter()
            .copied()
            .filter(|s| layer.get(s) == Some(&l))
            .collect();
        if l > 0 {
            let median_of_preds = |name: &str| -> usize {
                let mut preds: Vec<usize> = graph
                    .edges
                    .iter()
                    .filter(|e| {
                        e.to == name
                            && e.class.shapes_layout()
                            && layer.get(e.from.as_str()) == Some(&(l - 1))
                    })
                    .filter_map(|e| slots.get(e.from.as_str()).copied())
                    .collect();
                preds.sort_unstable();
                preds.get(preds.len() / 2).copied().unwrap_or(usize::MAX)
            };
            // Stable sort keeps definition order among ties.
            column.sort_by_key(|n| median_of_preds(n));
        }
        for (i, name) in column.iter().enumerate() {
            slots.insert(name, i);
        }
    }

    // ── 3. Assemble ─────────────────────────────────────────────────────────
    let nodes: Vec<NodeSlot> = names
        .iter()
        .map(|name| NodeSlot {
            name: (*name).to_string(),
            layer: layer[name],
            slot: slots[name],
        })
        .collect();

    GraphLayout { nodes, max_layer }
}

#[cfg(test)]
mod tests {
    use super::super::model::{EdgeClass, NodeKind, StageEdge, StageGraph, StageKind, StageNode};
    use super::*;
    use leviath_core::TransitionCondition;

    fn node(name: &str) -> StageNode {
        StageNode {
            id: name.to_string(),
            kind: NodeKind::Stage(StageKind::Autonomous),
            is_entry: false,
            is_terminal: false,
            allow_complete: false,
            self_loop: false,
            max_iterations: None,
            max_revisits: None,
            description: None,
        }
    }

    fn edge(from: &str, to: &str, class: EdgeClass, back_edge: bool) -> StageEdge {
        StageEdge {
            from: from.to_string(),
            to: to.to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: "direct",
            class,
            back_edge,
        }
    }

    /// A graph with the back-edges already classified, the way the model
    /// hands them over.
    fn graph(entry: &str, stages: &[&str], edges: &[(&str, &str, EdgeClass, bool)]) -> StageGraph {
        StageGraph {
            nodes: stages.iter().map(|s| node(s)).collect(),
            edges: edges
                .iter()
                .map(|(f, t, c, b)| edge(f, t, *c, *b))
                .collect(),
            entry: entry.to_string(),
            is_branching: true,
        }
    }

    const P: EdgeClass = EdgeClass::Primary;

    #[test]
    fn a_linear_chain_gets_one_node_per_layer() {
        let g = graph(
            "a",
            &["a", "b", "c"],
            &[("a", "b", P, false), ("b", "c", P, false)],
        );
        let l = layout(&g);
        assert_eq!(l.max_layer, 2);
        assert_eq!(l.layer_of("a"), Some(0));
        assert_eq!(l.layer_of("b"), Some(1));
        assert_eq!(l.layer_of("c"), Some(2));
        assert_eq!(l.layer_of("nope"), None);
    }

    #[test]
    fn a_diamond_shares_a_layer_and_the_join_lands_below() {
        let g = graph(
            "a",
            &["a", "b", "c", "d"],
            &[
                ("a", "b", P, false),
                ("a", "c", P, false),
                ("b", "d", P, false),
                ("c", "d", P, false),
            ],
        );
        let l = layout(&g);
        assert_eq!(l.layer_of("b"), Some(1));
        assert_eq!(l.layer_of("c"), Some(1));
        assert_eq!(l.layer_of("d"), Some(2));
        assert_eq!(l.layer_nodes(1).len(), 2);
    }

    #[test]
    fn a_revisit_back_edge_is_skipped_so_layering_terminates() {
        // plan -> implement -> review -> (back to) implement
        let g = graph(
            "plan",
            &["plan", "implement", "review"],
            &[
                ("plan", "implement", P, false),
                ("implement", "review", P, false),
                ("review", "implement", P, true),
            ],
        );
        let l = layout(&g);
        assert_eq!(l.layer_of("implement"), Some(1));
        assert_eq!(l.layer_of("review"), Some(2));
    }

    #[test]
    fn longest_path_wins_when_a_shortcut_exists() {
        // a -> b -> c and a -> c directly: c sits after b, not beside it.
        let g = graph(
            "a",
            &["a", "b", "c"],
            &[
                ("a", "b", P, false),
                ("a", "c", P, false),
                ("b", "c", P, false),
            ],
        );
        let l = layout(&g);
        assert_eq!(l.layer_of("c"), Some(2));
    }

    #[test]
    fn unreachable_stages_get_trailing_layers_not_dropped() {
        let g = graph("a", &["a", "b", "island"], &[("a", "b", P, false)]);
        let l = layout(&g);
        assert_eq!(l.nodes.len(), 3);
        let island = l.slot_of("island").unwrap();
        assert!(island.layer > l.layer_of("b").unwrap());
    }

    #[test]
    fn an_entry_missing_from_the_stage_list_produces_a_flat_fallback() {
        // Defensive: a malformed blueprint (entry name not among stages).
        let g = graph("ghost", &["a", "b"], &[("a", "b", P, false)]);
        let l = layout(&g);
        assert_eq!(l.nodes.len(), 2);
        // Everyone still gets a distinct trailing layer.
        assert_ne!(l.layer_of("a"), l.layer_of("b"));
    }

    #[test]
    fn layout_is_deterministic() {
        let g = graph(
            "a",
            &["a", "b", "c"],
            &[("a", "b", P, false), ("a", "c", P, false)],
        );
        assert_eq!(layout(&g), layout(&g));
    }

    #[test]
    fn median_sweep_orders_a_layer_under_its_predecessors() {
        // Two parallel chains: a->x->x2, a->y->y2. x defined before y, so x2
        // should stay beside x (slot 0) and y2 beside y (slot 1).
        let g = graph(
            "a",
            &["a", "x", "y", "x2", "y2"],
            &[
                ("a", "x", P, false),
                ("a", "y", P, false),
                ("x", "x2", P, false),
                ("y", "y2", P, false),
            ],
        );
        let l = layout(&g);
        assert_eq!(l.slot_of("x2").unwrap().slot, l.slot_of("x").unwrap().slot);
        assert_eq!(l.slot_of("y2").unwrap().slot, l.slot_of("y").unwrap().slot);
    }

    #[test]
    fn escape_edges_do_not_influence_layers_but_fan_out_edges_do() {
        // Every stage escapes to `hub`; only the primary chain shapes layers.
        let g = graph(
            "a",
            &["a", "b", "hub", "w"],
            &[
                ("a", "b", P, false),
                ("a", "hub", EdgeClass::Escape, false),
                ("b", "hub", EdgeClass::Escape, false),
                ("b", "w", EdgeClass::FanOut, false),
            ],
        );
        let l = layout(&g);
        assert_eq!(l.layer_of("b"), Some(1));
        assert_eq!(l.layer_of("w"), Some(2), "fan-out hand-off shapes layout");
        assert_eq!(l.layer_of("hub"), Some(3), "escape-only target trails");
    }

    #[test]
    fn positions_are_distinct_advance_by_layer_and_slot_and_centre_short_columns() {
        let g = graph(
            "a",
            &["a", "b", "c", "d"],
            &[
                ("a", "b", P, false),
                ("a", "c", P, false),
                ("b", "d", P, false),
                ("c", "d", P, false),
            ],
        );
        let l = layout(&g);
        let pos = l.positions(Direction::LeftToRight, 10.0, 3.0, 4.0, 1.0);
        assert_eq!(pos.len(), 4);
        let at = |n: &str| pos.iter().find(|(id, _)| id == n).unwrap().1;
        // Layers step along x by node_w + gap_x.
        assert_eq!(at("a").0, 0.0);
        assert_eq!(at("b").0, 14.0);
        assert_eq!(at("d").0, 28.0);
        // Slots step along y by node_h + gap_y; a lone node centres on the
        // tallest column (two nodes), so it sits half a step down.
        assert_eq!(at("b").1, 0.0);
        assert_eq!(at("c").1, 4.0);
        assert_eq!(at("a").1, 2.0);
        assert_eq!(at("d").1, 2.0);
        let mut seen: Vec<(i64, i64)> = pos
            .iter()
            .map(|(_, (x, y))| (*x as i64, *y as i64))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 4, "no two nodes share a cell");
        assert_eq!(
            l.extent(Direction::LeftToRight, 10.0, 3.0, 4.0, 1.0),
            (38.0, 7.0)
        );
        // Top to bottom swaps the axes: layers are rows.
        let pos = l.positions(Direction::TopToBottom, 10.0, 3.0, 4.0, 1.0);
        let at = |n: &str| pos.iter().find(|(id, _)| id == n).unwrap().1;
        assert_eq!(at("a"), (7.0, 0.0));
        assert_eq!(at("b"), (0.0, 4.0));
        assert_eq!(at("c"), (14.0, 4.0));
        assert_eq!(at("d"), (7.0, 8.0));
        assert_eq!(
            l.extent(Direction::TopToBottom, 10.0, 3.0, 4.0, 1.0),
            (24.0, 11.0)
        );
        assert_eq!(Direction::LeftToRight.rotated(), Direction::TopToBottom);
        assert_eq!(Direction::TopToBottom.rotated(), Direction::LeftToRight);
        assert!(
            GraphLayout::default()
                .positions(Direction::LeftToRight, 1.0, 1.0, 1.0, 1.0)
                .is_empty()
        );
    }
}
