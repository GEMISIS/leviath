//! Pure layered layout for a graph agent's stage DAG.
//!
//! Turns [`GraphTransitionInfo`] into node boxes on layers with routed edges,
//! ready for `render/graph_view.rs` to paint. Deliberately free of ratatui:
//! layer assignment, back-edge classification, and ordering are plain data
//! transforms, testable without a terminal.
//!
//! Approach: DFS from the entry stage classifies back-edges (a stage can be
//! revisited, so cycles are normal); layers are longest-path depth over the
//! remaining DAG (which terminates precisely because the back-edges were
//! removed); within a layer, nodes keep a deterministic order seeded by the
//! blueprint's definition order and refined by one median-of-predecessors
//! sweep so edges cross less.

use std::collections::{HashMap, HashSet};

use super::graph::GraphTransitionInfo;

/// One stage box: which layer (row band) it sits on and its order within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NodeSlot {
    pub(super) name: String,
    pub(super) layer: usize,
    /// Position within the layer, left to right.
    pub(super) slot: usize,
}

/// One directed edge, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EdgeLine {
    pub(super) from: String,
    pub(super) to: String,
    /// A cycle edge (points at an ancestor on the DFS stack): drawn up the
    /// side gutter, styled distinctly, because revisits are exactly what the
    /// old one-row strip could not show.
    pub(super) back_edge: bool,
    pub(super) condition: String,
    pub(super) hint: Option<String>,
}

/// The laid-out graph.
#[derive(Debug, Clone, Default)]
pub(super) struct GraphLayout {
    /// Nodes with layer/slot assignments; every stage in the blueprint
    /// appears exactly once (unreachable stages get trailing layers).
    pub(super) nodes: Vec<NodeSlot>,
    pub(super) edges: Vec<EdgeLine>,
    /// Highest layer index in `nodes`.
    pub(super) max_layer: usize,
}

impl GraphLayout {
    /// Test-only lookup; production renders whole layers.
    #[cfg(test)]
    pub(super) fn slot_of(&self, name: &str) -> Option<&NodeSlot> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// Node names on `layer`, ordered by slot.
    pub(super) fn layer_nodes(&self, layer: usize) -> Vec<&NodeSlot> {
        let mut nodes: Vec<&NodeSlot> = self.nodes.iter().filter(|n| n.layer == layer).collect();
        nodes.sort_by_key(|n| n.slot);
        nodes
    }
}

/// Lay out `graph`. Deterministic: same input, same layout.
pub(super) fn layout(graph: &GraphTransitionInfo) -> GraphLayout {
    // ── 1. Classify back-edges with an iterative DFS from the entry ─────────
    let mut back: HashSet<(String, String)> = HashSet::new();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    // (node, next-child-index, entered) triples drive the iterative DFS.
    let mut stack: Vec<(&str, usize)> = Vec::new();

    let neighbors = |name: &str| -> Vec<&str> {
        graph
            .edges
            .get(name)
            .map(|edges| edges.iter().map(|e| e.target.as_str()).collect())
            .unwrap_or_default()
    };

    if graph.stage_names.iter().any(|s| s == &graph.entry_stage) {
        stack.push((graph.entry_stage.as_str(), 0));
        visited.insert(graph.entry_stage.as_str());
        on_stack.insert(graph.entry_stage.as_str());
        while let Some((node, child_idx)) = stack.pop() {
            let kids = neighbors(node);
            if child_idx < kids.len() {
                stack.push((node, child_idx + 1));
                let child = kids[child_idx];
                if on_stack.contains(child) {
                    back.insert((node.to_string(), child.to_string()));
                } else if !visited.contains(child) && graph.stage_names.iter().any(|s| s == child) {
                    visited.insert(child);
                    on_stack.insert(child);
                    stack.push((child, 0));
                }
            } else {
                on_stack.remove(node);
            }
        }
    }

    // ── 2. Layers: longest path from the entry over the forward edges ──────
    let mut layer: HashMap<&str, usize> = HashMap::new();
    if graph.stage_names.iter().any(|s| s == &graph.entry_stage) {
        layer.insert(graph.entry_stage.as_str(), 0);
        // Relax repeatedly; bounded by node count since back-edges are gone.
        for _ in 0..graph.stage_names.len() {
            let mut changed = false;
            for name in &graph.stage_names {
                let Some(&from_layer) = layer.get(name.as_str()) else {
                    continue;
                };
                for target in neighbors(name) {
                    if back.contains(&(name.clone(), target.to_string())) {
                        continue;
                    }
                    if !graph.stage_names.iter().any(|s| s == target) {
                        continue;
                    }
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

    // Unreached stages (no path from the entry) get layers after the last
    // reached one, in definition order - shown rather than silently dropped.
    let mut max_layer = layer.values().copied().max().unwrap_or(0);
    for name in &graph.stage_names {
        if !layer.contains_key(name.as_str()) {
            max_layer += 1;
            layer.insert(name.as_str(), max_layer);
        }
    }

    // ── 3. Order within layers ──────────────────────────────────────────────
    // Seed by definition order, then one median-of-predecessor-slots sweep.
    let mut slots: HashMap<&str, usize> = HashMap::new();
    for l in 0..=max_layer {
        let mut names: Vec<&str> = graph
            .stage_names
            .iter()
            .map(|s| s.as_str())
            .filter(|s| layer.get(s) == Some(&l))
            .collect();
        if l > 0 {
            let median_of_preds = |name: &str| -> usize {
                let mut preds: Vec<usize> = graph
                    .edges
                    .iter()
                    .filter(|(src, edges)| {
                        layer.get(src.as_str()) == Some(&(l - 1))
                            && edges.iter().any(|e| e.target == name)
                    })
                    .filter_map(|(src, _)| slots.get(src.as_str()).copied())
                    .collect();
                preds.sort_unstable();
                preds.get(preds.len() / 2).copied().unwrap_or(usize::MAX)
            };
            // Stable sort keeps definition order among ties.
            names.sort_by_key(|n| median_of_preds(n));
        }
        for (i, name) in names.iter().enumerate() {
            slots.insert(name, i);
        }
    }

    // ── 4. Assemble ─────────────────────────────────────────────────────────
    let nodes: Vec<NodeSlot> = graph
        .stage_names
        .iter()
        .map(|name| NodeSlot {
            name: name.clone(),
            layer: layer.get(name.as_str()).copied().unwrap_or(0),
            slot: slots.get(name.as_str()).copied().unwrap_or(0),
        })
        .collect();

    let edges: Vec<EdgeLine> = graph
        .stage_names
        .iter()
        .flat_map(|src| {
            graph
                .edges
                .get(src)
                .map(|list| {
                    list.iter()
                        .filter(|e| graph.stage_names.iter().any(|s| s == &e.target))
                        .map(|e| EdgeLine {
                            from: src.clone(),
                            to: e.target.clone(),
                            back_edge: back.contains(&(src.clone(), e.target.clone())),
                            condition: e.condition.clone(),
                            hint: e.hint.clone(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();

    GraphLayout {
        nodes,
        edges,
        max_layer,
    }
}

#[cfg(test)]
mod tests {
    use super::super::graph::{GraphEdge, GraphTransitionInfo};
    use super::*;

    fn edge(target: &str) -> GraphEdge {
        GraphEdge {
            target: target.to_string(),
            hint: Some("go".to_string()),
            condition: "always".to_string(),
            transform: "direct".to_string(),
        }
    }

    fn graph(entry: &str, stages: &[&str], edges: &[(&str, &[&str])]) -> GraphTransitionInfo {
        GraphTransitionInfo {
            entry_stage: entry.to_string(),
            stage_names: stages.iter().map(|s| s.to_string()).collect(),
            edges: edges
                .iter()
                .map(|(src, targets)| {
                    (
                        src.to_string(),
                        targets.iter().map(|t| edge(t)).collect::<Vec<_>>(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn a_linear_chain_gets_one_node_per_layer() {
        let g = graph("a", &["a", "b", "c"], &[("a", &["b"]), ("b", &["c"])]);
        let l = layout(&g);
        assert_eq!(l.max_layer, 2);
        assert_eq!(l.slot_of("a").unwrap().layer, 0);
        assert_eq!(l.slot_of("b").unwrap().layer, 1);
        assert_eq!(l.slot_of("c").unwrap().layer, 2);
        assert!(l.edges.iter().all(|e| !e.back_edge));
    }

    #[test]
    fn a_diamond_shares_a_layer_and_the_join_lands_below() {
        let g = graph(
            "a",
            &["a", "b", "c", "d"],
            &[("a", &["b", "c"]), ("b", &["d"]), ("c", &["d"])],
        );
        let l = layout(&g);
        assert_eq!(l.slot_of("b").unwrap().layer, 1);
        assert_eq!(l.slot_of("c").unwrap().layer, 1);
        assert_eq!(l.slot_of("d").unwrap().layer, 2);
        assert_eq!(l.layer_nodes(1).len(), 2);
    }

    #[test]
    fn a_revisit_cycle_is_classified_as_a_back_edge_and_still_terminates() {
        // plan → implement → review → (back to) implement
        let g = graph(
            "plan",
            &["plan", "implement", "review"],
            &[
                ("plan", &["implement"]),
                ("implement", &["review"]),
                ("review", &["implement"]),
            ],
        );
        let l = layout(&g);
        let back: Vec<&EdgeLine> = l.edges.iter().filter(|e| e.back_edge).collect();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].from, "review");
        assert_eq!(back[0].to, "implement");
        assert_eq!(l.slot_of("implement").unwrap().layer, 1);
        assert_eq!(l.slot_of("review").unwrap().layer, 2);
    }

    #[test]
    fn longest_path_wins_when_a_shortcut_exists() {
        // a → b → c and a → c directly: c sits below b, not beside it.
        let g = graph("a", &["a", "b", "c"], &[("a", &["b", "c"]), ("b", &["c"])]);
        let l = layout(&g);
        assert_eq!(l.slot_of("c").unwrap().layer, 2);
    }

    #[test]
    fn unreachable_stages_get_trailing_layers_not_dropped() {
        let g = graph("a", &["a", "b", "island"], &[("a", &["b"])]);
        let l = layout(&g);
        assert_eq!(l.nodes.len(), 3);
        let island = l.slot_of("island").unwrap();
        assert!(island.layer > l.slot_of("b").unwrap().layer);
    }

    #[test]
    fn an_entry_missing_from_the_stage_list_produces_a_flat_fallback() {
        // Defensive: a malformed blueprint (entry name not among stages).
        let g = graph("ghost", &["a", "b"], &[("a", &["b"])]);
        let l = layout(&g);
        assert_eq!(l.nodes.len(), 2);
        // Everyone still gets a distinct trailing layer.
        assert_ne!(l.slot_of("a").unwrap().layer, l.slot_of("b").unwrap().layer);
    }

    #[test]
    fn edges_to_unknown_stages_are_dropped_and_layout_is_deterministic() {
        let g = graph("a", &["a", "b"], &[("a", &["b", "phantom"])]);
        let l1 = layout(&g);
        let l2 = layout(&g);
        assert!(l1.edges.iter().all(|e| e.to != "phantom"));
        assert_eq!(l1.nodes, l2.nodes);
        assert_eq!(l1.edges, l2.edges);
    }

    #[test]
    fn median_sweep_orders_a_layer_under_its_predecessors() {
        // Two parallel chains: a→x→x2, a→y→y2. x defined before y, so x2
        // should stay under x (slot 0) and y2 under y (slot 1).
        let g = graph(
            "a",
            &["a", "x", "y", "x2", "y2"],
            &[("a", &["x", "y"]), ("x", &["x2"]), ("y", &["y2"])],
        );
        let l = layout(&g);
        assert_eq!(l.slot_of("x2").unwrap().slot, l.slot_of("x").unwrap().slot);
        assert_eq!(l.slot_of("y2").unwrap().slot, l.slot_of("y").unwrap().slot);
    }
}
