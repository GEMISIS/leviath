//! Unified stage loop: `StageContext` + `StageCallbacks` trait.
//!
//! Both foreground and worker modes implement `StageCallbacks` and share the
//! same `run_stage_loop` driver, eliminating the ~70% code duplication between
//! `foreground.rs` and `worker.rs`.

use async_trait::async_trait;
use leviath_core::blueprint::{StageMode, StageResult};
use leviath_core::lifecycle::CompactionConfig;
use leviath_core::Blueprint;
use leviath_providers::{InferenceResponse, ToolCall};
use leviath_runtime::{
    AgentEngine, AgentPool, AgentState, ContextWindow, InferenceConfig, ToolResultRoutingConfig,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::tools::ToolRegistry;

use super::graph::{apply_edge_transform, is_graph_mode, resolve_transition};
use super::helpers::swap_context_layout;
use super::io::RunIO;
use super::stages::{run_interactive_points_stage, run_interactive_stage};

use crate::runstate::RunMeta;

/// Shared state for the stage loop, passed by the caller.
pub struct StageContext<'a> {
    /// The agent blueprint defining stages, transitions, and configuration.
    pub blueprint: &'a Blueprint,
    /// The agent engine providing inference and ECS world access.
    pub engine: &'a mut AgentEngine,
    /// The ECS entity representing the running agent.
    pub entity: bevy_ecs::prelude::Entity,
    /// The agent pool managing agent lifecycle.
    pub pool: &'a mut AgentPool,
    /// Registry of available tools for stage execution.
    pub tool_registry: &'a Arc<ToolRegistry>,
    /// Shared lock for the current stage name (used by tool executor closure).
    pub current_stage_name: Arc<Mutex<String>>,
    /// Shared lock for current stage tool permissions (used by tool executor closure).
    pub current_stage_perms: Arc<Mutex<HashMap<String, String>>>,
    /// Shared lock for current stage index (used by worker for recording).
    pub current_stage_idx: Arc<Mutex<usize>>,
    /// Optional model override from CLI args.
    pub model_override: Option<String>,
    /// Optional compaction configuration for context management.
    pub compaction_ref: Option<&'a CompactionConfig>,
}

/// Trait for mode-specific behavior in the stage loop.
///
/// Foreground and worker modes each implement this trait, and the unified
/// `run_stage_loop` calls into it at every divergence point.
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait StageCallbacks: Send {
    /// Called when the provider for a stage is not configured.
    /// Return `true` to abort the entire run; `false` to continue (unlikely).
    async fn on_provider_missing(&mut self, provider: &str, stage_idx: usize) -> bool;

    /// Called when entering a new stage (print header, record log, mark active, etc.).
    async fn on_stage_enter(
        &mut self,
        stage_name: &str,
        stage_idx: usize,
        provider: &str,
        model: &str,
        visit_label: &str,
    );

    /// Called when a stage uses the `claude-code` provider.
    async fn on_claude_code_warning(&mut self, stage_idx: usize);

    /// Start mid-run message reader (foreground: spawn stdin task; worker: no-op).
    /// Returns an optional join handle that will be aborted when the stage ends.
    fn start_message_reader(
        &mut self,
        engine: &AgentEngine,
        agent_id: &str,
        accepts: bool,
    ) -> Option<tokio::task::JoinHandle<()>>;

    /// Get run_context for interactive stages.
    /// Foreground returns `None` (stdin); worker returns `Some((run_id, meta))`.
    fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)>;

    /// Run an autonomous stage.
    ///
    /// Foreground calls `run_autonomous_stage`; worker calls
    /// `engine.run_inference_loop_filtered` directly.
    async fn run_autonomous<F, Fut>(
        &mut self,
        engine: &mut AgentEngine,
        entity: bevy_ecs::prelude::Entity,
        provider: &str,
        model: &str,
        max_iterations: usize,
        tools: Vec<leviath_providers::Tool>,
        routing: Option<&ToolResultRoutingConfig>,
        compaction: Option<&CompactionConfig>,
        io: &mut dyn RunIO,
        executor: &mut F,
    ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
    where
        F: FnMut(Vec<ToolCall>) -> Fut + Send,
        Fut: std::future::Future<Output = Vec<(String, String)>> + Send;

    /// Handle post-execution: record output, update tokens, mark stage complete/error.
    async fn on_stage_result(
        &mut self,
        stage_name: &str,
        stage_idx: usize,
        result: &StageResult,
        response: Option<&InferenceResponse>,
        engine: &mut AgentEngine,
        entity: bevy_ecs::prelude::Entity,
    );

    /// Handle a stage error. Returns the `StageResult` to use for transition
    /// resolution (typically `StageResult::Error` in graph mode).
    async fn on_stage_error(
        &mut self,
        stage_name: &str,
        stage_idx: usize,
        error: &anyhow::Error,
        is_graph_mode: bool,
    ) -> Option<StageResult>;

    /// Called after transition resolution, before moving to the next stage.
    async fn on_transition(&mut self, from_stage: &str, to_stage: &str, stage_idx: usize);

    /// Called after all stages complete.
    async fn on_complete(&mut self, last_stage_idx: usize);

    /// Post-stage state update (worker writes meta + context snapshot).
    async fn on_post_stage(
        &mut self,
        engine: &AgentEngine,
        entity: bevy_ecs::prelude::Entity,
        stage_name: &str,
    );
}

/// The unified stage loop. Both foreground and worker modes call this with
/// their respective `StageCallbacks` implementation.
///
/// COVERAGE-CONFIRMED-ARTIFACT: this function is generic over both
/// `CB: StageCallbacks` and the executor closure `F`/`Fut`, so every distinct
/// (`CB`, `F`)
/// combination a caller uses compiles as a separate monomorphized
/// instantiation. `cargo-llvm-cov`'s own instantiation-group merging can
/// under-report a handful of regions/lines for this function even when every
/// branch is genuinely exercised by some instantiation (confirmed by
/// inspecting the merged per-file segment data directly: it shows 100% real
/// coverage even when the summary table's region/line counts show a small
/// residual miss). `run_worker_inner` (worker.rs) and
/// `run_foreground_with_registry` (foreground.rs) both take their
/// provider-registry builder as a concrete `fn` pointer rather than `impl
/// FnOnce` specifically to keep their own (and therefore this function's)
/// instantiation count to the legitimate minimum -- one per production
/// `StageCallbacks` impl, plus one per test double that genuinely needs
/// distinct behavior (see this module's `MockCallbacks`/`ModelCapture`/
/// `VisitCapture`). Making `StageCallbacks` object-safe (erasing `CB` too)
/// would remove the rest, but `run_autonomous`'s own `<F, Fut>` generics
/// make that a cascading refactor through every implementor and through
/// `stages.rs`'s executor-closure plumbing -- out of scope for a
/// coverage-only pass.
#[allow(clippy::too_many_arguments)]
pub async fn run_stage_loop<CB, F, Fut>(
    ctx: &mut StageContext<'_>,
    cb: &mut CB,
    agent_id: &str,
    io: &mut dyn RunIO,
    exec: &mut F,
) -> anyhow::Result<()>
where
    CB: StageCallbacks,
    F: FnMut(Vec<ToolCall>) -> Fut + Send,
    Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
{
    let entry_name = ctx.blueprint.resolve_entry_stage_name();
    let mut current_stage_name_val = entry_name;
    let mut current_stage_idx_val = ctx
        .blueprint
        .stages
        .iter()
        .position(|s| s.name == current_stage_name_val)
        .unwrap_or(0);
    let mut visit_counts: HashMap<String, usize> = HashMap::new();

    loop {
        let stage = ctx
            .blueprint
            .find_stage(&current_stage_name_val)
            .expect("resolve_entry_stage_name and resolve_transition both guarantee a valid name");

        let stage_idx = current_stage_idx_val;

        // Resolve provider + model, supporting:
        //   1. --model provider/model   (override both)
        //   2. --model model-name       (override model only, keep stage provider)
        //   3. Fallback chain from ModelConfig.fallbacks
        let (resolved_provider, resolved_model) = {
            let (override_provider, override_model) = match ctx.model_override.as_deref() {
                Some(ov) if ov.contains('/') => {
                    let (p, m) = ov.split_once('/').unwrap();
                    (Some(p.to_string()), Some(m.to_string()))
                }
                Some(ov) => (None, Some(ov.to_string())),
                None => (None, None),
            };

            let primary_provider = override_provider
                .as_deref()
                .unwrap_or(&stage.model.provider);
            let primary_model = override_model.as_deref().unwrap_or(&stage.model.model);

            if ctx.engine.providers().has(primary_provider) {
                (primary_provider.to_string(), primary_model.to_string())
            } else {
                // Try fallbacks from the stage's model config
                let mut found = None;
                for fb in &stage.model.fallbacks {
                    if ctx.engine.providers().has(&fb.provider) {
                        tracing::info!(
                            primary_provider = primary_provider,
                            fallback_provider = %fb.provider,
                            fallback_model = %fb.model,
                            "Primary provider unavailable, using fallback"
                        );
                        found = Some((fb.provider.clone(), fb.model.clone()));
                        break;
                    }
                }
                found.unwrap_or_else(|| (primary_provider.to_string(), primary_model.to_string()))
            }
        };
        let provider_name = &resolved_provider;
        let model_name = &resolved_model;

        // Update shared locks: perms, idx, name
        {
            let mut sp = ctx.current_stage_perms.lock().await;
            *sp = stage.tool_permissions.clone();
        }
        {
            let mut si = ctx.current_stage_idx.lock().await;
            *si = stage_idx;
        }
        {
            let mut sn = ctx.current_stage_name.lock().await;
            *sn = stage.name.clone();
        }

        // Provider check (after fallback resolution)
        if !ctx.engine.providers().has(provider_name)
            && cb.on_provider_missing(provider_name, stage_idx).await
        {
            return Ok(());
        }

        // Visit count + label
        let visit_num = visit_counts.get(&stage.name).copied().unwrap_or(0);
        let visit_label = if visit_num > 0 {
            format!(" (visit {})", visit_num + 1)
        } else {
            String::new()
        };

        cb.on_stage_enter(
            &stage.name,
            stage_idx,
            provider_name,
            model_name,
            &visit_label,
        )
        .await;

        // Claude-code warning
        if provider_name == "claude-code" {
            cb.on_claude_code_warning(stage_idx).await;
        }

        // Update accepts_messages (AgentState is always present after spawn_agent)
        ctx.engine
            .world_mut()
            .get_mut::<AgentState>(ctx.entity)
            .expect("AgentState always present after spawn_agent")
            .accepts_messages = stage.accepts_messages;

        if stage.accepts_messages {
            println!("\u{1f4ac} Type a message and press Enter to send input to the agent while it runs.");
        }

        // Stage layout swap
        if let Some(ref stage_layout) = stage.context_layout {
            swap_context_layout(ctx.engine, ctx.entity, stage_layout);
        }

        // Set per-stage inference config from ModelConfig.parameters
        {
            let temperature = stage
                .model
                .parameters
                .get("temperature")
                .and_then(|v| v.as_f64())
                .map(|t| t as f32);
            let max_output_tokens = stage
                .model
                .parameters
                .get("max_output_tokens")
                .and_then(|v| v.as_u64())
                .map(|t| t as usize);
            ctx.engine
                .world_mut()
                .entity_mut(ctx.entity)
                .insert(InferenceConfig {
                    temperature,
                    max_output_tokens,
                });
        }

        // System prompt injection (ContextWindow is always present after spawn_agent)
        if let Some(sp) = stage.config.get("system_prompt").and_then(|v| v.as_str()) {
            let tokens = sp.len() / 4 + 1;
            let _ = ctx
                .engine
                .world_mut()
                .get_mut::<ContextWindow>(ctx.entity)
                .expect("ContextWindow always present after spawn_agent")
                .add_to_region(
                    "conversation",
                    format!("[Stage instructions: {}]", sp),
                    tokens,
                );
        }

        // Tool filtering
        let all_tools = ctx.tool_registry.all_tool_defs();
        let effective_tools: Vec<leviath_providers::Tool> = if stage.available_tools.is_empty() {
            Vec::new()
        } else {
            all_tools
                .into_iter()
                .filter(|t| stage.available_tools.iter().any(|f| f == &t.name))
                .collect()
        };

        // Routing config
        let routing_config = stage
            .tool_result_routing
            .as_ref()
            .map(|r| ToolResultRoutingConfig {
                default_region: r.default_region.clone(),
                tool_overrides: r.tool_overrides.clone(),
                persist: r.persist,
                max_result_tokens: r.max_result_tokens,
            });
        let routing_ref = routing_config.as_ref();
        let max_iterations = stage.max_iterations.unwrap_or(20);

        // Mid-run message reader
        let stdin_handle = cb.start_message_reader(ctx.engine, agent_id, stage.accepts_messages);

        // Clone values needed after the match (stage is borrowed from blueprint)
        let stage_name_owned = stage.name.clone();
        let stage_mode = stage.mode.clone();
        // Stage execution
        let stage_result_val: StageResult;

        match &stage_mode {
            StageMode::Interactive => {
                let run_context = cb.get_run_context();
                run_interactive_stage(
                    ctx.engine,
                    ctx.entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    run_context,
                    &stage_name_owned,
                    io,
                    exec,
                )
                .await?;
                stage_result_val = StageResult::Success;
                cb.on_stage_result(
                    &stage_name_owned,
                    stage_idx,
                    &stage_result_val,
                    None,
                    ctx.engine,
                    ctx.entity,
                )
                .await;
            }
            StageMode::InteractivePoints { points } => {
                let pts = points.clone();
                let run_context = cb.get_run_context();
                run_interactive_points_stage(
                    ctx.engine,
                    ctx.entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    routing_ref,
                    ctx.compaction_ref,
                    &pts,
                    run_context,
                    io,
                    exec,
                )
                .await?;
                stage_result_val = StageResult::Success;
                cb.on_stage_result(
                    &stage_name_owned,
                    stage_idx,
                    &stage_result_val,
                    None,
                    ctx.engine,
                    ctx.entity,
                )
                .await;
            }
            StageMode::Autonomous => {
                match cb
                    .run_autonomous(
                        ctx.engine,
                        ctx.entity,
                        provider_name,
                        model_name,
                        max_iterations,
                        effective_tools,
                        routing_ref,
                        ctx.compaction_ref,
                        io,
                        exec,
                    )
                    .await
                {
                    Ok((result, response)) => {
                        cb.on_stage_result(
                            &stage_name_owned,
                            stage_idx,
                            &result,
                            response.as_ref(),
                            ctx.engine,
                            ctx.entity,
                        )
                        .await;
                        stage_result_val = result;
                    }
                    Err(e) => {
                        let graph = is_graph_mode(ctx.blueprint);
                        match cb
                            .on_stage_error(&stage_name_owned, stage_idx, &e, graph)
                            .await
                        {
                            Some(result) => {
                                stage_result_val = result;
                            }
                            None => {
                                // Propagate the error (linear mode)
                                // Cancel stdin reader first
                                if let Some(handle) = stdin_handle {
                                    handle.abort();
                                }
                                return Err(e);
                            }
                        }
                    }
                }
            }
        }

        // Cancel the stdin reader task now that the stage is complete
        if let Some(handle) = stdin_handle {
            handle.abort();
        }

        // Drain any undelivered messages at stage boundary so they don't
        // silently accumulate and leak into the next stage's context.
        ctx.engine.drain_pending_messages(ctx.entity);

        *visit_counts.entry(stage_name_owned.clone()).or_default() += 1;

        // Post-stage callback
        cb.on_post_stage(ctx.engine, ctx.entity, &stage_name_owned)
            .await;

        // Resolve the next transition
        let stage_ref = ctx.blueprint.find_stage(&stage_name_owned).unwrap();
        let transition = resolve_transition(
            stage_ref,
            current_stage_idx_val,
            ctx.blueprint,
            &visit_counts,
            &stage_result_val,
            ctx.engine,
            ctx.entity,
            provider_name,
            model_name,
        )
        .await;

        match transition {
            Some((edge, next_idx)) => {
                let next_name = edge.target.clone();

                cb.on_transition(&stage_name_owned, &next_name, stage_idx)
                    .await;

                let marker = format!(
                    "[Stage complete: {}, transitioning to: {}]",
                    stage_name_owned, next_name
                );
                let tokens = marker.len() / 4 + 1;
                let _ = ctx
                    .engine
                    .world_mut()
                    .get_mut::<ContextWindow>(ctx.entity)
                    .expect("ContextWindow always present after spawn_agent")
                    .add_to_region("conversation", marker, tokens);

                // Apply edge transform
                apply_edge_transform(
                    &edge,
                    &visit_counts,
                    ctx.engine,
                    ctx.entity,
                    provider_name,
                    model_name,
                    ctx.compaction_ref,
                )
                .await;

                current_stage_name_val = next_name;
                current_stage_idx_val = next_idx;
            }
            None => {
                break; // terminal: no valid transitions
            }
        }
    }

    cb.on_complete(current_stage_idx_val).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::layout::RegionDefinition;
    use leviath_core::{ContextLayout, RegionKind, Stage};
    use leviath_runtime::ProviderRegistry;

    use super::super::helpers::initialize_context_window;

    // ─── MockCallbacks ──────────────────────────────────────────────────────

    /// Mock callbacks that track all calls for assertions.
    struct MockCallbacks {
        stage_entries: Vec<(String, usize)>,
        stage_results: Vec<(String, StageResult)>,
        transitions: Vec<(String, String)>,
        provider_missing: Vec<String>,
        claude_code_warnings: Vec<usize>,
        post_stages: Vec<String>,
        completed_at: Option<usize>,
        errors: Vec<(String, String)>,
        /// If true, on_provider_missing returns true (abort).
        abort_on_provider_missing: bool,
        /// If true, on_stage_error returns Some(Error) for graph mode.
        graph_error_result: bool,
        /// If true, run_autonomous returns Err instead of Ok.
        run_autonomous_should_error: bool,
        /// When Some, get_run_context returns a borrow of this pair instead of None.
        run_context: Option<(String, RunMeta)>,
    }

    impl MockCallbacks {
        fn new() -> Self {
            Self {
                stage_entries: Vec::new(),
                stage_results: Vec::new(),
                transitions: Vec::new(),
                provider_missing: Vec::new(),
                claude_code_warnings: Vec::new(),
                post_stages: Vec::new(),
                completed_at: None,
                errors: Vec::new(),
                abort_on_provider_missing: false,
                graph_error_result: true,
                run_autonomous_should_error: false,
                run_context: None,
            }
        }
    }

    #[async_trait]
    impl StageCallbacks for MockCallbacks {
        async fn on_provider_missing(&mut self, provider: &str, _stage_idx: usize) -> bool {
            self.provider_missing.push(provider.to_string());
            self.abort_on_provider_missing
        }

        async fn on_stage_enter(
            &mut self,
            stage_name: &str,
            stage_idx: usize,
            _provider: &str,
            _model: &str,
            _visit_label: &str,
        ) {
            self.stage_entries.push((stage_name.to_string(), stage_idx));
        }

        async fn on_claude_code_warning(&mut self, stage_idx: usize) {
            self.claude_code_warnings.push(stage_idx);
        }

        fn start_message_reader(
            &mut self,
            _engine: &AgentEngine,
            _agent_id: &str,
            accepts: bool,
        ) -> Option<tokio::task::JoinHandle<()>> {
            // Mirrors the real foreground/worker implementations, which only
            // spawn a reader task (and therefore return `Some`) when the
            // stage actually accepts messages.
            if accepts {
                Some(tokio::spawn(std::future::ready(())))
            } else {
                None
            }
        }

        fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)> {
            self.run_context
                .as_mut()
                .map(|(id, meta)| (id.as_str(), meta))
        }

        async fn run_autonomous<F, Fut>(
            &mut self,
            _engine: &mut AgentEngine,
            _entity: bevy_ecs::prelude::Entity,
            _provider: &str,
            _model: &str,
            _max_iterations: usize,
            _tools: Vec<leviath_providers::Tool>,
            _routing: Option<&ToolResultRoutingConfig>,
            _compaction: Option<&CompactionConfig>,
            _io: &mut dyn RunIO,
            _executor: &mut F,
        ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
        where
            F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
            Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
        {
            if self.run_autonomous_should_error {
                return Err(anyhow::anyhow!("simulated autonomous failure"));
            }
            // Simulate successful autonomous completion
            Ok((StageResult::Success, None))
        }

        async fn on_stage_result(
            &mut self,
            stage_name: &str,
            _stage_idx: usize,
            result: &StageResult,
            _response: Option<&InferenceResponse>,
            _engine: &mut AgentEngine,
            _entity: bevy_ecs::prelude::Entity,
        ) {
            self.stage_results
                .push((stage_name.to_string(), result.clone()));
        }

        async fn on_stage_error(
            &mut self,
            stage_name: &str,
            _stage_idx: usize,
            error: &anyhow::Error,
            is_graph_mode: bool,
        ) -> Option<StageResult> {
            self.errors
                .push((stage_name.to_string(), error.to_string()));
            if is_graph_mode && self.graph_error_result {
                Some(StageResult::Error)
            } else {
                None
            }
        }

        async fn on_transition(&mut self, from_stage: &str, to_stage: &str, _stage_idx: usize) {
            self.transitions
                .push((from_stage.to_string(), to_stage.to_string()));
        }

        async fn on_complete(&mut self, last_stage_idx: usize) {
            self.completed_at = Some(last_stage_idx);
        }

        async fn on_post_stage(
            &mut self,
            _engine: &AgentEngine,
            _entity: bevy_ecs::prelude::Entity,
            stage_name: &str,
        ) {
            self.post_stages.push(stage_name.to_string());
        }
    }

    // ─── Test helpers ───────────────────────────────────────────────────────

    use super::super::io::mock::MockIO;

    fn make_blueprint(stages: Vec<Stage>) -> Blueprint {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow { max_items: 50 },
                    10000,
                ),
            ],
            12000,
        );
        Blueprint::new("test".to_string(), "test agent".to_string(), stages, layout)
    }

    fn make_stage(name: &str) -> Stage {
        Stage::new(
            name.to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        )
    }

    fn make_engine_and_entity(
        blueprint: &Blueprint,
    ) -> (AgentEngine, AgentPool, bevy_ecs::prelude::Entity) {
        let registry = ProviderRegistry::new();
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        (engine, pool, entity)
    }

    async fn make_tool_registry() -> Arc<ToolRegistry> {
        let config = crate::config::Config::default();
        let workdir = std::env::current_dir().unwrap();
        Arc::new(ToolRegistry::build(workdir, &config).await)
    }

    /// A mock provider that returns a canned response — used to exercise the
    /// Interactive/InteractivePoints stage paths, which call real engine
    /// inference (unlike MockCallbacks::run_autonomous, which fakes it).
    struct CannedProvider;

    #[async_trait]
    impl leviath_providers::Provider for CannedProvider {
        async fn infer(
            &self,
            _request: leviath_providers::InferenceRequest,
        ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
        {
            Ok(leviath_providers::InferenceResponse {
                content: "canned response".to_string(),
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

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "canned"
        }

        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn make_engine_and_entity_with_provider(
        blueprint: &Blueprint,
    ) -> (AgentEngine, AgentPool, bevy_ecs::prelude::Entity) {
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(CannedProvider));
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        (engine, pool, entity)
    }

    fn noop_exec(
        _calls: Vec<leviath_providers::ToolCall>,
    ) -> std::future::Ready<Vec<(String, String)>> {
        std::future::ready(vec![])
    }

    // ─── Tests ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn single_stage_fires_enter_result_complete() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_eq!(cb.stage_entries.len(), 1);
        assert_eq!(cb.stage_entries[0].0, "main");
        assert_eq!(cb.stage_entries[0].1, 0);
        assert_eq!(cb.stage_results.len(), 1);
        assert_eq!(cb.stage_results[0].0, "main");
        assert_eq!(cb.post_stages, vec!["main"]);
        assert_eq!(cb.completed_at, Some(0));
    }

    #[tokio::test]
    async fn linear_multi_stage_fires_transitions() {
        let bp = make_blueprint(vec![
            make_stage("plan"),
            make_stage("code"),
            make_stage("review"),
        ]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // All 3 stages entered
        assert_eq!(cb.stage_entries.len(), 3);
        assert_eq!(cb.stage_entries[0].0, "plan");
        assert_eq!(cb.stage_entries[1].0, "code");
        assert_eq!(cb.stage_entries[2].0, "review");

        // Transitions: plan→code, code→review
        assert_eq!(cb.transitions.len(), 2);
        assert_eq!(cb.transitions[0], ("plan".to_string(), "code".to_string()));
        assert_eq!(
            cb.transitions[1],
            ("code".to_string(), "review".to_string())
        );

        // Post-stage called for each
        assert_eq!(cb.post_stages, vec!["plan", "code", "review"]);

        // Completed at last stage
        assert_eq!(cb.completed_at, Some(2));
    }

    #[tokio::test]
    async fn provider_missing_aborts_run() {
        // Use a provider that isn't registered
        let mut stage = make_stage("main");
        stage.model.provider = "nonexistent".to_string();
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.abort_on_provider_missing = true; // provider_missing test: abort run

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_eq!(cb.provider_missing, vec!["nonexistent"]);
        // No stages should have been entered
        assert!(cb.stage_entries.is_empty());
        assert!(cb.completed_at.is_none());
    }

    #[tokio::test]
    async fn claude_code_warning_fires() {
        let mut stage = make_stage("main");
        stage.model.provider = "claude-code".to_string();
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        // Provider won't be registered, so provider_missing fires first.
        // Let it continue by not aborting.
        cb.abort_on_provider_missing = false;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // claude-code warning should have fired for stage 0
        assert_eq!(cb.claude_code_warnings, vec![0]);
    }

    #[tokio::test]
    async fn autonomous_stage_error_graph_mode_records_error_and_continues() {
        // A single stage with an (empty) `transitions` map is graph mode by
        // `is_graph_mode`'s definition, and has no outgoing edges, so the
        // run terminates right after the error is recorded.
        let mut stage = make_stage("main");
        stage.transitions = Some(HashMap::new());
        stage.accepts_messages = true; // exercise the stdin-reader Some(handle) path too
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.run_autonomous_should_error = true;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        let result = run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await;

        // Graph mode should recover from the error via on_stage_error, not propagate it.
        assert!(result.is_ok());
        assert_eq!(cb.errors.len(), 1);
        assert_eq!(cb.errors[0].0, "main");
        assert!(cb.errors[0].1.contains("simulated autonomous failure"));
        // No transition edges -> terminal after the error is handled.
        assert_eq!(cb.completed_at, Some(0));
    }

    #[tokio::test]
    async fn autonomous_stage_error_linear_mode_propagates_and_aborts_stdin_reader() {
        // No `transitions` set -> linear mode -> on_stage_error returns None
        // -> the error propagates out of run_stage_loop.
        let mut stage = make_stage("main");
        stage.accepts_messages = true; // exercise the stdin-reader abort-before-return path
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.run_autonomous_should_error = true;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        let result = run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await;

        // Linear mode must propagate the error.
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated autonomous failure"));
        assert_eq!(cb.errors.len(), 1);
        // on_complete is never reached when the error propagates.
        assert!(cb.completed_at.is_none());
    }

    #[tokio::test]
    async fn stage_with_layout_prompt_tool_filter_and_routing_completes() {
        // Exercises the per-stage context_layout swap, system_prompt
        // injection, non-empty available_tools filter, and
        // tool_result_routing construction -- all otherwise-untouched by
        // every other test in this file, which use `make_stage`'s defaults
        // (no layout override, no system_prompt, empty available_tools,
        // no routing).
        let mut stage = make_stage("main");
        stage.context_layout = Some(ContextLayout::new(
            vec![RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow { max_items: 10 },
                5000,
            )],
            5000,
        ));
        stage
            .config
            .insert("system_prompt".to_string(), serde_json::json!("Be terse."));
        stage.available_tools = vec!["read_file".to_string()];
        stage.tool_result_routing = Some(leviath_core::blueprint::ToolResultRouting::default());

        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_eq!(
            cb.stage_results,
            vec![("main".to_string(), StageResult::Success)]
        );
        assert_eq!(cb.completed_at, Some(0));
    }

    #[tokio::test]
    async fn noop_exec_returns_empty_vec() {
        assert_eq!(noop_exec(vec![]).await, Vec::<(String, String)>::new());
    }

    #[tokio::test]
    async fn canned_provider_trivial_trait_methods() {
        let provider = CannedProvider;
        assert_eq!(
            leviath_providers::Provider::count_tokens(&provider, "abcd", "m"),
            1
        );
        assert_eq!(
            leviath_providers::Provider::max_context_tokens(&provider, "m"),
            100_000
        );
        assert_eq!(leviath_providers::Provider::name(&provider), "canned");
    }

    #[tokio::test]
    async fn stage_idx_lock_updated_per_stage() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let stage_idx = Arc::new(Mutex::new(99usize));

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: stage_idx.clone(),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // After the loop, stage_idx should reflect the last stage
        let final_idx = *stage_idx.lock().await;
        assert_eq!(final_idx, 1);
    }

    #[tokio::test]
    async fn stage_name_lock_updated_per_stage() {
        let bp = make_blueprint(vec![make_stage("alpha"), make_stage("beta")]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let stage_name = Arc::new(Mutex::new(String::new()));

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: stage_name.clone(),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        let final_name = stage_name.lock().await.clone();
        assert_eq!(final_name, "beta");
    }

    #[tokio::test]
    async fn model_override_is_used() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;

        // Custom callbacks that capture model name
        struct ModelCapture {
            models: Vec<String>,
        }

        #[async_trait]
        impl StageCallbacks for ModelCapture {
            async fn on_provider_missing(&mut self, _p: &str, _i: usize) -> bool {
                false
            }
            async fn on_stage_enter(
                &mut self,
                _n: &str,
                _i: usize,
                _p: &str,
                model: &str,
                _v: &str,
            ) {
                self.models.push(model.to_string());
            }
            async fn on_claude_code_warning(&mut self, _i: usize) {}
            fn start_message_reader(
                &mut self,
                _e: &AgentEngine,
                _a: &str,
                _acc: bool,
            ) -> Option<tokio::task::JoinHandle<()>> {
                None
            }
            fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)> {
                None
            }
            async fn run_autonomous<F, Fut>(
                &mut self,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _p: &str,
                _m: &str,
                _mi: usize,
                _t: Vec<leviath_providers::Tool>,
                _r: Option<&ToolResultRoutingConfig>,
                _c: Option<&CompactionConfig>,
                _io: &mut dyn RunIO,
                _ex: &mut F,
            ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
            where
                F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
                Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
            {
                Ok((StageResult::Success, None))
            }
            async fn on_stage_result(
                &mut self,
                _n: &str,
                _i: usize,
                _r: &StageResult,
                _resp: Option<&InferenceResponse>,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
            ) {
            }
            async fn on_stage_error(
                &mut self,
                _n: &str,
                _i: usize,
                _err: &anyhow::Error,
                _g: bool,
            ) -> Option<StageResult> {
                None
            }
            async fn on_transition(&mut self, _f: &str, _t: &str, _i: usize) {}
            async fn on_complete(&mut self, _i: usize) {}
            async fn on_post_stage(
                &mut self,
                _e: &AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _n: &str,
            ) {
            }
        }

        let mut cb = ModelCapture { models: Vec::new() };

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: Some("my-custom-model".to_string()),
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_eq!(cb.models, vec!["my-custom-model"]);

        // Exercise this narrow test double's remaining trait methods, which
        // this particular scenario (model-override propagation) never
        // reaches on its own -- the same production behavior for each is
        // already covered via `MockCallbacks` elsewhere in this file.
        cb.on_claude_code_warning(0).await;
        assert!(cb.start_message_reader(&engine, "agent-1", false).is_none());
        assert!(cb.get_run_context().is_none());
        assert!(cb
            .on_stage_error("main", 0, &anyhow::anyhow!("e"), false)
            .await
            .is_none());
        cb.on_transition("a", "b", 0).await;
        cb.on_complete(0).await;
        cb.on_post_stage(&engine, entity, "main").await;
    }

    #[tokio::test]
    async fn visit_label_shows_on_revisits() {
        use leviath_core::blueprint::{EdgeTransform, TransitionCondition, TransitionEdge};

        // Create a graph: a → b → a (with max_revisits = 1)
        let mut stage_a = make_stage("a");
        let mut stage_b = make_stage("b");
        stage_a.max_revisits = Some(1);

        let mut a_transitions = HashMap::new();
        a_transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(a_transitions);

        let mut b_transitions = HashMap::new();
        b_transitions.insert(
            "a".to_string(),
            TransitionEdge {
                target: "a".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_b.transitions = Some(b_transitions);

        let bp = make_blueprint(vec![stage_a, stage_b]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;

        // Capture visit labels
        struct VisitCapture {
            labels: Vec<(String, String)>,
        }

        #[async_trait]
        impl StageCallbacks for VisitCapture {
            async fn on_provider_missing(&mut self, _p: &str, _i: usize) -> bool {
                false
            }
            async fn on_stage_enter(
                &mut self,
                name: &str,
                _i: usize,
                _p: &str,
                _m: &str,
                visit: &str,
            ) {
                self.labels.push((name.to_string(), visit.to_string()));
            }
            async fn on_claude_code_warning(&mut self, _i: usize) {}
            fn start_message_reader(
                &mut self,
                _e: &AgentEngine,
                _a: &str,
                _acc: bool,
            ) -> Option<tokio::task::JoinHandle<()>> {
                None
            }
            fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)> {
                None
            }
            async fn run_autonomous<F, Fut>(
                &mut self,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _p: &str,
                _m: &str,
                _mi: usize,
                _t: Vec<leviath_providers::Tool>,
                _r: Option<&ToolResultRoutingConfig>,
                _c: Option<&CompactionConfig>,
                _io: &mut dyn RunIO,
                _ex: &mut F,
            ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
            where
                F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
                Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
            {
                Ok((StageResult::Success, None))
            }
            async fn on_stage_result(
                &mut self,
                _n: &str,
                _i: usize,
                _r: &StageResult,
                _resp: Option<&InferenceResponse>,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
            ) {
            }
            async fn on_stage_error(
                &mut self,
                _n: &str,
                _i: usize,
                _err: &anyhow::Error,
                _g: bool,
            ) -> Option<StageResult> {
                Some(StageResult::Error)
            }
            async fn on_transition(&mut self, _f: &str, _t: &str, _i: usize) {}
            async fn on_complete(&mut self, _i: usize) {}
            async fn on_post_stage(
                &mut self,
                _e: &AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _n: &str,
            ) {
            }
        }

        let mut cb = VisitCapture { labels: Vec::new() };

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Should visit: a (first), b (first), a (revisit 2), b (visit 2)
        // Then b→a is blocked (a has max_revisits=1, visits=2 > 1), so b is terminal.
        assert_eq!(cb.labels.len(), 4);
        assert_eq!(cb.labels[0], ("a".to_string(), "".to_string()));
        assert_eq!(cb.labels[1], ("b".to_string(), "".to_string()));
        assert_eq!(cb.labels[2], ("a".to_string(), " (visit 2)".to_string()));
        assert_eq!(cb.labels[3], ("b".to_string(), " (visit 2)".to_string()));

        // Exercise this narrow test double's remaining trait methods, which
        // this particular scenario (visit-label formatting) never reaches on
        // its own -- the same production behavior for each is already
        // covered via `MockCallbacks` elsewhere in this file.
        cb.on_claude_code_warning(0).await;
        assert!(cb.start_message_reader(&engine, "agent-1", false).is_none());
        assert!(cb.get_run_context().is_none());
        assert_eq!(
            cb.on_stage_error("a", 0, &anyhow::anyhow!("e"), true).await,
            Some(StageResult::Error)
        );
        cb.on_transition("a", "b", 0).await;
        cb.on_complete(0).await;
        cb.on_post_stage(&engine, entity, "a").await;
    }

    // ─── on_stage_result must fire for Interactive/InteractivePoints too ────
    //
    // Regression test: previously only the Autonomous branch called
    // cb.on_stage_result(), so the stage record for Interactive/
    // InteractivePoints stages was never marked Complete — it stayed stuck
    // at StageRunStatus::Active forever (confirmed via real run-state data:
    // a "plan" stage with mode = interactive_points stuck at status "active",
    // ended_at: null, long after the run had moved on to later stages). That
    // made the dashboard keep showing a spinner on a stage tab that wasn't
    // actually running anymore.

    #[tokio::test]
    async fn interactive_stage_fires_on_stage_result() {
        let mut stage = make_stage("plan");
        stage.mode = StageMode::Interactive;
        stage.max_iterations = Some(1);
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        // No stdin input queued — `get_user_input` returns None, which the
        // interactive loop treats as empty input and ends the session.
        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Interactive stage must report on_stage_result so its stage record is marked Complete.
        assert_eq!(
            cb.stage_results,
            vec![("plan".to_string(), StageResult::Success)]
        );
        assert_eq!(cb.completed_at, Some(0));
    }

    #[tokio::test]
    async fn interactive_points_stage_fires_on_stage_result() {
        let mut stage = make_stage("plan");
        stage.mode = StageMode::InteractivePoints { points: vec![] };
        stage.max_iterations = Some(1);
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // InteractivePoints stage must report on_stage_result so its stage record is marked Complete.
        assert_eq!(
            cb.stage_results,
            vec![("plan".to_string(), StageResult::Success)]
        );
        assert_eq!(cb.completed_at, Some(0));
    }

    // ─── Interactive/InteractivePoints error propagation ────────────────────

    #[tokio::test]
    async fn interactive_stage_missing_provider_propagates_error() {
        // make_engine_and_entity has no registered provider → run_interactive_stage
        // returns Err during inference; the ? at the Interactive match arm propagates.
        let mut stage = make_stage("main");
        stage.mode = StageMode::Interactive;
        stage.accepts_messages = false; // stdin_handle = None (avoids leaked handle)
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        // abort_on_provider_missing defaults to false → execution reaches run_interactive_stage

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        let result = run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await;

        // Missing provider should propagate Err.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn interactive_points_stage_ipc_write_failure_propagates_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_stage_ipc_write_failure_propagates_error",
        );
        // InteractivePoints with a non-empty points slice and a run_context whose
        // run_dir doesn't exist → write_request fails inside request_interaction_async
        // → Err propagates via the ? at the InteractivePoints match arm.
        use crate::runstate::RunMeta;
        use leviath_core::blueprint::InteractionPoint;

        let mut stage = make_stage("main");
        stage.mode = StageMode::InteractivePoints {
            points: vec![InteractionPoint {
                name: "confirm".to_string(),
                prompt: "Continue?".to_string(),
                required: false,
                style: Default::default(),
                options: vec![],
                followups: Default::default(),
            }],
        };
        stage.accepts_messages = false;
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let _ = std::fs::remove_dir_all(crate::runstate::run_dir("executor-test-no-dir-ipc"));

        let mut cb = MockCallbacks::new();
        cb.run_context = Some((
            "executor-test-no-dir-ipc".to_string(),
            RunMeta::new(
                "executor-test-no-dir-ipc".to_string(),
                "test-agent".to_string(),
                "/path".to_string(),
                "task".to_string(),
                None,
                "/tmp".to_string(),
                1,
            ),
        ));

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        let result = run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await;

        // IPC write failure should propagate Err from InteractivePoints.
        assert!(result.is_err());
    }

    // ─── Fallback chain tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn fallback_used_when_primary_provider_missing() {
        // Create a stage with primary provider "nonexistent" and fallback "anthropic"
        let mut stage = make_stage("main");
        stage.model = ModelConfig::new("nonexistent".to_string(), "some-model".to_string())
            .with_fallbacks(vec![ModelConfig::new(
                "anthropic".to_string(),
                "claude-sonnet-4-6".to_string(),
            )]);
        let bp = make_blueprint(vec![stage]);
        // make_engine_and_entity_with_provider registers "anthropic"
        let (mut engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;

        // Track which model was entered
        struct ModelCapture {
            models: Vec<(String, String)>,
        }
        #[async_trait]
        impl StageCallbacks for ModelCapture {
            async fn on_provider_missing(&mut self, _p: &str, _i: usize) -> bool {
                false
            }
            async fn on_stage_enter(
                &mut self,
                _n: &str,
                _i: usize,
                provider: &str,
                model: &str,
                _v: &str,
            ) {
                self.models.push((provider.to_string(), model.to_string()));
            }
            async fn on_claude_code_warning(&mut self, _i: usize) {}
            fn start_message_reader(
                &mut self,
                _e: &AgentEngine,
                _a: &str,
                _acc: bool,
            ) -> Option<tokio::task::JoinHandle<()>> {
                None
            }
            fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)> {
                None
            }
            async fn run_autonomous<F, Fut>(
                &mut self,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _p: &str,
                _m: &str,
                _mi: usize,
                _t: Vec<leviath_providers::Tool>,
                _r: Option<&ToolResultRoutingConfig>,
                _c: Option<&CompactionConfig>,
                _io: &mut dyn RunIO,
                _ex: &mut F,
            ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
            where
                F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
                Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
            {
                Ok((StageResult::Success, None))
            }
            async fn on_stage_result(
                &mut self,
                _n: &str,
                _i: usize,
                _r: &StageResult,
                _resp: Option<&InferenceResponse>,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
            ) {
            }
            async fn on_stage_error(
                &mut self,
                _n: &str,
                _i: usize,
                _err: &anyhow::Error,
                _g: bool,
            ) -> Option<StageResult> {
                None
            }
            async fn on_transition(&mut self, _f: &str, _t: &str, _i: usize) {}
            async fn on_complete(&mut self, _i: usize) {}
            async fn on_post_stage(
                &mut self,
                _e: &AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _n: &str,
            ) {
            }
        }

        let mut cb = ModelCapture { models: Vec::new() };
        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Should have used the fallback provider/model
        assert_eq!(cb.models.len(), 1);
        assert_eq!(cb.models[0].0, "anthropic");
        assert_eq!(cb.models[0].1, "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn model_override_with_provider_slash_syntax() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;

        struct ModelCapture {
            models: Vec<(String, String)>,
        }
        #[async_trait]
        impl StageCallbacks for ModelCapture {
            async fn on_provider_missing(&mut self, _p: &str, _i: usize) -> bool {
                false
            }
            async fn on_stage_enter(
                &mut self,
                _n: &str,
                _i: usize,
                provider: &str,
                model: &str,
                _v: &str,
            ) {
                self.models.push((provider.to_string(), model.to_string()));
            }
            async fn on_claude_code_warning(&mut self, _i: usize) {}
            fn start_message_reader(
                &mut self,
                _e: &AgentEngine,
                _a: &str,
                _acc: bool,
            ) -> Option<tokio::task::JoinHandle<()>> {
                None
            }
            fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)> {
                None
            }
            async fn run_autonomous<F, Fut>(
                &mut self,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _p: &str,
                _m: &str,
                _mi: usize,
                _t: Vec<leviath_providers::Tool>,
                _r: Option<&ToolResultRoutingConfig>,
                _c: Option<&CompactionConfig>,
                _io: &mut dyn RunIO,
                _ex: &mut F,
            ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
            where
                F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
                Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
            {
                Ok((StageResult::Success, None))
            }
            async fn on_stage_result(
                &mut self,
                _n: &str,
                _i: usize,
                _r: &StageResult,
                _resp: Option<&InferenceResponse>,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
            ) {
            }
            async fn on_stage_error(
                &mut self,
                _n: &str,
                _i: usize,
                _err: &anyhow::Error,
                _g: bool,
            ) -> Option<StageResult> {
                None
            }
            async fn on_transition(&mut self, _f: &str, _t: &str, _i: usize) {}
            async fn on_complete(&mut self, _i: usize) {}
            async fn on_post_stage(
                &mut self,
                _e: &AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _n: &str,
            ) {
            }
        }

        let mut cb = ModelCapture { models: Vec::new() };
        let mut ctx = StageContext {
            blueprint: &bp,
            engine: &mut engine,
            entity,
            pool: &mut pool,
            tool_registry: &tool_registry,
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: Some("anthropic/gpt-custom".to_string()),
            compaction_ref: None,
        };

        run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Should have used the provider/model from the override
        assert_eq!(cb.models.len(), 1);
        assert_eq!(cb.models[0].0, "anthropic");
        assert_eq!(cb.models[0].1, "gpt-custom");
    }
}
