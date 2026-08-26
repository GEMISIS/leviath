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
    /// Set by [`snake`]: how many nodes a row holds before the path wraps.
    /// With it, `layer` is the node's place in the sequence rather than a
    /// column, and the whole layout is read as a wrapped grid instead.
    pub(crate) wrap: Option<usize>,
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

    /// Which row and column `name` sits on, for a snaking layout. `None`
    /// for a layered one, where a node has a layer and a slot instead.
    pub(crate) fn cell(&self, name: &str) -> Option<(usize, usize)> {
        let per_row = self.wrap?;
        let slot = self.slot_of(name)?;
        Some(snake_cell(slot.layer, per_row))
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
        // A snake is a wrapped grid, not a stack of layers: the sequence
        // index gives the cell directly, and short rows are NOT centred the
        // way short layers are. Centring is what makes a diamond read as a
        // diamond, but here it would shift the last row off the column its
        // first node has to share with the row above, and that shared column
        // is the whole point of snaking.
        if let Some(per_row) = self.wrap {
            return self
                .nodes
                .iter()
                .map(|node| {
                    let (row, col) = snake_cell(node.layer, per_row);
                    (
                        node.name.clone(),
                        (col as f64 * (node_w + gap_x), row as f64 * (node_h + gap_y)),
                    )
                })
                .collect();
        }
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

    GraphLayout {
        nodes,
        max_layer,
        wrap: None,
    }
}

/// Lay `graph`'s nodes out as a snaking path: `per_row` of them left to
/// right, then the next `per_row` right to left on the row below, and so on.
///
/// Nodes are taken in graph order, which for a run's path is the order the
/// run walked it. The row direction alternates so the last node of a row ends
/// up directly above the first node of the next one: the hand-off between
/// rows is then a short vertical hop rather than a full-width jump back
/// across the canvas, which is what keeps a path readable while it is still
/// being drawn. (The Lair's run view snakes for the same reason.)
///
/// Edges are not consulted at all: the path is a chain, and its shape is its
/// order.
pub(crate) fn snake(graph: &StageGraph, per_row: usize) -> GraphLayout {
    let per_row = per_row.max(1);
    let nodes: Vec<NodeSlot> = graph
        .ids()
        .enumerate()
        .map(|(index, name)| NodeSlot {
            name: name.to_string(),
            layer: index,
            slot: 0,
        })
        .collect();
    GraphLayout {
        max_layer: nodes.len().saturating_sub(1),
        nodes,
        wrap: Some(per_row),
    }
}

/// Which row and column a sequence index lands on for a snake `per_row` wide.
fn snake_cell(index: usize, per_row: usize) -> (usize, usize) {
    let row = index / per_row;
    let in_row = index % per_row;
    let col = if row.is_multiple_of(2) {
        in_row
    } else {
        per_row - 1 - in_row
    };
    (row, col)
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

    /// A chain of `n` nodes, the shape a run's path always has.
    fn chain(n: usize) -> StageGraph {
        let names: Vec<String> = (0..n).map(|i| format!("s{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edges: Vec<(&str, &str, EdgeClass, bool)> = refs
            .windows(2)
            .map(|pair| (pair[0], pair[1], P, false))
            .collect();
        graph("s0", &refs, &edges)
    }

    #[test]
    fn a_snake_wraps_every_per_row_and_turns_round() {
        let l = snake(&chain(10), 4);
        assert_eq!(l.max_layer, 9);
        assert_eq!(l.wrap, Some(4));
        // Sequence index, not a column: every node is its own "layer".
        assert_eq!(l.layer_of("s7"), Some(7));
        // Row 0 runs left to right, row 1 right to left, row 2 left again.
        assert_eq!(l.cell("s0"), Some((0, 0)));
        assert_eq!(l.cell("s3"), Some((0, 3)));
        assert_eq!(l.cell("s4"), Some((1, 3)));
        assert_eq!(l.cell("s7"), Some((1, 0)));
        assert_eq!(l.cell("s8"), Some((2, 0)));
        assert_eq!(l.cell("s9"), Some((2, 1)));
        assert_eq!(l.cell("nope"), None);
        // A layered layout has no cells to give.
        assert_eq!(layout(&chain(3)).cell("s0"), None);
    }

    #[test]
    fn the_last_box_of_a_row_sits_directly_above_the_first_of_the_next() {
        // The whole point of snaking: the hand-off between rows is a short
        // vertical hop, not a jump back across the canvas.
        let pos = snake(&chain(10), 4).positions(Direction::LeftToRight, 10.0, 3.0, 4.0, 1.0);
        let at = |n: &str| pos.iter().find(|(id, _)| id == n).unwrap().1;
        assert_eq!(at("s4").0, at("s3").0, "same column across the row change");
        assert!(at("s4").1 > at("s3").1, "and one row down");
        assert!(at("s5").0 < at("s4").0, "row 1 runs the other way");
        assert_eq!(at("s8").0, at("s7").0);
        assert!(at("s9").0 > at("s8").0, "row 2 runs left to right again");
        // Columns step by the box plus its gap, rows by the box plus theirs;
        // a short last row is NOT centred, or it would lose the shared
        // column above.
        assert_eq!(at("s0"), (0.0, 0.0));
        assert_eq!(at("s1"), (14.0, 0.0));
        assert_eq!(at("s4"), (42.0, 4.0));
        assert_eq!(at("s8"), (0.0, 8.0));
        assert_eq!(
            snake(&chain(10), 4).extent(Direction::LeftToRight, 10.0, 3.0, 4.0, 1.0),
            (52.0, 11.0)
        );
    }

    #[test]
    fn a_snake_one_wide_is_a_column_and_an_empty_one_lays_out_nothing() {
        // `per_row` is clamped up: a zero would divide by it.
        let l = snake(&chain(3), 0);
        assert_eq!(l.wrap, Some(1));
        assert_eq!(l.cell("s0"), Some((0, 0)));
        assert_eq!(l.cell("s1"), Some((1, 0)));
        assert_eq!(l.cell("s2"), Some((2, 0)));
        let empty = snake(&graph("a", &[], &[]), 4);
        assert_eq!(empty.max_layer, 0);
        assert!(
            empty
                .positions(Direction::LeftToRight, 1.0, 1.0, 1.0, 1.0)
                .is_empty()
        );
    }

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
