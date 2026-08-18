//! The stage graph as data: what a blueprint's stages and transitions look
//! like once the two manifest shapes (a linear stage list, a graph with
//! `transitions`) are read the same way.
//!
//! Deliberately free of ratatui and rataflow so the shape can be asserted on
//! without a terminal, and so the same model feeds the dashboard canvases and
//! the plain-text render behind `lev validate --graph`.

use std::collections::{HashMap, HashSet};

use leviath_core::blueprint::StageMode;
use leviath_core::{Blueprint, EdgeTransform, TransitionCondition};

/// A blueprint's stages and transitions, ready to lay out and draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageGraph {
    /// Every stage in definition order, then any external worker blueprints.
    pub(crate) nodes: Vec<StageNode>,
    /// Every drawable transition. Self-loops are not here (see
    /// [`StageNode::self_loop`]); neither are transitions to stages that do
    /// not exist, which `parse_manifest` lets through.
    pub(crate) edges: Vec<StageEdge>,
    /// The stage a run starts in.
    pub(crate) entry: String,
    /// Whether any stage declares `transitions` (a graph agent) rather than
    /// falling through the list (a linear one).
    pub(crate) is_branching: bool,
}

/// One node on the canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageNode {
    /// The stage name, or `ext:<blueprint>` for an external worker.
    pub(crate) id: String,
    pub(crate) kind: NodeKind,
    /// The run starts here.
    pub(crate) is_entry: bool,
    /// The run can end here: an empty `transitions` table, or the last stage
    /// of a linear list.
    pub(crate) is_terminal: bool,
    /// The stage may call the run complete from inside.
    pub(crate) allow_complete: bool,
    /// The stage has a transition to itself. Drawn as a badge, never as an
    /// edge: the canvas rejects self-referential edges.
    pub(crate) self_loop: bool,
    pub(crate) max_iterations: Option<usize>,
    pub(crate) max_revisits: Option<usize>,
    pub(crate) description: Option<String>,
}

/// What a node stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NodeKind {
    /// A stage of this blueprint.
    Stage(StageKind),
    /// A separate blueprint a fan-out stage runs its workers as.
    ExternalBlueprint,
}

/// A stage's mode, as the canvas cares about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StageKind {
    Autonomous,
    Interactive,
    InteractivePoints,
    FanOut {
        worker: WorkerRef,
        merge: Option<String>,
        max_workers: usize,
    },
    Output,
}

impl StageKind {
    /// The word the node shows for its mode.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            StageKind::Autonomous => "autonomous",
            StageKind::Interactive => "interactive",
            StageKind::InteractivePoints => "interactive points",
            StageKind::FanOut { .. } => "fan-out",
            StageKind::Output => "output",
        }
    }
}

/// Where a fan-out stage gets its workers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerRef {
    /// A separate installed blueprint.
    Agent(String),
    /// A stage of this blueprint.
    Stage(String),
    /// A discovery query matched against installed blueprints at run time.
    Query(String),
}

/// How an edge is drawn and whether it shapes the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeClass {
    /// The normal flow: `always` and `llm_choice`.
    Primary,
    /// A conditional escape: `error`, `dead_end`, `stuck`, `max_iterations`.
    /// Hidden by default and kept out of the layout, because nearly every
    /// stage has one to the same hub and drawing them all is a hairball.
    Escape,
    /// A fan-out stage's worker or merge hand-off, which is not a transition.
    FanOut,
}

impl EdgeClass {
    /// Which class a transition condition falls in.
    pub(crate) fn of(condition: &TransitionCondition) -> Self {
        match condition {
            TransitionCondition::Always | TransitionCondition::LlmChoice => EdgeClass::Primary,
            TransitionCondition::Error
            | TransitionCondition::DeadEnd
            | TransitionCondition::Stuck
            | TransitionCondition::MaxIterations => EdgeClass::Escape,
        }
    }

    /// Whether edges of this class shape the layered layout.
    pub(crate) fn shapes_layout(self) -> bool {
        !matches!(self, EdgeClass::Escape)
    }
}

/// One drawable transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageEdge {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) condition: TransitionCondition,
    pub(crate) hint: Option<String>,
    /// How context crosses: `direct`, `clear`, `compact` or `custom`.
    pub(crate) transform: &'static str,
    pub(crate) class: EdgeClass,
    /// Points at an ancestor on the depth-first walk from the entry: a
    /// revisit loop. Only layout-shaping edges are classified.
    pub(crate) back_edge: bool,
}

/// The word for an edge transform.
fn transform_label(transform: &EdgeTransform) -> &'static str {
    match transform {
        EdgeTransform::Direct => "direct",
        EdgeTransform::Clear => "clear",
        EdgeTransform::Compact { .. } => "compact",
        EdgeTransform::Custom { .. } => "custom",
    }
}

impl StageEdge {
    /// The `[condition]` label an edge shows, empty for `always`.
    pub(crate) fn condition_label(&self) -> &'static str {
        match self.condition {
            TransitionCondition::Always => "",
            TransitionCondition::Error => "error",
            TransitionCondition::MaxIterations => "max_iterations",
            TransitionCondition::LlmChoice => "llm_choice",
            TransitionCondition::Stuck => "stuck",
            TransitionCondition::DeadEnd => "dead_end",
        }
    }
}

impl StageNode {
    /// The word the node shows for what it is.
    pub(crate) fn kind_label(&self) -> &'static str {
        match &self.kind {
            NodeKind::Stage(kind) => kind.label(),
            NodeKind::ExternalBlueprint => "blueprint",
        }
    }
}

/// The node id an external worker blueprint gets.
fn external_id(name: &str) -> String {
    format!("ext:{name}")
}

impl StageGraph {
    /// Read a blueprint's shape. Total: an odd manifest yields an odd graph,
    /// never an error.
    pub(crate) fn from_blueprint(blueprint: &Blueprint) -> Self {
        let names: Vec<&str> = blueprint.stages.iter().map(|s| s.name.as_str()).collect();
        let known: HashSet<&str> = names.iter().copied().collect();
        let entry = blueprint.resolve_entry_stage_name();
        let is_branching = blueprint.stages.iter().any(|s| s.transitions.is_some());

        let mut nodes: Vec<StageNode> = Vec::with_capacity(names.len());
        let mut externals: Vec<StageNode> = Vec::new();
        let mut edges: Vec<StageEdge> = Vec::new();

        for (i, stage) in blueprint.stages.iter().enumerate() {
            let name = stage.name.as_str();
            let mut self_loop = false;
            let is_terminal = match &stage.transitions {
                None => {
                    if let Some(next) = names.get(i + 1) {
                        edges.push(StageEdge {
                            from: name.to_string(),
                            to: (*next).to_string(),
                            condition: TransitionCondition::Always,
                            hint: None,
                            transform: "direct",
                            class: EdgeClass::Primary,
                            back_edge: false,
                        });
                    }
                    i + 1 == names.len()
                }
                Some(transitions) => {
                    let mut targets: Vec<&String> = transitions.keys().collect();
                    targets.sort();
                    for target in targets {
                        let edge = &transitions[target];
                        if target == name {
                            self_loop = true;
                        } else if known.contains(target.as_str()) {
                            edges.push(StageEdge {
                                from: name.to_string(),
                                to: target.clone(),
                                condition: edge.condition.clone(),
                                hint: edge.hint.clone(),
                                transform: transform_label(&edge.transform),
                                class: EdgeClass::of(&edge.condition),
                                back_edge: false,
                            });
                        }
                    }
                    transitions.is_empty()
                }
            };

            let kind = match &stage.mode {
                StageMode::Autonomous => StageKind::Autonomous,
                StageMode::Interactive => StageKind::Interactive,
                StageMode::InteractivePoints { .. } => StageKind::InteractivePoints,
                StageMode::Output => StageKind::Output,
                StageMode::FanOut { config } => {
                    let fan_out_edge = |to: String| StageEdge {
                        from: name.to_string(),
                        to,
                        condition: TransitionCondition::Always,
                        hint: None,
                        transform: "direct",
                        class: EdgeClass::FanOut,
                        back_edge: false,
                    };
                    let worker = if let Some(agent) = &config.worker_agent {
                        let id = external_id(agent);
                        if !externals.iter().any(|n| n.id == id) {
                            externals.push(StageNode {
                                id: id.clone(),
                                kind: NodeKind::ExternalBlueprint,
                                is_entry: false,
                                is_terminal: false,
                                allow_complete: false,
                                self_loop: false,
                                max_iterations: None,
                                max_revisits: None,
                                description: Some(format!("worker blueprint {agent}")),
                            });
                        }
                        edges.push(fan_out_edge(id));
                        WorkerRef::Agent(agent.to_string())
                    } else if let Some(stage_name) = &config.worker_stage {
                        if known.contains(stage_name.as_str()) && stage_name != name {
                            edges.push(fan_out_edge(stage_name.clone()));
                        }
                        WorkerRef::Stage(stage_name.clone())
                    } else {
                        WorkerRef::Query(config.worker_query.clone().unwrap_or_default())
                    };
                    if let Some(merge) = &config.merge_stage
                        && known.contains(merge.as_str())
                        && merge != name
                    {
                        edges.push(fan_out_edge(merge.clone()));
                    }
                    StageKind::FanOut {
                        worker,
                        merge: config.merge_stage.clone(),
                        max_workers: config.max_workers,
                    }
                }
            };

            nodes.push(StageNode {
                id: name.to_string(),
                kind: NodeKind::Stage(kind),
                is_entry: name == entry,
                is_terminal,
                allow_complete: stage.allow_complete,
                self_loop,
                max_iterations: stage.max_iterations,
                max_revisits: stage.max_revisits,
                description: stage.description.clone(),
            });
        }
        nodes.extend(externals);

        // A fan-out hand-off that duplicates a declared transition (the usual
        // `merge_stage` that is also the stage's `always` edge) is drawn once,
        // as the transition: that is the one that says how context crosses.
        let declared: HashSet<(String, String)> = edges
            .iter()
            .filter(|e| e.class != EdgeClass::FanOut)
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();
        edges.retain(|e| {
            e.class != EdgeClass::FanOut || !declared.contains(&(e.from.clone(), e.to.clone()))
        });

        // Definition order for edges too, so every downstream walk is
        // deterministic regardless of the manifest's table order.
        let order: HashMap<&str, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        edges.sort_by_key(|e| (order[e.from.as_str()], order[e.to.as_str()]));

        let mut graph = StageGraph {
            nodes,
            edges,
            entry,
            is_branching,
        };
        graph.classify_back_edges();
        graph
    }

    /// Mark layout-shaping edges that point at an ancestor on a depth-first
    /// walk from the entry. Cycles are normal (a stage can be revisited); the
    /// layout needs to know which edges close them so its layering
    /// terminates, and the canvas draws them as loops.
    fn classify_back_edges(&mut self) {
        let mut back: HashSet<(String, String)> = HashSet::new();
        let mut visited: HashSet<&str> = HashSet::new();
        let mut on_stack: HashSet<&str> = HashSet::new();
        let mut stack: Vec<(&str, usize)> = Vec::new();

        let neighbors = |name: &str| -> Vec<&str> {
            self.edges
                .iter()
                .filter(|e| e.from == name && e.class.shapes_layout())
                .map(|e| e.to.as_str())
                .collect()
        };

        if self.nodes.iter().any(|n| n.id == self.entry) {
            stack.push((self.entry.as_str(), 0));
            visited.insert(self.entry.as_str());
            on_stack.insert(self.entry.as_str());
            while let Some((node, child_idx)) = stack.pop() {
                let kids = neighbors(node);
                if child_idx < kids.len() {
                    stack.push((node, child_idx + 1));
                    let child = kids[child_idx];
                    if on_stack.contains(child) {
                        back.insert((node.to_string(), child.to_string()));
                    } else if !visited.contains(child) {
                        visited.insert(child);
                        on_stack.insert(child);
                        stack.push((child, 0));
                    }
                } else {
                    on_stack.remove(node);
                }
            }
        }

        for edge in &mut self.edges {
            edge.back_edge = back.contains(&(edge.from.clone(), edge.to.clone()));
        }
    }

    /// The node called `id`, if any.
    pub(crate) fn node(&self, id: &str) -> Option<&StageNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Position of the stage `id` in the blueprint's stage list: `None` for
    /// an external worker blueprint, which is a node but not a stage.
    pub(crate) fn stage_index(&self, id: &str) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.id == id && n.kind != NodeKind::ExternalBlueprint)
    }

    /// Edges leaving `id`, in definition order.
    pub(crate) fn outgoing(&self, id: &str) -> impl Iterator<Item = &StageEdge> {
        self.edges.iter().filter(move |e| e.from == id)
    }

    /// Node ids in definition order.
    pub(crate) fn ids(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().map(|n| n.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::manifest::parse_manifest;

    fn graph(toml: &str) -> StageGraph {
        StageGraph::from_blueprint(&parse_manifest(toml).expect("fixture parses"))
    }

    fn edge<'a>(g: &'a StageGraph, from: &str, to: &str) -> &'a StageEdge {
        let missing = format!("edge {from} -> {to} in {:?}", g.edges);
        g.edges
            .iter()
            .find(|e| e.from == from && e.to == to)
            .expect(&missing)
    }

    #[test]
    fn linear_blueprint_gets_fall_through_edges_and_a_terminal_last_stage() {
        let g = graph(
            r#"
[agent]
name = "linear"
[stages.a]
[stages.b]
[stages.c]
allow_complete = true
"#,
        );
        assert!(!g.is_branching);
        assert_eq!(g.entry, "a");
        assert_eq!(
            g.edges
                .iter()
                .map(|e| (e.from.as_str(), e.to.as_str()))
                .collect::<Vec<_>>(),
            vec![("a", "b"), ("b", "c")]
        );
        assert!(
            g.edges
                .iter()
                .all(|e| e.class == EdgeClass::Primary && !e.back_edge)
        );
        assert!(g.node("a").unwrap().is_entry);
        assert!(!g.node("b").unwrap().is_terminal);
        let c = g.node("c").unwrap();
        assert!(c.is_terminal && c.allow_complete);
        assert_eq!(g.stage_index("c"), Some(2));
        assert_eq!(g.stage_index("nope"), None);
        assert_eq!(g.ids().collect::<Vec<_>>(), vec!["a", "b", "c"]);
    }

    #[test]
    fn some_empty_transitions_marks_terminal_and_a_missing_entry_marks_nobody() {
        let g = graph(
            r#"
[agent]
name = "t"
entry_stage = "ghost"
[stages.a]
[stages.a.transitions.b]
[stages.b]
[stages.b.transitions]
"#,
        );
        assert!(g.is_branching);
        assert!(g.node("b").unwrap().is_terminal);
        assert!(!g.node("a").unwrap().is_terminal);
        assert!(g.nodes.iter().all(|n| !n.is_entry));
        // No entry to walk from: nothing is a back-edge, nothing panics.
        assert!(g.edges.iter().all(|e| !e.back_edge));
    }

    #[test]
    fn edges_are_sorted_in_definition_order_whatever_the_table_order() {
        let g = graph(
            r#"
[agent]
name = "order"
[stages.z]
[stages.z.transitions.b]
[stages.z.transitions.a]
[stages.a]
[stages.b]
"#,
        );
        // `a` and `b` declare no transitions, so `a` falls through to `b` by
        // position even inside a graph blueprint: that edge sorts after z's.
        assert_eq!(
            g.edges
                .iter()
                .map(|e| (e.from.as_str(), e.to.as_str()))
                .collect::<Vec<_>>(),
            vec![("z", "a"), ("z", "b"), ("a", "b")]
        );
    }

    #[test]
    fn a_self_loop_becomes_a_badge_not_an_edge_and_dangling_targets_are_dropped() {
        let g = graph(
            r#"
[agent]
name = "loops"
[stages.plan]
max_revisits = 3
[stages.plan.transitions.plan]
[stages.plan.transitions.phantom]
[stages.plan.transitions.go]
[stages.go]
"#,
        );
        assert!(g.node("plan").unwrap().self_loop);
        assert!(!g.node("go").unwrap().self_loop);
        assert_eq!(g.node("plan").unwrap().max_revisits, Some(3));
        assert!(g.edges.iter().all(|e| e.from != e.to));
        assert!(g.edges.iter().all(|e| e.to != "phantom"));
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn revisit_cycles_are_back_edges_and_escape_edges_are_not_classified() {
        let g = graph(
            r#"
[agent]
name = "cycle"
[stages.plan]
[stages.plan.transitions.implement]
[stages.implement]
[stages.implement.transitions.review]
transform = "clear"
[stages.implement.transitions.recover]
condition = "error"
transform = "compact"
[stages.review]
[stages.review.transitions.implement]
condition = "llm_choice"
hint = "needs another pass"
transform = "custom"
[stages.review.transitions.implement.transform_config]
carry = ["task"]
[stages.review.transitions.done]
[stages.recover]
[stages.recover.transitions.plan]
[stages.done]
[stages.done.transitions]
"#,
        );
        assert_eq!(edge(&g, "plan", "implement").transform, "direct");
        assert_eq!(edge(&g, "implement", "review").transform, "clear");
        assert_eq!(edge(&g, "implement", "recover").transform, "compact");
        assert_eq!(edge(&g, "review", "implement").transform, "custom");
        assert!(edge(&g, "review", "implement").back_edge);
        assert_eq!(
            edge(&g, "review", "implement").condition_label(),
            "llm_choice"
        );
        assert_eq!(
            edge(&g, "review", "implement").hint.as_deref(),
            Some("needs another pass")
        );
        assert!(!edge(&g, "plan", "implement").back_edge);
        assert_eq!(edge(&g, "plan", "implement").condition_label(), "");
        let escape = edge(&g, "implement", "recover");
        assert_eq!(escape.class, EdgeClass::Escape);
        assert_eq!(escape.condition_label(), "error");
        assert!(!escape.class.shapes_layout());
        // `recover` is only reachable through an escape edge, so its edge back
        // to `plan` is not on the walk and is not a back-edge.
        assert!(!edge(&g, "recover", "plan").back_edge);
        assert_eq!(
            g.outgoing("review")
                .map(|e| e.to.as_str())
                .collect::<Vec<_>>(),
            vec!["implement", "done"]
        );
    }

    #[test]
    fn edge_class_of_every_condition() {
        assert_eq!(
            EdgeClass::of(&TransitionCondition::Always),
            EdgeClass::Primary
        );
        assert_eq!(
            EdgeClass::of(&TransitionCondition::LlmChoice),
            EdgeClass::Primary
        );
        assert_eq!(
            EdgeClass::of(&TransitionCondition::Error),
            EdgeClass::Escape
        );
        assert_eq!(
            EdgeClass::of(&TransitionCondition::DeadEnd),
            EdgeClass::Escape
        );
        assert_eq!(
            EdgeClass::of(&TransitionCondition::Stuck),
            EdgeClass::Escape
        );
        assert_eq!(
            EdgeClass::of(&TransitionCondition::MaxIterations),
            EdgeClass::Escape
        );
        assert!(EdgeClass::Primary.shapes_layout());
        assert!(EdgeClass::FanOut.shapes_layout());
        let labels: Vec<&str> = [
            TransitionCondition::MaxIterations,
            TransitionCondition::Stuck,
            TransitionCondition::DeadEnd,
        ]
        .into_iter()
        .map(|condition| {
            StageEdge {
                from: "a".into(),
                to: "b".into(),
                condition,
                hint: None,
                transform: "direct",
                class: EdgeClass::Escape,
                back_edge: false,
            }
            .condition_label()
        })
        .collect();
        assert_eq!(labels, vec!["max_iterations", "stuck", "dead_end"]);
    }

    #[test]
    fn fan_out_worker_stage_and_merge_stage_become_fan_out_edges() {
        let g = graph(
            r#"
[agent]
name = "fan"
[stages.split]
mode = "fan_out"
worker_stage = "worker"
merge_stage = "merge"
max_workers = 3
[stages.split.transitions.merge]
[stages.worker]
allow_as_worker = true
[stages.merge]
"#,
        );
        assert_eq!(edge(&g, "split", "worker").class, EdgeClass::FanOut);
        // The transition and the merge hand-off both point at `merge`; the
        // transition wins because it says how context crosses.
        let to_merge: Vec<EdgeClass> = g
            .outgoing("split")
            .filter(|e| e.to == "merge")
            .map(|e| e.class)
            .collect();
        assert_eq!(to_merge, vec![EdgeClass::Primary]);
        assert_eq!(
            g.node("split").unwrap().kind,
            NodeKind::Stage(StageKind::FanOut {
                worker: WorkerRef::Stage("worker".to_string()),
                merge: Some("merge".to_string()),
                max_workers: 3,
            })
        );
        assert_eq!(g.node("split").unwrap().kind_label(), "fan-out");
    }

    #[test]
    fn fan_out_worker_agent_becomes_one_external_node_and_a_query_becomes_none() {
        let g = graph(
            r#"
[agent]
name = "fan"
[stages.a]
mode = "fan_out"
worker_agent = "researcher"
[stages.a.transitions.b]
[stages.b]
mode = "fan_out"
worker_agent = "researcher"
[stages.b.transitions.c]
[stages.c]
mode = "fan_out"
worker_query = "anything that reads logs"
[stages.c.transitions]
"#,
        );
        let ext: Vec<&StageNode> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::ExternalBlueprint)
            .collect();
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].id, "ext:researcher");
        assert_eq!(edge(&g, "a", "ext:researcher").class, EdgeClass::FanOut);
        assert_eq!(edge(&g, "b", "ext:researcher").class, EdgeClass::FanOut);
        assert!(g.outgoing("c").next().is_none());
        assert_eq!(
            g.node("c").unwrap().kind,
            NodeKind::Stage(StageKind::FanOut {
                worker: WorkerRef::Query("anything that reads logs".to_string()),
                merge: None,
                max_workers: 4,
            })
        );
        // External nodes come after every stage, and are not stages.
        assert_eq!(g.ids().last(), Some("ext:researcher"));
        assert_eq!(g.stage_index("ext:researcher"), None);
        assert_eq!(g.node("ext:researcher").unwrap().kind_label(), "blueprint");
    }

    #[test]
    fn every_stage_mode_maps_to_a_kind_with_a_label() {
        let g = graph(
            r#"
[agent]
name = "modes"
[stages.a]
mode = "autonomous"
description = "first"
max_iterations = 4
[stages.b]
mode = "interactive"
[stages.c]
mode = "interactive_points"
[[stages.c.interaction_points]]
name = "confirm"
prompt = "ok?"
[stages.d]
mode = "output"
"#,
        );
        let labels: Vec<&str> = g.nodes.iter().map(|n| n.kind_label()).collect();
        assert_eq!(
            labels,
            vec!["autonomous", "interactive", "interactive points", "output"]
        );
        let a = g.node("a").unwrap();
        assert_eq!(a.description.as_deref(), Some("first"));
        assert_eq!(a.max_iterations, Some(4));
    }

    #[test]
    fn every_bundled_agent_builds_a_valid_stage_graph() {
        let mut seen = 0;
        for agent in crate::bundled::BUNDLED_AGENTS {
            let manifest = agent
                .files
                .iter()
                .find(|(path, _)| *path == "agent.leviath")
                .map(|(_, content)| *content)
                .expect("a bundled agent has a manifest");
            let g = StageGraph::from_blueprint(&parse_manifest(manifest).expect("bundled parses"));
            let ids: HashSet<&str> = g.ids().collect();
            assert_eq!(ids.len(), g.nodes.len(), "{}: duplicate ids", agent.name);
            assert!(
                g.edges.iter().all(|e| e.from != e.to),
                "{}: self-loop edge",
                agent.name
            );
            assert!(
                g.edges
                    .iter()
                    .all(|e| ids.contains(e.from.as_str()) && ids.contains(e.to.as_str())),
                "{}: dangling edge",
                agent.name
            );
            assert!(
                g.nodes.iter().any(|n| n.is_entry),
                "{}: no entry",
                agent.name
            );
            assert!(
                g.nodes.iter().any(|n| n.is_terminal || n.allow_complete),
                "{}: no way to finish",
                agent.name
            );
            assert!(g.is_branching, "{}: bundled agents are graphs", agent.name);
            seen += 1;
        }
        assert!(seen > 0, "the binary bundles agents");
    }
}
