//! The persist system's run-file half: a run placed from a spec has its state
//! recorded beside its snapshot, and what the file says at the end is what the
//! world says.

use std::sync::{Arc, Mutex};

use bevy_ecs::prelude::*;
use leviath_core::{Region, RegionKind};
use leviath_providers::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider, TokenUsage,
    ToolCall,
};
use tokio::runtime::Handle;

use crate::components::{AgentState, AgentStatus, ContextWindow, InferenceConfig, MessageInbox};
use crate::pipeline::{
    ReadyToInfer, StageCursor, StageInference, StageProgress, StageSetup, ToolProgress,
    ToolService, VisitCounts,
};
use crate::runfile::RunFileReader;
use crate::tool_bridge::BoxedToolExec;
use crate::world::PipelineWorld;

struct Script {
    responses: Mutex<std::collections::VecDeque<InferenceResponse>>,
}

#[async_trait::async_trait]
impl Provider for Script {
    async fn infer(&self, _req: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text("done")))
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
            prompt_tokens: 3,
            completion_tokens: 2,
            total_tokens: 5,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reported_cost_usd: None,
        },
        finish_reason: FinishReason::Complete,
        reasoning: None,
    }
}

fn with_tool(id: &str) -> InferenceResponse {
    let mut r = text("calling");
    r.tool_calls.push(ToolCall {
        id: id.to_string(),
        name: "do".to_string(),
        arguments: serde_json::json!({"n": id}),
        thought_signature: None,
    });
    r
}

struct EchoTools;
impl ToolService for EchoTools {
    fn exec_for(&self, _e: Entity, calls: Vec<ToolCall>, _p: ToolProgress) -> BoxedToolExec {
        Box::new(move || {
            Box::pin(async move { calls.into_iter().map(|c| (c.id, "ok".into())).collect() })
        })
    }
}

fn stage() -> StageInference {
    StageInference {
        provider_name: "script".to_string(),
        model: "m".to_string(),
        tools: vec![leviath_providers::Tool {
            name: "do".to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }],
        tool_filter: None,
        fallbacks: Vec::new(),
        output: None,
    }
}

fn setup() -> StageSetup {
    StageSetup {
        inference_config: InferenceConfig {
            temperature: None,
            max_output_tokens: None,
            extra_params: Default::default(),
            batch_tool_hint: false,
            shell_hint: false,
            request_timeout_secs: None,
            as_text: Vec::new(),
        },
        routing: None,
        accepts_messages: true,
        context_layout: None,
        context_hide: Vec::new(),
        context_reset: Vec::new(),
        system_prompt: None,
    }
}

fn graph() -> crate::spec::graph::RunGraph {
    use crate::test_graph::{layout, model, region, stage};
    let s = crate::spec::graph::StageDef {
        model: model("script", "m"),
        ..stage("s")
    };
    let mut graph = crate::test_graph::graph(
        vec![s],
        layout(
            vec![region("conversation", RegionKind::Clearable, 10_000)],
            12_000,
        ),
    );
    graph.description = Some("d".into());
    graph
}

fn window() -> ContextWindow {
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 2000));
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    w
}

pub(crate) fn metadata(run_id: &str) -> crate::persistence::RunMetadata {
    crate::persistence::RunMetadata {
        run_id: run_id.to_string(),
        agent_name: "a".to_string(),
        agent_path: "/p".to_string(),
        task: "t".to_string(),
        model: None,
        workdir: std::env::temp_dir().to_string_lossy().to_string(),
        num_stages: 1,
        started_at: 0,
        parent_run_id: None,
        metadata: std::collections::HashMap::new(),
        callback_url: None,
        callback_secret: None,
        title: Some("A run".to_string()),
        title_error: None,
        blueprint_digest: None,
        unattended: false,
        yolo_profile: None,
        read_paths: None,
        output_request: None,
        model_override: None,
    }
}

fn agent(stage: &str) -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_visit: String::new(),
        current_stage: stage.to_string(),
        iteration: 0,
        status: AgentStatus::Active,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

/// The spec a spawn of [`graph`] places.
fn spec() -> Arc<crate::spec::run_spec::RunSpec> {
    crate::test_graph::both(graph()).0
}

/// A mock agent runs a few turns; afterwards its run file's last state is the
/// state the world holds.
#[tokio::test]
async fn the_run_file_ends_where_the_run_does() {
    let dir = tempfile::tempdir().unwrap();
    let mut providers = crate::ProviderRegistry::new();
    providers.register(
        "script".to_string(),
        Arc::new(Script {
            responses: Mutex::new(
                [with_tool("c1"), with_tool("c2"), text("all done")]
                    .into_iter()
                    .collect(),
            ),
        }),
    );
    let mut world = PipelineWorld::new(
        providers,
        Arc::new(EchoTools),
        crate::InferencePoolConfig::new(),
        1,
        Some(dir.path().to_path_buf()),
        Handle::current(),
    );
    let id = world.spawn_agent((
        (
            StageCursor { index: 0 },
            agent("s"),
            MessageInbox::default(),
            StageProgress::default(),
            VisitCounts::default(),
            window(),
            stage(),
            setup().inference_config,
        ),
        (
            metadata("run-42"),
            crate::persistence::TokenTotals::default(),
            crate::pipeline::PersistWatermark::default(),
            crate::persistence::RunClock::default(),
            ReadyToInfer,
            crate::insert::RunSpecC(spec()),
        ),
    ));
    world.run_until_idle(40).await;
    world.flush_and_stop().await;

    let live = crate::state::inspect::inspect(world.world(), id.entity()).unwrap();
    assert_eq!(live.status, crate::state::RunStatus::Complete);
    let path = dir
        .path()
        .join("run-42")
        .join(leviath_core::files::RUN_FILE);
    let file = RunFileReader::open(&path).unwrap();
    let recorded = file.latest_state().unwrap();
    assert!(recorded.seq > 1);
    // The file numbers the steps, and a live read says which one it is at.
    assert_eq!(recorded, live);
    // Every step on the way reads back, and the tool calls are in the deltas.
    let events: Vec<_> = file
        .deltas(1, file.last_seq())
        .unwrap()
        .into_iter()
        .flat_map(|d| d.events)
        .collect();
    let started = events
        .iter()
        .filter(|e| matches!(e, crate::state::RunEvent::ToolStarted(_)))
        .count();
    assert_eq!(started, 2);
    for seq in 0..=file.last_seq() {
        file.state_at(seq).unwrap();
    }
    // Nothing in the older layout is written beside it.
    assert!(!dir.path().join("run-42").join("meta.json").exists());
}

fn persist_world() -> (
    World,
    tokio::sync::mpsc::UnboundedReceiver<crate::persistence_bridge::PersistMsg>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(super::PersistenceStage(tx));
    (world, rx)
}

fn spawn_run(world: &mut World, stage: &str, with_spec: bool) -> Entity {
    let mut e = world.spawn((
        metadata("r1"),
        agent(stage),
        window(),
        StageCursor { index: 0 },
        crate::persistence::TokenTotals::default(),
        crate::pipeline::PersistWatermark::default(),
    ));
    if with_spec {
        e.insert(crate::insert::RunSpecC(spec()));
    }
    e.id()
}

fn sent_step(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::persistence_bridge::PersistMsg>,
) -> Option<bool> {
    match rx.try_recv().ok()? {
        crate::persistence_bridge::PersistMsg::Snapshot(job) => Some(job.run_file.is_some()),
        _ => None,
    }
}

#[test]
fn only_a_run_placed_from_a_spec_with_a_readable_state_carries_a_step() {
    for (stage, with_spec, carries) in [("s", true, true), ("s", false, false), ("", true, false)] {
        let (mut world, mut rx) = persist_world();
        spawn_run(&mut world, stage, with_spec);
        super::dispatch_persistence(&mut world);
        assert_eq!(sent_step(&mut rx), Some(carries));
    }
}

#[test]
fn a_world_with_no_persistence_lane_persists_nothing() {
    let mut world = World::new();
    spawn_run(&mut world, "s", true);
    super::dispatch_persistence(&mut world);
    assert!(world.get_resource::<super::PersistenceStage>().is_none());
}

/// The step the next snapshot on the lane carries, if the next message is
/// one and carries one.
fn next_step(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::persistence_bridge::PersistMsg>,
) -> Option<crate::state::RunState> {
    match rx.try_recv().ok()? {
        crate::persistence_bridge::PersistMsg::Snapshot(job) => {
            job.run_file.and_then(|s| s.now).map(|now| now.state)
        }
        _ => None,
    }
}

/// A fan-out's parent records a step when one of its workers finishes and
/// is reaped, though nothing else about the parent moved: its file has to say
/// which workers are done, or a restart counts them as never having run.
#[test]
fn a_worker_finishing_is_a_step_of_its_parents() {
    let (mut world, mut rx) = persist_world();
    let parent = spawn_run(&mut world, "s", true);
    world.get_mut::<AgentState>(parent).unwrap().status = AgentStatus::Waiting;
    let worker = |world: &mut World, id: &str| {
        world
            .spawn(AgentState {
                agent_id: id.to_string(),
                ..agent("s")
            })
            .id()
    };
    let first = worker(&mut world, "w-1");
    let second = worker(&mut world, "w-2");
    crate::fanout::restore_fan_out_waiting(
        &mut world,
        parent,
        crate::fanout::FanOutState {
            config: crate::spec::graph::FanOutDef::same_graph(
                crate::spec::names::StageName::new("s").unwrap(),
            ),
            max_workers: Some(2),
            pending: Vec::new(),
            active: vec![
                ("i1".to_string(), "w-1".to_string()),
                ("i2".to_string(), "w-2".to_string()),
            ],
            summaries: Vec::new(),
            failures: Vec::new(),
            parts: Vec::new(),
            paused: false,
            origin: Default::default(),
        },
        &|run| match run {
            "w-1" => Some(first),
            _ => Some(second),
        },
    );
    super::dispatch_persistence(&mut world);
    assert!(next_step(&mut rx).is_some());
    super::dispatch_persistence(&mut world);
    assert!(next_step(&mut rx).is_none(), "nothing moved, nothing sent");

    world.get_mut::<AgentState>(first).unwrap().status = AgentStatus::Complete;
    crate::fanout::fan_out_collect(&mut world);
    super::dispatch_persistence(&mut world);
    let step = next_step(&mut rx).expect("the reaped worker is a step");
    let fan_out = step.fan_out.expect("still fanning out");
    assert_eq!(fan_out.done.len(), 1);
    assert_eq!(fan_out.done[0].0, "i1");
    assert_eq!(fan_out.active.len(), 1);
}

/// A reply that calls `do` once per id, in order.
fn with_tools(ids: &[&str]) -> InferenceResponse {
    let mut r = text("calling");
    r.tool_calls = ids
        .iter()
        .map(|id| ToolCall {
            id: id.to_string(),
            name: "do".to_string(),
            arguments: serde_json::json!({"n": id}),
            thought_signature: None,
        })
        .collect();
    r
}

/// A tool service that notes every call it is asked to run and answers each
/// as it goes, reporting it the moment it lands. With `hold`, every call after
/// a batch's first waits forever, as a slow command would.
struct FirstThenHold {
    asked: Arc<Mutex<Vec<String>>>,
    hold: bool,
}

impl ToolService for FirstThenHold {
    fn exec_for(&self, _e: Entity, calls: Vec<ToolCall>, progress: ToolProgress) -> BoxedToolExec {
        let asked = self.asked.clone();
        let hold = self.hold;
        Box::new(move || {
            Box::pin(async move {
                asked
                    .lock()
                    .unwrap()
                    .extend(calls.iter().map(|c| c.id.clone()));
                let mut out = Vec::new();
                for (i, call) in calls.into_iter().enumerate() {
                    if hold && i > 0 {
                        std::future::pending::<()>().await;
                    }
                    let result: leviath_core::region::EntryContent =
                        format!("ran {}", call.id).into();
                    progress(&call.id, &result);
                    out.push((call.id, result));
                }
                out
            })
        })
    }
}

/// A world over `dir` whose model answers `replies` in order and whose tools
/// are `tools`.
fn tool_world(
    dir: &std::path::Path,
    replies: Vec<InferenceResponse>,
    tools: FirstThenHold,
) -> PipelineWorld {
    let mut providers = crate::ProviderRegistry::new();
    providers.register(
        "script".to_string(),
        Arc::new(Script {
            responses: Mutex::new(replies.into_iter().collect()),
        }),
    );
    PipelineWorld::new(
        providers,
        Arc::new(tools),
        crate::InferencePoolConfig::new(),
        1,
        Some(dir.to_path_buf()),
        Handle::current(),
    )
}

/// Whether `run_id`'s file under `dir` records call `call` finishing.
fn file_has_finished(dir: &std::path::Path, run_id: &str, call: &str) -> bool {
    let path = dir.join(run_id).join(leviath_core::files::RUN_FILE);
    let Ok(file) = RunFileReader::open(&path) else {
        return false;
    };
    file.deltas(1, file.last_seq())
        .unwrap_or_default()
        .iter()
        .flat_map(|d| &d.events)
        .any(|e| matches!(e, crate::state::RunEvent::ToolFinished { call_id, .. } if call_id == call))
}

/// A call that finished inside a batch still running is in the run's file as
/// done, read live and read back, whether the daemon stops cleanly or dies,
/// and is never run again. A call still running is run again after a clean
/// stop, which killed it; after the daemon died it may still be running, so
/// it comes back as interrupted, for the model to check, and is not run again.
#[tokio::test]
async fn a_call_that_finished_mid_batch_is_not_run_again_after_a_restart() {
    for clean_stop in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        assert!(!crate::restore::begin_session(dir.path()));
        let first = Arc::new(Mutex::new(Vec::new()));
        let mut world = tool_world(
            dir.path(),
            vec![with_tools(&["c1", "c2"])],
            FirstThenHold {
                asked: first.clone(),
                hold: true,
            },
        );
        let id = world.spawn_agent((
            (
                StageCursor { index: 0 },
                agent("s"),
                MessageInbox::default(),
                StageProgress::default(),
                VisitCounts::default(),
                window(),
                stage(),
                setup().inference_config,
            ),
            (
                metadata("run-r1"),
                crate::persistence::TokenTotals::default(),
                crate::pipeline::PersistWatermark::default(),
                crate::persistence::RunClock::default(),
                ReadyToInfer,
                crate::insert::RunSpecC(spec()),
            ),
        ));
        for _ in 0..500 {
            world.run_to_fixed_point();
            if file_has_finished(dir.path(), "run-r1", "c1") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(file_has_finished(dir.path(), "run-r1", "c1"));
        let live = crate::state::inspect::inspect(world.world(), id.entity()).unwrap();
        let live_done: Vec<&String> = live.pending.as_ref().unwrap().done.keys().collect();
        assert_eq!(live_done, ["c1"], "a live read shows the call that landed");
        if clean_stop {
            world.flush_and_stop().await;
        }

        let mut run = crate::restore::read_for_resume(&dir.path().join("run-r1"))
            .unwrap()
            .unwrap();
        let pending = run.state.pending.clone().expect("the batch is in flight");
        assert_eq!(
            pending.done.keys().collect::<Vec<_>>(),
            ["c1"],
            "the file holds the finished call as done"
        );
        assert_eq!(pending.done["c1"].text, "ran c1");
        let crashed = crate::restore::begin_session(dir.path());
        assert_eq!(
            crashed, !clean_stop,
            "only a daemon that died leaves its mark"
        );
        if crashed {
            crate::restore::interrupt_in_flight(&mut run.state);
            let done = &run.state.pending.as_ref().unwrap().done;
            assert_eq!(done["c2"].text, crate::restore::INTERRUPTED_TOOL_RESULT);
            assert!(done["c2"].is_error);
            assert_eq!(done["c1"].text, "ran c1");
        }
        if !clean_stop {
            world.flush_and_stop().await;
        }

        let second = Arc::new(Mutex::new(Vec::new()));
        let mut again = tool_world(
            dir.path(),
            vec![text("all done")],
            FirstThenHold {
                asked: second.clone(),
                hold: false,
            },
        );
        let at = run.state.seq;
        let e = crate::restore::resume(
            again.world_mut(),
            run,
            crate::spec::env::Bindings::new().with(stage()),
        );
        let placed = crate::state::inspect::inspect(again.world(), e).unwrap();
        assert_eq!(
            placed.seq, at,
            "a resumed run is at the step its file ended on"
        );
        again.run_until_idle(40).await;
        again.flush_and_stop().await;
        let rerun: Vec<String> = match clean_stop {
            true => vec!["c2".to_string()],
            false => Vec::new(),
        };
        assert_eq!(*second.lock().unwrap(), rerun, "what runs again");
        let after = crate::state::inspect::inspect(again.world(), e).unwrap();
        assert_eq!(after.status, crate::state::RunStatus::Complete);
        let told = after
            .context
            .regions
            .iter()
            .flat_map(|r| &r.entries)
            .any(|e| e.text == crate::restore::INTERRUPTED_TOOL_RESULT);
        assert_eq!(told, !clean_stop, "the model is told what it must check");
    }
}
