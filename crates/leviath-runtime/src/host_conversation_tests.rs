//! An interactive stage end to end through a host: it replies and waits, a
//! message resumes it with its conversation intact, and the conversation
//! ends three ways. A sibling file, like `host_tests`, because these drive
//! the world through its async lanes.

use super::*;
use crate::pipeline::AwaitingMessage;
use crate::spec::graph::{InteractiveDef, StageMode};
use crate::state::PipelinePhase;
use leviath_core::RegionKind;
use leviath_providers::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider, ProviderError,
    TokenUsage, ToolCall,
};
use std::sync::Mutex;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

/// A provider that answers from a script, and keeps what each request said.
#[derive(Default)]
struct Script {
    responses: Mutex<std::collections::VecDeque<InferenceResponse>>,
    seen: Mutex<Vec<String>>,
    /// When set, the next call waits for this before it answers: a reply
    /// still in flight.
    held: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl Script {
    fn new(responses: Vec<InferenceResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            seen: Mutex::default(),
            held: Mutex::default(),
        })
    }

    /// Hold the next call until the returned handle is notified.
    fn hold_next(&self) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        *self.held.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// How many requests it has been sent.
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    /// The last request it was sent, as text.
    fn last(&self) -> String {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Provider for Script {
    async fn infer(&self, req: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{:?} {:?}", req.system, req.messages));
        let held = self.held.lock().unwrap().take();
        if let Some(gate) = held {
            gate.notified().await;
        }
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Other("exhausted".to_string()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "script"
    }
    fn capabilities(&self, _m: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn text(content: &str) -> InferenceResponse {
    InferenceResponse {
        parts: Vec::new(),
        content: content.to_string(),
        tool_calls: vec![],
        tokens_used: TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reported_cost_usd: None,
        },
        finish_reason: FinishReason::Complete,
        reasoning: None,
    }
}

/// A reply that says goodbye and calls `end_conversation`.
fn goodbye() -> InferenceResponse {
    let mut reply = text("Goodbye.");
    reply.tool_calls.push(ToolCall {
        id: "c1".to_string(),
        name: leviath_core::stage_tools::END_CONVERSATION_TOOL.to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    });
    reply
}

fn host(script: Arc<Script>, pool: crate::inference_pool::InferencePoolConfig) -> WorldHost {
    let mut registry = crate::providers::ProviderRegistry::new();
    registry.register("script".to_string(), script);
    WorldHost::new(PipelineWorld::new(
        registry,
        Arc::new(super::tests::NoTools),
        pool,
        1,
        None,
        Handle::current(),
    ))
}

/// The spec of a one-stage run `run_id` in `mode`, offered
/// `end_conversation` the way a spawn grants it. The nudge is off, so one
/// text reply is a stage's answer whatever its mode.
fn spec(run_id: &str, mode: StageMode) -> Arc<crate::spec::run_spec::RunSpec> {
    use crate::test_graph as g;
    let layout = g::layout(
        vec![g::region("conversation", RegionKind::Clearable, 10_000)],
        12_000,
    );
    let stage = crate::spec::graph::StageDef {
        model: g::model("script", "m"),
        mode,
        ..g::stage("s")
    };
    let mut graph = g::graph(vec![stage], layout);
    graph.nudge = Some(crate::spec::graph::NudgeDef {
        enabled: Some(false),
        ..Default::default()
    });
    let mut spec = g::both(graph).0;
    let own = Arc::make_mut(&mut spec);
    own.run_id = crate::spec::names::RunId::new(run_id).unwrap();
    own.stages[0].tools.push(crate::spec::run_spec::ToolDef {
        name: crate::spec::names::ToolName::new(leviath_core::stage_tools::END_CONVERSATION_TOOL)
            .unwrap(),
        description: String::new(),
        schema: leviath_core::JsonDoc::new(serde_json::json!({"type": "object", "properties": {}})),
        source: crate::spec::run_spec::ToolSource::StageControl,
    });
    spec
}

fn interactive(idle: Option<u64>) -> StageMode {
    StageMode::Interactive(InteractiveDef {
        idle_timeout_secs: idle,
    })
}

/// Place `spec` in the host's world and register it under its run id.
fn place(host: &mut WorldHost, spec: Arc<crate::spec::run_spec::RunSpec>) -> AgentId {
    let state = crate::insert::initial_state(&spec);
    let run_id = spec.run_id.to_string();
    let entity = crate::insert::insert(
        host.world_mut().world_mut(),
        spec,
        crate::spec::env::Bindings::new(),
        &state,
    );
    let agent = host.world.own_agent(entity);
    host.register(run_id, agent);
    agent
}

async fn ask<T>(host: &mut WorldHost, make: impl FnOnce(oneshot::Sender<T>) -> ControlOp) -> T {
    let (tx, rx) = oneshot::channel();
    host.handle(make(tx));
    rx.await.unwrap()
}

/// Whether the run is parked on its next message, every way a reader can
/// tell: its status, its wait reason, the phase its state records, and the
/// messages it reads.
fn parked(host: &WorldHost, agent: AgentId) -> bool {
    let world = host.world.world();
    let state = crate::state::inspect::inspect(world, agent.entity()).unwrap();
    host.world.agent_status(agent) == Some(AgentStatus::Waiting)
        && host.wait_reason(agent) == Some(WaitReason::AwaitingMessage)
        && state.phase == PipelinePhase::AwaitingMessage
        && state.wait_reason == Some(crate::state::WaitState::AwaitingMessage)
        && state.accepts_messages
        && world.get::<AwaitingMessage>(agent.entity()).is_some()
}

/// The status each run that finished since the last look finished with.
fn completions(rx: &mut tokio::sync::broadcast::Receiver<WorldEvent>) -> Vec<String> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|e| match e {
            WorldEvent::Completed { status, .. } => Some(status),
            _ => None,
        })
        .collect()
}

/// The same reply finishes an autonomous run and parks an interactive one:
/// only the mode differs.
#[tokio::test]
async fn an_interactive_stage_waits_where_an_autonomous_one_finishes() {
    let script = Script::new(vec![text("Hello."), text("Hello.")]);
    let mut host = host(script.clone(), Default::default());
    let autonomous = place(&mut host, spec("auto", StageMode::Autonomous));
    let chat = place(&mut host, spec("chat", interactive(None)));
    host.world_mut().run_until_idle(20).await;

    assert_eq!(
        host.world.agent_status(autonomous),
        Some(AgentStatus::Complete)
    );
    assert!(parked(&host, chat));
    assert_eq!(script.calls(), 2);
}

/// A message asks the model again in the same stage, with what was said
/// before still in front of it, and the stage waits again after the reply.
#[tokio::test]
async fn a_message_resumes_the_conversation_and_it_waits_again() {
    let script = Script::new(vec![text("Hello there."), text("Of course.")]);
    let mut host = host(script.clone(), Default::default());
    let chat = place(&mut host, spec("chat", interactive(None)));
    host.world_mut().run_until_idle(20).await;
    assert!(parked(&host, chat));

    let delivered = ask(&mut host, |reply| ControlOp::Message {
        agent_id: "chat".to_string(),
        content: "Also handle the empty case".to_string(),
        target_region: None,
        parts: Vec::new(),
        reply,
    })
    .await;
    assert_eq!(delivered, Ok(true));
    host.world_mut().run_until_idle(20).await;

    assert_eq!(script.calls(), 2);
    let second = script.last();
    assert!(second.contains("Hello there."), "{second}");
    assert!(second.contains("Also handle the empty case"), "{second}");
    assert!(parked(&host, chat));
}

/// A person ending the conversation finishes a final stage, once.
#[tokio::test]
async fn ending_the_conversation_finishes_a_final_stage_once() {
    let script = Script::new(vec![text("Hello.")]);
    let mut host = host(script.clone(), Default::default());
    let mut events = host.subscribe();
    let chat = place(&mut host, spec("chat", interactive(None)));
    host.world_mut().run_until_idle(20).await;
    host.emit_events();
    assert!(completions(&mut events).is_empty());

    let ended = ask(&mut host, |reply| ControlOp::EndConversation {
        run_id: "chat".to_string(),
        reply,
    })
    .await;
    assert_eq!(ended, Ok(true));
    host.world_mut().run_until_idle(20).await;
    host.emit_events();
    host.world_mut().run_until_idle(20).await;
    host.emit_events();

    assert_eq!(completions(&mut events), vec!["complete".to_string()]);
    assert_ne!(host.world.agent_status(chat), Some(AgentStatus::Waiting));
    assert_eq!(script.calls(), 1, "leaving asks the model nothing more");
}

/// The model closing the conversation with its tool finishes the run without
/// waiting for another message.
#[tokio::test]
async fn the_end_conversation_tool_moves_the_stage_on() {
    let script = Script::new(vec![goodbye()]);
    let mut host = host(script.clone(), Default::default());
    let mut events = host.subscribe();
    let chat = place(&mut host, spec("chat", interactive(None)));
    host.world_mut().run_until_idle(20).await;
    host.emit_events();

    assert_eq!(completions(&mut events), vec!["complete".to_string()]);
    assert_ne!(host.world.agent_status(chat), Some(AgentStatus::Waiting));
    assert_eq!(script.calls(), 1);
}

/// A stage whose idle limit runs out moves on as though the conversation had
/// been ended.
#[tokio::test]
async fn an_idle_limit_finishes_a_final_stage() {
    let script = Script::new(vec![text("Hello.")]);
    let mut host = host(script.clone(), Default::default());
    let chat = place(&mut host, spec("chat", interactive(Some(60))));
    host.world_mut().run_until_idle(20).await;
    assert!(parked(&host, chat));

    let now = chrono::Utc::now().timestamp();
    host.world_mut()
        .world_mut()
        .get_mut::<crate::pipeline::StageProgress>(chat.entity())
        .unwrap()
        .waiting_since = Some(now - 61);
    host.world_mut().run_until_idle(20).await;

    assert_eq!(host.world.agent_status(chat), Some(AgentStatus::Complete));
}

/// Ending a conversation names a run that has one: a run that is not here,
/// or not in an interactive stage, is refused with the reason.
#[tokio::test]
async fn ending_a_conversation_that_is_not_there_is_refused() {
    let script = Script::new(vec![text("Hello.")]);
    let mut host = host(script, Default::default());
    place(&mut host, spec("auto", StageMode::Autonomous));
    let missing = ask(&mut host, |reply| ControlOp::EndConversation {
        run_id: "nobody".to_string(),
        reply,
    })
    .await;
    assert!(missing.unwrap_err().contains("no run 'nobody'"));
    let autonomous = ask(&mut host, |reply| ControlOp::EndConversation {
        run_id: "auto".to_string(),
        reply,
    })
    .await;
    assert!(autonomous.unwrap_err().contains("not interactive"));
}

/// A run the daemon would have to page in for the end, and cannot, is
/// refused with why: stopped, held off this machine, or damaged on disk.
#[tokio::test]
async fn ending_a_conversation_of_a_run_that_cannot_come_back_says_why() {
    let cases = [
        (
            NotPlaced::Stopped(AgentStatus::Complete),
            "has stopped (complete)",
        ),
        (
            NotPlaced::Held(Box::new(crate::restore::damaged_entry("gone", "moved"))),
            "cannot go on on this machine",
        ),
        (
            NotPlaced::Damaged {
                entry: Box::new(crate::restore::damaged_entry("gone", "torn")),
                why: "the file is torn".to_string(),
            },
            "cannot be read back: the file is torn",
        ),
    ];
    for (placed, says) in cases {
        let mut host = host(Script::new(Vec::new()), Default::default());
        host.set_reloader(Box::new(move |_run_id, _purpose| {
            let placed = placed.clone();
            Box::pin(async move { Err(placed) })
        }));
        let refused = ask(&mut host, |reply| ControlOp::EndConversation {
            run_id: "gone".to_string(),
            reply,
        })
        .await;
        assert!(refused.unwrap_err().contains(says));
    }
}

/// A parked run holds no inference slot: with room for one call at a time,
/// a second run still gets its turn while the first waits for a message.
#[tokio::test]
async fn a_parked_run_does_not_hold_an_inference_slot() {
    let script = Script::new(vec![text("First."), text("Second.")]);
    let pool = crate::inference_pool::InferencePoolConfig::new().with_default(Some(1));
    let mut host = host(script.clone(), pool);
    let first = place(&mut host, spec("first", interactive(None)));
    host.world_mut().run_until_idle(20).await;
    assert!(parked(&host, first));

    let second = place(&mut host, spec("second", interactive(None)));
    host.world_mut().run_until_idle(20).await;
    assert!(parked(&host, second));
    assert_eq!(script.calls(), 2);
}

/// A parked run written down and brought back, as a daemon restart does,
/// waits again without asking its model anything; a message then resumes it.
#[tokio::test]
async fn a_parked_run_comes_back_parked_without_a_model_call() {
    let before = Script::new(vec![text("Hello.")]);
    let mut first = host(before, Default::default());
    let chat = place(&mut first, spec("chat", interactive(None)));
    first.world_mut().run_until_idle(20).await;
    assert!(parked(&first, chat));
    let state = crate::state::inspect::inspect(first.world.world(), chat.entity()).unwrap();
    let bytes = postcard::to_stdvec(&state).unwrap();
    let state: crate::state::RunState = postcard::from_bytes(&bytes).unwrap();

    let after = Script::new(vec![text("Welcome back.")]);
    let mut second = host(after.clone(), Default::default());
    let entity = crate::restore::resume(
        second.world_mut().world_mut(),
        crate::restore::Resumable {
            spec: spec("chat", interactive(None)),
            state,
            code: Default::default(),
            answer: None,
            asked: 0,
            audit: Vec::new(),
        },
        crate::spec::env::Bindings::new(),
    );
    let back = second.world.own_agent(entity);
    second.register("chat", back);
    second.world_mut().run_until_idle(20).await;
    assert!(parked(&second, back));
    assert_eq!(after.calls(), 0, "nothing is asked until somebody speaks");

    let delivered = ask(&mut second, |reply| ControlOp::Message {
        agent_id: "chat".to_string(),
        content: "Still there?".to_string(),
        target_region: None,
        parts: Vec::new(),
        reply,
    })
    .await;
    assert_eq!(delivered, Ok(true));
    second.world_mut().run_until_idle(20).await;
    assert_eq!(after.calls(), 1);
    assert!(after.last().contains("Hello."));
    assert!(parked(&second, back));
}

/// A parked run paused and resumed goes back to waiting for its message,
/// not to asking its model.
#[tokio::test]
async fn resuming_a_paused_parked_run_waits_again() {
    let script = Script::new(vec![text("Hello.")]);
    let mut host = host(script.clone(), Default::default());
    let chat = place(&mut host, spec("chat", interactive(None)));
    host.world_mut().run_until_idle(20).await;
    host.world_mut().set_status(chat, AgentStatus::Paused);
    assert!(host.world_mut().resume(chat));
    host.world_mut().run_until_idle(20).await;
    assert!(parked(&host, chat));
    assert_eq!(script.calls(), 1);
}

/// A message that arrives while the stage's reply is still on its way is
/// answered: the reply lands, and the stage asks its model again with the
/// message rather than waiting on a message it never read.
#[tokio::test]
async fn a_message_sent_mid_turn_is_answered_before_the_stage_waits() {
    let script = Script::new(vec![text("Working on it."), text("Got your note.")]);
    let gate = script.hold_next();
    let mut host = host(script.clone(), Default::default());
    let chat = place(&mut host, spec("chat", interactive(None)));
    host.world_mut().run_to_fixed_point();
    while script.calls() == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let delivered = ask(&mut host, |reply| ControlOp::Message {
        agent_id: "chat".to_string(),
        content: "also the empty case".to_string(),
        target_region: None,
        parts: Vec::new(),
        reply,
    })
    .await;
    assert_eq!(delivered, Ok(true));
    host.world_mut().run_to_fixed_point();
    gate.notify_one();
    host.world_mut().run_until_idle(20).await;

    assert_eq!(script.calls(), 2);
    assert!(script.last().contains("also the empty case"));
    assert!(parked(&host, chat));
}

/// The same for a run paused mid-turn: the message is answered once it is
/// resumed.
#[tokio::test]
async fn a_message_sent_while_paused_mid_turn_is_answered_after_resume() {
    let script = Script::new(vec![text("Working on it."), text("Got your note.")]);
    let gate = script.hold_next();
    let mut host = host(script.clone(), Default::default());
    let chat = place(&mut host, spec("chat", interactive(None)));
    host.world_mut().run_to_fixed_point();
    while script.calls() == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(host.world_mut().pause(chat));

    let delivered = ask(&mut host, |reply| ControlOp::Message {
        agent_id: "chat".to_string(),
        content: "also the empty case".to_string(),
        target_region: None,
        parts: Vec::new(),
        reply,
    })
    .await;
    assert_eq!(delivered, Ok(true));
    host.world_mut().run_to_fixed_point();
    gate.notify_one();
    // The reply lands and is held while the run is paused; nothing wakes the
    // world for it, so drive it a while by hand.
    for _ in 0..50 {
        host.world_mut().run_to_fixed_point();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(script.calls(), 1, "nothing more while it is paused");

    assert!(host.world_mut().resume(chat));
    host.world_mut().run_until_idle(20).await;
    assert_eq!(script.calls(), 2);
    assert!(script.last().contains("also the empty case"));
    assert!(parked(&host, chat));
}

/// A parked run can be paused, which holds its idle clock: it does not move
/// on while somebody holds it, and comes back waiting when resumed.
#[tokio::test]
async fn a_parked_run_can_be_paused_and_its_idle_limit_waits() {
    let script = Script::new(vec![text("Hello.")]);
    let mut host = host(script.clone(), Default::default());
    let chat = place(&mut host, spec("chat", interactive(Some(60))));
    host.world_mut().run_until_idle(20).await;
    assert!(parked(&host, chat));
    assert!(host.world_mut().pause(chat));
    let now = chrono::Utc::now().timestamp();
    host.world_mut()
        .world_mut()
        .get_mut::<crate::pipeline::StageProgress>(chat.entity())
        .unwrap()
        .waiting_since = Some(now - 61);
    host.world_mut().run_until_idle(20).await;
    assert_eq!(host.world.agent_status(chat), Some(AgentStatus::Paused));
    assert!(host.world_mut().resume(chat));
    host.world_mut().run_until_idle(20).await;
    assert_ne!(host.world.agent_status(chat), Some(AgentStatus::Paused));
}
