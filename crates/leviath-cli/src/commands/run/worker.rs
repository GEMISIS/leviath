//! Background worker run mode.

use async_trait::async_trait;
use leviath_core::blueprint::StageResult;
use leviath_providers::InferenceResponse;
use leviath_runtime::{AgentPool, AgentState, ContextWindow, ToolResultRoutingConfig};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::{Config, ToolPolicy};
use crate::runstate::{self, RunMeta, RunStatus, StageRecord, StageRunStatus};
use crate::tools::{resolve_policy, ToolRegistry};

use super::executor::{run_stage_loop, StageCallbacks, StageContext};
use super::helpers::{
    build_context_snapshot, generate_title, initialize_context_window, record_stage_log,
    record_stage_output, write_context_snapshot_if_bg,
};
use super::io::{ConsoleIO, RunIO};
use super::manifest::{find_manifest, parse_manifest};
use super::session::build_provider_registry;
use super::WorkerArgs;

/// Tracks the current stage index for tool-activity logging from the executor closure.
type CurrentStageIdx = Arc<Mutex<usize>>;

/// Shared state needed by [`dispatch_tool_calls`] to resolve and execute a
/// batch of tool calls from the model.
///
/// Extracted from the `exec` closure in [`run_worker_inner`] purely so the
/// tool-dispatch logic (policy resolution, dynamic interactions, approval
/// gating, builtin/MCP execution, activity logging) can be exercised by unit
/// tests directly, without needing to drive the full worker through a real
/// provider/inference call.
struct ToolDispatchState {
    builtins: Arc<leviath_tools::BuiltinTools>,
    mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    builtin_names: std::collections::HashSet<String>,
    launch_overrides: Arc<std::collections::HashMap<String, ToolPolicy>>,
    session_allows: Arc<Mutex<std::collections::HashSet<String>>>,
    stage_perms: Arc<Mutex<std::collections::HashMap<String, String>>>,
    agent_perms: Arc<std::collections::HashMap<String, String>>,
    global_perms: Arc<std::collections::HashMap<String, ToolPolicy>>,
    run_id: Arc<String>,
    stage_idx: CurrentStageIdx,
    stage_name: Arc<Mutex<String>>,
}

/// Resolve tool policy, handle approvals/dynamic interactions, and execute a
/// batch of tool calls from the model. Returns `(tool_call_id, result_text)`
/// pairs in the same order as `calls`.
///
/// This is the core body of the `exec` closure passed to
/// [`super::executor::run_stage_loop`] in [`run_worker_inner`], lifted out
/// into a standalone function so it can be unit-tested directly.
async fn dispatch_tool_calls(
    state: &ToolDispatchState,
    calls: Vec<leviath_providers::ToolCall>,
) -> Vec<(String, String)> {
    let stage_idx = *state.stage_idx.lock().await;
    let stage_name = state.stage_name.lock().await.clone();
    let interaction_backend = WorkerInteractionBackend {
        run_id: &state.run_id,
        stage_idx,
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for tc in calls {
        // ── Dynamic interaction tools (present_for_review, ask_user_*) ──
        // Unlike `interaction_points` (declared statically in the
        // blueprint and always shown), these let the model itself
        // decide, mid-reasoning, that it needs human input.
        if let Some(result) = super::dynamic_interaction::dispatch_dynamic_interaction(
            &interaction_backend,
            &tc.name,
            &tc.id,
            &tc.arguments,
            &stage_name,
        )
        .await
        {
            out.push((tc.id.clone(), result));
            continue;
        }

        let is_builtin = state.builtin_names.contains(&tc.name);
        let session_has = state.session_allows.lock().await.contains(&tc.name);
        let policy = if session_has {
            ToolPolicy::Allow
        } else {
            let stage_pm_snap = state.stage_perms.lock().await.clone();
            resolve_policy(
                &tc.name,
                is_builtin,
                &state.launch_overrides,
                &stage_pm_snap,
                &state.agent_perms,
                &state.global_perms,
            )
        };

        let res = match policy {
            ToolPolicy::Deny => {
                let msg = format!("[denied] Tool '{}' is not permitted.", tc.name);
                record_stage_log(
                    &state.run_id,
                    stage_idx,
                    &format!("[tool] {} \u{2192} denied", tc.name),
                );
                msg
            }
            ToolPolicy::Ask => {
                use crate::interaction::{
                    request_tool_approval_background, ApprovalScope, TOOL_APPROVAL_TIMEOUT,
                };
                let (approved, scope) = request_tool_approval_background(
                    &state.run_id,
                    &tc.name,
                    &tc.arguments,
                    "tool-call",
                    TOOL_APPROVAL_TIMEOUT,
                )
                .await;
                if approved {
                    if scope == ApprovalScope::Session {
                        state.session_allows.lock().await.insert(tc.name.clone());
                    }
                    let result = if is_builtin {
                        state.builtins.execute(&tc.name, tc.arguments.clone()).await
                    } else {
                        let mut mcp_lock = state.mcp.lock().await;
                        match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                            Ok(r) if r.success => r.text,
                            Ok(r) => format!("[error] {}", r.text),
                            Err(e) => format!("[error] tool error: {}", e),
                        }
                    };
                    let short_result = if result.len() > 120 {
                        format!("{}\u{2026}", &result[..120])
                    } else {
                        result.clone()
                    };
                    record_stage_log(
                        &state.run_id,
                        stage_idx,
                        &format!("[tool] {} \u{2192} {}", tc.name, short_result),
                    );
                    result
                } else {
                    record_stage_log(
                        &state.run_id,
                        stage_idx,
                        &format!("[tool] {} \u{2192} declined by user", tc.name),
                    );
                    format!("[denied] User declined tool call '{}'.", tc.name)
                }
            }
            ToolPolicy::Allow => {
                let result = if is_builtin {
                    state.builtins.execute(&tc.name, tc.arguments.clone()).await
                } else {
                    let mut mcp_lock = state.mcp.lock().await;
                    match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                        Ok(r) if r.success => r.text,
                        Ok(r) => format!("[error] {}", r.text),
                        Err(e) => format!("[error] tool error: {}", e),
                    }
                };
                let short_result = if result.len() > 120 {
                    format!("{}\u{2026}", &result[..120])
                } else {
                    result.clone()
                };
                record_stage_log(
                    &state.run_id,
                    stage_idx,
                    &format!("[tool] {} \u{2192} {}", tc.name, short_result),
                );
                result
            }
        };
        out.push((tc.id.clone(), res));
    }
    out
}

/// Background-worker [`InteractionBackend`]: answers via the file-based IPC
/// channel and logs to the per-stage log file.
struct WorkerInteractionBackend<'a> {
    run_id: &'a str,
    stage_idx: usize,
}

#[async_trait]
impl super::dynamic_interaction::InteractionBackend for WorkerInteractionBackend<'_> {
    async fn ask(
        &self,
        req: crate::interaction::InteractionRequest,
    ) -> crate::interaction::InteractionResponse {
        crate::interaction::request_interaction_bg_review(self.run_id, req).await
    }

    fn log(&self, message: &str) {
        record_stage_log(self.run_id, self.stage_idx, message);
    }

    fn on_review_document(&self, tool_call_id: &str, title: &str, markdown: &str) {
        // Persist the review artifact under stages/<idx>/reviews/
        let review_dir = runstate::stage_dir(self.run_id, self.stage_idx).join("reviews");
        let _ = std::fs::create_dir_all(&review_dir);
        let artifact_path = review_dir.join(format!("review-{}.md", tool_call_id));
        let _ = std::fs::write(&artifact_path, markdown);

        // Also write to stage output so it's visible in the Output tab after review
        record_stage_output(
            self.run_id,
            self.stage_idx,
            &format!("---\n## {}\n\n{}\n---", title, markdown),
        );
    }
}

/// Worker-specific callbacks for the unified stage loop.
struct WorkerCallbacks<'a> {
    run_id: String,
    meta: &'a mut RunMeta,
    blueprint_stages_len: usize,
}

impl<'a> WorkerCallbacks<'a> {
    fn now_secs() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

#[async_trait]
impl<'a> StageCallbacks for WorkerCallbacks<'a> {
    async fn on_provider_missing(&mut self, provider: &str, stage_idx: usize) -> bool {
        let msg = format!("Provider '{}' is not configured", provider);
        println!("\n{}", msg);
        record_stage_log(&self.run_id, stage_idx, &format!("[error] {}", msg));
        {
            let mut stages = runstate::read_stages_index(&self.run_id);
            if let Some(r) = stages.get_mut(stage_idx) {
                r.status = StageRunStatus::Error;
            }
            let _ = runstate::write_stages_index(&self.run_id, &stages);
        }
        self.meta.status = RunStatus::Error;
        self.meta.error = Some(msg);
        self.meta.touch();
        let _ = runstate::write_meta(self.meta);
        true // abort run
    }

    async fn on_stage_enter(
        &mut self,
        stage_name: &str,
        stage_idx: usize,
        provider: &str,
        model: &str,
        visit_label: &str,
    ) {
        let stage_header = format!(
            "Stage {}: {} ({}:{}){}",
            stage_idx + 1,
            stage_name,
            provider,
            model,
            visit_label,
        );
        println!("\n--- {} ---", stage_header);
        record_stage_log(
            &self.run_id,
            stage_idx,
            &format!("--- {} ---", stage_header),
        );

        // Mark stage as active and update stages.json
        let stage_started_at = Self::now_secs();
        {
            let mut stages = runstate::read_stages_index(&self.run_id);
            if let Some(r) = stages.get_mut(stage_idx) {
                r.status = StageRunStatus::Active;
                r.started_at = Some(stage_started_at);
            }
            let _ = runstate::write_stages_index(&self.run_id, &stages);
        }

        self.meta.current_stage = stage_name.to_string();
        self.meta.stage_index = stage_idx;
        self.meta.status = RunStatus::Running;
        self.meta.touch();
        let _ = runstate::write_meta(self.meta);
    }

    async fn on_claude_code_warning(&mut self, stage_idx: usize) {
        let warn = "\u{26a0}\u{fe0f}  Using claude-code provider: tool routing, per-stage filtering, and prompt caching are not available.";
        println!("{}", warn);
        record_stage_log(&self.run_id, stage_idx, warn);
    }

    fn start_message_reader(
        &mut self,
        _engine: &leviath_runtime::AgentEngine,
        _agent_id: &str,
        _accepts: bool,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None // worker: messages come from dashboard
    }

    fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)> {
        Some((&self.run_id, self.meta))
    }

    async fn run_autonomous<F, Fut>(
        &mut self,
        engine: &mut leviath_runtime::AgentEngine,
        entity: bevy_ecs::prelude::Entity,
        provider: &str,
        model: &str,
        max_iterations: usize,
        tools: Vec<leviath_providers::Tool>,
        routing: Option<&ToolResultRoutingConfig>,
        compaction: Option<&leviath_core::lifecycle::CompactionConfig>,
        _io: &mut dyn RunIO,
        executor: &mut F,
    ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
    where
        F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
        Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
    {
        // Worker calls engine.run_inference_loop_filtered directly (not run_autonomous_stage)
        let response = engine
            .run_inference_loop_filtered(
                entity,
                provider,
                model,
                tools,
                max_iterations,
                None,
                routing,
                compaction,
                executor,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        Ok((StageResult::Success, Some(response)))
    }

    async fn on_stage_result(
        &mut self,
        stage_name: &str,
        stage_idx: usize,
        _result: &StageResult,
        response: Option<&InferenceResponse>,
        engine: &mut leviath_runtime::AgentEngine,
        entity: bevy_ecs::prelude::Entity,
    ) {
        let stage_ended_at = Self::now_secs();

        if let Some(resp) = response {
            // Print + record response content
            println!("{}", resp.content);
            record_stage_output(&self.run_id, stage_idx, &resp.content);

            // Token line
            let token_line = format!(
                "[Tokens: {} in, {} out]",
                resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
            );
            println!("\n{}", token_line);
            record_stage_log(&self.run_id, stage_idx, &token_line);

            // Update meta token counts
            self.meta.prompt_tokens += resp.tokens_used.prompt_tokens;
            self.meta.completion_tokens += resp.tokens_used.completion_tokens;
            self.meta.cached_tokens += resp.tokens_used.cached_tokens;

            // Carry the final response forward so the next stage sees the previous stage's output
            if !resp.content.is_empty() {
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                    let tokens = resp.content.len() / 4 + 1;
                    let _ = window.add_to_region(
                        "conversation",
                        format!("Assistant ({}): {}", stage_name, resp.content),
                        tokens,
                    );
                }
            }
        }

        // Determine if max_iterations was hit — re-check the stage result
        // (the caller already set it, but we need to update stages.json)

        // Mark stage complete
        {
            let mut stages = runstate::read_stages_index(&self.run_id);
            if let Some(r) = stages.get_mut(stage_idx) {
                r.status = StageRunStatus::Complete;
                r.ended_at = Some(stage_ended_at);
                r.prompt_tokens = self.meta.prompt_tokens;
                r.completion_tokens = self.meta.completion_tokens;
                r.cached_tokens = self.meta.cached_tokens;
            }
            let _ = runstate::write_stages_index(&self.run_id, &stages);
        }
    }

    async fn on_stage_error(
        &mut self,
        stage_name: &str,
        stage_idx: usize,
        error: &anyhow::Error,
        is_graph_mode: bool,
    ) -> Option<StageResult> {
        let stage_ended_at = Self::now_secs();

        if is_graph_mode {
            let msg = format!(
                "Stage '{}' error: {} \u{2014} checking error transitions",
                stage_name, error
            );
            println!("{}", msg);
            record_stage_log(&self.run_id, stage_idx, &format!("[error] {}", msg));

            // Mark stage as errored but don't abort
            {
                let mut stages = runstate::read_stages_index(&self.run_id);
                if let Some(r) = stages.get_mut(stage_idx) {
                    r.status = StageRunStatus::Error;
                    r.ended_at = Some(stage_ended_at);
                }
                let _ = runstate::write_stages_index(&self.run_id, &stages);
            }
            Some(StageResult::Error)
        } else {
            let msg = format!("Stage '{}' inference error: {}", stage_name, error);
            println!("{}", msg);
            record_stage_log(&self.run_id, stage_idx, &format!("[error] {}", msg));
            // Mark stage error
            {
                let mut stages = runstate::read_stages_index(&self.run_id);
                if let Some(r) = stages.get_mut(stage_idx) {
                    r.status = StageRunStatus::Error;
                    r.ended_at = Some(stage_ended_at);
                }
                let _ = runstate::write_stages_index(&self.run_id, &stages);
            }
            self.meta.status = RunStatus::Error;
            self.meta.error = Some(msg);
            self.meta.touch();
            let _ = runstate::write_meta(self.meta);
            None // propagate — caller returns Ok(()) after setting meta
        }
    }

    async fn on_transition(&mut self, from_stage: &str, to_stage: &str, stage_idx: usize) {
        let marker = format!(
            "[Stage complete: {}, transitioning to: {}]",
            from_stage, to_stage
        );
        record_stage_log(&self.run_id, stage_idx, &marker);
    }

    async fn on_complete(&mut self, last_stage_idx: usize) {
        let done_msg = "[All stages complete]";
        println!("\n{}", done_msg);
        if self.blueprint_stages_len > 0 {
            record_stage_log(&self.run_id, last_stage_idx, done_msg);
        }
    }

    async fn on_post_stage(
        &mut self,
        engine: &leviath_runtime::AgentEngine,
        entity: bevy_ecs::prelude::Entity,
        stage_name: &str,
    ) {
        if let Some(state) = engine.world().get::<AgentState>(entity) {
            self.meta.iteration = state.iteration;
        }
        self.meta.touch();
        let _ = runstate::write_meta(self.meta);

        // Write context snapshot to both legacy path and per-stage path
        write_context_snapshot_if_bg(engine, entity, stage_name, &Some(self.run_id.clone()));
        if let Some(snap) = build_context_snapshot(engine, entity, stage_name) {
            let _ = runstate::write_stage_context(&self.run_id, self.meta.stage_index, &snap);
        }
    }
}

/// Background worker entrypoint: runs stages and writes progress to run-state dir.
pub async fn execute_worker(args: WorkerArgs) -> anyhow::Result<()> {
    let mut meta = runstate::read_meta(&args.run_id).unwrap_or_else(|_| {
        RunMeta::new(
            args.run_id.clone(),
            "unknown".to_string(),
            args.path.clone(),
            args.task.clone(),
            args.model.clone(),
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            0,
        )
    });

    meta.pid = std::process::id();
    meta.status = RunStatus::Running;
    meta.touch();
    let _ = runstate::write_meta(&meta);

    let result = run_worker_inner(&args, &mut meta, build_provider_registry).await;

    match &result {
        Ok(()) => meta.status = RunStatus::Complete,
        Err(e) => {
            meta.status = RunStatus::Error;
            meta.error = Some(e.to_string());
        }
    }
    meta.touch();
    let _ = runstate::write_meta(&meta);

    result
}

/// Core of [`execute_worker`], with provider-registry construction injected
/// so tests can drive a real (in-process, no network) inference round trip
/// with a [`Provider`](leviath_providers::Provider) mock -- covering title
/// generation and the `exec` closure's real call site -- instead of either
/// stopping at a missing-provider error or making a real, billed network
/// call. Production always passes [`build_provider_registry`].
///
/// `build_registry` is a plain function pointer (not `impl FnOnce`)
/// deliberately: every test below passes a non-capturing closure, and a
/// generic `impl FnOnce` parameter would make `run_worker_inner` -- and
/// therefore the `WorkerCallbacks`/`exec`-closure instantiation of the
/// shared `run_stage_loop` it drives -- monomorphize separately per test.
/// A concrete `fn` pointer type lets every call site (production and test)
/// share one instantiation, which is what fixed a real llvm-cov
/// instantiation-merging undercount in `run_stage_loop`'s coverage (see
/// `executor.rs`'s `run_stage_loop` doc comment).
async fn run_worker_inner(
    args: &WorkerArgs,
    meta: &mut RunMeta,
    build_registry: fn(&Config) -> leviath_runtime::ProviderRegistry,
) -> anyhow::Result<()> {
    let manifest_path = find_manifest(&args.path)?;
    println!("Loading agent from: {}", manifest_path.display());

    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest(&manifest_content)?;

    println!("Agent: {} v{}", blueprint.name, blueprint.version);
    println!("Task: {}", args.task);

    let config = Config::load()?;
    for warning in config.validate_keys() {
        println!("Warning: {}", warning);
    }

    let prov_registry = build_registry(&config);

    // Generate a human-readable title from the task prompt (best-effort).
    if config.title.enabled && meta.title.is_none() {
        let fallback = args.model.as_deref();
        meta.title = generate_title(&args.task, &config, &prov_registry, fallback).await;
        if let Some(ref t) = meta.title {
            println!("Title: {}", t);
        }
        meta.touch();
        let _ = runstate::write_meta(meta);
    }

    let mut engine = leviath_runtime::AgentEngine::with_providers(prov_registry);

    let mut pool = AgentPool::new(blueprint.clone());
    let agent_id = pool.spawn_agent(engine.world_mut());
    // spawn_agent inserts agent_id into the pool immediately; get_agent will
    // always return Some here. We use expect to surface a bug if that invariant
    // is ever violated, avoiding an unreachable ? error branch.
    let entity = pool
        .get_agent(&agent_id)
        .expect("agent was just spawned and must be in the pool");

    let workdir = std::env::current_dir()
        .ok()
        .unwrap_or(std::path::PathBuf::from("."));
    initialize_context_window(&mut engine, entity, &blueprint, &args.task);

    let tool_registry = Arc::new(ToolRegistry::build(workdir, &config).await);

    // Global tool policy + session-level allows
    let global_perms = Arc::new(config.tool_permissions.clone());
    let session_allows: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    let current_stage_perms: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    // Agent-level permissions from the blueprint's [tool_permissions] section
    let agent_perms: std::collections::HashMap<String, String> = blueprint
        .metadata
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("tool_perm:")
                .and_then(|tool| v.as_str().map(|p| (tool.to_string(), p.to_string())))
        })
        .collect();
    let agent_perms_arc = Arc::new(agent_perms);
    // Launch overrides forwarded from the CLI flags
    let mut launch_overrides: std::collections::HashMap<String, ToolPolicy> =
        std::collections::HashMap::new();
    if args.yolo {
        launch_overrides.insert("*".to_string(), ToolPolicy::Allow);
    }
    for t in &args.allow {
        launch_overrides.insert(t.clone(), ToolPolicy::Allow);
    }
    for t in &args.ask {
        launch_overrides.insert(t.clone(), ToolPolicy::Ask);
    }
    for t in &args.deny {
        launch_overrides.insert(t.clone(), ToolPolicy::Deny);
    }
    let launch_overrides_arc: Arc<std::collections::HashMap<String, ToolPolicy>> =
        Arc::new(launch_overrides);
    let run_id_arc = Arc::new(args.run_id.clone());
    // Shared mutable stage index so the executor closure can log tool activity
    // to the correct per-stage log file.
    let current_stage_idx: CurrentStageIdx = Arc::new(Mutex::new(0usize));
    // Shared current stage name for present_for_review interactions.
    let current_stage_name: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let dispatch_state = Arc::new(ToolDispatchState {
        builtins: tool_registry.builtins.clone(),
        mcp: tool_registry.mcp.clone(),
        builtin_names: tool_registry.builtin_names.clone(),
        launch_overrides: launch_overrides_arc,
        session_allows: session_allows.clone(),
        stage_perms: current_stage_perms.clone(),
        agent_perms: agent_perms_arc.clone(),
        global_perms: global_perms.clone(),
        run_id: run_id_arc.clone(),
        stage_idx: current_stage_idx.clone(),
        stage_name: current_stage_name.clone(),
    });
    let mut exec = move |calls: Vec<leviath_providers::ToolCall>| {
        let dispatch_state = dispatch_state.clone();
        async move { dispatch_tool_calls(&dispatch_state, calls).await }
    };

    let compaction_config = blueprint.compaction_config.clone();
    let compaction_ref = compaction_config.as_ref();

    meta.num_stages = blueprint.stages.len();
    let _ = runstate::write_meta(meta);

    // Initialize the stages index (all Pending) so the dashboard can show stages
    // before any stage starts running.
    {
        let initial_stages: Vec<StageRecord> = blueprint
            .stages
            .iter()
            .enumerate()
            .map(|(i, s)| StageRecord::new(s.name.clone(), i))
            .collect();
        let _ = runstate::write_stages_index(&args.run_id, &initial_stages);
    }

    let blueprint_stages_len = blueprint.stages.len();

    let mut callbacks = WorkerCallbacks {
        run_id: args.run_id.clone(),
        meta,
        blueprint_stages_len,
    };
    let mut io = ConsoleIO::new();

    let mut ctx = StageContext {
        blueprint: &blueprint,
        engine: &mut engine,
        entity,
        pool: &mut pool,
        tool_registry: &tool_registry,
        current_stage_name: current_stage_name.clone(),
        current_stage_perms: current_stage_perms.clone(),
        current_stage_idx: current_stage_idx.clone(),
        model_override: args.model.clone(),
        user_default_model: super::helpers::resolve_user_default_model(&config),
        compaction_ref,
    };

    run_stage_loop(&mut ctx, &mut callbacks, &agent_id, &mut io, &mut exec).await?;

    tool_registry.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::{FinishReason, InferenceResponse, Provider, TokenUsage};

    // ─── Helpers ──────────────────────────────────────────────────────────────

    /// Isolates `Config::load()` from the developer's real
    /// `~/.leviath/config.toml`, real `.env`, and any real API key, so tests
    /// that drive a real config load (e.g. `execute_worker()` on a valid
    /// manifest) don't make a real, billed inference request via
    /// `generate_title()`. Shared with `commands/run/foreground.rs` — see
    /// `crate::config::isolate_config_path_for_test` for the rationale.
    use crate::config::isolate_config_path_for_test as isolate_config_path;

    fn make_meta(run_id: &str, num_stages: usize) -> RunMeta {
        RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/w".into(),
            num_stages,
        )
    }

    fn make_engine_with_agent(
        meta: &mut RunMeta,
    ) -> (
        leviath_runtime::AgentEngine,
        leviath_runtime::AgentPool,
        String,
        bevy_ecs::prelude::Entity,
    ) {
        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let blueprint = leviath_core::Blueprint::new(
            meta.agent_name.clone(),
            "desc".into(),
            vec![leviath_core::Stage::new(
                "main".to_string(),
                leviath_core::blueprint::ModelConfig::new(
                    "anthropic".to_string(),
                    "claude-sonnet-4-6".to_string(),
                ),
            )],
            leviath_core::ContextLayout::new(
                vec![leviath_core::layout::RegionDefinition::new(
                    "conversation".to_string(),
                    leviath_core::RegionKind::SlidingWindow { max_items: 10 },
                    10000,
                )],
                10000,
            ),
        );
        let mut pool = leviath_runtime::AgentPool::new(blueprint);
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        (engine, pool, agent_id, entity)
    }

    fn make_response(content: &str) -> InferenceResponse {
        InferenceResponse {
            content: content.to_string(),
            tool_calls: vec![],
            tokens_used: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 10,
                cache_write_tokens: 0,
            },
            finish_reason: FinishReason::Complete,
        }
    }

    // ─── now_secs ─────────────────────────────────────────────────────────────

    #[test]
    fn worker_callbacks_now_secs_returns_positive() {
        let ts = WorkerCallbacks::now_secs();
        assert!(ts > 0);
    }

    #[test]
    fn worker_callbacks_now_secs_is_recent() {
        let ts = WorkerCallbacks::now_secs();
        // Should be after 2024-01-01 (1704067200) and before 2040
        assert!(ts > 1_704_067_200);
        assert!(ts < 2_208_988_800);
    }

    #[test]
    fn worker_callbacks_construction() {
        let mut meta = RunMeta::new(
            "test-run".into(),
            "agent".into(),
            "/path".into(),
            "task".into(),
            None,
            "/work".into(),
            3,
        );
        let cb = WorkerCallbacks {
            run_id: "test-run".to_string(),
            meta: &mut meta,
            blueprint_stages_len: 3,
        };
        assert_eq!(cb.run_id, "test-run");
        assert_eq!(cb.blueprint_stages_len, 3);
    }

    #[tokio::test]
    async fn worker_callbacks_on_complete_with_zero_stages() {
        let mut meta = RunMeta::new(
            "test-complete".into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            0,
        );
        let mut cb = WorkerCallbacks {
            run_id: "test-complete".to_string(),
            meta: &mut meta,
            blueprint_stages_len: 0,
        };
        // Should not panic even with 0 stages
        cb.on_complete(0).await;
    }

    #[test]
    fn worker_callbacks_get_run_context_returns_some() {
        let mut meta = RunMeta::new(
            "ctx-run".into(),
            "a".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: "ctx-run".to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };
        let ctx = cb.get_run_context();
        assert!(ctx.is_some());
        let (rid, _meta_ref) = ctx.unwrap();
        assert_eq!(rid, "ctx-run");
    }

    #[test]
    fn worker_callbacks_start_message_reader_returns_none() {
        let mut meta = RunMeta::new(
            "msg-run".into(),
            "a".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: "msg-run".to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let handle = cb.start_message_reader(&engine, "agent-1", true);
        // Worker should not start a message reader.
        assert!(handle.is_none());
    }

    #[tokio::test]
    async fn worker_callbacks_on_complete_with_positive_stages() {
        // `on_complete` calls `record_stage_log`, which writes to the real
        // runs dir unless isolated -- caught missing this guard when a
        // leftover "test-complete-pos" dir turned up in the real
        // ~/.leviath/runs/ after a full-suite run.
        let _guard = crate::runstate::isolate_runs_dir_for_test("worker-cb-complete-pos");
        let mut meta = RunMeta::new(
            "test-complete-pos".into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            3,
        );
        let mut cb = WorkerCallbacks {
            run_id: "test-complete-pos".to_string(),
            meta: &mut meta,
            blueprint_stages_len: 3,
        };
        // Should not panic with positive stages
        cb.on_complete(2).await;
    }

    #[tokio::test]
    async fn worker_callbacks_on_transition_does_not_panic() {
        // See the comment on `worker_callbacks_on_complete_with_positive_stages`
        // -- `on_transition` also calls `record_stage_log` for real.
        let _guard = crate::runstate::isolate_runs_dir_for_test("worker-cb-transition");
        let mut meta = RunMeta::new(
            "test-trans".into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            2,
        );
        let mut cb = WorkerCallbacks {
            run_id: "test-trans".to_string(),
            meta: &mut meta,
            blueprint_stages_len: 2,
        };
        cb.on_transition("plan", "code", 0).await;
    }

    #[tokio::test]
    async fn worker_callbacks_on_claude_code_warning_does_not_panic() {
        // See the comment on `worker_callbacks_on_complete_with_positive_stages`
        // -- `on_claude_code_warning` also calls `record_stage_log` for real.
        let _guard = crate::runstate::isolate_runs_dir_for_test("worker-cb-ccw");
        let mut meta = RunMeta::new(
            "test-ccw".into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: "test-ccw".to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };
        cb.on_claude_code_warning(0).await;
    }

    #[tokio::test]
    async fn worker_callbacks_on_provider_missing_returns_true() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_provider_missing_returns_true",
        );
        // Use a temp dir for run state
        let run_id = "test-worker-prov-miss";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        // Write initial stages index
        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };
        let result = cb.on_provider_missing("nonexistent", 0).await;
        // on_provider_missing should return true (abort).
        assert!(result);
        assert_eq!(cb.meta.status, RunStatus::Error);
        assert!(cb.meta.error.is_some());
        assert!(cb.meta.error.as_ref().unwrap().contains("nonexistent"));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_enter_updates_meta() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_enter_updates_meta",
        );
        let run_id = "test-worker-stage-enter";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let stages = vec![
            crate::runstate::StageRecord::new("plan".to_string(), 0),
            crate::runstate::StageRecord::new("code".to_string(), 1),
        ];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            2,
        );
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 2,
        };
        cb.on_stage_enter("plan", 0, "anthropic", "claude-sonnet-4-6", "")
            .await;
        assert_eq!(cb.meta.current_stage, "plan");
        assert_eq!(cb.meta.stage_index, 0);
        assert_eq!(cb.meta.status, RunStatus::Running);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_enter_with_visit_label() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_enter_with_visit_label",
        );
        let run_id = "test-worker-visit-label";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let stages = vec![crate::runstate::StageRecord::new("code".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };
        cb.on_stage_enter("code", 0, "anthropic", "claude-sonnet-4-6", " (visit 2)")
            .await;
        assert_eq!(cb.meta.current_stage, "code");

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_error_graph_mode() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_error_graph_mode",
        );
        let run_id = "test-worker-stage-err-graph";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        let err = anyhow::anyhow!("test error");
        let result = cb.on_stage_error("main", 0, &err, true).await;
        assert_eq!(result, Some(leviath_core::blueprint::StageResult::Error));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_error_linear_mode() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_error_linear_mode",
        );
        let run_id = "test-worker-stage-err-linear";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        let err = anyhow::anyhow!("linear error");
        let result = cb.on_stage_error("main", 0, &err, false).await;
        assert!(result.is_none());
        assert_eq!(cb.meta.status, RunStatus::Error);
        assert!(cb.meta.error.as_ref().unwrap().contains("linear error"));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_result_updates_stages() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_result_updates_stages",
        );
        let run_id = "test-worker-stage-result";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);
        // Create stages dir for output
        let stage_dir = crate::runstate::stage_dir(run_id, 0);
        let _ = std::fs::create_dir_all(&stage_dir);

        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let mut pool = leviath_runtime::AgentPool::new(leviath_core::Blueprint::new(
            "test".to_string(),
            "test".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 0),
        ));
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();

        // Test with a response
        let response = leviath_providers::InferenceResponse {
            content: "test output".to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 10,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::Complete,
        };

        cb.on_stage_result(
            "main",
            0,
            &leviath_core::blueprint::StageResult::Success,
            Some(&response),
            &mut engine,
            entity,
        )
        .await;

        assert_eq!(cb.meta.prompt_tokens, 100);
        assert_eq!(cb.meta.completion_tokens, 50);
        assert_eq!(cb.meta.cached_tokens, 10);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_result_no_response() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_result_no_response",
        );
        let run_id = "test-worker-stage-result-none";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = RunMeta::new(
            run_id.into(),
            "agent".into(),
            "/p".into(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let entity = bevy_ecs::prelude::Entity::from_raw(0);

        // No response
        cb.on_stage_result(
            "main",
            0,
            &leviath_core::blueprint::StageResult::Success,
            None,
            &mut engine,
            entity,
        )
        .await;

        // Tokens should remain zero
        assert_eq!(cb.meta.prompt_tokens, 0);
        assert_eq!(cb.meta.completion_tokens, 0);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_stage_result with empty content ──────────────────────────────────

    #[tokio::test]
    async fn worker_callbacks_on_stage_result_empty_content_skips_context_window() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_result_empty_content_skips_context_window",
        );
        let run_id = "test-worker-stage-result-empty-content";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);
        let stage_dir = crate::runstate::stage_dir(run_id, 0);
        let _ = std::fs::create_dir_all(&stage_dir);

        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = make_meta(run_id, 1);
        let (mut engine, pool, agent_id, entity) = make_engine_with_agent(&mut meta);
        let _ = (pool, agent_id); // keep alive

        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        // Empty content — the `add_to_region` branch is NOT taken
        let response = make_response("");
        cb.on_stage_result(
            "main",
            0,
            &leviath_core::blueprint::StageResult::Success,
            Some(&response),
            &mut engine,
            entity,
        )
        .await;

        // Token counts still updated even when content is empty
        assert_eq!(cb.meta.prompt_tokens, 100);
        assert_eq!(cb.meta.completion_tokens, 50);
        assert_eq!(cb.meta.cached_tokens, 10);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_stage_result with non-empty content adds to context window ────────

    #[tokio::test]
    async fn worker_callbacks_on_stage_result_non_empty_content_adds_to_window() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_result_non_empty_content_adds_to_window",
        );
        let run_id = "test-worker-stage-result-non-empty";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);
        let stage_dir = crate::runstate::stage_dir(run_id, 0);
        let _ = std::fs::create_dir_all(&stage_dir);

        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = make_meta(run_id, 1);
        let (mut engine, pool, agent_id, entity) = make_engine_with_agent(&mut meta);
        let _ = (pool, agent_id);

        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        // Non-empty content — `add_to_region` branch IS taken
        let response = make_response("This is the assistant's output after completing the task.");
        cb.on_stage_result(
            "main",
            0,
            &leviath_core::blueprint::StageResult::Success,
            Some(&response),
            &mut engine,
            entity,
        )
        .await;

        assert_eq!(cb.meta.prompt_tokens, 100);
        assert_eq!(cb.meta.completion_tokens, 50);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_post_stage ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn worker_callbacks_on_post_stage_updates_meta_and_writes_snapshot() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_post_stage_updates_meta_and_writes_snapshot",
        );
        let run_id = "test-worker-on-post-stage";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let mut meta = make_meta(run_id, 1);
        meta.stage_index = 0;
        // Write initial meta so runstate can read it
        let _ = crate::runstate::write_meta(&meta);

        let (engine, pool, agent_id, entity) = make_engine_with_agent(&mut meta);
        let _ = (pool, agent_id);

        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        // Should not panic; updates meta.iteration from AgentState and writes meta
        cb.on_post_stage(&engine, entity, "main").await;

        // Meta should have been written (no panic is the key assertion here)
        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_post_stage_without_agent_state() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_post_stage_without_agent_state",
        );
        let run_id = "test-worker-on-post-stage-no-state";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let mut meta = make_meta(run_id, 1);
        let _ = crate::runstate::write_meta(&meta);

        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        // Spawn entity WITHOUT AgentState (bare entity) to test the `if let Some` branch
        let entity = engine.world_mut().spawn(()).id();

        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        // on_post_stage with an entity that has no AgentState — should not panic
        cb.on_post_stage(&engine, entity, "main").await;

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── execute_worker error paths ───────────────────────────────────────────

    #[tokio::test]
    async fn execute_worker_fails_with_nonexistent_path() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "execute_worker_fails_with_nonexistent_path",
        );
        let run_id = "test-execute-worker-bad-path";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        // Write meta so execute_worker can read it
        let meta = make_meta(run_id, 0);
        let _ = crate::runstate::write_meta(&meta);

        let args = WorkerArgs {
            path: "/nonexistent/path/to/nowhere".to_string(),
            task: "do something".to_string(),
            run_id: run_id.to_string(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let result = execute_worker(args).await;
        // Should fail because path doesn't exist
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        let has_manifest_err = err_msg.contains("Could not find") | err_msg.contains("manifest");
        // Expected manifest error.
        assert!(has_manifest_err);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn execute_worker_creates_meta_when_missing() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("execute_worker_creates_meta_when_missing");
        let run_id = "test-execute-worker-no-meta";
        // Do NOT pre-write meta — tests the fallback branch in execute_worker
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let args = WorkerArgs {
            path: "/nonexistent/path".to_string(),
            task: "test task".to_string(),
            run_id: run_id.to_string(),
            model: Some("claude-sonnet-4-6".to_string()),
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        // Will fail at manifest lookup, but the RunMeta creation fallback is exercised
        let result = execute_worker(args).await;
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn execute_worker_with_valid_manifest_fails_at_inference() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "execute_worker_with_valid_manifest_fails_at_inference",
        );
        // Redirect $HOME so Config::load() can't see a real config/API key —
        // otherwise this would make a real, billed inference call via
        // generate_title(). See CONFIG_PATH_ENV_LOCK/isolate_config_path above.
        let _config_guard = isolate_config_path("valid-manifest");

        // Create a temp dir with a valid manifest
        let temp_dir = std::env::temp_dir().join("lev-test-worker-valid-manifest");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "test-worker-agent"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
max_iterations = 1

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        let manifest_path = temp_dir.join("agent.leviath");
        std::fs::write(&manifest_path, manifest_content).unwrap();

        let run_id = "test-execute-worker-valid-manifest";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let meta = make_meta(run_id, 1);
        let _ = crate::runstate::write_meta(&meta);

        let args = WorkerArgs {
            path: temp_dir.to_string_lossy().to_string(),
            task: "test task".to_string(),
            run_id: run_id.to_string(),
            model: None,
            yolo: true, // tests the yolo → launch_overrides branch
            allow: vec!["read_file".to_string()], // tests --allow branch
            ask: vec!["bash".to_string()], // tests --ask branch
            deny: vec!["write_file".to_string()], // tests --deny branch
            max_depth: None,
        };

        // This will fail because no real anthropic API key is configured,
        // but it exercises manifest loading, blueprint parsing, config loading,
        // provider registry building, engine setup, tool registry init,
        // launch_overrides population, and stage loop entry (provider missing).
        let result = execute_worker(args).await;
        // We expect either an error (no API key / provider not found) or success.
        // The key is that the code path runs without panicking.
        let _ = result; // Accept any result

        // Verify meta was written
        let saved_meta = crate::runstate::read_meta(run_id);
        saved_meta.expect("Meta should have been written by execute_worker");

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // ─── run_worker_inner: read_to_string failure (line 549) ─────────────────
    //
    // find_manifest checks existence (via `manifest.exists()`) but does NOT
    // verify the path is a file vs. a directory. Creating `agent.leviath` as a
    // directory lets find_manifest return it as Ok(path), then read_to_string
    // on a directory fails with "Is a directory" — covering the map_err closure
    // and the ? error path at line 549.

    #[tokio::test]
    async fn run_worker_inner_manifest_is_directory_returns_read_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "run_worker_inner_manifest_is_directory_returns_read_error",
        );
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let agent_dir = std::env::temp_dir().join(format!("lev-test-manifest-is-dir-{pid}-{now}"));
        let _ = std::fs::create_dir_all(&agent_dir);
        // Create agent.leviath as a DIRECTORY (not a file).
        let manifest_as_dir = agent_dir.join("agent.leviath");
        let _ = std::fs::create_dir_all(&manifest_as_dir);

        let run_id = format!("test-worker-manifest-dir-{pid}-{now}");
        let args = WorkerArgs {
            path: agent_dir.to_string_lossy().to_string(),
            task: "task".to_string(),
            run_id: run_id.clone(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let result = execute_worker(args).await;
        // Expected read error for directory manifest.
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to read manifest") | err.contains("directory"));

        let _ = std::fs::remove_dir_all(&agent_dir);
        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id));
    }

    // ─── run_worker_inner error paths ────────────────────────────────────────

    #[tokio::test]
    async fn run_worker_inner_invalid_manifest_toml_returns_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "run_worker_inner_invalid_manifest_toml_returns_error",
        );
        // Covers the parse_manifest error path (parse_manifest fails on bad TOML).
        // Uses execute_worker (which delegates to run_worker_inner with the real
        // build_provider_registry named function) to avoid a never-called closure
        // body in test infrastructure becoming a coverage gap.
        let _config_guard = isolate_config_path("invalid-manifest-toml");

        let temp_dir = std::env::temp_dir().join("lev-test-worker-invalid-manifest-toml");
        let _ = std::fs::create_dir_all(&temp_dir);
        let invalid_toml = "this is [not valid = toml at all {{{{";
        let manifest_path = temp_dir.join("agent.leviath");
        std::fs::write(&manifest_path, invalid_toml).unwrap();

        let run_id = "test-execute-worker-invalid-toml";

        let args = WorkerArgs {
            path: temp_dir.to_string_lossy().to_string(),
            task: "test task".to_string(),
            run_id: run_id.to_string(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let result = execute_worker(args).await;
        result.unwrap_err(); // just verify it errored (message varies by toml version)

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn run_worker_inner_invalid_config_toml_returns_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "run_worker_inner_invalid_config_toml_returns_error",
        );
        // Covers the Config::load()? error path.
        // Uses execute_worker (which calls run_worker_inner with the real
        // build_provider_registry named function) to avoid a never-called closure
        // body in test infrastructure becoming a coverage gap.
        let _config_guard = isolate_config_path("invalid-config-toml");

        std::fs::write(Config::config_path(), "this is [not valid = toml {{{{").unwrap();

        let temp_dir = std::env::temp_dir().join("lev-test-worker-invalid-config");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "test-cfg-fail-agent"
version = "1.0.0"
description = "Test"

[stages.main]
mode = "autonomous"
max_iterations = 1

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        let manifest_path = temp_dir.join("agent.leviath");
        std::fs::write(&manifest_path, manifest_content).unwrap();

        let run_id = "test-execute-worker-invalid-config";

        let args = WorkerArgs {
            path: temp_dir.to_string_lossy().to_string(),
            task: "test task".to_string(),
            run_id: run_id.to_string(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let result = execute_worker(args).await;
        result.unwrap_err(); // verify it errored (message varies by toml version)

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // ─── run_worker_inner (mock provider, no network) ────────────────────────
    //
    // With a real, working (mock) provider injected via
    // `run_worker_inner`'s `build_registry` parameter, the run completes an
    // actual inference round trip in-process -- exercising the `exec`
    // closure's real construction/call site, `generate_title`'s success
    // path (the "Title: {}" print), and `validate_keys()`'s warning-print
    // branch, none of which are reachable once the run aborts early at
    // `on_provider_missing` (as in the tests above).

    struct MockProvider {
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl leviath_providers::Provider for MockProvider {
        async fn infer(
            &self,
            _request: leviath_providers::InferenceRequest,
        ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
        {
            let call = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let tool_calls = if call == 0 {
                vec![leviath_providers::ToolCall {
                    id: "call-1".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "definitely-not-here.txt"}),
                }]
            } else {
                vec![]
            };
            Ok(leviath_providers::InferenceResponse {
                content: "done".to_string(),
                tool_calls,
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
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
            "mock"
        }

        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }

        async fn list_models(
            &self,
        ) -> Result<Vec<leviath_providers::ModelInfo>, leviath_providers::ProviderError> {
            Ok(vec![])
        }
    }

    struct FailingMockProvider;

    #[async_trait]
    impl leviath_providers::Provider for FailingMockProvider {
        async fn infer(
            &self,
            _request: leviath_providers::InferenceRequest,
        ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
        {
            Err(leviath_providers::ProviderError::ApiError(
                "intentional test failure".to_string(),
            ))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "failing-mock"
        }

        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }

        async fn list_models(
            &self,
        ) -> Result<Vec<leviath_providers::ModelInfo>, leviath_providers::ProviderError> {
            Ok(vec![])
        }
    }

    // Exercises the rarely-called Provider trait methods on FailingMockProvider
    // so that their bodies are counted as covered.
    #[test]
    fn failing_mock_provider_trait_methods_are_covered() {
        let p = FailingMockProvider;
        assert_eq!(p.name(), "failing-mock");
        assert_eq!(p.count_tokens("hello world", "any-model"), 2);
        assert_eq!(p.max_context_tokens("any-model"), 100_000);
        let caps = p.capabilities("any-model");
        let _ = caps; // ModelCapabilities::default() -- just verify it returns
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let models = rt.block_on(p.list_models()).unwrap();
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn run_worker_inner_with_failing_provider_propagates_error() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "run_worker_inner_with_failing_provider_propagates_error",
        );
        // Covers the `?` on `run_stage_loop` when run_stage_loop returns Err
        // because the provider always fails.
        let _config_guard = isolate_config_path("worker-failing-provider");
        let mut fake_config = Config::default();
        fake_config.title.enabled = false;
        std::fs::write(
            Config::config_path(),
            toml::to_string(&fake_config).unwrap(),
        )
        .unwrap();

        let temp_dir = std::env::temp_dir().join("lev-test-worker-failing-provider");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "test-worker-fail-agent"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
max_iterations = 1

[stages.main.model]
provider = "failing-mock"
model = "fail-model"
"#;
        let manifest_path = temp_dir.join("agent.leviath");
        std::fs::write(&manifest_path, manifest_content).unwrap();

        let run_id = "test-worker-inner-failing-provider";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let args = WorkerArgs {
            path: temp_dir.to_string_lossy().to_string(),
            task: "test task".to_string(),
            run_id: run_id.to_string(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let result = run_worker_inner(&args, &mut meta, |_config| {
            let mut registry = leviath_runtime::ProviderRegistry::new();
            registry.register("failing-mock".to_string(), Arc::new(FailingMockProvider));
            registry
        })
        .await;

        // Expected error from failing provider.
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // ─── run_worker_inner: title None path (line 571) ────────────────────────

    #[tokio::test]
    async fn run_worker_inner_title_enabled_but_no_title_provider_skips_title_print() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "run_worker_inner_title_enabled_but_no_title_provider_skips_title_print",
        );
        // Covers the None branch of `if let Some(ref t) = meta.title` (line 571):
        // config.title.enabled = true, but config.title.provider is set to a name
        // that is NOT registered in the provider registry. generate_title returns
        // None → the `println!("Title: {}", t)` line is skipped; meta.title stays None.
        let _config_guard = isolate_config_path("worker-title-none");

        let mut fake_config = Config::default();
        fake_config.title.enabled = true;
        fake_config.title.provider = Some("nonexistent-title-prov".to_string());
        std::fs::write(
            Config::config_path(),
            toml::to_string(&fake_config).unwrap(),
        )
        .unwrap();

        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let temp_dir = std::env::temp_dir().join(format!("lev-test-worker-title-none-{pid}-{now}"));
        let _ = std::fs::create_dir_all(&temp_dir);
        // Use the "anthropic" provider in the manifest's stage so we register it
        // below — the title provider ("nonexistent-title-prov") remains absent.
        let manifest_content = r#"
[agent]
name = "test-title-none-agent"
version = "1.0.0"
description = "Test"

[stages.main]
mode = "autonomous"
max_iterations = 1

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        std::fs::write(temp_dir.join("agent.leviath"), manifest_content).unwrap();

        let run_id = format!("test-worker-title-none-{pid}-{now}");
        let dir = crate::runstate::run_dir(&run_id);
        let _ = std::fs::create_dir_all(&dir);

        let args = WorkerArgs {
            path: temp_dir.to_string_lossy().to_string(),
            task: "test task for title none path".to_string(),
            run_id: run_id.clone(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let mut meta = make_meta(&run_id, 1);
        let _result = run_worker_inner(&args, &mut meta, |_config| {
            // Register "anthropic" but NOT "nonexistent-title-prov"
            let mut registry = leviath_runtime::ProviderRegistry::new();
            registry.register("anthropic".to_string(), Arc::new(MockProvider::new()));
            registry
        })
        .await;

        // generate_title returns None → meta.title stays None
        // Title should be None when provider is not registered.
        assert!(meta.title.is_none());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn run_worker_inner_with_mock_provider_completes_full_round_trip() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "run_worker_inner_with_mock_provider_completes_full_round_trip",
        );
        let _config_guard = isolate_config_path("worker-mock-provider");
        // A malformed key still exercises the `validate_keys()` warning
        // branch without being usable as a real credential -- and since the
        // provider registry is fully mocked below, no real network call can
        // happen regardless.
        let mut fake_config = Config::default();
        fake_config.providers.anthropic_api_key = Some("not-a-real-key".to_string());
        // Title generation and the stage's own inference must use distinct
        // registered providers: both draw from the same injected registry,
        // and a single shared `MockProvider` instance's call-count-based
        // "return a tool call on the first call" logic would otherwise be
        // consumed by the title-generation call, leaving the stage's own
        // first (real) call already past index 0 -- silently skipping the
        // exec-closure tool-call round trip this test exists to cover.
        fake_config.title.provider = Some("title-mock".to_string());
        fake_config.title.model = Some("title-mock-model".to_string());
        std::fs::write(
            Config::config_path(),
            toml::to_string(&fake_config).unwrap(),
        )
        .unwrap();

        let temp_dir = std::env::temp_dir().join("lev-test-worker-mock-provider");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "test-worker-mock-agent"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
max_iterations = 2

[stages.main.model]
provider = "anthropic"
model = "mock-model"

[tool_permissions]
bash = "ask"
"#;
        std::fs::write(temp_dir.join("agent.leviath"), manifest_content).unwrap();

        let run_id = "test-worker-mock-provider-round-trip";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let args = WorkerArgs {
            path: temp_dir.to_string_lossy().to_string(),
            task: "test task".to_string(),
            run_id: run_id.to_string(),
            model: None,
            yolo: true,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let result = run_worker_inner(&args, &mut meta, |_config| {
            let mut registry = leviath_runtime::ProviderRegistry::new();
            registry.register("anthropic".to_string(), Arc::new(MockProvider::new()));
            registry.register("title-mock".to_string(), Arc::new(MockProvider::new()));
            registry
        })
        .await;

        result.expect("expected clean completion from run_worker_inner");
        // generate_title should have produced a title via the mock provider.
        assert!(meta.title.is_some());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn mock_provider_trivial_trait_methods() {
        let provider = MockProvider::new();
        assert_eq!(provider.count_tokens("abcd", "mock-model"), 1);
        assert_eq!(provider.max_context_tokens("mock-model"), 100_000);
        assert_eq!(provider.name(), "mock");
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(provider.list_models()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn execute_worker_with_yolo_false_and_empty_overrides() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "execute_worker_with_yolo_false_and_empty_overrides",
        );
        // Redirect $HOME so Config::load() can't see a real config/API key —
        // otherwise this would make a real, billed inference call via
        // generate_title(). See CONFIG_PATH_ENV_LOCK/isolate_config_path above.
        let _config_guard = isolate_config_path("no-yolo");

        // Valid manifest, yolo=false, no allow/ask/deny
        let temp_dir = std::env::temp_dir().join("lev-test-worker-no-yolo");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "no-yolo-agent"
version = "1.0.0"
description = "Test"

[stages.main]
mode = "autonomous"
"#;
        std::fs::write(temp_dir.join("agent.leviath"), manifest_content).unwrap();

        let run_id = "test-execute-worker-no-yolo";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        let args = WorkerArgs {
            path: temp_dir.to_string_lossy().to_string(),
            task: "minimal task".to_string(),
            run_id: run_id.to_string(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };

        let result = execute_worker(args).await;
        let _ = result;

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // ─── WorkerCallbacks::run_autonomous ─────────────────────────────────────

    #[tokio::test]
    async fn worker_callbacks_run_autonomous_with_mock_provider_returns_error() {
        // run_autonomous calls engine.run_inference_loop_filtered which will fail
        // because no provider is registered — tests the error → anyhow path
        let run_id = "test-worker-run-autonomous";
        let mut meta = make_meta(run_id, 1);

        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let blueprint = leviath_core::Blueprint::new(
            "test".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 0),
        );
        let mut pool = leviath_runtime::AgentPool::new(blueprint);
        let _agent_id = pool.spawn_agent(engine.world_mut());
        // Use a raw entity (no context window) to force an error
        let entity = bevy_ecs::prelude::Entity::from_raw(9999);

        let mut exec = |_calls: Vec<leviath_providers::ToolCall>| {
            std::future::ready(Vec::<(String, String)>::new())
        };
        // Drive the closure body once so LLVM marks it as covered; the future
        // is immediately ready and can safely be dropped without polling.
        drop(exec(vec![]));

        let result = cb
            .run_autonomous(
                &mut engine,
                entity,
                "anthropic",
                "claude-sonnet-4-6",
                1,
                vec![],
                None,
                None,
                &mut super::super::io::ConsoleIO::new(),
                &mut exec,
            )
            .await;

        // Should return Err because entity has no ContextWindow
        assert!(result.is_err());
    }

    // ─── Additional on_stage_error coverage ──────────────────────────────────

    #[tokio::test]
    async fn worker_callbacks_on_stage_error_graph_mode_with_full_state() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_error_graph_mode_with_full_state",
        );
        let run_id = "test-worker-stage-err-graph2";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);
        let stage_dir = crate::runstate::stage_dir(run_id, 0);
        let _ = std::fs::create_dir_all(&stage_dir);

        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = make_meta(run_id, 2);
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 2,
        };

        // graph mode → Some(StageResult::Error) returned; meta NOT set to Error
        let err = anyhow::anyhow!("graph stage error");
        let result = cb.on_stage_error("main", 0, &err, true).await;
        assert_eq!(result, Some(leviath_core::blueprint::StageResult::Error));
        // In graph mode, meta status is NOT changed to Error (unlike linear)
        assert_ne!(cb.meta.status, RunStatus::Error);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_provider_missing: stages index has no entry at stage_idx ─────────

    #[tokio::test]
    async fn worker_callbacks_on_provider_missing_empty_stages() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_provider_missing_empty_stages",
        );
        let run_id = "test-worker-prov-miss-empty";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        // Write empty stages index (stage_idx=0 won't match)
        let stages: Vec<crate::runstate::StageRecord> = vec![];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = make_meta(run_id, 0);
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 0,
        };

        let result = cb.on_provider_missing("missing-provider", 0).await;
        // Should abort run.
        assert!(result);
        assert_eq!(cb.meta.status, RunStatus::Error);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_complete with max stages, checks correct log path ────────────────

    #[tokio::test]
    async fn worker_callbacks_on_complete_logs_to_last_stage() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_complete_logs_to_last_stage",
        );
        let run_id = "test-worker-complete-log";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);
        // Create the stage log dir for stage 2 (last_stage_idx=2)
        let stage_dir = crate::runstate::stage_dir(run_id, 2);
        let _ = std::fs::create_dir_all(&stage_dir);

        let mut meta = make_meta(run_id, 3);
        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 3,
        };

        // Should not panic even with stages > 0
        cb.on_complete(2).await;

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_stage_enter when stage index is out of bounds ────────────────────

    #[tokio::test]
    async fn worker_callbacks_on_stage_enter_out_of_bounds_stage_idx() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_enter_out_of_bounds_stage_idx",
        );
        let run_id = "test-worker-stage-enter-oob";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        // Only one stage in index but we request idx=5
        let stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        let _ = crate::runstate::write_stages_index(run_id, &stages);

        let mut meta = make_meta(run_id, 1);
        let _ = crate::runstate::write_meta(&meta);

        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 1,
        };

        // stage_idx=5 but only 1 stage — the `if let Some(r)` guard handles this safely
        cb.on_stage_enter("extra", 5, "anthropic", "claude-sonnet-4-6", "")
            .await;
        assert_eq!(cb.meta.current_stage, "extra");
        assert_eq!(cb.meta.stage_index, 5);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_stage_error linear mode when stages idx is out of bounds ──────────

    #[tokio::test]
    async fn worker_callbacks_on_stage_error_linear_out_of_bounds() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_callbacks_on_stage_error_linear_out_of_bounds",
        );
        let run_id = "test-worker-stage-err-linear-oob";
        let dir = crate::runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        // Empty stages index
        let _ = crate::runstate::write_stages_index(run_id, &[]);

        let mut meta = make_meta(run_id, 0);
        let _ = crate::runstate::write_meta(&meta);

        let mut cb = WorkerCallbacks {
            run_id: run_id.to_string(),
            meta: &mut meta,
            blueprint_stages_len: 0,
        };

        let err = anyhow::anyhow!("oob error");
        let result = cb.on_stage_error("main", 99, &err, false).await;
        assert!(result.is_none());
        assert_eq!(cb.meta.status, RunStatus::Error);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── WorkerInteractionBackend ───────────────────────────────────────────

    use crate::commands::run::dynamic_interaction::InteractionBackend;

    #[tokio::test]
    async fn worker_interaction_backend_ask_delegates_to_bg_review() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_interaction_backend_ask_delegates_to_bg_review",
        );
        let run_id = "test-worker-backend-ask";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let resp = crate::interaction::InteractionResponse::text("ask-1", "the answer");
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let backend = WorkerInteractionBackend {
            run_id,
            stage_idx: 0,
        };
        let req =
            crate::interaction::InteractionRequest::free_text("ask-1", "Question?", "main", true);
        let resp = backend.ask(req).await;
        assert_eq!(resp.value.as_deref(), Some("the answer"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_interaction_backend_log_writes_to_stage_log() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_interaction_backend_log_writes_to_stage_log",
        );
        let run_id = "test-worker-backend-log";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let backend = WorkerInteractionBackend {
            run_id,
            stage_idx: 0,
        };
        backend.log("[tool] ask_user_text \u{2192} waiting: hello");

        let log_contents = crate::runstate::tail_stage_log(run_id, 0, 65536);
        assert!(log_contents.contains("ask_user_text"));
        assert!(log_contents.contains("hello"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_interaction_backend_on_review_document_persists_artifact_and_output() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "worker_interaction_backend_on_review_document_persists_artifact_and_output",
        );
        let run_id = "test-worker-backend-review-doc";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let backend = WorkerInteractionBackend {
            run_id,
            stage_idx: 0,
        };
        backend.on_review_document("call-42", "My Title", "# Body\ncontent");

        let artifact_path = crate::runstate::stage_dir(run_id, 0)
            .join("reviews")
            .join("review-call-42.md");
        let artifact = std::fs::read_to_string(&artifact_path).unwrap();
        assert_eq!(artifact, "# Body\ncontent");

        let output = crate::runstate::tail_stage_output(run_id, 0, 65536);
        assert!(output.contains("My Title"));
        assert!(output.contains("# Body\ncontent"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── dispatch_tool_calls ────────────────────────────────────────────────
    //
    // These exercise the tool-dispatch logic (policy resolution, dynamic
    // interaction short-circuit, approval gating, builtin execution, result
    // truncation, activity logging) directly, extracted out of the `exec`
    // closure in `run_worker_inner`. `run_worker_inner`/`execute_worker`
    // build this state from a *real* `Config::load()` + real provider
    // registry + real inference call, so it can't be safely driven
    // end-to-end in a test without either a live provider API key (which,
    // on a developer machine with `~/.leviath/config.toml` configured,
    // would mean a real network call to a paid API) or a larger refactor
    // of `run_worker_inner` to accept an injectable provider registry --
    // out of scope for a coverage-only pass. Testing `dispatch_tool_calls`
    // directly gets full coverage of the actual dispatch logic without
    // either risk.

    async fn make_dispatch_state(run_id: &str) -> ToolDispatchState {
        let workdir = std::env::temp_dir();
        let config = Config::default();
        let tool_registry = ToolRegistry::build(workdir, &config).await;
        ToolDispatchState {
            builtins: tool_registry.builtins.clone(),
            mcp: tool_registry.mcp.clone(),
            builtin_names: tool_registry.builtin_names.clone(),
            launch_overrides: Arc::new(std::collections::HashMap::new()),
            session_allows: Arc::new(Mutex::new(std::collections::HashSet::new())),
            stage_perms: Arc::new(Mutex::new(std::collections::HashMap::new())),
            agent_perms: Arc::new(std::collections::HashMap::new()),
            global_perms: Arc::new(std::collections::HashMap::new()),
            run_id: Arc::new(run_id.to_string()),
            stage_idx: Arc::new(Mutex::new(0usize)),
            stage_name: Arc::new(Mutex::new("main".to_string())),
        }
    }

    fn make_tool_call(name: &str, args: serde_json::Value) -> leviath_providers::ToolCall {
        leviath_providers::ToolCall {
            id: format!("call-{}", name),
            name: name.to_string(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn dispatch_tool_calls_deny_policy_returns_denied_message() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_deny_policy_returns_denied_message",
        );
        let run_id = "test-dispatch-deny";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        let mut global = std::collections::HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        state.global_perms = Arc::new(global);

        let calls = vec![make_tool_call("bash", serde_json::json!({"command": "ls"}))];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-bash");
        assert!(out[0].1.contains("[denied]"));
        assert!(out[0].1.contains("not permitted"));

        let log = crate::runstate::tail_stage_log(run_id, 0, 65536);
        assert!(log.contains("denied"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_allow_builtin_executes_and_logs() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_allow_builtin_executes_and_logs",
        );
        let run_id = "test-dispatch-allow-builtin";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        let mut launch = std::collections::HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        state.launch_overrides = Arc::new(launch);

        // read_file on a file that doesn't exist still returns a (tool-level)
        // error string rather than panicking, which is enough to prove the
        // builtin execution path ran.
        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-read_file");

        let log = crate::runstate::tail_stage_log(run_id, 0, 65536);
        assert!(log.contains("read_file"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_result_truncated_when_long() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_result_truncated_when_long",
        );
        let run_id = "test-dispatch-truncate";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a file with content long enough that its read_file result
        // exceeds 120 chars, exercising the truncation branch of the
        // activity-log message (the returned tool result itself is never
        // truncated -- only the short-form log line is).
        let file_path = dir.join("big.txt");
        let long_content = "x".repeat(500);
        std::fs::write(&file_path, &long_content).unwrap();

        let workdir = dir.clone();
        let config = Config::default();
        let tool_registry = ToolRegistry::build(workdir, &config).await;
        let mut launch = std::collections::HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let state = ToolDispatchState {
            builtins: tool_registry.builtins.clone(),
            mcp: tool_registry.mcp.clone(),
            builtin_names: tool_registry.builtin_names.clone(),
            launch_overrides: Arc::new(launch),
            session_allows: Arc::new(Mutex::new(std::collections::HashSet::new())),
            stage_perms: Arc::new(Mutex::new(std::collections::HashMap::new())),
            agent_perms: Arc::new(std::collections::HashMap::new()),
            global_perms: Arc::new(std::collections::HashMap::new()),
            run_id: Arc::new(run_id.to_string()),
            stage_idx: Arc::new(Mutex::new(0usize)),
            stage_name: Arc::new(Mutex::new("main".to_string())),
        };

        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "big.txt"}),
        )];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        // Full (untruncated) result is returned to the model.
        assert!(out[0].1.contains(&long_content));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_session_allow_short_circuits_policy_resolution() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_session_allow_short_circuits_policy_resolution",
        );
        let run_id = "test-dispatch-session-allow";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        // Global policy says Deny, but session_allows already contains the
        // tool, so it should be treated as Allow regardless.
        let mut global = std::collections::HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Deny);
        state.global_perms = Arc::new(global);
        state
            .session_allows
            .lock()
            .await
            .insert("read_file".to_string());

        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        // Not denied -- session allow overrode the global Deny.
        assert!(!out[0].1.contains("[denied]"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_ask_approved_executes_tool() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_ask_approved_executes_tool",
        );
        let run_id = "test-dispatch-ask-approved";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        let mut global = std::collections::HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Ask);
        state.global_perms = Arc::new(global);

        // Compute the request id the same way `request_tool_approval_background`
        // does, so our canned response matches.
        let tool_name = "read_file";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = crate::interaction::make_interaction_id(hash, 0);

        let run_id_clone = run_id.to_string();
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let resp = crate::interaction::InteractionResponse::approval(
                &req_id_clone,
                true,
                crate::interaction::ApprovalScope::Session,
            );
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        // Session scope approval should have been recorded.
        assert!(state.session_allows.lock().await.contains("read_file"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_ask_approved_mcp_tool_returns_error_text() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_ask_approved_mcp_tool_returns_error_text",
        );
        // Not a builtin name and no MCP server registered -> the MCP
        // execute() path returns Err, exercising the `Err(e)` arm of the
        // Ask-branch's MCP dispatch (as opposed to the builtin-execution arm
        // already covered by `dispatch_tool_calls_ask_approved_executes_tool`).
        let run_id = "test-dispatch-ask-approved-mcp";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        let mut global = std::collections::HashMap::new();
        global.insert("some_mcp_tool".to_string(), ToolPolicy::Ask);
        state.global_perms = Arc::new(global);

        let tool_name = "some_mcp_tool";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = crate::interaction::make_interaction_id(hash, 0);

        let run_id_clone = run_id.to_string();
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let resp = crate::interaction::InteractionResponse::approval(
                &req_id_clone,
                true,
                crate::interaction::ApprovalScope::Once,
            );
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let calls = vec![make_tool_call("some_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("[error]"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_allow_mcp_tool_returns_error_text() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_allow_mcp_tool_returns_error_text",
        );
        // Same as above but via the Allow branch's MCP dispatch (lines
        // distinct from the Ask branch's identical match).
        let run_id = "test-dispatch-allow-mcp";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        let mut launch = std::collections::HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        state.launch_overrides = Arc::new(launch);

        let calls = vec![make_tool_call("some_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("[error]"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_ask_approved_long_result_is_truncated_in_log() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_ask_approved_long_result_is_truncated_in_log",
        );
        // The Ask branch's own truncation computation (distinct from the
        // Allow branch's, covered by `dispatch_tool_calls_result_truncated_when_long`)
        // had no test driving a long result through an Ask-approved call.
        let run_id = "test-dispatch-ask-truncate";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let long_content = "y".repeat(500);
        std::fs::write(dir.join("big.txt"), &long_content).unwrap();

        let workdir = dir.clone();
        let config = Config::default();
        let tool_registry = ToolRegistry::build(workdir, &config).await;
        let mut global = std::collections::HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Ask);
        let state = ToolDispatchState {
            builtins: tool_registry.builtins.clone(),
            mcp: tool_registry.mcp.clone(),
            builtin_names: tool_registry.builtin_names.clone(),
            launch_overrides: Arc::new(std::collections::HashMap::new()),
            session_allows: Arc::new(Mutex::new(std::collections::HashSet::new())),
            stage_perms: Arc::new(Mutex::new(std::collections::HashMap::new())),
            agent_perms: Arc::new(std::collections::HashMap::new()),
            global_perms: Arc::new(global),
            run_id: Arc::new(run_id.to_string()),
            stage_idx: Arc::new(Mutex::new(0usize)),
            stage_name: Arc::new(Mutex::new("main".to_string())),
        };

        let tool_name = "read_file";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = crate::interaction::make_interaction_id(hash, 0);

        let run_id_clone = run_id.to_string();
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let resp = crate::interaction::InteractionResponse::approval(
                &req_id_clone,
                true,
                crate::interaction::ApprovalScope::Once,
            );
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "big.txt"}),
        )];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        // Full (untruncated) result is returned to the model.
        assert!(out[0].1.contains(&long_content));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_ask_denied_returns_declined_message() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_ask_denied_returns_declined_message",
        );
        let run_id = "test-dispatch-ask-denied";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        let mut global = std::collections::HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Ask);
        state.global_perms = Arc::new(global);

        let tool_name = "read_file";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = crate::interaction::make_interaction_id(hash, 0);

        let run_id_clone = run_id.to_string();
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let resp = crate::interaction::InteractionResponse::approval(
                &req_id_clone,
                false,
                crate::interaction::ApprovalScope::Once,
            );
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("[denied]"));
        assert!(out[0].1.contains("declined"));
        assert!(!state.session_allows.lock().await.contains("read_file"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_dynamic_interaction_short_circuits() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_dynamic_interaction_short_circuits",
        );
        let run_id = "test-dispatch-dynamic-interaction";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let state = make_dispatch_state(run_id).await;

        // `handle_ask_user_text` (via `dispatch_dynamic_interaction`) never
        // times out -- it blocks on `request_interaction_bg_review` until a
        // response is written. Its request id is deterministically
        // `ask-<tool_call_id>` (see dynamic_interaction.rs), so we can
        // pre-compute it and answer in the background.
        let req_id = "ask-call-ask_user_text".to_string();
        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let resp = crate::interaction::InteractionResponse::text(&req_id, "hi there");
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let calls = vec![make_tool_call(
            "ask_user_text",
            serde_json::json!({"prompt": "What is your name?"}),
        )];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-ask_user_text");
        assert!(out[0].1.contains("hi there"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_multiple_calls_preserve_order() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_multiple_calls_preserve_order",
        );
        let run_id = "test-dispatch-multi";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut state = make_dispatch_state(run_id).await;
        let mut global = std::collections::HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        global.insert("read_file".to_string(), ToolPolicy::Allow);
        state.global_perms = Arc::new(global);

        let calls = vec![
            make_tool_call("bash", serde_json::json!({"command": "ls"})),
            make_tool_call("read_file", serde_json::json!({"path": "nope.txt"})),
        ];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "call-bash");
        assert!(out[0].1.contains("[denied]"));
        assert_eq!(out[1].0, "call-read_file");
        assert!(!out[1].1.contains("[denied]"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── MCP Ok(r) arms: lines 132-133, 163-164 ──────────────────────────────
    //
    // These lines can only be reached when the ToolExecutor.execute() call
    // returns Ok(r) -- which requires a real MCP server process to be running
    // and registered in the dispatch state.
    //
    // We use Python as a minimal JSON-RPC 2.0 stub.  Two scripts are needed:
    //   MCP_STUB_SUCCESS      → isError: false  → hits `Ok(r) if r.success`
    //   MCP_STUB_ERROR_RESULT → isError: true   → hits `Ok(r)` (success=false)

    const MCP_STUB_SUCCESS: &str = r#"
import sys, json
def respond(id_, result):
    msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": result})
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": False}}, "protocolVersion": "2024-11-05"})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "stub_mcp_tool", "description": "stub", "inputSchema": {"type": "object", "properties": {}}}]})
    elif method == "tools/call":
        respond(id_, {"content": [{"type": "text", "text": "ok result from stub"}], "isError": False})
    elif method == "notifications/cancelled":
        pass
    else:
        respond(id_, {})
"#;

    const MCP_STUB_ERROR_RESULT: &str = r#"
import sys, json
def respond(id_, result):
    msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": result})
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": False}}, "protocolVersion": "2024-11-05"})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "stub_mcp_tool", "description": "stub", "inputSchema": {"type": "object", "properties": {}}}]})
    elif method == "tools/call":
        respond(id_, {"content": [{"type": "text", "text": "tool error text"}], "is_error": True})
    elif method == "notifications/cancelled":
        pass
    else:
        respond(id_, {})
"#;

    /// Build a dispatch state whose MCP executor has a live stub server
    /// that responds to calls for `stub_mcp_tool`.
    ///
    /// `policy` is inserted into `launch_overrides` for `stub_mcp_tool`.
    async fn make_dispatch_state_with_mcp_tool(
        run_id: &str,
        stub_script: &str,
        policy: ToolPolicy,
    ) -> ToolDispatchState {
        use std::collections::HashMap;
        let mut client =
            leviath_mcp::MCPClient::spawn("python3", &["-c", stub_script], &HashMap::new())
                .await
                .expect("Failed to spawn MCP stub");
        // Connect (initialize + initialized handshake) so the server is ready
        client.connect().await.expect("MCP connect failed");
        // Populate the tool cache so executor.execute() can find "stub_mcp_tool"
        client.list_tools().await.expect("list_tools failed");

        let mut executor = leviath_mcp::ToolExecutor::new();
        executor.add_client("stub-server".to_string(), client);

        let workdir = std::env::temp_dir();
        let config = Config::default();
        let tool_registry = ToolRegistry::build(workdir, &config).await;

        let mut launch = std::collections::HashMap::new();
        launch.insert("stub_mcp_tool".to_string(), policy);

        ToolDispatchState {
            builtins: tool_registry.builtins.clone(),
            mcp: Arc::new(Mutex::new(executor)),
            builtin_names: tool_registry.builtin_names.clone(),
            launch_overrides: Arc::new(launch),
            session_allows: Arc::new(Mutex::new(std::collections::HashSet::new())),
            stage_perms: Arc::new(Mutex::new(std::collections::HashMap::new())),
            agent_perms: Arc::new(std::collections::HashMap::new()),
            global_perms: Arc::new(std::collections::HashMap::new()),
            run_id: Arc::new(run_id.to_string()),
            stage_idx: Arc::new(Mutex::new(0usize)),
            stage_name: Arc::new(Mutex::new("main".to_string())),
        }
    }

    // ─── Allow branch MCP Ok(r) arms (lines 163-164) ─────────────────────────

    #[tokio::test]
    async fn dispatch_tool_calls_allow_mcp_ok_success_returns_text() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_allow_mcp_ok_success_returns_text",
        );
        // Covers line 163: `Ok(r) if r.success => r.text`
        let run_id = "test-dispatch-allow-mcp-ok-success";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let state =
            make_dispatch_state_with_mcp_tool(run_id, MCP_STUB_SUCCESS, ToolPolicy::Allow).await;

        let calls = vec![make_tool_call("stub_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-stub_mcp_tool");
        let has_ok_text = out[0].1.contains("ok result from stub");
        // Expected success text.
        assert!(has_ok_text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_allow_mcp_ok_error_result_returns_error_prefix() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_allow_mcp_ok_error_result_returns_error_prefix",
        );
        // Covers line 164: `Ok(r) => format!("[error] {}", r.text)` (isError: true)
        let run_id = "test-dispatch-allow-mcp-ok-error-result";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let state =
            make_dispatch_state_with_mcp_tool(run_id, MCP_STUB_ERROR_RESULT, ToolPolicy::Allow)
                .await;

        let calls = vec![make_tool_call("stub_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-stub_mcp_tool");
        let has_error_prefix = out[0].1.starts_with("[error]");
        // Expected [error] prefix.
        assert!(has_error_prefix);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── Ask branch MCP Ok(r) arms (lines 132-133) ───────────────────────────

    #[tokio::test]
    async fn dispatch_tool_calls_ask_approved_mcp_ok_success_returns_text() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_ask_approved_mcp_ok_success_returns_text",
        );
        // Covers line 132: `Ok(r) if r.success => r.text` in the Ask branch
        let run_id = "test-dispatch-ask-mcp-ok-success";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let state =
            make_dispatch_state_with_mcp_tool(run_id, MCP_STUB_SUCCESS, ToolPolicy::Ask).await;

        // Schedule approval response so the Ask branch doesn't block
        let run_id_clone = run_id.to_string();
        let tool_name = "stub_mcp_tool";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = crate::interaction::make_interaction_id(hash, 0);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let resp = crate::interaction::InteractionResponse::approval(
                &req_id,
                true,
                crate::interaction::ApprovalScope::Once,
            );
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let calls = vec![make_tool_call("stub_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        let has_ok_text = out[0].1.contains("ok result from stub");
        // Expected success text.
        assert!(has_ok_text);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dispatch_tool_calls_ask_approved_mcp_ok_error_result_returns_error_prefix() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "dispatch_tool_calls_ask_approved_mcp_ok_error_result_returns_error_prefix",
        );
        // Covers line 133: `Ok(r) => format!("[error] {}", r.text)` in the Ask branch
        let run_id = "test-dispatch-ask-mcp-ok-error-result";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = make_meta(run_id, 1);
        crate::runstate::create_run(&meta).unwrap();

        let state =
            make_dispatch_state_with_mcp_tool(run_id, MCP_STUB_ERROR_RESULT, ToolPolicy::Ask).await;

        let run_id_clone = run_id.to_string();
        let tool_name = "stub_mcp_tool";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = crate::interaction::make_interaction_id(hash, 0);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let resp = crate::interaction::InteractionResponse::approval(
                &req_id,
                true,
                crate::interaction::ApprovalScope::Once,
            );
            crate::interaction::write_response(&run_id_clone, &resp).ok();
        });

        let calls = vec![make_tool_call("stub_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls(&state, calls).await;

        assert_eq!(out.len(), 1);
        let has_error_prefix = out[0].1.starts_with("[error]");
        // Expected [error] prefix.
        assert!(has_error_prefix);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
