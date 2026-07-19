//! The pipeline driver: a single [`PipelineWorld`] that hosts every agent as
//! ECS data and ticks the [`crate::pipeline`] systems over all of them — the
//! traditional-game-loop core of the shared world.
//!
//! The world owns the bevy [`World`], the tick [`Schedule`], the per-model
//! inference pools, and the async bridges (inference jobs + the tool worker).
//! Systems never block: they dispatch async work to the bridges and collect the
//! results on a later tick. Between ticks the driver **parks** on a wake
//! [`Notify`] until an async result lands or an external message arrives, so an
//! idle world costs ~0 CPU regardless of how many (paused/blocked) agents it
//! holds.
//!
//! ## Idle detection (no busy-spin)
//!
//! Each outer iteration drives the schedule to a **fixed point**: it ticks until
//! a tick produces no change in the per-phase marker counts (the "fingerprint").
//! At quiescence every remaining agent is either waiting on an in-flight async
//! job (which will `notify` on completion) or blocked on a resource that only an
//! async completion can free (a full pool) or on nothing at all (a missing
//! provider / no input) — so the driver parks on the wake instead of spinning.
//! A fresh async result or an external `send_message` fires the wake and the
//! fixed-point loop re-runs.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_ecs::query::QueryFilter;
use leviath_providers::ProviderError;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::components::{AgentMessage, AgentState, AgentStatus};
use crate::engine::ProviderRegistry;
use crate::inference_pool::{InferencePoolConfig, InferencePools};
use crate::persistence_bridge::persistence_worker;
use crate::pipeline::{
    AwaitingCompaction, AwaitingInference, AwaitingTools, AwaitingTransitionChoice,
    AwaitingTransitionResponse, CompactionResults, InferenceResults, InferenceStage, MessageIntake,
    PersistenceStage, ProcessResponse, Providers, ReadyForTools, ReadyForTransition, ReadyToInfer,
    ResolveTransition, ToolResults, ToolService, ToolServiceRes, ToolStage, TransitionResults,
    collect_compaction, collect_inference, collect_tools, collect_transition_choice,
    deliver_messages, dispatch_compaction, dispatch_inference, dispatch_persistence,
    dispatch_tools, dispatch_transition_choice, handle_empty_response, process_response,
    resolve_transition,
};
use crate::tool_bridge::tool_worker;

/// Counts of agents in each phase-marker — the world's per-tick "fingerprint".
/// Two consecutive equal fingerprints mean a tick changed nothing (quiescence).
type Fingerprint = [usize; 10];

/// The shared ECS world that hosts and drives every agent.
pub struct PipelineWorld {
    world: World,
    schedule: Schedule,
    wake: Arc<Notify>,
    shutdown: Arc<Notify>,
    msg_tx: UnboundedSender<AgentMessage>,
    /// The tool worker task; kept so it lives as long as the world. It exits on
    /// its own when the world (and thus the [`ToolStage`] sender) is dropped.
    _tool_task: JoinHandle<()>,
}

impl PipelineWorld {
    /// Build a world: wire the pool/bridge resources, register the providers and
    /// tool service, spawn the tool worker onto `runtime`, and assemble the tick
    /// schedule. Agents are added later via [`Self::spawn_agent`].
    pub fn new(
        providers: ProviderRegistry,
        tool_service: Arc<dyn ToolService>,
        pool_config: InferencePoolConfig,
        runs_dir: std::path::PathBuf,
        runtime: Handle,
    ) -> Self {
        let wake = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());

        let (inf_tx, inf_rx) = unbounded_channel();
        let (trans_tx, trans_rx) = unbounded_channel();
        let (compact_tx, compact_rx) = unbounded_channel();
        let (tool_job_tx, tool_job_rx) = unbounded_channel();
        let (tool_res_tx, tool_res_rx) = unbounded_channel();
        let (persist_tx, persist_rx) = unbounded_channel();
        let (msg_tx, msg_rx) = unbounded_channel();

        let tool_task = runtime.spawn(tool_worker(tool_job_rx, tool_res_tx, wake.clone()));
        // Fire-and-forget: the persistence worker exits when the world (and thus
        // its PersistenceStage sender) is dropped.
        runtime.spawn(persistence_worker(runs_dir, persist_rx));

        let mut world = World::new();
        world.insert_resource(Providers(providers));
        world.insert_resource(InferenceStage {
            pools: Arc::new(InferencePools::new(pool_config)),
            outcomes: inf_tx,
            transition_outcomes: trans_tx,
            compaction_outcomes: compact_tx,
            wake: wake.clone(),
            runtime,
        });
        world.insert_resource(InferenceResults(inf_rx));
        world.insert_resource(TransitionResults(trans_rx));
        world.insert_resource(CompactionResults(compact_rx));
        world.insert_resource(ToolServiceRes(tool_service));
        world.insert_resource(ToolStage(tool_job_tx));
        world.insert_resource(ToolResults(tool_res_rx));
        world.insert_resource(PersistenceStage(persist_tx));
        world.insert_resource(MessageIntake(msg_rx));

        let mut schedule = Schedule::default();
        schedule.add_systems(
            (
                deliver_messages,
                collect_compaction,
                dispatch_compaction,
                dispatch_inference,
                collect_inference,
                process_response,
                dispatch_tools,
                collect_tools,
                handle_empty_response,
                resolve_transition,
                dispatch_transition_choice,
                collect_transition_choice,
                dispatch_persistence,
            )
                .chain(),
        );

        Self {
            world,
            schedule,
            wake,
            shutdown,
            msg_tx,
            _tool_task: tool_task,
        }
    }

    /// Mutable access to the underlying ECS world, for spawning agents (the CLI /
    /// daemon builds each agent's component bundle) and inspection.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Read-only access to the underlying ECS world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Spawn an agent from its pre-built component bundle and wake the driver so
    /// the next fixed-point picks it up. Returns the new entity.
    pub fn spawn_agent(&mut self, bundle: impl Bundle) -> Entity {
        let e = self.world.spawn(bundle).id();
        self.wake.notify_one();
        e
    }

    /// Spawn an agent from a blueprint + task + per-stage resolution (see
    /// [`crate::pipeline::spawn_agent`]) and wake the driver. Returns the new
    /// entity, or an error if the first stage's system prompt doesn't fit.
    pub fn spawn_from_blueprint(
        &mut self,
        agent_id: String,
        blueprint: leviath_core::Blueprint,
        task: &str,
        stages: Vec<crate::pipeline::ResolvedStage>,
    ) -> Result<Entity, String> {
        let e = crate::pipeline::spawn_agent(&mut self.world, agent_id, blueprint, task, stages)?;
        self.wake.notify_one();
        Ok(e)
    }

    /// Deliver a message to a running agent (routed to its inbox on the next
    /// tick) and wake the driver.
    pub fn send_message(&self, msg: AgentMessage) -> Result<(), ProviderError> {
        self.msg_tx
            .send(msg)
            .map_err(|e| ProviderError::Other(format!("world message channel closed: {e}")))?;
        self.wake.notify_one();
        Ok(())
    }

    /// A clone of the wake handle, so external producers (e.g. a control socket)
    /// can nudge the driver after mutating the world directly.
    pub fn wake_handle(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// Request the [`Self::run`] loop to stop after its current fixed point.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// A clone of the shutdown handle, so a supervisor can stop a [`Self::run`]
    /// loop that has taken ownership of the world on another task.
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// The status of an agent, if it still exists.
    pub fn agent_status(&self, entity: Entity) -> Option<AgentStatus> {
        self.world
            .get::<AgentState>(entity)
            .map(|s| s.status.clone())
    }

    /// Run one schedule tick over every agent.
    pub fn tick(&mut self) {
        self.schedule.run(&mut self.world);
    }

    fn count<F: QueryFilter>(&mut self) -> usize {
        let mut q = self.world.query_filtered::<(), F>();
        q.iter(&self.world).count()
    }

    /// Snapshot the per-phase marker counts.
    fn fingerprint(&mut self) -> Fingerprint {
        [
            self.count::<With<ReadyToInfer>>(),
            self.count::<With<AwaitingInference>>(),
            self.count::<With<ProcessResponse>>(),
            self.count::<With<ReadyForTools>>(),
            self.count::<With<ReadyForTransition>>(),
            self.count::<With<ResolveTransition>>(),
            self.count::<With<AwaitingTools>>(),
            self.count::<With<AwaitingTransitionChoice>>(),
            self.count::<With<AwaitingTransitionResponse>>(),
            self.count::<With<AwaitingCompaction>>(),
        ]
    }

    /// Any agent waiting on an in-flight async job (inference, tools, a
    /// transition choice, or compaction) whose completion will wake the driver.
    fn has_async_inflight(&mut self) -> bool {
        self.count::<With<AwaitingInference>>() > 0
            || self.count::<With<AwaitingTools>>() > 0
            || self.count::<With<AwaitingTransitionResponse>>() > 0
            || self.count::<With<AwaitingCompaction>>() > 0
    }

    /// Drive the schedule until a tick changes nothing (quiescence).
    fn run_to_fixed_point(&mut self) {
        let mut prev = self.fingerprint();
        loop {
            self.tick();
            let now = self.fingerprint();
            if now == prev {
                break;
            }
            prev = now;
        }
    }

    /// Drive every agent as far as it can go **right now**, then, while async
    /// work is in flight, wait for each completion and drive again — returning
    /// once the world is fully quiescent with nothing in flight. Bounded by
    /// `max_waits` wake-waits as a safety valve so a lost/never-arriving wake
    /// can't hang a caller (e.g. a test) forever.
    pub async fn run_until_idle(&mut self, max_waits: usize) {
        self.run_to_fixed_point();
        let mut waits = 0;
        while self.has_async_inflight() && waits < max_waits {
            self.wake.notified().await;
            waits += 1;
            self.run_to_fixed_point();
        }
    }

    /// Run forever: drive to quiescence, then park until an async completion or
    /// an external `send_message`/`spawn_agent` wakes the driver. Returns when
    /// [`Self::shutdown`] is signalled.
    pub async fn run(&mut self) {
        loop {
            self.run_to_fixed_point();
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = self.shutdown.notified() => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{AgentState, ContextWindow, InferenceConfig};
    use crate::pipeline::{
        AgentBlueprint, MessageIntake, StageCursor, StageInference, StageInferences, StageProgress,
        StageSetup, StageSetups, VisitCounts,
    };
    use crate::tool_bridge::BoxedToolExec;
    use leviath_core::{Region, RegionKind};
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider, TokenUsage,
        ToolCall,
    };
    use std::sync::Mutex;

    /// A provider scripted with a queue of responses; each `infer` pops the next.
    struct Script {
        responses: Mutex<std::collections::VecDeque<InferenceResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for Script {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            let next = self.responses.lock().unwrap().pop_front();
            next.ok_or_else(|| ProviderError::Other("script exhausted".to_string()))
        }
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
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
            content: content.to_string(),
            tool_calls: vec![],
            tokens_used: TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: FinishReason::Complete,
        }
    }

    fn with_tool(id: &str, name: &str) -> InferenceResponse {
        let mut r = text("");
        r.tool_calls.push(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        });
        r
    }

    /// A tool service that returns a fixed result string for every call.
    struct EchoTools;
    impl ToolService for EchoTools {
        fn exec_for(&self, _entity: Entity, calls: Vec<ToolCall>) -> BoxedToolExec {
            Box::new(move || {
                Box::pin(async move {
                    calls
                        .into_iter()
                        .map(|c| (c.id, "ok".to_string()))
                        .collect()
                })
            })
        }
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(10_000);
        w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 2000));
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w.add_region(Region::new(
            "tool_results".to_string(),
            RegionKind::Temporary,
            5000,
        ));
        w
    }

    fn agent_state() -> AgentState {
        AgentState {
            agent_id: "a".to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn stage(model: &str) -> StageInference {
        StageInference {
            provider_name: "script".to_string(),
            model: model.to_string(),
            tools: vec![],
            tool_filter: None,
        }
    }

    fn setup() -> StageSetup {
        StageSetup {
            inference_config: InferenceConfig {
                temperature: None,
                max_output_tokens: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            system_prompt: None,
        }
    }

    fn blueprint() -> leviath_core::Blueprint {
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let s = leviath_core::Stage::new(
            "s".to_string(),
            leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
        );
        leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout)
    }

    /// Spawn a single-stage agent, initially ready to infer.
    fn spawn(world: &mut PipelineWorld) -> Entity {
        world.spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            ReadyToInfer,
        ))
    }

    fn build_world(providers: ProviderRegistry) -> PipelineWorld {
        // These agents carry no RunMetadata, so persistence never fires and the
        // runs dir is never written; any path is fine.
        PipelineWorld::new(
            providers,
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            std::env::temp_dir(),
            Handle::current(),
        )
    }

    fn registry_with(responses: Vec<InferenceResponse>) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        r.register(
            "script".to_string(),
            Arc::new(Script {
                responses: Mutex::new(responses.into_iter().collect()),
            }),
        );
        r
    }

    #[tokio::test]
    async fn agent_completes_after_nudges_exhausted() {
        // Text-only responses with no tool calls get nudged up to the max; the
        // response after the last nudge is accepted and the single-stage
        // blueprint terminates the agent. (Exercises the handle_empty_response
        // nudge loop end-to-end through the driver.)
        let mut world = build_world(registry_with(vec![
            text("thinking"),
            text("still"),
            text("more"),
            text("final"),
        ]));
        let e = spawn(&mut world);

        world.run_until_idle(30).await;

        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
    }

    #[tokio::test]
    async fn agent_runs_tools_then_completes() {
        // First response calls a tool; after the tool result comes back the
        // second response is text-only, finishing the run.
        let mut world = build_world(registry_with(vec![with_tool("c1", "do"), text("done")]));
        let e = spawn(&mut world);

        world.run_until_idle(20).await;

        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
        // With no routing configured, tool results land in the conversation
        // region.
        assert!(
            world
                .world()
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[tokio::test]
    async fn provider_error_marks_agent_error() {
        // Empty script ⇒ the very first infer errors.
        let mut world = build_world(registry_with(vec![]));
        let e = spawn(&mut world);

        world.run_until_idle(20).await;

        assert_eq!(
            std::mem::discriminant(&world.agent_status(e).unwrap()),
            std::mem::discriminant(&AgentStatus::Error {
                message: String::new()
            })
        );
    }

    #[tokio::test]
    async fn send_message_reaches_the_agent_inbox() {
        // No responses queued: the agent dispatches inference and parks awaiting
        // it. We deliver a message; the deliver system routes it to context.
        let mut world = build_world(registry_with(vec![]));
        let e = spawn(&mut world);
        // Drive to the point the first (doomed) inference is dispatched/collected.
        world.run_until_idle(20).await;

        world
            .send_message(AgentMessage {
                agent_id: "a".to_string(),
                content: "hello".to_string(),
                target_region: Some("conversation".to_string()),
                priority: 0,
            })
            .unwrap();
        world.tick(); // deliver_messages runs

        assert!(
            world
                .world()
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[tokio::test]
    async fn run_returns_on_shutdown() {
        let mut world = build_world(registry_with(vec![text("done")]));
        spawn(&mut world);
        world.shutdown(); // pre-signal: run parks then returns
        // Must return rather than loop forever.
        world.run().await;
    }

    #[tokio::test]
    async fn run_wakes_then_shuts_down() {
        // Drives run() on its own task: a wake makes it loop once (wake branch),
        // then a shutdown makes it return (shutdown branch).
        let mut world = build_world(registry_with(vec![
            text("t1"),
            text("t2"),
            text("t3"),
            text("t4"),
        ]));
        spawn(&mut world);
        let wake = world.wake_handle();
        let shutdown = world.shutdown_handle();
        let handle = tokio::spawn(async move { world.run().await });

        wake.notify_one();
        tokio::task::yield_now().await;
        shutdown.notify_one();

        handle.await.unwrap(); // returns cleanly
    }

    #[tokio::test]
    async fn send_message_errors_when_intake_dropped() {
        let mut world = build_world(registry_with(vec![]));
        // Drop the intake receiver via the world accessor, closing the channel.
        let removed = world.world_mut().remove_resource::<MessageIntake>();
        drop(removed);

        let err = world.send_message(AgentMessage {
            agent_id: "a".to_string(),
            content: "x".to_string(),
            target_region: None,
            priority: 0,
        });
        assert!(err.is_err());
    }

    #[test]
    fn script_provider_metadata_is_exercised() {
        // Keep the mock's non-`infer`/`capabilities` methods measured.
        let p = Script {
            responses: Mutex::new(std::collections::VecDeque::new()),
        };
        assert_eq!(p.name(), "script");
        assert_eq!(p.count_tokens("t", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }

    #[tokio::test]
    async fn agent_status_is_none_for_unknown_entity() {
        let world = build_world(registry_with(vec![]));
        assert_eq!(world.agent_status(Entity::from_raw(999)), None);
    }

    #[tokio::test]
    async fn spawn_from_blueprint_builds_a_runnable_agent() {
        // End-to-end via the blueprint resolver: build → drive → complete.
        let mut world = build_world(registry_with(vec![with_tool("c1", "do"), text("done")]));
        let e = world
            .spawn_from_blueprint(
                "agent-1".to_string(),
                blueprint(),
                "do the task",
                vec![crate::pipeline::ResolvedStage {
                    provider_name: "script".to_string(),
                    model: "m".to_string(),
                    tools: vec![],
                }],
            )
            .unwrap();

        world.run_until_idle(20).await;

        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
    }

    #[tokio::test]
    async fn persists_agent_snapshot_to_runs_dir() {
        // An agent carrying RunMetadata + TokenTotals is snapshotted to disk as it
        // runs; after it completes, meta.json exists with the final status.
        let dir = tempfile::tempdir().unwrap();
        let mut world = PipelineWorld::new(
            registry_with(vec![with_tool("c1", "do"), text("done")]),
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            dir.path().to_path_buf(),
            Handle::current(),
        );
        world.spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            crate::persistence::RunMetadata {
                run_id: "run-42".to_string(),
                agent_name: "a".to_string(),
                agent_path: "/p".to_string(),
                task: "t".to_string(),
                model: None,
                workdir: "/w".to_string(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: std::collections::HashMap::new(),
                callback_url: None,
                title: None,
            },
            crate::persistence::TokenTotals::default(),
            crate::pipeline::PersistWatermark::default(),
            ReadyToInfer,
        ));

        world.run_until_idle(20).await;

        // The persistence worker is fire-and-forget on its own task; poll until the
        // final (Complete) snapshot has been flushed.
        let meta_path = dir.path().join("run-42").join("meta.json");
        let mut meta = None;
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&meta_path)
                && let Ok(m) = serde_json::from_str::<leviath_core::run_meta::RunMeta>(&text)
                && m.status == leviath_core::run_meta::RunStatus::Complete
            {
                meta = Some(m);
                break;
            }
            tokio::task::yield_now().await;
        }

        let meta = meta.expect("final Complete snapshot flushed to disk");
        assert_eq!(meta.run_id, "run-42");
        assert!(dir.path().join("run-42").join("context.json").exists());
    }

    #[tokio::test]
    async fn spawn_from_blueprint_errors_on_oversized_system_prompt() {
        let mut world = build_world(registry_with(vec![]));
        // A blueprint whose stage carries an enormous system prompt in a tiny
        // pinned region overflows at spawn.
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                50,
            )],
            1000,
        );
        let mut s = leviath_core::Stage::new(
            "s".to_string(),
            leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
        );
        s.config.insert(
            "system_prompt".to_string(),
            serde_json::Value::String("x".repeat(100_000)),
        );
        let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout);

        let err = world.spawn_from_blueprint(
            "a".to_string(),
            bp,
            "task",
            vec![crate::pipeline::ResolvedStage {
                provider_name: "script".to_string(),
                model: "m".to_string(),
                tools: vec![],
            }],
        );
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn wake_handle_and_run_until_idle_bound_are_exposed() {
        // Exercises the wake handle accessor and the max-waits safety bound on a
        // world with an agent parked on an in-flight inference that never
        // resolves within the bound (script returns after we stop waiting).
        let mut world = build_world(registry_with(vec![with_tool("c1", "do"), text("done")]));
        let _ = world.wake_handle();
        let e = spawn(&mut world);
        world.run_until_idle(0).await; // bound 0 ⇒ no extra waits
        // With no waits allowed we may not have observed completion yet; drain.
        world.run_until_idle(20).await;
        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
    }
}
