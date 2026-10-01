//! The stage graph as data: a run graph's stages and edges, read into the
//! shape the canvases draw.
//!
//! Deliberately free of ratatui and rataflow so the shape can be asserted on
//! without a terminal, and so the same model feeds the dashboard canvases and
//! the plain-text render behind `lev validate --graph`.

use std::collections::{HashMap, HashSet};

use leviath_providers::capabilities::pattern_covers;
use leviath_runtime::spec::graph::{
    EdgeCarry, EdgeCondition, EdgeDef, FALL_THROUGH_EDGE, RunGraph, StageDef, StageMode,
    WorkerSource,
};
use leviath_runtime::spec::names::BlueprintPath;

/// A graph's stages and edges, ready to lay out and draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageGraph {
    /// Every stage in definition order, then any external worker blueprints.
    pub(crate) nodes: Vec<StageNode>,
    /// Every drawable edge. Self-loops are not here (see
    /// [`StageNode::self_loop`]); neither are edges into stages that do not
    /// exist, which an unvalidated graph can hold.
    pub(crate) edges: Vec<StageEdge>,
    /// The stage a run starts in.
    pub(crate) entry: String,
    /// Whether any edge does more than fall through to the next stage in the
    /// list: a linear graph is drawn as a list, a branching one as a graph.
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
    /// The run can end here: no edge leaves the stage.
    pub(crate) is_terminal: bool,
    /// The stage may call the run complete from inside.
    pub(crate) allow_complete: bool,
    /// The stage has an edge to itself. Drawn as a badge, never as an
    /// edge: the canvas rejects self-referential edges.
    pub(crate) self_loop: bool,
    pub(crate) max_iterations: Option<usize>,
    pub(crate) max_revisits: Option<usize>,
    pub(crate) description: Option<String>,
    /// Mime type patterns the stage takes as parts beyond text: its
    /// `input_accepts`, or the `accepts` of the regions it sees. A region
    /// that takes anything (`*/*`) is no constraint and is left out.
    pub(crate) inputs: Vec<String>,
    /// The types of the files the stage declares it hands back (its
    /// `output.artifacts`), in declaration order.
    pub(crate) outputs: Vec<String>,
}

/// What a node stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NodeKind {
    /// A stage of this blueprint.
    Stage(StageKind),
    /// A separate blueprint a fan-out stage runs its workers as, installed
    /// or read from its directory.
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
    /// A separate blueprint: an installed one by name, or one read from its
    /// directory by path.
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
    /// A fan-out stage's worker or merge hand-off, which is not an edge of
    /// the graph.
    FanOut,
}

impl EdgeClass {
    /// Which class an edge condition falls in.
    pub(crate) fn of(condition: EdgeCondition) -> Self {
        match condition {
            EdgeCondition::Always | EdgeCondition::LlmChoice => EdgeClass::Primary,
            EdgeCondition::Error
            | EdgeCondition::DeadEnd
            | EdgeCondition::Stuck
            | EdgeCondition::MaxIterations => EdgeClass::Escape,
        }
    }

    /// Whether edges of this class shape the layered layout.
    pub(crate) fn shapes_layout(self) -> bool {
        !matches!(self, EdgeClass::Escape)
    }
}

/// One drawable edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageEdge {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) condition: EdgeCondition,
    pub(crate) hint: Option<String>,
    /// How context crosses: `direct`, `clear`, `compact` or `custom`.
    pub(crate) transform: &'static str,
    pub(crate) class: EdgeClass,
    /// Points at an ancestor on the depth-first walk from the entry: a
    /// revisit loop. Only layout-shaping edges are classified.
    pub(crate) back_edge: bool,
    /// Artifact types the leaving stage declares that no region of the
    /// target takes: they cross this path as stand-ins at best. Text is
    /// always taken and never listed.
    pub(crate) unseen: Vec<String>,
}

/// The word for what an edge carries.
fn carry_label(carry: &EdgeCarry) -> &'static str {
    match carry {
        EdgeCarry::Direct => "direct",
        EdgeCarry::Clear => "clear",
        EdgeCarry::Compact { .. } => "compact",
        EdgeCarry::Custom { .. } => "custom",
    }
}

impl StageEdge {
    /// The word the editor puts on every edge: what makes it fire.
    pub(crate) fn editor_label(&self) -> &'static str {
        match self.condition {
            // A hint on an `always` or `llm_choice` edge is what the model
            // routes on, so the hint is what the edge is about.
            EdgeCondition::Always | EdgeCondition::LlmChoice if self.hint.is_some() => "hint",
            EdgeCondition::Always => "always",
            EdgeCondition::LlmChoice => "model's choice",
            EdgeCondition::Error => "on error",
            EdgeCondition::MaxIterations => "too many tries",
            EdgeCondition::Stuck => "when stuck",
            EdgeCondition::DeadEnd => "dead end",
        }
    }

    /// The `[condition]` label an edge shows, empty for `always`.
    pub(crate) fn condition_label(&self) -> &'static str {
        match self.condition {
            EdgeCondition::Always => "",
            EdgeCondition::Error => "error",
            EdgeCondition::MaxIterations => "max_iterations",
            EdgeCondition::LlmChoice => "llm_choice",
            EdgeCondition::Stuck => "stuck",
            EdgeCondition::DeadEnd => "dead_end",
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

/// Add the node for a fan-out's worker blueprint, once however many fan-outs
/// name it, and hand back its id.
fn external_node(externals: &mut Vec<StageNode>, name: &str, description: String) -> String {
    let id = external_id(name);
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
            description: Some(description),
            inputs: Vec::new(),
            outputs: Vec::new(),
        });
    }
    id
}

/// The name a worker blueprint read from a directory is drawn under: the
/// directory's last component, or the whole path for a root.
fn directory_name(path: &BlueprintPath) -> String {
    path.path()
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// The mime type patterns `stage` takes as parts: its own `input_accepts`
/// when it declares any, else the union of `accepts` across the regions it
/// sees (its layout, less the regions it hides). Text is always taken and
/// never listed, so an empty answer means "text only, unless a region takes
/// anything". A visible region with no `accepts` takes anything, and is
/// reported as `*/*`.
pub(crate) fn stage_inputs(graph: &RunGraph, stage: &StageDef) -> Vec<String> {
    if !stage.input_accepts.is_empty() {
        return stage
            .input_accepts
            .iter()
            .map(|p| p.as_str().to_string())
            .collect();
    }
    let mut out: Vec<String> = Vec::new();
    for region in &graph.layout_for(stage).regions {
        if stage.hide.contains(&region.name) {
            continue;
        }
        let patterns: Vec<String> = match region.accepts.is_empty() {
            true => vec!["*/*".to_string()],
            false => region
                .accepts
                .iter()
                .map(|p| p.as_str().to_string())
                .collect(),
        };
        for p in patterns {
            if !p.starts_with("text/") && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Whether an edge only falls through to the stage after its own in the
/// list: the edge a linear graph has between each pair of stages.
fn is_fall_through(graph: &RunGraph, edge: &EdgeDef) -> bool {
    let next = graph
        .stages
        .iter()
        .position(|s| s.name == edge.from)
        .and_then(|i| graph.stages.get(i + 1));
    edge.name.as_str() == FALL_THROUGH_EDGE
        && edge.when == EdgeCondition::Always
        && edge.carry == EdgeCarry::Direct
        && edge.gate.is_none()
        && next.is_some_and(|n| n.name == edge.to)
}

impl StageGraph {
    /// Read a run graph's shape. Total: an odd graph yields an odd drawing,
    /// never an error.
    pub(crate) fn from_graph(graph: &RunGraph) -> Self {
        let known: HashSet<&str> = graph.stages.iter().map(|s| s.name.as_str()).collect();
        let entry = match &graph.entry {
            Some(name) => name.to_string(),
            None => graph
                .stages
                .first()
                .map(|s| s.name.to_string())
                .unwrap_or_default(),
        };
        let is_branching = graph.edges.iter().any(|e| !is_fall_through(graph, e));

        let mut nodes: Vec<StageNode> = Vec::with_capacity(graph.stages.len());
        let mut externals: Vec<StageNode> = Vec::new();
        let mut edges: Vec<StageEdge> = Vec::new();

        for stage in &graph.stages {
            let name = stage.name.as_str();
            let mut self_loop = false;
            let mut leaves = false;
            for edge in graph.edges_from(name) {
                leaves = true;
                if edge.to.as_str() == name {
                    self_loop = true;
                } else if known.contains(edge.to.as_str()) {
                    edges.push(StageEdge {
                        from: name.to_string(),
                        to: edge.to.to_string(),
                        condition: edge.when,
                        hint: edge.hint.clone(),
                        transform: carry_label(&edge.carry),
                        class: EdgeClass::of(edge.when),
                        back_edge: false,
                        unseen: Vec::new(),
                    });
                }
            }

            let kind = match &stage.mode {
                StageMode::Autonomous => StageKind::Autonomous,
                StageMode::Interactive => StageKind::Interactive,
                StageMode::InteractivePoints(_) => StageKind::InteractivePoints,
                StageMode::Output => StageKind::Output,
                StageMode::FanOut(config) => {
                    let fan_out_edge = |to: String| StageEdge {
                        from: name.to_string(),
                        to,
                        condition: EdgeCondition::Always,
                        hint: None,
                        transform: "direct",
                        class: EdgeClass::FanOut,
                        back_edge: false,
                        unseen: Vec::new(),
                    };
                    let worker = match &config.worker {
                        WorkerSource::Stage(stage_name) => {
                            if known.contains(stage_name.as_str()) && stage_name.as_str() != name {
                                edges.push(fan_out_edge(stage_name.to_string()));
                            }
                            WorkerRef::Stage(stage_name.to_string())
                        }
                        WorkerSource::Query(query) => WorkerRef::Query(query.clone()),
                        WorkerSource::Blueprint(reference) => {
                            let worker = reference.name.to_string();
                            let description = format!("worker blueprint {worker}");
                            let id = external_node(&mut externals, &worker, description);
                            edges.push(fan_out_edge(id));
                            WorkerRef::Agent(worker)
                        }
                        WorkerSource::BlueprintFile(path) => {
                            let worker = directory_name(path);
                            let description =
                                format!("worker blueprint at {}", path.path().display());
                            let id = external_node(&mut externals, &worker, description);
                            edges.push(fan_out_edge(id));
                            WorkerRef::Agent(worker)
                        }
                    };
                    let merge = config.merge_stage.as_ref().map(|m| m.to_string());
                    if let Some(merge) = &merge
                        && known.contains(merge.as_str())
                        && merge != name
                    {
                        edges.push(fan_out_edge(merge.clone()));
                    }
                    StageKind::FanOut {
                        worker,
                        merge,
                        max_workers: config.max_workers as usize,
                    }
                }
            };

            nodes.push(StageNode {
                id: name.to_string(),
                kind: NodeKind::Stage(kind),
                is_entry: name == entry,
                is_terminal: !leaves,
                allow_complete: stage.allow_complete,
                self_loop,
                max_iterations: stage.max_iterations.map(|n| n as usize),
                max_revisits: stage.max_revisits.map(|n| n as usize),
                description: stage.description.clone(),
                inputs: stage_inputs(graph, stage)
                    .into_iter()
                    .filter(|p| p != "*/*")
                    .collect(),
                outputs: stage
                    .output
                    .as_ref()
                    .map(|o| {
                        o.artifacts
                            .iter()
                            .map(|a| a.mime_type.as_str().to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
        nodes.extend(externals);

        // A fan-out hand-off that duplicates a declared edge (the usual
        // `merge_stage` that is also the stage's `always` edge) is drawn once,
        // as the declared edge: that is the one that says how context crosses.
        let declared: HashSet<(String, String)> = edges
            .iter()
            .filter(|e| e.class != EdgeClass::FanOut)
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();
        edges.retain(|e| {
            e.class != EdgeClass::FanOut || !declared.contains(&(e.from.clone(), e.to.clone()))
        });

        // Definition order for edges too, so every downstream walk is
        // deterministic whatever order the graph lists its edges in.
        let order: HashMap<&str, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        edges.sort_by_key(|e| (order[e.from.as_str()], order[e.to.as_str()]));

        // What each stage takes, `*/*` included: a region with no `accepts`
        // takes anything, and that is what decides whether a file crosses.
        let takes: HashMap<&str, Vec<String>> = graph
            .stages
            .iter()
            .map(|s| (s.name.as_str(), stage_inputs(graph, s)))
            .collect();
        for edge in &mut edges {
            if edge.class == EdgeClass::Escape {
                continue;
            }
            let (Some(from), Some(to)) = (
                nodes.iter().find(|n| n.id == edge.from),
                takes.get(edge.to.as_str()),
            ) else {
                continue;
            };
            edge.unseen = from
                .outputs
                .iter()
                .filter(|t| !t.starts_with("text/"))
                .filter(|t| !to.iter().any(|have| pattern_covers(have, t)))
                .cloned()
                .collect();
        }

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

    /// How many stages the blueprint has (external worker blueprints are
    /// nodes, not stages).
    pub(crate) fn stage_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.kind != NodeKind::ExternalBlueprint)
            .count()
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

/// A run graph from the text below `[graph]` in an `agent.toml`, for tests.
/// `[blueprint]` is written for it, and so is an empty shared layout when the
/// text declares none.
#[cfg(test)]
pub(crate) fn test_run_graph(body: &str) -> RunGraph {
    // A `layout =` key belongs to `[graph]` only above the first header;
    // further down it is a stage's own.
    let graph_keys = body.split("\n[").next().unwrap_or_default();
    let layout = match body.contains("[graph.layout]") || graph_keys.contains("layout =") {
        true => "",
        false => "layout = { total_budget_tokens = 0, regions = [] }\n",
    };
    let text = format!("[blueprint]\nname = \"t\"\nversion = \"0.1.0\"\n[graph]\n{layout}{body}");
    leviath_blueprint::BlueprintFile::parse(&text)
        .map(|file| file.run_graph())
        .expect("fixture parses")
}

/// Every bundled blueprint's name and run graph, read from its `agent.toml`.
#[cfg(test)]
pub(crate) fn bundled_run_graphs() -> Vec<(&'static str, RunGraph)> {
    crate::bundled::BUNDLED_AGENTS
        .iter()
        .map(|agent| {
            let text = agent
                .files
                .iter()
                .find(|(path, _)| *path == leviath_blueprint::FILE_NAME)
                .map(|(_, content)| *content)
                .expect("a bundled agent has an agent.toml");
            let file = leviath_blueprint::BlueprintFile::parse(text).expect("bundled parses");
            (agent.name, file.run_graph())
        })
        .collect()
}

/// The stage graph of a whole `agent.toml`, for tests.
#[cfg(test)]
pub(crate) fn toml_graph(text: &str) -> StageGraph {
    leviath_blueprint::BlueprintFile::parse(text)
        .map(|file| StageGraph::from_graph(&file.run_graph()))
        .expect("fixture parses")
}

/// The stage graph of [`test_run_graph`]'s graph.
#[cfg(test)]
pub(crate) fn test_graph(body: &str) -> StageGraph {
    StageGraph::from_graph(&test_run_graph(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(body: &str) -> StageGraph {
        test_graph(body)
    }

    fn edge<'a>(g: &'a StageGraph, from: &str, to: &str) -> &'a StageEdge {
        let missing = format!("edge {from} -> {to} in {:?}", g.edges);
        g.edges
            .iter()
            .find(|e| e.from == from && e.to == to)
            .expect(&missing)
    }

    /// What a stage takes and hands back rides on its node, and a path
    /// whose file the next stage cannot take says so; an escape path and a
    /// hand-off to a worker blueprint are never judged.
    #[test]
    fn mime_in_and_out_ride_on_the_nodes_and_mark_a_path_that_drops_a_file() {
        let g = graph(
            r#"
[graph.layout]
total_budget_tokens = 0
regions = [
    { name = "brief", kind = "pinned", budget = 100, accepts = ["text/*"] },
    { name = "shots", kind = "pinned", budget = 100, accepts = ["image/*", "audio/wav"] },
]

[[graph.stages]]
name = "render"
output = { artifacts = [{ name = "final", mime_type = "video/mp4" }, { name = "notes", mime_type = "text/markdown" }] }

[[graph.stages]]
name = "check"
layout = { total_budget_tokens = 0, regions = [{ name = "clips", kind = "pinned", budget = 100, accepts = ["video/*"] }, { name = "scratch", kind = "temporary", budget = 100 }] }

[[graph.stages]]
name = "publish"
mode = { fan_out = { worker = { blueprint = { name = "uploader" } } } }

[[graph.stages]]
name = "recover"

[[graph.edges]]
name = "check"
from = "render"
to = "check"
when = "llm_choice"

[[graph.edges]]
name = "publish"
from = "render"
to = "publish"

[[graph.edges]]
name = "recover"
from = "render"
to = "recover"
when = "error"

[[graph.edges]]
name = "publish"
from = "check"
to = "publish"
"#,
        );
        let render = g.node("render").unwrap();
        assert_eq!(render.inputs, vec!["image/*", "audio/wav"]);
        assert_eq!(render.outputs, vec!["video/mp4", "text/markdown"]);
        // A region that takes anything is no constraint to show.
        assert_eq!(g.node("check").unwrap().inputs, vec!["video/*"]);
        assert!(g.node("check").unwrap().outputs.is_empty());
        // check takes video in its own layout; publish inherits the shared
        // one, which takes none, so the video crosses as a stand-in. The
        // markdown is text and always crosses.
        assert!(edge(&g, "render", "check").unseen.is_empty());
        assert_eq!(edge(&g, "render", "publish").unseen, vec!["video/mp4"]);
        assert!(edge(&g, "render", "recover").unseen.is_empty());
        assert!(edge(&g, "check", "publish").unseen.is_empty());
        assert!(edge(&g, "publish", "ext:uploader").unseen.is_empty());
        assert!(g.node("ext:uploader").unwrap().inputs.is_empty());
    }

    /// A stage's own `input_accepts` wins over its regions, and a region it
    /// hides is not one it takes from.
    #[test]
    fn stage_inputs_read_input_accepts_first_and_skip_hidden_regions() {
        let rg = test_run_graph(
            r#"
[graph.layout]
total_budget_tokens = 0
regions = [
    { name = "shots", kind = "pinned", budget = 100, accepts = ["image/*"] },
    { name = "clips", kind = "pinned", budget = 100, accepts = ["video/*", "image/*"] },
]

[[graph.stages]]
name = "own"
input_accepts = ["audio/*"]

[[graph.stages]]
name = "hider"
hide = ["shots"]
"#,
        );
        assert_eq!(stage_inputs(&rg, &rg.stages[0]), vec!["audio/*"]);
        assert_eq!(stage_inputs(&rg, &rg.stages[1]), vec!["video/*", "image/*"]);
    }

    #[test]
    fn linear_graph_draws_its_fall_through_edges_and_a_terminal_last_stage() {
        let g = graph(
            r#"
[[graph.stages]]
name = "a"

[[graph.stages]]
name = "b"

[[graph.stages]]
name = "c"
allow_complete = true

[[graph.edges]]
name = "next"
from = "a"
to = "b"

[[graph.edges]]
name = "next"
from = "b"
to = "c"
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
        assert_eq!(g.stage_count(), 3);
        assert_eq!(g.ids().collect::<Vec<_>>(), vec!["a", "b", "c"]);
    }

    /// An edge is a plain fall-through only when every part of it says so;
    /// any one difference makes the graph a branching one.
    #[test]
    fn any_edge_that_is_more_than_a_fall_through_makes_the_graph_branching() {
        let stages = "[[graph.stages]]\nname = \"a\"\n[[graph.stages]]\nname = \"b\"\n\
                      [[graph.stages]]\nname = \"c\"\n";
        for edge in [
            "name = \"go\"\nfrom = \"a\"\nto = \"b\"",
            "name = \"next\"\nfrom = \"a\"\nto = \"b\"\nwhen = \"error\"",
            "name = \"next\"\nfrom = \"a\"\nto = \"b\"\ncarry = \"clear\"",
            "name = \"next\"\nfrom = \"a\"\nto = \"b\"\ngate = { require_modifications = true }",
            "name = \"next\"\nfrom = \"a\"\nto = \"c\"",
            "name = \"next\"\nfrom = \"c\"\nto = \"a\"",
            "name = \"next\"\nfrom = \"ghost\"\nto = \"a\"",
        ] {
            let g = graph(&format!("{stages}[[graph.edges]]\n{edge}\n"));
            assert!(g.is_branching, "{edge}");
        }
        assert!(!graph(stages).is_branching, "no edges at all is a list");
    }

    #[test]
    fn a_stage_with_no_edges_is_terminal_and_a_missing_entry_marks_nobody() {
        let g = graph(
            r#"
entry = "ghost"

[[graph.stages]]
name = "a"

[[graph.stages]]
name = "b"

[[graph.edges]]
name = "b"
from = "a"
to = "b"
"#,
        );
        assert!(g.is_branching);
        assert!(g.node("b").unwrap().is_terminal);
        assert!(!g.node("a").unwrap().is_terminal);
        assert!(g.nodes.iter().all(|n| !n.is_entry));
        // No entry to walk from: nothing is a back-edge, nothing panics.
        assert!(g.edges.iter().all(|e| !e.back_edge));
        // A graph with no stages has no entry either.
        assert_eq!(
            StageGraph::from_graph(&test_run_graph("stages = []\n")).entry,
            ""
        );
    }

    #[test]
    fn edges_are_sorted_in_definition_order_whatever_the_list_order() {
        let g = graph(
            r#"
[[graph.stages]]
name = "z"

[[graph.stages]]
name = "a"

[[graph.stages]]
name = "b"

[[graph.edges]]
name = "b"
from = "a"
to = "b"

[[graph.edges]]
name = "b"
from = "z"
to = "b"

[[graph.edges]]
name = "a"
from = "z"
to = "a"
"#,
        );
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
[[graph.stages]]
name = "plan"
max_revisits = 3

[[graph.stages]]
name = "go"

[[graph.edges]]
name = "plan"
from = "plan"
to = "plan"

[[graph.edges]]
name = "phantom"
from = "plan"
to = "phantom"

[[graph.edges]]
name = "go"
from = "plan"
to = "go"
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
[[graph.stages]]
name = "plan"

[[graph.stages]]
name = "implement"

[[graph.stages]]
name = "review"

[[graph.stages]]
name = "recover"

[[graph.stages]]
name = "done"

[[graph.edges]]
name = "implement"
from = "plan"
to = "implement"

[[graph.edges]]
name = "review"
from = "implement"
to = "review"
carry = "clear"

[[graph.edges]]
name = "recover"
from = "implement"
to = "recover"
when = "error"
carry = { compact = {} }

[[graph.edges]]
name = "implement"
from = "review"
to = "implement"
when = "llm_choice"
hint = "needs another pass"
carry = { custom = { carry = ["task"] } }

[[graph.edges]]
name = "done"
from = "review"
to = "done"

[[graph.edges]]
name = "plan"
from = "recover"
to = "plan"
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
        assert_eq!(EdgeClass::of(EdgeCondition::Always), EdgeClass::Primary);
        assert_eq!(EdgeClass::of(EdgeCondition::LlmChoice), EdgeClass::Primary);
        assert_eq!(EdgeClass::of(EdgeCondition::Error), EdgeClass::Escape);
        assert_eq!(EdgeClass::of(EdgeCondition::DeadEnd), EdgeClass::Escape);
        assert_eq!(EdgeClass::of(EdgeCondition::Stuck), EdgeClass::Escape);
        assert_eq!(
            EdgeClass::of(EdgeCondition::MaxIterations),
            EdgeClass::Escape
        );
        assert!(EdgeClass::Primary.shapes_layout());
        assert!(EdgeClass::FanOut.shapes_layout());
        let labels: Vec<&str> = [
            EdgeCondition::MaxIterations,
            EdgeCondition::Stuck,
            EdgeCondition::DeadEnd,
        ]
        .into_iter()
        .map(|condition| {
            StageEdge {
                unseen: Vec::new(),
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
[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "worker" }, merge_stage = "merge", max_workers = 3 } }

[[graph.stages]]
name = "worker"
allow_as_worker = true

[[graph.stages]]
name = "merge"

[[graph.edges]]
name = "merge"
from = "split"
to = "merge"
"#,
        );
        assert_eq!(edge(&g, "split", "worker").class, EdgeClass::FanOut);
        // The declared edge and the merge hand-off both point at `merge`; the
        // declared edge wins because it says how context crosses.
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

        // A fan-out naming itself as its worker and its merge (the validator
        // rejects it, the reader does not) draws no self-referential hand-off.
        let g = graph(
            r#"
[[graph.stages]]
name = "split"
mode = { fan_out = { worker = { stage = "split" }, merge_stage = "split" } }
"#,
        );
        assert!(g.edges.is_empty());
        assert!(!g.node("split").unwrap().self_loop);
    }

    #[test]
    fn fan_out_worker_blueprints_become_one_external_node_each_and_a_query_none() {
        let dir = std::env::temp_dir().join("uploads").join("thumbnailer");
        let root = dir.ancestors().last().unwrap().to_path_buf();
        let g = graph(&format!(
            r#"
[[graph.stages]]
name = "a"
mode = {{ fan_out = {{ worker = {{ blueprint = {{ name = "researcher" }} }} }} }}

[[graph.stages]]
name = "b"
mode = {{ fan_out = {{ worker = {{ blueprint = {{ name = "researcher" }} }} }} }}

[[graph.stages]]
name = "c"
mode = {{ fan_out = {{ worker = {{ query = "anything that reads logs" }} }} }}

[[graph.stages]]
name = "d"
mode = {{ fan_out = {{ worker = {{ blueprint_file = '{}' }} }} }}

[[graph.stages]]
name = "e"
mode = {{ fan_out = {{ worker = {{ blueprint_file = '{}' }} }} }}

[[graph.edges]]
name = "b"
from = "a"
to = "b"

[[graph.edges]]
name = "c"
from = "b"
to = "c"
"#,
            dir.display(),
            root.display()
        ));
        let ext: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::ExternalBlueprint)
            .map(|n| n.id.as_str())
            .collect();
        let root_id = format!("ext:{}", root.display());
        assert_eq!(ext, vec!["ext:researcher", "ext:thumbnailer", &root_id]);
        assert_eq!(edge(&g, "a", "ext:researcher").class, EdgeClass::FanOut);
        assert_eq!(edge(&g, "b", "ext:researcher").class, EdgeClass::FanOut);
        assert_eq!(edge(&g, "d", "ext:thumbnailer").class, EdgeClass::FanOut);
        assert_eq!(
            g.node("ext:thumbnailer").unwrap().description,
            Some(format!("worker blueprint at {}", dir.display()))
        );
        assert_eq!(
            g.node("d").unwrap().kind,
            NodeKind::Stage(StageKind::FanOut {
                worker: WorkerRef::Agent("thumbnailer".to_string()),
                merge: None,
                max_workers: 30,
            })
        );
        assert!(g.outgoing("c").next().is_none());
        let default_workers = leviath_runtime::spec::graph::FanOutDef::same_graph(
            leviath_runtime::spec::names::StageName::new("x").unwrap(),
        )
        .max_workers as usize;
        assert_eq!(
            g.node("c").unwrap().kind,
            NodeKind::Stage(StageKind::FanOut {
                worker: WorkerRef::Query("anything that reads logs".to_string()),
                merge: None,
                max_workers: default_workers,
            })
        );
        // External nodes come after every stage, and are not stages.
        assert_eq!(g.ids().nth(5), Some("ext:researcher"));
        assert_eq!(g.stage_index("ext:researcher"), None);
        assert_eq!(g.stage_count(), 5, "the external nodes are not stages");
        assert_eq!(g.node("ext:researcher").unwrap().kind_label(), "blueprint");
    }

    #[test]
    fn every_stage_mode_maps_to_a_kind_with_a_label() {
        let g = graph(
            r#"
[[graph.stages]]
name = "a"
mode = "autonomous"
description = "first"
max_iterations = 4

[[graph.stages]]
name = "b"
mode = "interactive"

[[graph.stages]]
name = "c"
mode = { interactive_points = [{ name = "confirm", prompt = "ok?" }] }

[[graph.stages]]
name = "d"
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
        for (name, run_graph) in bundled_run_graphs() {
            let g = StageGraph::from_graph(&run_graph);
            let ids: HashSet<&str> = g.ids().collect();
            assert_eq!(ids.len(), g.nodes.len(), "{name}: duplicate ids");
            assert!(
                g.edges.iter().all(|e| e.from != e.to),
                "{name}: self-loop edge"
            );
            assert!(
                g.edges
                    .iter()
                    .all(|e| ids.contains(e.from.as_str()) && ids.contains(e.to.as_str())),
                "{name}: dangling edge"
            );
            assert!(g.nodes.iter().any(|n| n.is_entry), "{name}: no entry");
            assert!(
                g.nodes.iter().any(|n| n.is_terminal || n.allow_complete),
                "{name}: no way to finish"
            );
            // A multi-stage agent is a branching graph; a single-stage one (a
            // pure provider pipeline: a picture straight to a mesh) is one
            // node and hands its answer back on its own, which is a valid graph
            // too. Only a multi-stage agent that is a bare list is not.
            assert!(
                g.is_branching || g.nodes.len() == 1,
                "{name}: a multi-stage agent must be a branching graph"
            );
            seen += 1;
        }
        assert!(seen > 0, "the binary bundles agents");
    }
}
