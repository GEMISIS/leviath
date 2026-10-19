//! A stage's answer end to end through a host, against a model that calls
//! `submit_output` on every turn and never sends a reply without a tool call.
//! A stage with nowhere else to go ends on its first accepted answer; one with
//! somewhere to go ends once it has repeated its answer; a gate that sends a
//! stage back still gets a stage that answers again; an interactive stage
//! talks on. A sibling file, like `host_tests`, because these drive the world
//! through its async lanes.

use super::conversation_tests::{Script, host, place, text};
use super::*;
use crate::spec::graph::{InteractiveDef, StageDef, StageMode};
use crate::test_graph as g;
use leviath_core::RegionKind;
use leviath_core::stage_tools::SUBMIT_OUTPUT_TOOL;
use leviath_providers::{InferenceResponse, ToolCall};
use serde_json::json;

/// A reply that makes `calls`, each with its own id.
fn calling(calls: &[(&str, serde_json::Value)]) -> InferenceResponse {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let mut reply = text("");
    for (name, arguments) in calls {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        reply.tool_calls.push(ToolCall {
            id: format!("call-{n}"),
            name: name.to_string(),
            arguments: arguments.clone(),
            thought_signature: None,
        });
    }
    reply
}

/// A reply that submits `content` and nothing else.
fn submit(content: &str) -> InferenceResponse {
    calling(&[(SUBMIT_OUTPUT_TOOL, json!({ "content": content }))])
}

/// An output stage named `name`, which must hand back an answer.
fn output(name: &str) -> StageDef {
    StageDef {
        mode: StageMode::Output,
        require_output: true,
        ..g::stage(name)
    }
}

/// The spec of run `run_id` over `stages`, each going on to the next, each
/// offered `submit_output` and `context_append`, with a required region
/// `notes` when `notes` is set. The nudge is off.
fn spec(run_id: &str, stages: Vec<StageDef>, notes: bool) -> Arc<crate::spec::run_spec::RunSpec> {
    let mut regions = vec![g::region("conversation", RegionKind::Clearable, 10_000)];
    if notes {
        regions.push(crate::spec::graph::RegionDef {
            required: true,
            ..g::region("notes", RegionKind::Pinned, 1_000)
        });
    }
    let stages = stages
        .into_iter()
        .map(|s| StageDef {
            model: g::model("script", "m"),
            tools: g::tools(&[SUBMIT_OUTPUT_TOOL, "context_append"]),
            ..s
        })
        .collect();
    let mut graph = g::graph(stages, g::layout(regions, 12_000));
    graph.nudge = Some(crate::spec::graph::NudgeDef {
        enabled: Some(false),
        ..Default::default()
    });
    let mut spec = g::both(graph).0;
    let own = Arc::make_mut(&mut spec);
    own.run_id = crate::spec::names::RunId::new(run_id).unwrap();
    for plan in &mut own.stages {
        for tool in [SUBMIT_OUTPUT_TOOL, "context_append"] {
            plan.tools.push(crate::spec::run_spec::ToolDef {
                name: crate::spec::names::ToolName::new(tool).unwrap(),
                description: String::new(),
                schema: leviath_core::JsonDoc::new(json!({ "type": "object" })),
                source: crate::spec::run_spec::ToolSource::StageControl,
            });
        }
    }
    spec
}

/// Every event since the last look.
fn drain(rx: &mut tokio::sync::broadcast::Receiver<WorldEvent>) -> Vec<WorldEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// The answers `events` announced, in order.
fn announced(events: &[WorldEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WorldEvent::OutputSubmitted { output, .. } => Some(output.content.clone()),
            _ => None,
        })
        .collect()
}

/// The status each run that finished among `events` finished with.
fn finished(events: &[WorldEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            WorldEvent::Completed { status, .. } => Some(status.clone()),
            _ => None,
        })
        .collect()
}

/// Each stage's resubmissions, by name.
fn resubmissions(host: &WorldHost, agent: AgentId) -> Vec<(String, usize)> {
    host.world
        .world()
        .get::<crate::pipeline::StageLedger>(agent.entity())
        .unwrap()
        .0
        .iter()
        .map(|r| (r.name.clone(), r.resubmissions))
        .collect()
}

/// Drive the host until it settles, and every answer it announced on the
/// way. Its runs stay loaded, so a finished one can still be read.
async fn settle(host: &mut WorldHost) -> Vec<WorldEvent> {
    let mut rx = host.subscribe();
    for _ in 0..3 {
        host.world_mut().run_until_idle(40).await;
    }
    drain(&mut rx)
}

/// The run's answer, as recorded.
fn answer(host: &WorldHost, agent: AgentId) -> Option<String> {
    host.world
        .world()
        .get::<crate::persistence::FinalOutput>(agent.entity())
        .map(|o| o.0.content.clone())
}

/// A terminal output stage ends on its first accepted answer: the model is
/// not asked again, however keen it is to submit, and the answer is
/// announced once.
#[tokio::test]
async fn a_terminal_stage_ends_on_its_first_accepted_answer() {
    let script = Script::new(vec![submit("x"), submit("x"), submit("x")]);
    let mut host = host(script.clone(), Default::default());
    let run = place(&mut host, spec("term", vec![output("answer")], false));
    let events = settle(&mut host).await;

    assert_eq!(script.calls(), 1);
    assert_eq!(host.world.agent_status(run), Some(AgentStatus::Complete));
    assert_eq!(answer(&host, run).as_deref(), Some("x"));
    assert_eq!(announced(&events), ["x"]);
    let mut rx = host.subscribe();
    host.emit_events();
    assert_eq!(finished(&drain(&mut rx)), ["complete"]);
    assert_eq!(resubmissions(&host, run), [("answer".to_string(), 0)]);
}

/// A stage with somewhere to go is told its repeated answer is already in,
/// records nothing again, and ends once it has repeated it as often as it
/// may. The next stage, a terminal one, ends on its own first answer.
#[tokio::test]
async fn a_stage_with_somewhere_to_go_ends_after_repeating_its_answer() {
    let script = Script::new(vec![
        submit("d"),
        submit("d"),
        submit("d"),
        submit("f"),
        submit("f"),
    ]);
    let mut host = host(script.clone(), Default::default());
    let run = place(
        &mut host,
        spec("mid", vec![output("draft"), output("final")], false),
    );
    let events = settle(&mut host).await;

    assert_eq!(script.calls(), 4);
    assert!(
        script.last().contains("Already recorded"),
        "{}",
        script.last()
    );
    assert_eq!(host.world.agent_status(run), Some(AgentStatus::Complete));
    assert_eq!(answer(&host, run).as_deref(), Some("f"));
    assert_eq!(announced(&events), ["d", "f"]);
    assert_eq!(
        resubmissions(&host, run),
        [("draft".to_string(), 2), ("final".to_string(), 0)]
    );
}

/// A changed answer is a new answer: each is recorded and announced, and the
/// stage leaves on a reply with no tool call, as it always could.
#[tokio::test]
async fn a_changed_answer_is_recorded_each_time() {
    let script = Script::new(vec![submit("a"), submit("b"), text("Done."), submit("f")]);
    let mut host = host(script.clone(), Default::default());
    let run = place(
        &mut host,
        spec("changed", vec![output("draft"), output("final")], false),
    );
    let events = settle(&mut host).await;

    assert_eq!(script.calls(), 4);
    assert_eq!(announced(&events), ["a", "b", "f"]);
    assert_eq!(
        resubmissions(&host, run),
        [("draft".to_string(), 1), ("final".to_string(), 0)]
    );
}

/// A gate that sends a terminal stage back (here, a required region it left
/// empty) gets a stage that can answer again: it fills the region, hands back
/// the answer it already gave, and ends then. The answer is announced once.
#[tokio::test]
async fn a_gate_that_sends_the_stage_back_gets_its_answer_again() {
    let script = Script::new(vec![
        submit("x"),
        calling(&[
            (
                "context_append",
                json!({ "region": "notes", "content": "checked" }),
            ),
            (SUBMIT_OUTPUT_TOOL, json!({ "content": "x" })),
        ]),
        submit("x"),
    ]);
    let mut host = host(script.clone(), Default::default());
    let run = place(&mut host, spec("gated", vec![output("answer")], true));
    let events = settle(&mut host).await;

    assert_eq!(script.calls(), 2);
    assert_eq!(host.world.agent_status(run), Some(AgentStatus::Complete));
    assert_eq!(announced(&events), ["x"]);
    assert_eq!(resubmissions(&host, run), [("answer".to_string(), 1)]);
}

/// An interactive stage is not ended by its answer, and records each
/// submission as it always did: its conversation says when it is over.
#[tokio::test]
async fn an_interactive_stage_talks_on_after_its_answer() {
    let chat = StageDef {
        mode: StageMode::Interactive(InteractiveDef {
            idle_timeout_secs: None,
        }),
        ..g::stage("chat")
    };
    let script = Script::new(vec![submit("x"), submit("x"), text("Anything else?")]);
    let mut host = host(script.clone(), Default::default());
    let run = place(&mut host, spec("chat", vec![chat], false));
    let events = settle(&mut host).await;

    assert_eq!(script.calls(), 3);
    assert_eq!(host.world.agent_status(run), Some(AgentStatus::Waiting));
    assert_eq!(announced(&events), ["x", "x"]);
}
