//! The ECS pipeline (Phase 2): components + systems that drive every agent
//! through check-input → infer → tools → apply → repeat, entirely as data.
//!
//! Agents are entities; their execution phase is a **marker component**
//! (`ReadyToInfer`, `AwaitingInference`, …) so systems can query by phase. A
//! system never blocks on I/O: the dispatch systems hand work to the async
//! bridges ([`crate::inference_bridge`], [`crate::tool_bridge`]) and the collect
//! systems apply the results on a later tick. This module is built alongside the
//! existing imperative engine; the two are unified in a later phase.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use leviath_providers::{InferenceRequest, Provider, Tool};

use crate::components::{AgentState, AgentStatus, ContextWindow, InferenceConfig};
use crate::engine::ProviderRegistry;
use crate::inference_bridge::{InferenceJob, InferenceOutcome, run_inference_job};
use crate::inference_pool::InferencePools;
use crate::tool_bridge::{BoxedToolExec, ToolJob};

// ─── Phase marker components (an agent is in exactly one) ────────────────────

/// The agent is active and ready to build a request and (permits allowing)
/// dispatch inference.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyToInfer;

/// Inference has been dispatched to the pool; the agent is waiting for its
/// result (which the inference-collect system will apply).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingInference;

// ─── Per-agent stage data the dispatch system reads ──────────────────────────

/// Resolved inference parameters for the agent's current stage, set when it
/// enters that stage. Pure data — the dispatch system reads it to build the
/// request.
#[derive(Component, Debug, Clone)]
pub struct StageInference {
    /// Registered provider to call.
    pub provider_name: String,
    /// Model id (also the key into the per-model inference pools).
    pub model: String,
    /// Tools advertised at this stage.
    pub tools: Vec<Tool>,
    /// Optional allow-list of tool names (`None`/empty = all `tools`).
    pub tool_filter: Option<Vec<String>>,
}

// ─── World resources for the inference stage ─────────────────────────────────

/// The registered providers, as a world resource.
#[derive(Resource)]
pub struct Providers(pub ProviderRegistry);

/// The plumbing the inference-dispatch system needs: the per-model pools, the
/// channel to report outcomes on, the tick wake handle, and a runtime handle to
/// spawn the (bounded, per-request) worker tasks onto.
#[derive(Resource, Clone)]
pub struct InferenceStage {
    /// Per-model concurrency pools.
    pub pools: Arc<InferencePools>,
    /// Where completed inferences are reported.
    pub outcomes: UnboundedSender<InferenceOutcome>,
    /// Signalled when an inference completes, to wake the tick loop.
    pub wake: Arc<Notify>,
    /// Runtime the worker tasks are spawned onto.
    pub runtime: Handle,
}

/// Build the [`InferenceRequest`] for an agent from its context window + stage
/// data. Pure; no `.await`. (Ported from `AgentEngine::build_inference_request`,
/// with provider resolution lifted into the caller so this stays query-friendly.)
fn build_request(
    window: &ContextWindow,
    config: Option<&InferenceConfig>,
    stage: &StageInference,
    provider: &Arc<dyn Provider>,
) -> InferenceRequest {
    let assembled = window.assemble();
    let remaining = window.max_tokens.saturating_sub(window.current_tokens);
    let caps = provider.capabilities(&stage.model);
    let output_cap = config
        .and_then(|c| c.max_output_tokens)
        .unwrap_or(caps.max_output_tokens);
    let max_tokens = remaining.min(output_cap);

    let filtered_tools = match stage.tool_filter.as_deref() {
        Some(filter) if !filter.is_empty() => stage
            .tools
            .iter()
            .filter(|t| filter.iter().any(|f| f == &t.name))
            .cloned()
            .collect(),
        _ => stage.tools.clone(),
    };

    let temperature = if caps.supports_temperature {
        config.and_then(|c| c.temperature).unwrap_or(0.7)
    } else {
        0.0
    };

    InferenceRequest {
        system: assembled.system_blocks,
        messages: assembled.messages,
        model: stage.model.clone(),
        max_tokens,
        temperature,
        tools: filtered_tools,
        extra: serde_json::Value::Null,
    }
}

/// Inference-dispatch system: for every `ReadyToInfer` agent, resolve its
/// provider and, **if a per-model permit is free**, build the request, spawn the
/// inference job, and move it to `AwaitingInference`. If its provider is missing
/// or no slot is free, it stays `ReadyToInfer` and is retried on a later tick —
/// no blocking, no wasted task.
pub fn dispatch_inference(
    agents: Query<
        (
            Entity,
            &ContextWindow,
            Option<&InferenceConfig>,
            &StageInference,
        ),
        With<ReadyToInfer>,
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    for (entity, window, config, si) in agents.iter() {
        let Some(provider) = providers.0.get(&si.provider_name).cloned() else {
            continue; // provider not registered — leave ready, retry later
        };
        let Some(permit) = stage.pools.try_acquire(&si.model) else {
            continue; // pool full — leave ready, retry next tick
        };
        let request = build_request(window, config, si, &provider);
        let job = InferenceJob {
            entity,
            provider,
            request,
            permit,
        };
        stage.runtime.spawn(run_inference_job(
            job,
            stage.outcomes.clone(),
            stage.wake.clone(),
        ));
        commands
            .entity(entity)
            .remove::<ReadyToInfer>()
            .insert(AwaitingInference);
    }
}

/// The response has been applied and is ready to be examined for tool calls (or
/// completion) by the process-response system.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessResponse;

/// The receiving end of the inference-outcomes channel, as a world resource for
/// the collect system. (The sending end lives in [`InferenceStage`].)
#[derive(Resource)]
pub struct InferenceResults(pub UnboundedReceiver<InferenceOutcome>);

/// Convert a provider response into the stored `InferenceResult` component.
/// (Ported from `AgentEngine::apply_inference_response`.)
fn to_inference_result(
    response: &leviath_providers::InferenceResponse,
) -> crate::components::InferenceResult {
    crate::components::InferenceResult {
        response: response.content.clone(),
        tool_calls: response
            .tool_calls
            .iter()
            .map(|tc| crate::components::ToolCall {
                tool_id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            })
            .collect(),
        tokens_used: response.tokens_used.total_tokens,
        timestamp: chrono::Utc::now().timestamp(),
    }
}

/// Inference-collect system: drain completed inferences and apply them. A
/// success is stored on the agent (bumping its iteration) and the agent advances
/// to `ProcessResponse`; an error marks the agent `Error`. An outcome for an
/// agent that is no longer `AwaitingInference` (cancelled or despawned between
/// dispatch and now) is dropped.
pub fn collect_inference(
    mut results: ResMut<InferenceResults>,
    mut agents: Query<&mut AgentState, With<AwaitingInference>>,
    mut commands: Commands,
) {
    while let Ok(outcome) = results.0.try_recv() {
        let Ok(mut state) = agents.get_mut(outcome.entity) else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        match outcome.result {
            Ok(response) => {
                state.iteration += 1;
                let result = to_inference_result(&response);
                commands
                    .entity(outcome.entity)
                    .insert(result)
                    .remove::<AwaitingInference>()
                    .insert(ProcessResponse);
            }
            Err(err) => {
                state.status = AgentStatus::Error {
                    message: err.to_string(),
                };
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingInference>();
            }
        }
    }
}

/// The response had tool calls; the agent is ready for the tool-dispatch system
/// to run them (the calls live on its `InferenceResult`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyForTools;

/// The response had no tool calls; the agent is ready for the transition system
/// to decide completion / next stage.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyForTransition;

/// Process-response system: route each `ProcessResponse` agent by whether its
/// last inference asked for tools. Tool calls present ⇒ `ReadyForTools`; none ⇒
/// `ReadyForTransition` (the transition system decides finish vs. next stage vs.
/// a "use your tools" nudge). Pure routing — no I/O.
pub fn process_response(
    agents: Query<(Entity, &crate::components::InferenceResult), With<ProcessResponse>>,
    mut commands: Commands,
) {
    for (entity, result) in agents.iter() {
        let mut e = commands.entity(entity);
        e.remove::<ProcessResponse>();
        if result.tool_calls.is_empty() {
            e.insert(ReadyForTransition);
        } else {
            e.insert(ReadyForTools);
        }
    }
}

/// The agent's tool batch has been handed to the tool lane; it is waiting for
/// the results (which the tool-collect system will apply).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingTools;

/// Provides a per-agent tool-execution closure. The concrete implementation
/// (in the CLI) holds each agent's tool registry, workdir, and permission
/// policy; the pipeline stays agnostic to *how* tools run. `exec_for` returns a
/// boxed closure the tool worker runs off the tick.
pub trait ToolService: Send + Sync {
    /// Build the closure that runs `calls` for `entity`, resolving `(id, result)`
    /// pairs.
    fn exec_for(&self, entity: Entity, calls: Vec<leviath_providers::ToolCall>) -> BoxedToolExec;
}

/// The tool service, as a world resource.
#[derive(Resource, Clone)]
pub struct ToolServiceRes(pub Arc<dyn ToolService>);

/// The job sender feeding the tool lane, as a world resource.
#[derive(Resource, Clone)]
pub struct ToolStage(pub UnboundedSender<ToolJob>);

/// Tool-dispatch system: for each `ReadyForTools` agent, turn its stored tool
/// calls into a job for the (sequential) tool lane and move it to
/// `AwaitingTools`. The lane serializes execution, so there is no permit gate
/// here — every ready agent is enqueued and processed in turn.
pub fn dispatch_tools(
    agents: Query<(Entity, &crate::components::InferenceResult), With<ReadyForTools>>,
    service: Res<ToolServiceRes>,
    stage: Res<ToolStage>,
    mut commands: Commands,
) {
    for (entity, result) in agents.iter() {
        let calls: Vec<leviath_providers::ToolCall> = result
            .tool_calls
            .iter()
            .map(|c| leviath_providers::ToolCall {
                id: c.tool_id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect();
        let exec = service.0.exec_for(entity, calls);
        // The lane worker is alive for the world's lifetime; a failed send would
        // only happen during shutdown, where dropping the job is fine.
        let _ = stage.0.send(ToolJob { entity, exec });
        commands
            .entity(entity)
            .remove::<ReadyForTools>()
            .insert(AwaitingTools);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_pool::{InferencePoolConfig, InferencePools};
    use leviath_core::{Region, RegionKind};
    use tokio::sync::mpsc;

    /// A provider whose capabilities can be toggled for the temperature branch.
    struct Cfg {
        supports_temperature: bool,
        max_output: usize,
    }
    #[async_trait::async_trait]
    impl Provider for Cfg {
        async fn infer(
            &self,
            _r: InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Ok(leviath_providers::InferenceResponse {
                content: "ok".to_string(),
                tool_calls: vec![],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::Complete,
            })
        }
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "cfg"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities {
                supports_temperature: self.supports_temperature,
                max_output_tokens: self.max_output,
                ..Default::default()
            }
        }
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(10_000);
        w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 1000));
        w
    }

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        }
    }

    fn stage(model: &str, tools: Vec<Tool>, filter: Option<Vec<String>>) -> StageInference {
        StageInference {
            provider_name: "cfg".to_string(),
            model: model.to_string(),
            tools,
            tool_filter: filter,
        }
    }

    fn provider(supports_temperature: bool, max_output: usize) -> Arc<dyn Provider> {
        Arc::new(Cfg {
            supports_temperature,
            max_output,
        })
    }

    // ── build_request branch coverage ──

    #[test]
    fn build_request_filters_tools_and_uses_config_overrides() {
        let cfg = InferenceConfig {
            temperature: Some(0.1),
            max_output_tokens: Some(42),
        };
        let si = stage(
            "m",
            vec![tool("keep"), tool("drop")],
            Some(vec!["keep".into()]),
        );
        let req = build_request(&window(), Some(&cfg), &si, &provider(true, 9999));
        assert_eq!(req.tools.len(), 1); // filtered to "keep"
        assert_eq!(req.tools[0].name, "keep");
        assert_eq!(req.max_tokens, 42); // config output cap wins
        assert_eq!(req.temperature, 0.1); // config temperature
    }

    #[test]
    fn build_request_all_tools_default_temperature_no_config() {
        let si = stage("m", vec![tool("a"), tool("b")], None); // None filter = all
        let req = build_request(&window(), None, &si, &provider(true, 500));
        assert_eq!(req.tools.len(), 2);
        assert_eq!(req.temperature, 0.7); // default when supported and no config
        assert_eq!(req.max_tokens, 500); // capability cap when no config override
    }

    #[test]
    fn build_request_empty_filter_is_all_and_no_temperature_when_unsupported() {
        let si = stage("m", vec![tool("a")], Some(vec![])); // empty filter = all
        let req = build_request(&window(), None, &si, &provider(false, 500));
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.temperature, 0.0); // model doesn't support temperature
    }

    #[test]
    fn cfg_provider_metadata_is_exercised() {
        // Keep the mock's non-`infer`/`capabilities` trait methods measured.
        let p = Cfg {
            supports_temperature: true,
            max_output: 1,
        };
        assert_eq!(p.name(), "cfg");
        assert_eq!(p.count_tokens("t", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
    }

    // ── dispatch system ──

    fn build_world(pools: InferencePools) -> (World, mpsc::UnboundedReceiver<InferenceOutcome>) {
        let mut registry = ProviderRegistry::new();
        registry.register("cfg".to_string(), provider(true, 1000));
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(Providers(registry));
        world.insert_resource(InferenceStage {
            pools: Arc::new(pools),
            outcomes: tx,
            wake: Arc::new(Notify::new()),
            runtime: Handle::current(),
        });
        (world, rx)
    }

    fn run(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(dispatch_inference);
        schedule.run(world);
    }

    #[tokio::test]
    async fn dispatch_moves_agent_to_awaiting_and_runs_the_job() {
        let (mut world, mut rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((window(), stage("m", vec![], None), ReadyToInfer))
            .id();

        run(&mut world);

        // Phase advanced.
        assert!(world.get::<AwaitingInference>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_none());
        // The spawned job ran and reported an outcome.
        let outcome = rx.recv().await.expect("outcome");
        assert_eq!(outcome.entity, e);
        assert!(outcome.result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_skips_when_pool_full() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = InferencePools::new(cfg);
        let _held = pools.try_acquire("m").unwrap(); // occupy the only slot
        let (mut world, _rx) = build_world(pools);
        let e = world
            .spawn((window(), stage("m", vec![], None), ReadyToInfer))
            .id();

        run(&mut world);

        // No slot ⇒ still ready, not dispatched.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingInference>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_skips_when_provider_missing() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                window(),
                stage("m", vec![], None).clone_with_provider("nope"),
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some()); // unknown provider ⇒ untouched
        assert!(world.get::<AwaitingInference>(e).is_none());
    }

    impl StageInference {
        fn clone_with_provider(mut self, name: &str) -> Self {
            self.provider_name = name.to_string();
            self
        }
    }

    // ── collect system ──

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

    fn resp(text: &str) -> leviath_providers::InferenceResponse {
        leviath_providers::InferenceResponse {
            content: text.to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::Complete,
        }
    }

    fn run_collect(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(collect_inference);
        schedule.run(world);
    }

    fn world_with_results() -> (World, mpsc::UnboundedSender<InferenceOutcome>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(InferenceResults(rx));
        (world, tx)
    }

    #[test]
    fn collect_applies_ok_and_advances_to_process_response() {
        let (mut world, tx) = world_with_results();
        let e = world.spawn((agent_state(), AwaitingInference)).id();
        let mut response = resp("hi");
        response.tool_calls.push(leviath_providers::ToolCall {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "x"}),
        });
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(response),
        })
        .unwrap();

        run_collect(&mut world);

        assert!(world.get::<ProcessResponse>(e).is_some());
        assert!(world.get::<AwaitingInference>(e).is_none());
        assert_eq!(world.get::<AgentState>(e).unwrap().iteration, 1);
        let stored = world.get::<crate::components::InferenceResult>(e).unwrap();
        assert_eq!(stored.response, "hi");
        // The tool call was mapped onto the stored result.
        assert_eq!(stored.tool_calls.len(), 1);
        assert_eq!(stored.tool_calls[0].name, "read_file");
    }

    #[test]
    fn collect_marks_error_on_failure() {
        let (mut world, tx) = world_with_results();
        let e = world.spawn((agent_state(), AwaitingInference)).id();
        tx.send(InferenceOutcome {
            entity: e,
            result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
        })
        .unwrap();

        run_collect(&mut world);

        // `ProviderError::Other`'s Display is the inner message ("boom").
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Error {
                message: "boom".to_string()
            }
        );
        assert!(world.get::<AwaitingInference>(e).is_none());
    }

    #[test]
    fn collect_drops_outcome_for_non_awaiting_agent() {
        let (mut world, tx) = world_with_results();
        let e = world.spawn(agent_state()).id(); // no AwaitingInference marker
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("x")),
        })
        .unwrap();

        run_collect(&mut world);

        // Untouched — the stale outcome was dropped.
        assert_eq!(world.get::<AgentState>(e).unwrap().iteration, 0);
        assert!(world.get::<ProcessResponse>(e).is_none());
    }

    // ── process-response routing ──

    fn infer_result(with_tools: bool) -> crate::components::InferenceResult {
        crate::components::InferenceResult {
            response: "r".to_string(),
            tool_calls: if with_tools {
                vec![crate::components::ToolCall {
                    tool_id: "t".to_string(),
                    name: "n".to_string(),
                    arguments: serde_json::Value::Null,
                }]
            } else {
                vec![]
            },
            tokens_used: 0,
            timestamp: 0,
        }
    }

    fn run_process(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(process_response);
        s.run(world);
    }

    #[test]
    fn process_routes_tool_calls_to_ready_for_tools() {
        let mut world = World::new();
        let e = world.spawn((infer_result(true), ProcessResponse)).id();
        run_process(&mut world);
        assert!(world.get::<ReadyForTools>(e).is_some());
        assert!(world.get::<ProcessResponse>(e).is_none());
        assert!(world.get::<ReadyForTransition>(e).is_none());
    }

    #[test]
    fn process_routes_no_tools_to_ready_for_transition() {
        let mut world = World::new();
        let e = world.spawn((infer_result(false), ProcessResponse)).id();
        run_process(&mut world);
        assert!(world.get::<ReadyForTransition>(e).is_some());
        assert!(world.get::<ReadyForTools>(e).is_none());
    }

    // ── tool-dispatch ──

    /// A tool service that echoes each call as `(id, "ran <name>")`.
    struct EchoService;
    impl ToolService for EchoService {
        fn exec_for(
            &self,
            _entity: Entity,
            calls: Vec<leviath_providers::ToolCall>,
        ) -> BoxedToolExec {
            Box::new(move || {
                Box::pin(async move {
                    calls
                        .into_iter()
                        .map(|c| (c.id, format!("ran {}", c.name)))
                        .collect()
                })
            })
        }
    }

    #[tokio::test]
    async fn dispatch_tools_enqueues_runnable_job_and_advances() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        let e = world.spawn((infer_result(true), ReadyForTools)).id();

        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        assert!(world.get::<AwaitingTools>(e).is_some());
        assert!(world.get::<ReadyForTools>(e).is_none());
        let job = jrx.try_recv().expect("job enqueued");
        assert_eq!(job.entity, e);
        // Run the produced closure (covers the service's exec path).
        let results = (job.exec)().await;
        assert_eq!(results, vec![("t".to_string(), "ran n".to_string())]);
    }
}
