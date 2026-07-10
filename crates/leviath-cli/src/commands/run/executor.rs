//! Unified stage loop: `StageContext` + `StageCallbacks` trait.
//!
//! Both foreground and worker modes implement `StageCallbacks` and share the
//! same `run_stage_loop` driver, eliminating the ~70% code duplication between
//! `foreground.rs` and `worker.rs`.

use async_trait::async_trait;
use leviath_core::blueprint::{StageMode, StageResult};
use leviath_core::lifecycle::CompactionConfig;
use leviath_core::Blueprint;
use leviath_providers::InferenceResponse;
use leviath_runtime::{AgentEngine, AgentPool, AgentState, ContextWindow, InferenceConfig};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::tools::ToolRegistry;

use super::graph::{apply_edge_transform, is_graph_mode, resolve_transition};
use super::helpers::swap_context_layout;
use super::io::RunIO;
use super::stages::{run_interactive_points_stage, run_interactive_stage, PointsOutcome};
use leviath_core::taint::{
    resolve_security, resolve_taint_enabled, SecurityConfig, ToolClassification, ToolDirection,
};
use leviath_core::PolicyConfig;
use leviath_runtime::taint::TaintGate;

/// Build the taint gate for a stage, cascading global → agent → stage. Returns
/// `None` when taint tracking resolves to disabled for this stage. Applies any
/// per-tool classification overrides from the policy's `mcp_overrides`.
fn build_stage_taint_gate(
    global: bool,
    agent_sec: Option<&SecurityConfig>,
    stage_sec: Option<&SecurityConfig>,
    policy: &PolicyConfig,
) -> Option<TaintGate> {
    if !resolve_taint_enabled(global, agent_sec, stage_sec) {
        return None;
    }
    let sec = resolve_security(global, agent_sec, stage_sec);
    let mut gate = TaintGate::new(sec);
    for (tool_key, ov) in &policy.mcp_overrides {
        let mut cls = ToolClassification::default();
        if let Some(s) = ov.sensitivity {
            cls.sensitivity = s;
        }
        if let Some(dir) = ov
            .direction
            .as_deref()
            .and_then(ToolDirection::from_str_loose)
        {
            cls.direction = dir;
        }
        if let Some(c) = ov.clearance {
            cls.clearance = c;
        }
        gate.set_tool_classification(tool_key.clone(), cls);
    }
    Some(gate)
}

use crate::runstate::RunMeta;

/// Type-erased tool-executor plumbing, re-exported from leviath-runtime so the
/// CLI stage loop and the engine's inference loop share one identical boxed
/// closure type. Erasing the executor closure (instead of a generic `F: FnMut`)
/// is what keeps `run_stage_loop`/`StageCallbacks` to a single monomorphization
/// across the foreground, worker, and test callers — see `run_stage_loop`'s doc.
pub use leviath_runtime::{ToolExecutorDyn, ToolResultsFuture};

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
    /// User's configured default model from config file (default_provider/default_model).
    /// Used as a last-resort fallback when `allow_user_default` is true and all
    /// models in the stage's models list are unavailable.
    pub user_default_model: Option<(String, String)>,
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
    async fn run_autonomous(
        &mut self,
        engine: &mut AgentEngine,
        entity: bevy_ecs::prelude::Entity,
        provider: &str,
        model: &str,
        max_iterations: usize,
        tools: Vec<leviath_providers::Tool>,
        compaction: Option<&CompactionConfig>,
        io: &mut dyn RunIO,
        executor: &mut ToolExecutorDyn<'_>,
    ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>;

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

    /// Called when an interactive-points stage is aborted by the user. The
    /// implementer should mark the run cancelled and persist that state. The
    /// run then ends terminally with no further inference or transition.
    /// Default no-op for callers with no run state (e.g. tests, foreground).
    async fn on_cancel(&mut self, stage_idx: usize) {
        let _ = stage_idx;
    }

    /// Global taint-tracking master switch (from user config). Default off, so
    /// callers that don't opt in (tests) get no enforcement.
    fn taint_global_enabled(&self) -> bool {
        false
    }

    /// Policy (allowlists / MCP overrides) consulted when the gate blocks.
    fn taint_policy(&self) -> leviath_core::PolicyConfig {
        leviath_core::PolicyConfig::default()
    }

    /// Build a resolver for interactively deciding a blocked outbound call.
    /// `None` → blocked calls are denied outright. Foreground/worker override
    /// this to prompt via stdin / the dashboard.
    fn make_gate_prompt(&self) -> Option<Box<dyn leviath_runtime::taint::GatePrompt>> {
        None
    }

    /// Called at the end of a stage with the taint audit events recorded during
    /// it, so the implementer can persist them. Default no-op.
    async fn on_taint_audit(
        &mut self,
        stage_idx: usize,
        events: &[leviath_core::taint::GateEvent],
    ) {
        let _ = (stage_idx, events);
    }

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
/// Fully type-erased: `cb` is a `&mut dyn StageCallbacks` and `exec` a
/// `&mut ToolExecutorDyn`, so this function (and everything it inlines) compiles
/// as a *single* monomorphization shared by the foreground, worker, and test
/// callers — rather than one per (`CB`, `F`) combination. That single
/// instantiation is exercised end-to-end by this module's tests, so its
/// coverage no longer depends on `cargo-llvm-cov`'s instantiation-group merging
/// (which previously under-reported this function's regions/lines whenever a
/// branch was covered only in some other, real-IO-only instantiation).
#[allow(clippy::too_many_arguments)]
pub async fn run_stage_loop(
    ctx: &mut StageContext<'_>,
    cb: &mut dyn StageCallbacks,
    agent_id: &str,
    io: &mut dyn RunIO,
    exec: &mut ToolExecutorDyn<'_>,
) -> anyhow::Result<()> {
    let entry_name = ctx.blueprint.resolve_entry_stage_name();
    let mut current_stage_name_val = entry_name;
    let mut current_stage_idx_val = ctx
        .blueprint
        .stages
        .iter()
        .position(|s| s.name == current_stage_name_val)
        .unwrap_or(0);
    let mut visit_counts: HashMap<String, usize> = HashMap::new();
    let mut aborted = false;

    loop {
        let stage = ctx
            .blueprint
            .find_stage(&current_stage_name_val)
            .expect("resolve_entry_stage_name and resolve_transition both guarantee a valid name");

        let stage_idx = current_stage_idx_val;

        // Resolve provider + model, supporting:
        //   1. --model provider/model   (override both)
        //   2. --model model-name       (override model only, keep stage provider)
        //   3. Models list: iterate in priority order, pick first available
        //   4. allow_user_default: fall back to user default model when true
        let (resolved_provider, resolved_model) = {
            let (override_provider, override_model) = match ctx.model_override.as_deref() {
                Some(ov) if ov.contains('/') => {
                    let (p, m) = ov.split_once('/').unwrap();
                    (Some(p.to_string()), Some(m.to_string()))
                }
                Some(ov) => (None, Some(ov.to_string())),
                None => (None, None),
            };

            if let Some(ref op) = override_provider {
                // Full provider/model override — use verbatim, regardless of
                // whether the provider is currently registered.
                (op.clone(), override_model.unwrap())
            } else {
                // No full override. `override_model` may still hold a
                // model-name-only override. Iterate the models list in priority
                // order; when a provider is available, keep the CLI-overridden
                // model name (if any) but pair it with that available provider.
                let mut found = None;
                for (i, entry) in stage.model.models.iter().enumerate() {
                    if ctx.engine.providers().has(&entry.provider) {
                        if i > 0 {
                            tracing::info!(
                                preferred_provider = %stage.model.models[0].provider,
                                selected_provider = %entry.provider,
                                selected_model = %entry.model,
                                "Using lower-priority model (higher-priority providers unavailable)"
                            );
                        }
                        let model = override_model
                            .clone()
                            .unwrap_or_else(|| entry.model.clone());
                        found = Some((entry.provider.clone(), model));
                        break;
                    }
                }
                if let Some(f) = found {
                    f
                } else if stage.model.allow_user_default {
                    // All listed models unavailable — try user defaults
                    if let Some(ref om) = override_model {
                        // CLI --model flag as last resort
                        let provider = ctx
                            .user_default_model
                            .as_ref()
                            .map(|(p, _)| p.clone())
                            .unwrap_or_else(|| stage.model.provider().to_string());
                        tracing::info!(
                            model = %om,
                            provider = %provider,
                            "All listed models unavailable, falling back to user override"
                        );
                        (provider, om.clone())
                    } else if let Some((ref dp, ref dm)) = ctx.user_default_model {
                        // Config default_provider + default_model as last resort
                        if ctx.engine.providers().has(dp) {
                            tracing::info!(
                                provider = %dp,
                                model = %dm,
                                "All listed models unavailable, falling back to user default from config"
                            );
                            (dp.clone(), dm.clone())
                        } else {
                            tracing::warn!(
                                provider = %dp,
                                "All listed models and user default provider unavailable"
                            );
                            (
                                stage.model.provider().to_string(),
                                stage.model.model().to_string(),
                            )
                        }
                    } else {
                        // No user default configured — use first listed model (will likely fail at provider check)
                        (
                            stage.model.provider().to_string(),
                            stage.model.model().to_string(),
                        )
                    }
                } else {
                    // allow_user_default is false — no fallback allowed
                    tracing::error!(
                        stage = %stage.name,
                        "All listed models unavailable and allow_user_default is false"
                    );
                    (
                        stage.model.provider().to_string(),
                        stage.model.model().to_string(),
                    )
                }
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

        let max_iterations = stage.max_iterations.unwrap_or(20);

        // Mid-run message reader
        let stdin_handle = cb.start_message_reader(ctx.engine, agent_id, stage.accepts_messages);

        // Clone values needed after the match (stage is borrowed from blueprint)
        let stage_name_owned = stage.name.clone();
        let stage_mode = stage.mode.clone();

        // ── Taint enforcement: resolve global → agent → stage and configure
        // the engine's gate for this stage (or clear it when disabled). ──
        {
            let global = cb.taint_global_enabled();
            let agent_sec = ctx.blueprint.security.as_ref();
            let stage_sec = stage.security.as_ref();
            if let Some(gate) =
                build_stage_taint_gate(global, agent_sec, stage_sec, &cb.taint_policy())
            {
                ctx.engine
                    .configure_taint(gate, cb.taint_policy(), cb.make_gate_prompt());
                ctx.engine.enable_entity_taint_tracking(ctx.entity);
            } else {
                ctx.engine.clear_taint();
            }
        }

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
                let outcome = run_interactive_points_stage(
                    ctx.engine,
                    ctx.entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    ctx.compaction_ref,
                    &pts,
                    run_context,
                    io,
                    exec,
                )
                .await?;
                match outcome {
                    PointsOutcome::Aborted => {
                        // Deterministic user abort: mark the run cancelled and
                        // end terminally — no transition, no completion.
                        cb.on_cancel(stage_idx).await;
                        aborted = true;
                        if let Some(handle) = stdin_handle {
                            handle.abort();
                        }
                        break;
                    }
                    PointsOutcome::Completed => {
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
                }
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

        // Persist this stage's taint audit events (if any) before moving on.
        let taint_audit = ctx.engine.taint_audit_log().to_vec();
        if !taint_audit.is_empty() {
            cb.on_taint_audit(stage_idx, &taint_audit).await;
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

    if !aborted {
        cb.on_complete(current_stage_idx_val).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::{ModelConfig, ModelEntry};
    use leviath_core::layout::RegionDefinition;
    use leviath_core::{ContextLayout, RegionKind, Stage};
    use leviath_runtime::ProviderRegistry;

    use super::super::helpers::initialize_context_window;

    // ─── MockCallbacks ──────────────────────────────────────────────────────

    /// Mock callbacks that track all calls for assertions.
    struct MockCallbacks {
        stage_entries: Vec<(String, usize)>,
        /// Resolved (provider, model) captured at each `on_stage_enter`.
        resolved_models: Vec<(String, String)>,
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
        /// Stage index recorded by on_cancel (None if never cancelled).
        cancelled_at: Option<usize>,
        /// When true, taint_global_enabled() returns true (drives the per-stage
        /// taint-config path in run_stage_loop).
        taint_global: bool,
        /// Stage indexes for which on_taint_audit fired.
        taint_audits: Vec<usize>,
    }

    impl MockCallbacks {
        fn new() -> Self {
            Self {
                stage_entries: Vec::new(),
                resolved_models: Vec::new(),
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
                cancelled_at: None,
                taint_global: false,
                taint_audits: Vec::new(),
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
            provider: &str,
            model: &str,
            _visit_label: &str,
        ) {
            self.stage_entries.push((stage_name.to_string(), stage_idx));
            self.resolved_models
                .push((provider.to_string(), model.to_string()));
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

        async fn run_autonomous(
            &mut self,
            _engine: &mut AgentEngine,
            _entity: bevy_ecs::prelude::Entity,
            _provider: &str,
            _model: &str,
            _max_iterations: usize,
            _tools: Vec<leviath_providers::Tool>,
            _compaction: Option<&CompactionConfig>,
            _io: &mut dyn RunIO,
            _executor: &mut ToolExecutorDyn<'_>,
        ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)> {
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

        fn taint_global_enabled(&self) -> bool {
            self.taint_global
        }

        async fn on_taint_audit(
            &mut self,
            stage_idx: usize,
            _events: &[leviath_core::taint::GateEvent],
        ) {
            self.taint_audits.push(stage_idx);
        }

        async fn on_cancel(&mut self, stage_idx: usize) {
            self.cancelled_at = Some(stage_idx);
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

    fn noop_exec(_calls: Vec<leviath_providers::ToolCall>) -> ToolResultsFuture<'static> {
        Box::pin(std::future::ready(vec![]))
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
            user_default_model: None,
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
            user_default_model: None,
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
        stage.model = ModelConfig::new("nonexistent".to_string(), "some-model".to_string());
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
            user_default_model: None,
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
        stage.model = ModelConfig::new("claude-code".to_string(), "test".to_string());
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
            user_default_model: None,
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
            user_default_model: None,
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
            user_default_model: None,
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
        // injection, and non-empty available_tools filter -- all
        // otherwise-untouched by every other test in this file, which use
        // `make_stage`'s defaults (no layout override, no system_prompt,
        // empty available_tools).
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
            user_default_model: None,
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
            user_default_model: None,
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
            user_default_model: None,
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
            async fn run_autonomous(
                &mut self,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _p: &str,
                _m: &str,
                _mi: usize,
                _t: Vec<leviath_providers::Tool>,
                _c: Option<&CompactionConfig>,
                _io: &mut dyn RunIO,
                _ex: &mut ToolExecutorDyn<'_>,
            ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)> {
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
            user_default_model: None,
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
        // Default (non-overridden) taint/cancel trait-method bodies.
        cb.on_cancel(0).await;
        assert!(!cb.taint_global_enabled());
        let _ = cb.taint_policy();
        assert!(cb.make_gate_prompt().is_none());
        cb.on_taint_audit(0, &[]).await;
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
            async fn run_autonomous(
                &mut self,
                _e: &mut AgentEngine,
                _ent: bevy_ecs::prelude::Entity,
                _p: &str,
                _m: &str,
                _mi: usize,
                _t: Vec<leviath_providers::Tool>,
                _c: Option<&CompactionConfig>,
                _io: &mut dyn RunIO,
                _ex: &mut ToolExecutorDyn<'_>,
            ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)> {
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
            user_default_model: None,
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
            user_default_model: None,
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
            user_default_model: None,
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

    #[tokio::test]
    async fn interactive_points_abort_cancels_run_and_skips_transition() {
        // Selecting an abort option must call on_cancel and end the run
        // terminally: NO transition resolution, NO on_stage_result, and
        // on_complete must NOT fire (the run is cancelled, not completed).
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_abort_cancels_run_and_skips_transition",
        );
        use crate::runstate::{self, RunMeta};
        use leviath_core::blueprint::InteractionPoint;

        let mut stage = make_stage("plan");
        stage.mode = StageMode::InteractivePoints {
            points: vec![InteractionPoint {
                name: "plan_approval".to_string(),
                prompt: "Approve?".to_string(),
                required: false,
                style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
                options: vec!["Approve".to_string(), "Abort".to_string()],
                directives: Default::default(),
                abort_options: vec!["Abort".to_string()],
                edit_options: Default::default(),
            }],
        };
        stage.max_iterations = Some(2);
        stage.accepts_messages = false;
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;

        let run_id = "exec-abort-run".to_string();
        let meta = RunMeta::new(
            run_id.clone(),
            "test".into(),
            "/p".into(),
            "t".into(),
            None,
            "/tmp".into(),
            1,
        );
        runstate::create_run(&meta).unwrap();

        let mut cb = MockCallbacks::new();
        cb.run_context = Some((run_id.clone(), meta));

        // Answer the plan_approval choice with "Abort" (index 1).
        let responder_run_id = run_id.clone();
        let responder = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                if let Some(req) = crate::interaction::read_request(&responder_run_id) {
                    let mut resp = crate::interaction::InteractionResponse::choice("", 1);
                    resp.request_id = req.id.clone();
                    crate::interaction::write_response(&responder_run_id, &resp).unwrap();
                    break;
                }
            }
        });

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
            user_default_model: None,
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

        let _ = responder.await;

        // on_cancel fired for stage 0; the run did NOT complete or transition,
        // and no stage_result was recorded (the stage was aborted, not run).
        assert_eq!(cb.cancelled_at, Some(0));
        assert_eq!(cb.completed_at, None);
        assert!(cb.transitions.is_empty());
        assert!(cb.stage_results.is_empty());

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
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
            user_default_model: None,
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
                directives: Default::default(),
                abort_options: Default::default(),
                edit_options: Default::default(),
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
            user_default_model: None,
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

    // ─── Models list priority tests ──────────────────────────────────────

    #[tokio::test]
    async fn models_list_selects_first_available_provider() {
        // Install the always-on subscriber so the lower-priority-selection
        // tracing::info! field expressions are actually evaluated (and covered).
        crate::test_support::with_tracing(|| {});
        // Create a stage with models list: "nonexistent" first, "anthropic" second
        let mut stage = make_stage("main");
        stage.model = ModelConfig {
            models: vec![
                ModelEntry::new("nonexistent".to_string(), "some-model".to_string()),
                ModelEntry::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
            ],
            allow_user_default: true,
            parameters: std::collections::HashMap::new(),
        };
        let bp = make_blueprint(vec![stage]);
        // make_engine_and_entity_with_provider registers "anthropic"
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
            user_default_model: None,
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

        // Should have selected the first available provider (skipping "nonexistent")
        assert_eq!(
            cb.resolved_models,
            vec![("anthropic".to_string(), "claude-sonnet-4-6".to_string())]
        );
    }

    #[tokio::test]
    async fn model_override_with_provider_slash_syntax() {
        let bp = make_blueprint(vec![make_stage("main")]);
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
            model_override: Some("anthropic/gpt-custom".to_string()),
            user_default_model: None,
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

        // Should have used the provider/model from the override verbatim
        assert_eq!(
            cb.resolved_models,
            vec![("anthropic".to_string(), "gpt-custom".to_string())]
        );
    }

    // ─── allow_user_default tests ───────────────────────────────────────

    #[tokio::test]
    async fn allow_user_default_falls_back_to_config_default() {
        crate::test_support::with_tracing(|| {});
        // All listed providers unavailable, allow_user_default=true,
        // user_default_model points to an available provider → use it
        let mut stage = make_stage("main");
        stage.model = ModelConfig {
            models: vec![
                ModelEntry::new("nonexistent1".to_string(), "model-a".to_string()),
                ModelEntry::new("nonexistent2".to_string(), "model-b".to_string()),
            ],
            allow_user_default: true,
            parameters: std::collections::HashMap::new(),
        };
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
            user_default_model: Some(("anthropic".to_string(), "claude-haiku".to_string())),
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

        // Should have entered main stage (provider_missing fires but doesn't abort)
        assert_eq!(cb.stage_entries.len(), 1);
    }

    #[tokio::test]
    async fn allow_user_default_false_does_not_fallback() {
        crate::test_support::with_tracing(|| {});
        // All listed providers unavailable, allow_user_default=false
        // → does NOT use user_default_model, uses first listed instead
        let mut stage = make_stage("main");
        stage.model = ModelConfig {
            models: vec![ModelEntry::new(
                "nonexistent".to_string(),
                "model-a".to_string(),
            )],
            allow_user_default: false,
            parameters: std::collections::HashMap::new(),
        };
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
            user_default_model: Some(("anthropic".to_string(), "claude-haiku".to_string())),
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

        // provider_missing should fire with "nonexistent", NOT "anthropic"
        assert_eq!(cb.provider_missing, vec!["nonexistent"]);
    }

    #[tokio::test]
    async fn allow_user_default_with_no_default_configured() {
        // allow_user_default=true but no user_default_model → first listed
        let mut stage = make_stage("main");
        stage.model = ModelConfig {
            models: vec![ModelEntry::new(
                "nonexistent".to_string(),
                "model-a".to_string(),
            )],
            allow_user_default: true,
            parameters: std::collections::HashMap::new(),
        };
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
            user_default_model: None,
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

        // Falls through to first listed — provider_missing fires with "nonexistent"
        assert_eq!(cb.provider_missing, vec!["nonexistent"]);
    }

    #[tokio::test]
    async fn allow_user_default_with_unavailable_default_provider() {
        crate::test_support::with_tracing(|| {});
        // allow_user_default=true, user default set but ITS provider also unavailable
        let mut stage = make_stage("main");
        stage.model = ModelConfig {
            models: vec![ModelEntry::new(
                "nonexistent".to_string(),
                "model-a".to_string(),
            )],
            allow_user_default: true,
            parameters: std::collections::HashMap::new(),
        };
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
            user_default_model: Some(("also_nonexistent".to_string(), "model-x".to_string())),
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

        // User default provider is also unavailable → falls through to first listed
        assert_eq!(cb.provider_missing, vec!["nonexistent"]);
    }

    #[tokio::test]
    async fn model_only_override_falls_back_to_user_default_provider() {
        crate::test_support::with_tracing(|| {});
        // A model-name-only `--model` override with NO available listed provider
        // must keep the override model but resolve the provider from the config
        // default (user_default_model), per the documented level-4 fallback.
        let mut stage = make_stage("main");
        stage.model = ModelConfig {
            models: vec![
                ModelEntry::new("nonexistent1".to_string(), "model-a".to_string()),
                ModelEntry::new("nonexistent2".to_string(), "model-b".to_string()),
            ],
            allow_user_default: true,
            parameters: std::collections::HashMap::new(),
        };
        let bp = make_blueprint(vec![stage]);
        // registers "anthropic"
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
            model_override: Some("my-override-model".to_string()),
            user_default_model: Some(("anthropic".to_string(), "claude-haiku".to_string())),
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

        // Resolves to (config-default-provider, override-model) — provider is
        // available, so the stage is entered with no provider_missing.
        assert!(cb.provider_missing.is_empty());
        assert_eq!(
            cb.resolved_models,
            vec![("anthropic".to_string(), "my-override-model".to_string())]
        );
    }

    #[tokio::test]
    async fn model_only_override_no_default_uses_stage_provider() {
        // A model-name-only `--model` override with NO available listed provider
        // and NO config default falls back to the first listed provider, still
        // keeping the override model name.
        let mut stage = make_stage("main");
        stage.model = ModelConfig {
            models: vec![ModelEntry::new(
                "nonexistent".to_string(),
                "model-a".to_string(),
            )],
            allow_user_default: true,
            parameters: std::collections::HashMap::new(),
        };
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
            model_override: Some("my-override-model".to_string()),
            user_default_model: None,
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

        // Provider falls back to the first listed ("nonexistent", unavailable) so
        // provider_missing fires, but the override model name is preserved.
        assert_eq!(cb.provider_missing, vec!["nonexistent"]);
        assert_eq!(
            cb.resolved_models,
            vec![("nonexistent".to_string(), "my-override-model".to_string())]
        );
    }

    #[tokio::test]
    async fn stage_parameters_populate_inference_config() {
        // A stage whose model.parameters carry temperature/max_output_tokens
        // exercises the per-stage InferenceConfig resolution block, and the
        // resulting InferenceConfig is written onto the entity.
        let mut stage = make_stage("main");
        let mut parameters = std::collections::HashMap::new();
        parameters.insert("temperature".to_string(), serde_json::json!(0.5));
        parameters.insert("max_output_tokens".to_string(), serde_json::json!(1024));
        stage.model = ModelConfig {
            models: vec![ModelEntry::new(
                "anthropic".to_string(),
                "claude-sonnet-4-6".to_string(),
            )],
            allow_user_default: true,
            parameters,
        };
        let bp = make_blueprint(vec![stage]);
        // registers "anthropic" so the stage resolves and runs
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
            user_default_model: None,
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

        let cfg = ctx
            .engine
            .world()
            .get::<InferenceConfig>(entity)
            .expect("InferenceConfig should be set from stage parameters");
        assert_eq!(cfg.temperature, Some(0.5));
        assert_eq!(cfg.max_output_tokens, Some(1024));
    }

    // ─── build_stage_taint_gate ─────────────────────────────────────────────

    fn sec(taint: bool) -> SecurityConfig {
        SecurityConfig {
            taint_tracking: taint,
            ..SecurityConfig::default()
        }
    }

    #[test]
    fn build_stage_taint_gate_disabled_returns_none() {
        // Global off, nothing opts in.
        assert!(build_stage_taint_gate(false, None, None, &PolicyConfig::default()).is_none());
        // Global on but agent opts out.
        assert!(
            build_stage_taint_gate(true, Some(&sec(false)), None, &PolicyConfig::default())
                .is_none()
        );
    }

    #[test]
    fn build_stage_taint_gate_enabled_when_global_on() {
        let gate = build_stage_taint_gate(true, None, None, &PolicyConfig::default())
            .expect("gate should be built when global taint is on");
        assert!(gate.is_enabled());
    }

    #[test]
    fn build_stage_taint_gate_applies_mcp_overrides() {
        let mut policy = PolicyConfig::default();
        policy.mcp_overrides.insert(
            "srv.sender".to_string(),
            leviath_core::policy::McpToolOverride {
                sensitivity: Some(leviath_core::TaintLevel::Public),
                direction: Some("outbound".to_string()),
                clearance: Some(leviath_core::TaintLevel::Public),
            },
        );
        // Stage opts in even though global is off.
        let gate = build_stage_taint_gate(false, None, Some(&sec(true)), &policy)
            .expect("stage opt-in should build a gate");
        let cls = gate.tool_classification("srv.sender");
        assert!(cls.is_outbound());
        assert_eq!(cls.clearance, leviath_core::TaintLevel::Public);
    }

    #[tokio::test]
    async fn run_stage_loop_configures_taint_when_global_enabled() {
        // With the global taint switch on, the per-stage taint-config path in
        // run_stage_loop must build+configure the gate and enable window taint
        // tracking for the stage (then run to completion).
        let mut stage = make_stage("main");
        stage.max_iterations = Some(1);
        let bp = make_blueprint(vec![stage]);
        let (mut engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.taint_global = true;

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
            user_default_model: None,
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

        // The run completed with the per-stage taint-config path exercised
        // (build_stage_taint_gate → configure_taint → enable_entity_taint_tracking).
        assert_eq!(cb.completed_at, Some(0));
    }
}
