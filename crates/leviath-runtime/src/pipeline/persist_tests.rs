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

fn blueprint() -> crate::spec::Blueprint {
    let layout = crate::spec::layout::ContextLayout::new(
        vec![crate::spec::layout::RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        )],
        12_000,
    );
    let s = crate::spec::Stage::new(
        "s".to_string(),
        crate::spec::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
    );
    crate::spec::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout)
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

/// The spec a spawn of [`blueprint`] places.
fn spec() -> Arc<crate::spec::run_spec::RunSpec> {
    crate::spec_bridge::test_support::both(blueprint()).0
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
    let mut recorded = file.latest_state().unwrap();
    assert!(recorded.seq > 1);
    // The file numbers the steps; a live read does not.
    recorded.seq = 0;
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
    // The older files are still written beside it.
    assert!(dir.path().join("run-42").join("meta.json").exists());
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
