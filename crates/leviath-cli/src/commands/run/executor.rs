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
use leviath_runtime::{
    AgentEngine, AgentPool, AgentState, ContextWindow, EngineHandle, InferenceConfig,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::tool_source::StageToolSource;

use super::graph::{apply_edge_transform, is_graph_mode, resolve_transition};
use super::helpers::swap_context_layout;
use super::io::RunIO;
use super::stages::{run_interactive_points_stage, run_interactive_stage, PointsOutcome};
use leviath_core::taint::{
    resolve_security, resolve_taint_enabled, SecurityConfig, ToolClassification, ToolDirection,
};
use leviath_core::PolicyConfig;
use leviath_runtime::taint::TaintGate;

/// Inject a stage's `system_prompt` into `target_region` as pinned
/// `[Stage instructions: ...]` context.
///
/// The stage system prompt is essential, must-include instruction context, so a
/// prompt that doesn't fit its region is a **hard error**, not something to
/// silently drop. Previously the `add_to_region` rejection (`TokenBudgetExceeded`
/// when the prompt plus the preloaded task exceeded the region's `max_tokens`,
/// 2000 by default) was swallowed by `let _ =`, leaving the model running with
/// no instructions at all and no signal to the operator. Now the failure
/// propagates as a clear, actionable error telling the author to size the
/// region (or shorten the prompt); the shipped default agents are all sized to
/// fit comfortably.
fn inject_stage_system_prompt(
    window: &mut ContextWindow,
    target_region: &str,
    system_prompt: &str,
) -> anyhow::Result<()> {
    let content = format!("[Stage instructions: {}]", system_prompt);
    let tokens = content.len() / 4 + 1;
    window
        .add_to_region(target_region, content, tokens)
        .map_err(|e| {
            anyhow::anyhow!(
                "stage system prompt (~{tokens} tokens) does not fit context region \
                 '{target_region}': {e}. Increase that region's `max_tokens` under \
                 [context.regions] (or shorten the system_prompt)."
            )
        })
}

/// Max times a stage is re-run to populate its `required` context regions
/// before the run proceeds anyway (with a warning). Overridable per stage via
/// `max_revisits`.
const DEFAULT_REQUIRED_REENTRY_CAP: usize = 3;

/// A stage can populate context regions only if it has a context-writing tool.
/// (context_write can target any region, so this is per-stage, not per-region.)
fn stage_can_write_context(tools: &[String]) -> bool {
    tools
        .iter()
        .any(|t| t == "context_write" || t == "context_append")
}

/// Required regions (from the stage's effective layout) that are still empty.
/// Returns `(name, optional custom message)` for each. Empty when the stage
/// can't write context (gating a stage that can't populate the region would
/// just loop pointlessly).
fn unmet_required_regions(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    window: &ContextWindow,
) -> Vec<(String, Option<String>)> {
    if !stage_can_write_context(&stage.available_tools) {
        return Vec::new();
    }
    let layout = stage
        .context_layout
        .as_ref()
        .unwrap_or(&blueprint.context_layout);
    layout
        .regions
        .iter()
        .filter(|r| r.required)
        .filter(|r| {
            window
                .get_region(&r.name)
                .map(|reg| reg.content.is_empty())
                .unwrap_or(true)
        })
        .map(|r| (r.name.clone(), r.required_message.clone()))
        .collect()
}

/// Inject a nudge into the conversation region for each unmet required region,
/// so the re-run of the stage tells the agent exactly what to populate.
fn inject_required_region_nudges(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    unmet: &[(String, Option<String>)],
) {
    let mut window = engine
        .world_mut()
        .get_mut::<ContextWindow>(entity)
        .expect("ContextWindow always present after spawn_agent");
    for (name, msg) in unmet {
        let text = msg.clone().unwrap_or_else(|| {
            format!(
                "Required context region '{name}' is still empty. You must populate it \
                 (e.g. via context_write with region=\"{name}\") before this stage can complete."
            )
        });
        let content = format!("[System] {text}");
        let tokens = content.len() / 4 + 1;
        let _ = window.add_to_region("conversation", content, tokens);
    }
    window.current_tokens = window.calculate_tokens();
}

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
    /// Shared handle to the agent engine (inference + ECS world access).
    ///
    /// Held as a shared `Arc<RwLock<..>>` rather than `&mut` so the same engine
    /// can be driven concurrently by in-process sub-agents (fan-out workers).
    /// `run_stage_loop` acquires a write guard per iteration for the sequential
    /// root-agent work; the fan-out path clones this handle to drive workers.
    pub engine: EngineHandle,
    /// The ECS entity representing the running agent.
    pub entity: bevy_ecs::prelude::Entity,
    /// The agent pool managing agent lifecycle.
    pub pool: &'a mut AgentPool,
    /// Source of stage tool definitions + a tool executor for fan-out workers.
    ///
    /// A `&dyn StageToolSource` rather than a concrete `ToolRegistry` so the
    /// stage loop is decoupled from the CLI-only registry (Phase 3 seam).
    pub tool_source: &'a dyn StageToolSource,
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
    /// Available agent types (local blueprint + installed agents), keyed by
    /// name. Used to resolve fan-out `worker_agent` / `worker_query`.
    pub agent_registry: Arc<HashMap<String, Blueprint>>,
}

/// A foreground interaction-point asker: resolves an `InteractionRequest` to a
/// response (real stdin in the binary, a mock in tests). Used by
/// [`crate::commands::run::stages::run_interactive_points_stage`] on the
/// foreground (no-run-context) path.
pub type InteractionAsker =
    fn(&crate::interaction::InteractionRequest) -> crate::interaction::InteractionResponse;

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

    /// The foreground (stdin) asker for interactive-points stages, or `None`.
    ///
    /// Foreground overrides this to return `Some(..)` (its injected asker);
    /// background/worker callers keep the default `None` and resolve interaction
    /// points via IPC (`get_run_context` returns `Some`).
    /// [`crate::commands::run::stages::run_interactive_points_stage`] requires a
    /// `Some` asker only when there's no run context (true foreground).
    fn interaction_point_asker(&self) -> Option<InteractionAsker> {
        None
    }

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
    // Set by a fan-out stage to force a deterministic jump into its merge stage.
    let mut forced_transition: Option<String> = None;

    loop {
        let stage = ctx
            .blueprint
            .find_stage(&current_stage_name_val)
            .expect("resolve_entry_stage_name and resolve_transition both guarantee a valid name");

        let stage_idx = current_stage_idx_val;

        // Acquire the engine for this iteration's sequential (root-agent)
        // work. Held across the iteration; the fan-out path is the only
        // place that releases it to drive concurrent workers.
        let engine_handle = ctx.engine.clone();
        let mut eng = engine_handle.write().await;

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
                    if eng.providers().has(&entry.provider) {
                        if i > 0 {
                            // Hoist the indexed access out of the tracing macro so
                            // it is evaluated eagerly (tracing defers field
                            // evaluation when the level is disabled).
                            let preferred_provider = &stage.model.models[0].provider;
                            tracing::info!(
                                preferred_provider = %preferred_provider,
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
                        if eng.providers().has(dp) {
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
        if !eng.providers().has(provider_name)
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
        eng.world_mut()
            .get_mut::<AgentState>(ctx.entity)
            .expect("AgentState always present after spawn_agent")
            .accepts_messages = stage.accepts_messages;

        if stage.accepts_messages {
            println!("\u{1f4ac} Type a message and press Enter to send input to the agent while it runs.");
        }

        // Stage layout swap
        if let Some(ref stage_layout) = stage.context_layout {
            swap_context_layout(&mut eng, ctx.entity, stage_layout);
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
            eng.world_mut()
                .entity_mut(ctx.entity)
                .insert(InferenceConfig {
                    temperature,
                    max_output_tokens,
                });
        }

        // Set per-stage tool result routing config
        {
            let mut entity_mut = eng.world_mut().entity_mut(ctx.entity);
            if let Some(ref routing) = stage.tool_result_routing {
                entity_mut.insert(leviath_runtime::ToolResultRoutingComponent {
                    routing: routing.clone(),
                });
            } else {
                entity_mut.remove::<leviath_runtime::ToolResultRoutingComponent>();
            }
        }

        // System prompt injection — inject into the first Pinned region so
        // the instructions stay in the cacheable system block rather than
        // getting evicted from the SlidingWindow conversation region.
        {
            let mut window = eng
                .world_mut()
                .get_mut::<ContextWindow>(ctx.entity)
                .expect("ContextWindow always present after spawn_agent");

            // Find first pinned region, or fall back to conversation
            let target_region = window
                .regions
                .iter()
                .find(|r| matches!(r.kind, leviath_core::RegionKind::Pinned))
                .map(|r| r.name.clone())
                .unwrap_or_else(|| "conversation".to_string());

            // Clear any previous stage instructions before injecting new ones
            if let Some(region) = window.regions.iter_mut().find(|r| r.name == target_region) {
                region.remove_entries_by_prefix("[Stage instructions:");
            }

            if let Some(sp) = stage.config.get("system_prompt").and_then(|v| v.as_str()) {
                inject_stage_system_prompt(&mut window, &target_region, sp)?;
            }
        }

        // Tool filtering
        let all_tools = ctx.tool_source.all_tool_defs();
        let mut effective_tools: Vec<leviath_providers::Tool> = if stage.available_tools.is_empty()
        {
            Vec::new()
        } else {
            all_tools
                .into_iter()
                .filter(|t| stage.available_tools.iter().any(|f| f == &t.name))
                .collect()
        };

        // When file tracking is enabled, update tool descriptions so the model
        // knows that file contents appear in the system prompt rather than in
        // the tool result. This prevents the model from re-reading files it
        // has already read.
        if let Some(ref ft) = ctx.blueprint.file_tracking {
            for tool in &mut effective_tools {
                match tool.name.as_str() {
                    "read_file" if ft.track_reads => {
                        tool.description = format!(
                            "Read a single file. Contents stored in [{}] section of your \
                             system prompt under ### [path]. Prefer read_files for multiple \
                             files.",
                            ft.region
                        );
                    }
                    "read_files" if ft.track_reads => {
                        tool.description = format!(
                            "Read multiple files at once (preferred over repeated read_file \
                             calls). All file contents will be stored in the [{}] section of \
                             your system prompt under ### [path] for each file. The tool \
                             result confirms where each file was stored. Reference them in \
                             your system prompt — do not re-read.",
                            ft.region
                        );
                    }
                    "write_file" if ft.track_writes => {
                        tool.description = format!(
                            "Write content to a file, creating it and parent directories if \
                             needed. The written content will also be tracked in the [{}] \
                             section of your system prompt so you can reference it later.",
                            ft.region
                        );
                    }
                    "edit_file" if ft.track_writes => {
                        tool.description = format!(
                            "Edit a file by replacing a specific string. The updated content \
                             will also be tracked in the [{}] section of your system prompt \
                             so you can reference it later.",
                            ft.region
                        );
                    }
                    _ => {}
                }
            }
        }

        // Log effective tools for debugging
        let tool_names: Vec<&str> = effective_tools.iter().map(|t| t.name.as_str()).collect();
        let tool_count = effective_tools.len();
        tracing::info!(
            stage = %stage.name,
            tool_count,
            tools = ?tool_names,
            "Stage tools resolved"
        );

        let max_iterations = stage.max_iterations.unwrap_or(20);

        // Mid-run message reader
        let stdin_handle = cb.start_message_reader(&eng, agent_id, stage.accepts_messages);

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
                eng.configure_taint(ctx.entity, gate, cb.taint_policy(), cb.make_gate_prompt());
                eng.enable_entity_taint_tracking(ctx.entity);
            } else {
                eng.clear_taint(ctx.entity);
            }
        }

        // Stage execution
        let stage_result_val: StageResult;

        match &stage_mode {
            StageMode::FanOut { config } => {
                // Release the engine lock so workers can be driven concurrently,
                // then run the fan-out stage, then re-acquire for post-stage work.
                drop(eng);
                let outcome = super::fanout::run_fan_out_stage(
                    &engine_handle,
                    ctx.entity,
                    ctx.blueprint,
                    config,
                    &ctx.agent_registry,
                    ctx.tool_source,
                    provider_name,
                    model_name,
                    io,
                )
                .await?;
                eng = engine_handle.write().await;
                stage_result_val = match outcome {
                    super::fanout::FanOutOutcome::Merge(target) => {
                        forced_transition = Some(target);
                        StageResult::Success
                    }
                    super::fanout::FanOutOutcome::Proceed => StageResult::Success,
                    super::fanout::FanOutOutcome::FailAll => StageResult::Error,
                };
                cb.on_stage_result(
                    &stage_name_owned,
                    stage_idx,
                    &stage_result_val,
                    None,
                    &mut eng,
                    ctx.entity,
                )
                .await;
            }
            StageMode::Interactive => {
                let run_context = cb.get_run_context();
                run_interactive_stage(
                    &mut eng,
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
                    &mut eng,
                    ctx.entity,
                )
                .await;
            }
            StageMode::InteractivePoints { points } => {
                let pts = points.clone();
                // Compute the foreground asker before borrowing `cb` mutably via
                // `get_run_context` (which returns a `&mut` into `cb`).
                let asker = cb.interaction_point_asker();
                let run_context = cb.get_run_context();
                let outcome = run_interactive_points_stage(
                    &mut eng,
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
                    asker,
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
                            &mut eng,
                            ctx.entity,
                        )
                        .await;
                    }
                }
            }
            StageMode::Autonomous => {
                // Required-region gate: if the stage can write context and a
                // `required` region is still empty when it finishes, feed the
                // stage back in with a nudge (before any transition/user
                // feedback) rather than completing with missing context.
                // Bounded by max_revisits (or DEFAULT_REQUIRED_REENTRY_CAP);
                // proceeds with a warning if still unmet after that.
                let reentry_cap = stage.max_revisits.unwrap_or(DEFAULT_REQUIRED_REENTRY_CAP);
                let mut round: usize = 0;
                loop {
                    match cb
                        .run_autonomous(
                            &mut eng,
                            ctx.entity,
                            provider_name,
                            model_name,
                            max_iterations,
                            effective_tools.clone(),
                            ctx.compaction_ref,
                            io,
                            exec,
                        )
                        .await
                    {
                        Ok((result, response)) => {
                            let unmet = {
                                let w = eng
                                    .world()
                                    .get::<ContextWindow>(ctx.entity)
                                    .expect("ContextWindow always present after spawn_agent");
                                unmet_required_regions(ctx.blueprint, stage, w)
                            };
                            if !unmet.is_empty() && round < reentry_cap {
                                round += 1;
                                inject_required_region_nudges(&mut eng, ctx.entity, &unmet);
                                continue;
                            }
                            if !unmet.is_empty() {
                                let names: Vec<&str> =
                                    unmet.iter().map(|(n, _)| n.as_str()).collect();
                                tracing::warn!(
                                    stage = %stage_name_owned,
                                    regions = ?names,
                                    attempts = reentry_cap,
                                    "required context regions still empty after re-run attempts; proceeding"
                                );
                            }
                            cb.on_stage_result(
                                &stage_name_owned,
                                stage_idx,
                                &result,
                                response.as_ref(),
                                &mut eng,
                                ctx.entity,
                            )
                            .await;
                            stage_result_val = result;
                            break;
                        }
                        Err(e) => {
                            let graph = is_graph_mode(ctx.blueprint);
                            match cb
                                .on_stage_error(&stage_name_owned, stage_idx, &e, graph)
                                .await
                            {
                                Some(result) => {
                                    stage_result_val = result;
                                    break;
                                }
                                None => {
                                    // Propagate the error (linear mode)
                                    // Cancel stdin reader first
                                    if let Some(handle) = &stdin_handle {
                                        handle.abort();
                                    }
                                    return Err(e);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Persist this stage's taint audit events (if any) before moving on.
        let taint_audit = eng.taint_audit_log(ctx.entity).to_vec();
        if !taint_audit.is_empty() {
            cb.on_taint_audit(stage_idx, &taint_audit).await;
        }

        // Cancel the stdin reader task now that the stage is complete
        if let Some(handle) = stdin_handle {
            handle.abort();
        }

        // Drain any undelivered messages at stage boundary so they don't
        // silently accumulate and leak into the next stage's context.
        eng.drain_pending_messages(ctx.entity);

        *visit_counts.entry(stage_name_owned.clone()).or_default() += 1;

        // Post-stage callback
        cb.on_post_stage(&eng, ctx.entity, &stage_name_owned).await;

        // A fan-out stage with a merge stage forces a deterministic jump into
        // it (bypassing LLM transition resolution). Release the engine lock
        // held for this iteration before looping.
        if let Some(target) = forced_transition.take() {
            let next_idx = ctx
                .blueprint
                .stages
                .iter()
                .position(|s| s.name == target)
                .expect("merge_stage existence is validated at load time");
            cb.on_transition(&stage_name_owned, &target, stage_idx)
                .await;
            drop(eng);
            current_stage_name_val = target;
            current_stage_idx_val = next_idx;
            continue;
        }

        // Resolve the next transition
        let stage_ref = ctx.blueprint.find_stage(&stage_name_owned).unwrap();
        let transition = resolve_transition(
            stage_ref,
            current_stage_idx_val,
            ctx.blueprint,
            &visit_counts,
            &stage_result_val,
            &mut eng,
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
                let _ = eng
                    .world_mut()
                    .get_mut::<ContextWindow>(ctx.entity)
                    .expect("ContextWindow always present after spawn_agent")
                    .add_to_region("conversation", marker, tokens);

                // Apply edge transform
                apply_edge_transform(
                    &edge,
                    &visit_counts,
                    &mut eng,
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
    use crate::tools::ToolRegistry;
    use leviath_core::blueprint::{ModelConfig, ModelEntry};
    use leviath_core::layout::RegionDefinition;
    use leviath_core::{ContextLayout, EvictionStrategy, RegionKind, Stage};
    use leviath_runtime::ProviderRegistry;

    use super::super::helpers::initialize_context_window;

    use leviath_core::Region;
    use leviath_runtime::ContextWindow;

    fn region_text(window: &ContextWindow, name: &str) -> String {
        window
            .get_region(name)
            .expect("region present")
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn inject_stage_system_prompt_errors_actionably_when_it_exceeds_budget() {
        // Regression: a system prompt larger than the pinned region's budget used
        // to be silently dropped (swallowed TokenBudgetExceeded), leaving the
        // model with only the tiny task text as its system field. It must now be
        // a clear, actionable error instead.
        let mut window = ContextWindow::new(1_000_000);
        window.add_region(Region::new("task".to_string(), RegionKind::Pinned, 2000));
        window
            .add_to_region("task", "do the thing".to_string(), 5)
            .unwrap();

        let big = "sys ".repeat(15_000); // ~60KB, ~15000 tokens >> 2000 budget
        let err = inject_stage_system_prompt(&mut window, "task", &big)
            .expect_err("oversized system prompt must error, not be dropped");
        let msg = err.to_string();
        assert!(msg.contains("does not fit"), "states the problem: {msg}");
        assert!(msg.contains("'task'"), "names the region: {msg}");
        assert!(msg.contains("max_tokens"), "says how to fix it: {msg}");
    }

    #[test]
    fn inject_stage_system_prompt_ok_when_it_fits() {
        let mut window = ContextWindow::new(1_000_000);
        window.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 2000));
        inject_stage_system_prompt(&mut window, "sys", "be concise and correct").unwrap();
        assert!(region_text(&window, "sys").contains("be concise and correct"));
    }

    #[test]
    fn inject_stage_system_prompt_errors_on_missing_region() {
        let mut window = ContextWindow::new(1000);
        // No region named "nope" → add_to_region errors → propagated, not swallowed.
        assert!(inject_stage_system_prompt(&mut window, "nope", "hi").is_err());
    }

    // ─── required-region gate ───────────────────────────────────────────────

    #[test]
    fn stage_can_write_context_detects_context_tools() {
        assert!(stage_can_write_context(&["context_write".to_string()]));
        assert!(stage_can_write_context(&[
            "read_file".to_string(),
            "context_append".to_string()
        ]));
        assert!(!stage_can_write_context(&[
            "read_file".to_string(),
            "list_dir".to_string()
        ]));
    }

    /// Build a blueprint whose layout has a required `plan` region (empty), plus
    /// a `task` pinned region (so the seeded task doesn't land in `plan`).
    fn required_region_fixture() -> (leviath_core::Blueprint, leviath_core::Stage) {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("task".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new("plan".to_string(), RegionKind::Pinned, 4000)
                    .with_required(true, Some("Write the plan.".to_string())),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    10000,
                ),
            ],
            16000,
        );
        let mut stage = make_stage("analyze");
        stage.available_tools = vec!["context_write".to_string()];
        let bp = Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![stage.clone()],
            layout,
        );
        (bp, stage)
    }

    #[test]
    fn unmet_required_regions_flags_empty_and_clears_when_filled() {
        let (bp, stage) = required_region_fixture();
        let (engine, _pool, entity) = make_engine_and_entity(&bp);

        // plan is empty → flagged, with its custom message.
        {
            let guard = engine.try_read().unwrap();
            let window = guard.world().get::<ContextWindow>(entity).unwrap();
            let unmet = unmet_required_regions(&bp, &stage, window);
            assert_eq!(unmet.len(), 1);
            assert_eq!(unmet[0].0, "plan");
            assert_eq!(unmet[0].1.as_deref(), Some("Write the plan."));

            // A stage without a context-writing tool is not gated (can't fill it).
            let mut no_write = stage.clone();
            no_write.available_tools = vec!["read_file".to_string()];
            assert!(unmet_required_regions(&bp, &no_write, window).is_empty());
        }

        // Fill plan → no longer unmet.
        engine
            .try_write()
            .unwrap()
            .world_mut()
            .get_mut::<ContextWindow>(entity)
            .unwrap()
            .add_to_region("plan", "the plan".to_string(), 3)
            .unwrap();
        let guard = engine.try_read().unwrap();
        let window = guard.world().get::<ContextWindow>(entity).unwrap();
        assert!(unmet_required_regions(&bp, &stage, window).is_empty());
    }

    #[test]
    fn inject_required_region_nudges_writes_custom_and_default_messages() {
        let (bp, _stage) = required_region_fixture();
        let (engine, _pool, entity) = make_engine_and_entity(&bp);

        inject_required_region_nudges(
            &mut engine.try_write().unwrap(),
            entity,
            &[
                ("plan".to_string(), Some("Write the plan.".to_string())),
                ("architecture".to_string(), None),
            ],
        );

        let guard = engine.try_read().unwrap();
        let window = guard.world().get::<ContextWindow>(entity).unwrap();
        let conv: String = window
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(conv.contains("Write the plan."), "custom message: {conv}");
        assert!(
            conv.contains("Required context region 'architecture' is still empty"),
            "default message: {conv}"
        );
    }

    #[test]
    fn unmet_required_regions_flags_region_absent_from_window() {
        // A required region not present in the window at all is treated as
        // unmet (covers the get_region-None path).
        let (bp, stage) = required_region_fixture();
        let window = ContextWindow::new(20_000); // bare — no "plan" region
        assert!(
            unmet_required_regions(&bp, &stage, &window)
                .iter()
                .any(|(n, _)| n == "plan"),
            "an absent required region must be flagged as unmet"
        );
    }

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
        /// Number of times run_autonomous was invoked (for the required-region
        /// re-run gate test).
        run_autonomous_calls: usize,
        /// When true, run_autonomous seeds a taint-audit event so the
        /// on_taint_audit persistence path is exercised.
        seed_taint_audit: bool,
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
                run_autonomous_calls: 0,
                seed_taint_audit: false,
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
            self.run_autonomous_calls += 1;
            if self.run_autonomous_should_error {
                return Err(anyhow::anyhow!("simulated autonomous failure"));
            }
            if self.seed_taint_audit {
                // Seed a recorded gate event so run_stage_loop's
                // `taint_audit_log(...)` is non-empty and on_taint_audit fires.
                let mut gate = leviath_runtime::taint::TaintGate::new(
                    leviath_core::taint::SecurityConfig::default(),
                );
                gate.record_allow(
                    "agent-1",
                    "seed_tool",
                    leviath_core::taint::InputMode::Traditional,
                    leviath_core::taint::TaintLevel::Public,
                    leviath_core::taint::TaintLevel::Public,
                    leviath_core::taint::GateDecisionSource::AutoAllow,
                );
                _engine.configure_taint(_entity, gate, leviath_core::PolicyConfig::default(), None);
            }
            // Simulate successful autonomous completion (never populates any
            // region — so a required-region gate keeps re-running).
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
                    RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
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
    ) -> (
        leviath_runtime::EngineHandle,
        AgentPool,
        bevy_ecs::prelude::Entity,
    ) {
        let registry = ProviderRegistry::new();
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        let engine = std::sync::Arc::new(tokio::sync::RwLock::new(engine));
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
    /// Provider whose first `infer` (a fan-out split) returns a JSON work-item
    /// array; later calls (workers) return no-tool "done".
    struct FanOutProvider {
        split: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl leviath_providers::Provider for FanOutProvider {
        async fn infer(
            &self,
            _request: leviath_providers::InferenceRequest,
        ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
        {
            let content = self
                .split
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "done".to_string());
            Ok(leviath_providers::InferenceResponse {
                content,
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
        fn count_tokens(&self, t: &str, _m: &str) -> usize {
            t.len() / 4
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "anthropic"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    #[test]
    fn fan_out_provider_trait_surface() {
        use leviath_providers::Provider;
        let p = FanOutProvider {
            split: std::sync::Mutex::new(None),
        };
        assert_eq!(p.count_tokens("abcd", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        assert_eq!(p.name(), "anthropic");
        let _ = p.capabilities("m");
    }

    #[tokio::test]
    async fn run_stage_loop_fan_out_stage_merges_then_completes() {
        // A fan_out stage splits into 2 workers, then jumps into its merge stage.
        let mut fan = make_stage("parallel");
        fan.mode = StageMode::FanOut {
            config: leviath_core::blueprint::FanOutConfig {
                worker_agent: None,
                worker_stage: Some("worker".to_string()),
                worker_query: None,
                merge_stage: Some("merge".to_string()),
                max_workers: 2,
                on_worker_failure: leviath_core::blueprint::WorkerFailurePolicy::Continue,
                split_prompt: "split it".to_string(),
            },
        };
        let mut worker = make_stage("worker");
        worker.allow_as_worker = true;
        let merge = make_stage("merge"); // terminal (last stage, linear)
        let bp = make_blueprint(vec![fan, worker, merge]);

        // Engine with a provider that returns the split array first.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(FanOutProvider {
                split: std::sync::Mutex::new(Some(
                    r#"[{"id":"a","context":{}},{"id":"b","context":{}}]"#.to_string(),
                )),
            }),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(bp.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, &bp, "task");
        let engine: leviath_runtime::EngineHandle =
            std::sync::Arc::new(tokio::sync::RwLock::new(engine));

        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let mut reg = std::collections::HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: Arc::new(reg),
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

        // The loop visited the fan_out stage then jumped to the merge stage.
        assert!(cb
            .transitions
            .iter()
            .any(|(f, t)| f == "parallel" && t == "merge"));
        // Two workers were spawned as children of the root.
        let eng = engine.read().await;
        let children = eng
            .world()
            .get::<leviath_runtime::SubAgentChildren>(entity)
            .unwrap();
        assert_eq!(children.children.len(), 2);
    }

    #[tokio::test]
    async fn run_stage_loop_fan_out_worker_stage_missing_errors() {
        // A fan_out whose worker_stage names a non-existent stage makes
        // run_fan_out_stage return Err (its entry-stage lookup fails), which
        // propagates through the `?` in the FanOut arm and fails run_stage_loop.
        let mut fan = make_stage("parallel");
        fan.mode = StageMode::FanOut {
            config: leviath_core::blueprint::FanOutConfig {
                worker_agent: None,
                worker_stage: Some("ghost".to_string()), // not a real stage
                worker_query: None,
                merge_stage: None,
                max_workers: 2,
                on_worker_failure: leviath_core::blueprint::WorkerFailurePolicy::Continue,
                split_prompt: "split it".to_string(),
            },
        };
        let done = make_stage("done");
        let bp = make_blueprint(vec![fan, done]);

        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(FanOutProvider {
                split: std::sync::Mutex::new(Some(r#"[{"id":"a","context":{}}]"#.to_string())),
            }),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(bp.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, &bp, "task");
        let engine: leviath_runtime::EngineHandle =
            std::sync::Arc::new(tokio::sync::RwLock::new(engine));

        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let mut reg = std::collections::HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: Arc::new(reg),
        };

        let res = run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await;
        assert!(res.is_err(), "missing worker entry stage must fail the run");
    }

    #[tokio::test]
    async fn run_stage_loop_fan_out_no_merge_proceeds() {
        use leviath_core::blueprint::{EdgeTransform, TransitionCondition, TransitionEdge};
        // fan_out with no merge stage → Proceed → follow the Always edge to done.
        let mut fan = make_stage("parallel");
        fan.mode = StageMode::FanOut {
            config: leviath_core::blueprint::FanOutConfig {
                worker_agent: None,
                worker_stage: Some("worker".to_string()),
                worker_query: None,
                merge_stage: None,
                max_workers: 2,
                on_worker_failure: leviath_core::blueprint::WorkerFailurePolicy::Continue,
                split_prompt: "split".to_string(),
            },
        };
        let mut fan_tr = HashMap::new();
        fan_tr.insert(
            "done".to_string(),
            TransitionEdge {
                target: "done".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        fan.transitions = Some(fan_tr);
        let mut worker = make_stage("worker");
        worker.allow_as_worker = true;
        let done = make_stage("done");
        let bp = make_blueprint(vec![fan, worker, done]);

        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            Arc::new(FanOutProvider {
                split: std::sync::Mutex::new(Some(r#"[{"id":"a","context":{}}]"#.to_string())),
            }),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(bp.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, &bp, "task");
        let engine: leviath_runtime::EngineHandle =
            std::sync::Arc::new(tokio::sync::RwLock::new(engine));
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let mut reg = std::collections::HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: Arc::new(reg),
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
        assert!(cb
            .transitions
            .iter()
            .any(|(f, t)| f == "parallel" && t == "done"));
    }

    #[tokio::test]
    async fn run_stage_loop_fan_out_fail_all_takes_error_edge() {
        use leviath_core::blueprint::{EdgeTransform, TransitionCondition, TransitionEdge};
        // A non-JSON split → FailAll → StageResult::Error → the error edge fires.
        let mut fan = make_stage("parallel");
        fan.mode = StageMode::FanOut {
            config: leviath_core::blueprint::FanOutConfig {
                worker_agent: None,
                worker_stage: Some("worker".to_string()),
                worker_query: None,
                merge_stage: None,
                max_workers: 2,
                on_worker_failure: leviath_core::blueprint::WorkerFailurePolicy::Continue,
                split_prompt: "split".to_string(),
            },
        };
        let mut fan_tr = HashMap::new();
        fan_tr.insert(
            "recover".to_string(),
            TransitionEdge {
                target: "recover".to_string(),
                condition: TransitionCondition::Error,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        fan.transitions = Some(fan_tr);
        let mut worker = make_stage("worker");
        worker.allow_as_worker = true;
        let recover = make_stage("recover");
        let bp = make_blueprint(vec![fan, worker, recover]);

        // CannedProvider returns "canned response" (not a JSON array) → split fails.
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(CannedProvider));
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(bp.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, &bp, "task");
        let engine: leviath_runtime::EngineHandle =
            std::sync::Arc::new(tokio::sync::RwLock::new(engine));
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let mut reg = std::collections::HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: Arc::new(reg),
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
        // No workers spawned (split failed), and the error edge was taken.
        assert!(cb
            .transitions
            .iter()
            .any(|(f, t)| f == "parallel" && t == "recover"));
    }

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
    ) -> (
        leviath_runtime::EngineHandle,
        AgentPool,
        bevy_ecs::prelude::Entity,
    ) {
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic".to_string(), Arc::new(CannedProvider));
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        let engine = std::sync::Arc::new(tokio::sync::RwLock::new(engine));
        (engine, pool, entity)
    }

    fn noop_exec(_calls: Vec<leviath_providers::ToolCall>) -> ToolResultsFuture<'static> {
        Box::pin(std::future::ready(vec![]))
    }

    // ─── Tests ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn single_stage_fires_enter_result_complete() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
    async fn required_region_gate_reruns_stage_then_proceeds() {
        // Install the always-on tracing subscriber so the gate's warn! (hit on
        // the proceed-after-cap path) has its macro-argument lines evaluated —
        // otherwise llvm-cov marks them uncovered on CI.
        crate::test_support::with_tracing(|| {});
        // A stage that can write context but never populates a `required` region
        // is re-run up to the cap (mock never fills it), then the run proceeds
        // with a warning (not a hard fail).
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("task".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new("plan".to_string(), RegionKind::Pinned, 4000)
                    .with_required(true, None),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    10000,
                ),
            ],
            16000,
        );
        let mut stage = make_stage("main");
        stage.available_tools = vec!["context_write".to_string()];
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);

        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        // 1 initial run + DEFAULT_REQUIRED_REENTRY_CAP re-runs.
        assert_eq!(cb.run_autonomous_calls, 1 + DEFAULT_REQUIRED_REENTRY_CAP);
        // Still proceeds to completion (warn-and-proceed, not hard-fail).
        assert_eq!(cb.completed_at, Some(0));
    }

    #[tokio::test]
    async fn linear_multi_stage_fires_transitions() {
        let bp = make_blueprint(vec![
            make_stage("plan"),
            make_stage("code"),
            make_stage("review"),
        ]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.abort_on_provider_missing = true; // provider_missing test: abort run

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        // Provider won't be registered, so provider_missing fires first.
        // Let it continue by not aborting.
        cb.abort_on_provider_missing = false;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.run_autonomous_should_error = true;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.run_autonomous_should_error = true;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                5000,
            )],
            5000,
        ));
        stage
            .config
            .insert("system_prompt".to_string(), serde_json::json!("Be terse."));
        stage.available_tools = vec!["read_file".to_string()];

        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
    async fn system_prompt_injected_into_pinned_region() {
        // When a Pinned region exists, system_prompt should go there instead
        // of the conversation SlidingWindow region.
        let mut stage = make_stage("main");
        stage.context_layout = Some(ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 10,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    5000,
                ),
            ],
            7000,
        ));
        stage
            .config
            .insert("system_prompt".to_string(), serde_json::json!("Be terse."));

        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        // Seed a benign conversation entry so the negative-assertion predicate
        // below actually iterates (an empty region never runs the closure).
        {
            let mut __wg = ctx.engine.write().await;
            let mut __w = __wg
                .world_mut()
                .get_mut::<ContextWindow>(ctx.entity)
                .unwrap();
            let _ = __w.add_to_region("conversation", "user message".to_string(), 3);
        }

        // Verify system_prompt landed in the "system" (Pinned) region
        let __g = ctx.engine.read().await;
        let window = __g.world().get::<ContextWindow>(ctx.entity).unwrap();
        let system_region = window.get_region("system").unwrap();
        let has_stage_instruction = system_region
            .content
            .iter()
            .any(|e| e.content.starts_with("[Stage instructions:"));
        assert!(
            has_stage_instruction,
            "system_prompt should be in pinned region"
        );

        // Verify conversation region does NOT have it
        let conv_region = window.get_region("conversation").unwrap();
        let conv_has_stage = conv_region
            .content
            .iter()
            .any(|e| e.content.starts_with("[Stage instructions:"));
        assert!(
            !conv_has_stage,
            "system_prompt should NOT be in conversation region"
        );
    }

    #[tokio::test]
    async fn system_prompt_injection_errors_when_target_region_missing() {
        // Stage layout has no Pinned region and no "conversation" region, so the
        // target-region fallback ("conversation") matches nothing: the
        // `find(...)` None-arm runs and inject_stage_system_prompt's `?`
        // propagates the add_to_region error, failing the run.
        let mut stage = make_stage("main");
        stage.context_layout = Some(ContextLayout::new(
            vec![RegionDefinition::new(
                "notes".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                5000,
            )],
            5000,
        ));
        stage
            .config
            .insert("system_prompt".to_string(), serde_json::json!("Be terse."));

        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
        };

        let res = run_stage_loop(
            &mut ctx,
            &mut cb,
            "agent-1",
            &mut MockIO::new(),
            &mut noop_exec,
        )
        .await;
        assert!(res.is_err(), "missing target region must fail the run");
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let stage_idx = Arc::new(Mutex::new(99usize));

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: stage_idx.clone(),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        let stage_name = Arc::new(Mutex::new(String::new()));

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: stage_name.clone(),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
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
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: Some("my-custom-model".to_string()),
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        assert!(cb
            .start_message_reader(&*engine.read().await, "agent-1", false)
            .is_none());
        assert!(cb.get_run_context().is_none());
        assert!(cb
            .on_stage_error("main", 0, &anyhow::anyhow!("e"), false)
            .await
            .is_none());
        cb.on_transition("a", "b", 0).await;
        cb.on_complete(0).await;
        cb.on_post_stage(&*engine.read().await, entity, "main")
            .await;
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
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
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        assert!(cb
            .start_message_reader(&*engine.read().await, "agent-1", false)
            .is_none());
        assert!(cb.get_run_context().is_none());
        assert_eq!(
            cb.on_stage_error("a", 0, &anyhow::anyhow!("e"), true).await,
            Some(StageResult::Error)
        );
        cb.on_transition("a", "b", 0).await;
        cb.on_complete(0).await;
        cb.on_post_stage(&*engine.read().await, entity, "a").await;
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
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
            // The stage posts its interaction request before awaiting the
            // response, so the request is present once this task is scheduled.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let req = crate::interaction::read_request(&responder_run_id)
                .expect("stage posts its interaction request before awaiting a response");
            let mut resp = crate::interaction::InteractionResponse::choice("", 1);
            resp.request_id = req.id.clone();
            crate::interaction::write_response(&responder_run_id, &resp).unwrap();
        });

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        // abort_on_provider_missing defaults to false → execution reaches run_interactive_stage

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
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
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;

        let mut cb = MockCallbacks::new();
        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;

        let mut cb = MockCallbacks::new();
        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: Some("anthropic/gpt-custom".to_string()),
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: Some(("anthropic".to_string(), "claude-haiku".to_string())),
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: Some(("anthropic".to_string(), "claude-haiku".to_string())),
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: Some(("also_nonexistent".to_string(), "model-x".to_string())),
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: Some("my-override-model".to_string()),
            user_default_model: Some(("anthropic".to_string(), "claude-haiku".to_string())),
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: Some("my-override-model".to_string()),
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        let __g = ctx.engine.read().await;
        let cfg = __g
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
            leviath_core::McpToolOverride {
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

    #[test]
    fn build_stage_taint_gate_mcp_override_all_none_leaves_defaults() {
        // An override with every field None exercises the None arms of each
        // `if let` (sensitivity / direction / clearance), leaving the tool's
        // classification unchanged from its default.
        let mut policy = PolicyConfig::default();
        policy.mcp_overrides.insert(
            "srv.plain".to_string(),
            leviath_core::McpToolOverride {
                sensitivity: None,
                direction: None,
                clearance: None,
            },
        );
        let gate =
            build_stage_taint_gate(true, None, None, &policy).expect("global-on builds a gate");
        let cls = gate.tool_classification("srv.plain");
        let def = ToolClassification::default();
        assert_eq!(cls.sensitivity, def.sensitivity);
        assert_eq!(cls.clearance, def.clearance);
    }

    #[tokio::test]
    async fn run_stage_loop_configures_taint_when_global_enabled() {
        // With the global taint switch on, the per-stage taint-config path in
        // run_stage_loop must build+configure the gate and enable window taint
        // tracking for the stage (then run to completion).
        let mut stage = make_stage("main");
        stage.max_iterations = Some(1);
        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.taint_global = true;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

    #[tokio::test]
    async fn run_stage_loop_persists_taint_audit_events() {
        // A stage that leaves taint-audit events behind must hand them to
        // on_taint_audit (the `!taint_audit.is_empty()` branch).
        let mut stage = make_stage("main");
        stage.max_iterations = Some(1);
        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();
        cb.seed_taint_audit = true;

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        assert_eq!(cb.taint_audits, vec![0]);
    }

    // ─── Tool result routing tests ─────────────────────────────────────────

    #[tokio::test]
    async fn tool_result_routing_inserted_when_stage_has_some() {
        use leviath_core::blueprint::ToolResultRouting;

        let mut stage = make_stage("main");
        stage.tool_result_routing = Some(ToolResultRouting {
            default_region: "my_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: false,
            max_result_tokens: Some(1024),
        });
        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        // The entity should have the ToolResultRoutingComponent with the
        // configuration we specified on the stage.
        let __g = ctx.engine.read().await;
        let comp = __g
            .world()
            .get::<leviath_runtime::ToolResultRoutingComponent>(ctx.entity)
            .expect("ToolResultRoutingComponent should be present");
        assert_eq!(comp.routing.default_region, "my_results");
        assert!(!comp.routing.persist);
        assert_eq!(comp.routing.max_result_tokens, Some(1024));
    }

    #[tokio::test]
    async fn tool_result_routing_removed_when_stage_has_none() {
        use leviath_core::blueprint::ToolResultRouting;

        let stage = make_stage("main"); // tool_result_routing defaults to None
        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        // Pre-insert the component so we can verify it gets removed.
        engine.write().await.world_mut().entity_mut(entity).insert(
            leviath_runtime::ToolResultRoutingComponent {
                routing: ToolResultRouting::default(),
            },
        );

        // Sanity: confirm it's there before the loop.
        assert!(engine
            .read()
            .await
            .world()
            .get::<leviath_runtime::ToolResultRoutingComponent>(entity)
            .is_some());

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        // After running a stage with tool_result_routing = None, the
        // component must have been removed.
        assert!(ctx
            .engine
            .read()
            .await
            .world()
            .get::<leviath_runtime::ToolResultRoutingComponent>(ctx.entity)
            .is_none());
    }

    #[tokio::test]
    async fn tool_result_routing_updated_across_stage_transitions() {
        use leviath_core::blueprint::ToolResultRouting;

        // Stage 1: has routing config
        let mut stage_a = make_stage("with_routing");
        stage_a.tool_result_routing = Some(ToolResultRouting {
            default_region: "stage_a_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: true,
            max_result_tokens: None,
        });

        // Stage 2: no routing config (should remove the component)
        let stage_b = make_stage("without_routing");

        // Stage 3: different routing config (should re-insert)
        let mut stage_c = make_stage("with_different_routing");
        stage_c.tool_result_routing = Some(ToolResultRouting {
            default_region: "stage_c_results".to_string(),
            tool_overrides: {
                let mut m = HashMap::new();
                m.insert("special_tool".to_string(), "special_region".to_string());
                m
            },
            persist: false,
            max_result_tokens: Some(512),
        });

        let bp = make_blueprint(vec![stage_a, stage_b, stage_c]);
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        // All 3 stages should have executed.
        assert_eq!(cb.stage_entries.len(), 3);

        // After the final stage (stage_c with routing), the entity should
        // have the component with stage_c's configuration.
        let __g = ctx.engine.read().await;
        let comp = __g
            .world()
            .get::<leviath_runtime::ToolResultRoutingComponent>(ctx.entity)
            .expect("ToolResultRoutingComponent should be present after stage_c");
        assert_eq!(comp.routing.default_region, "stage_c_results");
        assert!(!comp.routing.persist);
        assert_eq!(comp.routing.max_result_tokens, Some(512));
        assert_eq!(
            comp.routing
                .tool_overrides
                .get("special_tool")
                .map(|s| s.as_str()),
            Some("special_region")
        );
    }

    // ─── file-tracking tool-description patching ────────────────────────────

    #[tokio::test]
    async fn file_tracking_patches_file_tool_descriptions() {
        // With `file_tracking` enabled and the four file tools available, the
        // stage loop rewrites their descriptions to reference the tracked
        // system-prompt region (the description-patching block). This exercises
        // all four match arms (read_file / read_files / write_file / edit_file).
        let mut stage = make_stage("main");
        stage.available_tools = vec![
            "read_file".to_string(),
            "read_files".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
        ];
        let mut bp = make_blueprint(vec![stage]);
        bp.file_tracking = Some(leviath_core::blueprint::FileTrackingConfig {
            region: "workspace_files".to_string(),
            track_reads: true,
            track_writes: true,
            max_file_tokens: None,
        });
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        // The stage still completes normally after the tools are patched.
        assert_eq!(cb.completed_at, Some(0));
    }

    #[tokio::test]
    async fn file_tracking_reads_only_leaves_write_tools_untouched() {
        // track_writes=false must skip the write_file/edit_file arms while
        // still patching read_file/read_files (track_reads=true).
        let mut stage = make_stage("main");
        stage.available_tools = vec![
            "read_file".to_string(),
            "read_files".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
        ];
        let mut bp = make_blueprint(vec![stage]);
        bp.file_tracking = Some(leviath_core::blueprint::FileTrackingConfig {
            region: "reads_region".to_string(),
            track_reads: true,
            track_writes: false,
            max_file_tokens: None,
        });
        let (engine, mut pool, entity) = make_engine_and_entity(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        assert_eq!(cb.completed_at, Some(0));
    }

    // ─── multi-model priority fallback + tracing field evaluation ───────────

    #[tokio::test]
    async fn multi_model_selects_lower_priority_available_provider() {
        // models[0] is an unregistered provider, so the model-resolution loop
        // skips it and selects the registered `anthropic` at index 1 (the
        // "lower-priority model chosen" path).
        let mut stage = make_stage("main");
        stage.model.models.insert(
            0,
            ModelEntry::new("nonexistent".to_string(), "x".to_string()),
        );
        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;
        let mut cb = MockCallbacks::new();

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        // The registered lower-priority provider was chosen.
        assert_eq!(cb.resolved_models[0].0, "anthropic");
        assert_eq!(cb.completed_at, Some(0));
    }

    // ─── interactive-points abort cancels a live message reader ─────────────

    #[tokio::test]
    async fn interactive_points_abort_aborts_active_stdin_reader() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "interactive_points_abort_aborts_active_stdin_reader",
        );
        use crate::runstate::{self, RunMeta};
        use leviath_core::blueprint::InteractionPoint;

        // accepts_messages = true makes start_message_reader return Some, so the
        // abort path's `handle.abort()` (stdin-reader cleanup) is exercised.
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
        stage.accepts_messages = true;
        let bp = make_blueprint(vec![stage]);
        let (engine, mut pool, entity) = make_engine_and_entity_with_provider(&bp);
        let tool_registry = make_tool_registry().await;

        let run_id = "exec-abort-reader-run".to_string();
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

        let responder_run_id = run_id.clone();
        let responder = tokio::spawn(async move {
            // The stage posts its interaction request before awaiting the
            // response, so the request is present once this task is scheduled.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let req = crate::interaction::read_request(&responder_run_id)
                .expect("stage posts its interaction request before awaiting a response");
            let mut resp = crate::interaction::InteractionResponse::choice("", 1);
            resp.request_id = req.id.clone();
            crate::interaction::write_response(&responder_run_id, &resp).unwrap();
        });

        let mut ctx = StageContext {
            blueprint: &bp,
            engine: engine.clone(),
            entity,
            pool: &mut pool,
            tool_source: tool_registry.as_ref(),
            current_stage_name: Arc::new(Mutex::new(String::new())),
            current_stage_perms: Arc::new(Mutex::new(HashMap::new())),
            current_stage_idx: Arc::new(Mutex::new(0)),
            model_override: None,
            user_default_model: None,
            compaction_ref: None,
            agent_registry: std::sync::Arc::new(std::collections::HashMap::new()),
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

        assert_eq!(cb.cancelled_at, Some(0));
        assert_eq!(cb.completed_at, None);
        assert!(cb.transitions.is_empty());

        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }
}
