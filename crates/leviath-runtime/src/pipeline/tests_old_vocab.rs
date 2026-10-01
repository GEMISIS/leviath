//! The parsed-blueprint vocabulary these tests are written in, read as the
//! run graph and spec the pipeline runs on.
//!
//! The tests build stages, edges and gates the way a parsed blueprint holds
//! them. Each helper here reads one of those as the graph reads it (through
//! [`RunGraph::from_blueprint`], the same reading a spawn makes) and calls the
//! pipeline with that, so a test exercises exactly what a spawned run would.

use std::collections::HashMap;
use std::sync::Arc;

use crate::components::ContextWindow;
use crate::insert::RunSpecC;
use crate::pipeline::{GateDecision, StageInference, StageProgress, StuckMetrics};
use crate::spec::blueprint::{
    EdgeTransform, ModelConfig, StuckConfig, TransitionCondition, TransitionEdge, TransitionGate,
};
use crate::spec::graph::{
    EdgeCarry, EdgeCondition, EdgeDef, GateDef, RunGraph, StageDef, StuckDef,
};
use crate::spec::{Blueprint, ContextLayout, Stage};

/// The context window every plan built here claims, so a percentage budget
/// resolves the way a spawn without a registered provider resolves it.
const WINDOW: u32 = crate::pipeline::DEFAULT_CONTEXT_WINDOW_TOKENS as u32;

/// The inference stage `i` of a test blueprint runs on, unless a test says
/// otherwise: provider `p`, model `m{i}`.
pub(super) fn plan_inference(i: usize) -> StageInference {
    StageInference {
        provider_name: "p".to_string(),
        model: format!("m{i}"),
        ..Default::default()
    }
}

/// A blueprint as the spec a spawn of it runs, each stage on the matching
/// inference of `infs`.
pub(super) fn spec_with(bp: Blueprint, infs: &[StageInference]) -> RunSpecC {
    let mut spec = crate::spec_bridge::test_support::spec_of_blueprint(&bp, "t-run", infs);
    for plan in &mut spec.stages {
        plan.context_window = WINDOW;
    }
    RunSpecC(Arc::new(spec))
}

/// A blueprint as the spec a spawn of it runs, stage `i` on [`plan_inference`].
pub(super) fn spec_of(bp: Blueprint) -> RunSpecC {
    let infs: Vec<StageInference> = (0..bp.stages.len()).map(plan_inference).collect();
    spec_with(bp, &infs)
}

/// A stage as the graph reads it.
pub(super) fn stage_def(stage: &Stage) -> StageDef {
    crate::pipeline::spec_view::tests::stage_def_of(stage.clone())
}

/// An edge leaving a stage named `from`, as the graph reads it.
pub(super) fn edge_def(from: &str, edge: &TransitionEdge) -> EdgeDef {
    let mut stage = Stage::new(
        from.to_string(),
        ModelConfig::new("p".to_string(), "m".to_string()),
    );
    stage.transitions = Some(HashMap::from([(edge.target.clone(), edge.clone())]));
    let bp = Blueprint::new(
        "t".into(),
        "d".into(),
        vec![stage],
        ContextLayout::new(Vec::new(), 1000),
    );
    RunGraph::from_blueprint(&bp)
        .expect("a test edge reads as a graph edge")
        .edges
        .remove(0)
}

/// Edges leaving a stage named `s`, in the order given.
pub(super) fn edges_of(edges: &[TransitionEdge]) -> Vec<EdgeDef> {
    edges.iter().map(|e| edge_def("s", e)).collect()
}

/// A gate as the graph reads it.
pub(super) fn gate_def(gate: &TransitionGate) -> GateDef {
    let edge = TransitionEdge {
        target: "next".to_string(),
        condition: TransitionCondition::Always,
        hint: None,
        transform: EdgeTransform::Direct,
        gate: Some(gate.clone()),
        stuck: None,
    };
    edge_def("s", &edge).gate.expect("the gate survives")
}

/// A context transform as the graph reads it.
pub(super) fn carry_of(transform: &EdgeTransform) -> EdgeCarry {
    let edge = TransitionEdge {
        target: "next".to_string(),
        condition: TransitionCondition::Always,
        hint: None,
        transform: transform.clone(),
        gate: None,
        stuck: None,
    };
    edge_def("s", &edge).carry
}

/// Stuck thresholds as the graph reads them.
pub(super) fn stuck_def(cfg: &StuckConfig) -> StuckDef {
    let edge = TransitionEdge {
        target: "next".to_string(),
        condition: TransitionCondition::Stuck,
        hint: None,
        transform: EdgeTransform::Direct,
        gate: None,
        stuck: Some(*cfg),
    };
    edge_def("s", &edge).stuck.expect("the thresholds survive")
}

fn condition(c: TransitionCondition) -> EdgeCondition {
    match c {
        TransitionCondition::Always => EdgeCondition::Always,
        TransitionCondition::Error => EdgeCondition::Error,
        TransitionCondition::MaxIterations => EdgeCondition::MaxIterations,
        TransitionCondition::LlmChoice => EdgeCondition::LlmChoice,
        TransitionCondition::DeadEnd => EdgeCondition::DeadEnd,
        TransitionCondition::Stuck => EdgeCondition::Stuck,
    }
}

/// [`super::super::gate_blocks`] for a parsed gate and stage.
pub(super) fn gate_blocks(
    gate: Option<&TransitionGate>,
    stage: &Stage,
    progress: &StageProgress,
    window: &ContextWindow,
) -> GateDecision {
    let gate = gate.map(gate_def);
    super::super::gate_blocks(gate.as_ref(), &stage_def(stage), progress, window)
}

/// [`super::super::detect_stuck`] for parsed thresholds.
pub(super) fn detect_stuck(cfg: &StuckConfig, m: &StuckMetrics) -> Option<String> {
    super::super::detect_stuck(&stuck_def(cfg), m)
}

/// [`super::super::apply_edge_transform`] for a parsed transform.
pub(super) fn apply_edge_transform(window: &mut ContextWindow, t: &EdgeTransform) -> Vec<String> {
    super::super::apply_edge_transform(window, &carry_of(t))
}

/// [`super::super::find_conditioned_edge`] for a parsed blueprint and stage:
/// the target's position and the edge's context transform.
pub(super) fn find_conditioned_edge(
    bp: &Blueprint,
    stage: &Stage,
    visits: &HashMap<String, usize>,
    c: TransitionCondition,
) -> Option<(usize, EdgeCarry)> {
    let graph = RunGraph::from_blueprint(bp).expect("a test blueprint reads as a graph");
    let stage = graph
        .stage(&stage.name)
        .expect("the stage is in the blueprint");
    super::super::find_conditioned_edge(&graph, stage, visits, condition(c))
        .map(|next| (next.idx, next.carry))
}

/// [`super::super::build_transition_prompt`] for a parsed stage and edges.
pub(super) fn build_transition_prompt(stage: &Stage, edges: &[TransitionEdge]) -> String {
    super::super::build_transition_prompt(&stage_def(stage), &edges_of(edges))
}

/// [`super::super::match_transition_choice`] for parsed edges.
pub(super) fn match_transition_choice(
    choice: &str,
    edges: &[TransitionEdge],
    allow_complete: bool,
) -> Option<String> {
    super::super::match_transition_choice(choice, &edges_of(edges), allow_complete)
}

/// [`super::super::stage_expected_media`] for a parsed stage.
pub(super) fn stage_expected_media(stage: Option<&Stage>) -> Option<&'static str> {
    super::super::stage_expected_media(stage.map(stage_def).as_ref())
}

/// The digests [`crate::pipeline::transition::watched_region_digests`] takes
/// for a parsed stage on entry.
pub(super) fn watched_region_digests(
    stage: &Stage,
    window: &ContextWindow,
) -> std::collections::HashMap<String, u64> {
    let bp = Blueprint::new(
        "t".into(),
        "d".into(),
        vec![stage.clone()],
        ContextLayout::new(Vec::new(), 1000),
    );
    let graph = RunGraph::from_blueprint(&bp).expect("a test stage reads as a graph");
    crate::pipeline::transition::watched_region_digests(&graph, &graph.stages[0], window)
}

/// How a parsed stage is entered, as a one-stage spawn of it would enter it:
/// `agent` is the blueprint's own hint settings, `global` the operator's, and
/// `output` the stage's resolved output shape.
pub(super) fn stage_setup_from(
    stage: &Stage,
    global: leviath_core::config::PromptHints,
    agent: leviath_core::config::PromptHintOverrides,
    output: Option<leviath_core::output::OutputSpec>,
) -> crate::pipeline::StageSetup {
    let mut bp = Blueprint::new(
        "t".into(),
        "d".into(),
        vec![stage.clone()],
        ContextLayout::new(Vec::new(), 1000),
    );
    bp.batch_tool_hint = agent.batch_tool;
    bp.shell_hint = agent.shell;
    let mut inference = plan_inference(0);
    inference.output = output;
    let mut spec = spec_with(bp, &[inference]);
    let graph = &mut Arc::make_mut(&mut spec.0).graph;
    graph.batch_tool_hint = Some(graph.batch_tool_hint.unwrap_or(global.batch_tool));
    graph.shell_hint = Some(graph.shell_hint.unwrap_or(global.shell));
    crate::pipeline::spec_view::stage_setup(&spec.0, 0)
}

/// A parsed stage named `name` that a spawn would enter with `setup`.
pub(super) fn stage_from_setup(name: &str, setup: &crate::pipeline::StageSetup) -> Stage {
    use crate::spec::blueprint::OutputCap;
    use serde_json::json;
    let mut s = Stage::new(
        name.to_string(),
        ModelConfig::new("p".to_string(), "m".to_string()),
    );
    if let Some(prompt) = &setup.system_prompt {
        s.config.insert("system_prompt".to_string(), json!(prompt));
    }
    let cfg = &setup.inference_config;
    let params = &mut s.model.parameters;
    if let Some(t) = cfg.temperature {
        params.insert("temperature".to_string(), json!(t));
    }
    if let Some(cap) = &cfg.max_output_tokens {
        let written = match cap {
            OutputCap::Tokens(n) => json!(n),
            OutputCap::WindowPercent(p) => json!({ "percent": p * 100.0 }),
            OutputCap::RegionPercent { percent, region } => {
                json!({ "percent": percent * 100.0, "of": region })
            }
        };
        params.insert("max_output_tokens".to_string(), written);
    }
    for (k, v) in &cfg.extra_params {
        params.insert(k.clone(), v.clone());
    }
    s.model.request_timeout_secs = cfg.request_timeout_secs;
    s.batch_tool_hint = Some(cfg.batch_tool_hint);
    s.shell_hint = Some(cfg.shell_hint);
    s.input_as_text = cfg.as_text.clone();
    s.tool_result_routing = setup.routing.clone();
    s.accepts_messages = setup.accepts_messages;
    s.context_layout = setup.context_layout.clone();
    s.context_hide = setup.context_hide.clone();
    s.context_reset = setup.context_reset.clone();
    s
}

#[test]
fn every_parsed_condition_reads_as_the_graph_condition_of_the_same_name() {
    for c in [
        TransitionCondition::Always,
        TransitionCondition::Error,
        TransitionCondition::MaxIterations,
        TransitionCondition::LlmChoice,
        TransitionCondition::DeadEnd,
        TransitionCondition::Stuck,
    ] {
        assert_eq!(format!("{:?}", condition(c.clone())), format!("{c:?}"));
    }
}

#[test]
fn a_setup_reads_back_through_the_stage_written_for_it() {
    use crate::spec::blueprint::OutputCap;
    use crate::spec::graph::OutputCap as Cap;
    let mut setup = crate::pipeline::StageSetup::default();
    setup
        .inference_config
        .extra_params
        .insert("top_p".to_string(), serde_json::json!(0.9));
    let read = stage_def(&stage_from_setup("s", &setup));
    assert!(read.model.params.extra.contains_key("top_p"));
    for (cap, want) in [
        (OutputCap::Tokens(9), Cap::Tokens(9)),
        (OutputCap::WindowPercent(0.5), Cap::WindowPercent(0.5)),
    ] {
        setup.inference_config.max_output_tokens = Some(cap);
        let read = stage_def(&stage_from_setup("s", &setup));
        assert_eq!(read.model.params.max_output_tokens, Some(want));
    }
    setup.inference_config.max_output_tokens = Some(OutputCap::RegionPercent {
        percent: 0.25,
        region: "task".to_string(),
    });
    let read = stage_def(&stage_from_setup("s", &setup));
    assert!(matches!(
        read.model.params.max_output_tokens,
        Some(Cap::RegionPercent { ref region, .. }) if region.as_str() == "task"
    ));
}
