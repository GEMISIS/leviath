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
use super::manifest::{find_manifest, parse_manifest};
use super::session::build_provider_registry;
use super::WorkerArgs;

/// Tracks the current stage index for tool-activity logging from the executor closure.
type CurrentStageIdx = Arc<Mutex<usize>>;

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

    let result = run_worker_inner(&args, &mut meta).await;

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

async fn run_worker_inner(args: &WorkerArgs, meta: &mut RunMeta) -> anyhow::Result<()> {
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

    let prov_registry = build_provider_registry(&config);

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
    let entity = pool
        .get_agent(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("Failed to get spawned agent entity"))?;

    let workdir = std::env::current_dir()?;
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

    let builtins = tool_registry.builtins.clone();
    let mcp = tool_registry.mcp.clone();
    let builtin_names = tool_registry.builtin_names.clone();
    let exec_session_allows = session_allows.clone();
    let exec_stage_perms = current_stage_perms.clone();
    let exec_agent_perms = agent_perms_arc.clone();
    let exec_global_perms = global_perms.clone();
    let exec_run_id = run_id_arc.clone();
    let exec_stage_idx = current_stage_idx.clone();
    let exec_stage_name = current_stage_name.clone();
    let mut exec = move |calls: Vec<leviath_providers::ToolCall>| {
        let builtins = builtins.clone();
        let mcp = mcp.clone();
        let builtin_names = builtin_names.clone();
        let launch_ov = launch_overrides_arc.clone();
        let session_al = exec_session_allows.clone();
        let stage_pm = exec_stage_perms.clone();
        let agent_pm = exec_agent_perms.clone();
        let global_pm = exec_global_perms.clone();
        let run_id = exec_run_id.clone();
        let stage_idx_arc = exec_stage_idx.clone();
        let stage_name_arc = exec_stage_name.clone();
        async move {
            let stage_idx = *stage_idx_arc.lock().await;
            let stage_name = stage_name_arc.lock().await.clone();
            let mut out: Vec<(String, String)> = Vec::new();
            for tc in calls {
                // ── present_for_review: special built-in that raises an interaction ──
                if tc.name == "present_for_review" {
                    let title = tc
                        .arguments
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Review")
                        .to_string();
                    let markdown = tc
                        .arguments
                        .get("markdown")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // Persist the review artifact under stages/<idx>/reviews/
                    let review_dir = runstate::stage_dir(&run_id, stage_idx).join("reviews");
                    let _ = std::fs::create_dir_all(&review_dir);
                    let artifact_path = review_dir.join(format!("review-{}.md", tc.id));
                    let _ = std::fs::write(&artifact_path, &markdown);

                    // Also write to stage output so it's visible in the Output tab after review
                    record_stage_output(
                        &run_id,
                        stage_idx,
                        &format!("---\n## {}\n\n{}\n---", title, markdown),
                    );

                    // Log the event
                    record_stage_log(
                        &run_id,
                        stage_idx,
                        &format!(
                            "[tool] present_for_review \u{2192} waiting for user review: {}",
                            title
                        ),
                    );

                    // Build the interaction request with markdown body
                    let req = crate::interaction::InteractionRequest::review(
                        format!("review-{}", tc.id),
                        &title,
                        &markdown,
                        &stage_name,
                    );

                    // Write request and wait for response
                    let resp =
                        crate::interaction::request_interaction_bg_review(&run_id, req).await;

                    let user_feedback = crate::interaction::response_as_text(&resp);
                    let result = if user_feedback.trim().is_empty() {
                        "User reviewed the document and acknowledged.".to_string()
                    } else {
                        format!("User feedback: {}", user_feedback)
                    };
                    record_stage_log(
                        &run_id,
                        stage_idx,
                        "[tool] present_for_review \u{2192} done",
                    );
                    out.push((tc.id.clone(), result));
                    continue;
                }

                let is_builtin = builtin_names.contains(&tc.name);
                let session_has = session_al.lock().await.contains(&tc.name);
                let policy = if session_has {
                    ToolPolicy::Allow
                } else {
                    let stage_pm_snap = stage_pm.lock().await.clone();
                    resolve_policy(
                        &tc.name,
                        is_builtin,
                        &launch_ov,
                        &stage_pm_snap,
                        &agent_pm,
                        &global_pm,
                    )
                };

                let res = match policy {
                    ToolPolicy::Deny => {
                        let msg = format!("[denied] Tool '{}' is not permitted.", tc.name);
                        record_stage_log(
                            &run_id,
                            stage_idx,
                            &format!("[tool] {} \u{2192} denied", tc.name),
                        );
                        msg
                    }
                    ToolPolicy::Ask => {
                        use crate::interaction::{request_tool_approval_background, ApprovalScope};
                        let (approved, scope) = request_tool_approval_background(
                            &run_id,
                            &tc.name,
                            &tc.arguments,
                            "tool-call",
                        )
                        .await;
                        if approved {
                            if scope == ApprovalScope::Session {
                                session_al.lock().await.insert(tc.name.clone());
                            }
                            let result = if is_builtin {
                                builtins.execute(&tc.name, tc.arguments.clone()).await
                            } else {
                                let mut mcp_lock = mcp.lock().await;
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
                                &run_id,
                                stage_idx,
                                &format!("[tool] {} \u{2192} {}", tc.name, short_result),
                            );
                            result
                        } else {
                            record_stage_log(
                                &run_id,
                                stage_idx,
                                &format!("[tool] {} \u{2192} declined by user", tc.name),
                            );
                            format!("[denied] User declined tool call '{}'.", tc.name)
                        }
                    }
                    ToolPolicy::Allow => {
                        let result = if is_builtin {
                            builtins.execute(&tc.name, tc.arguments.clone()).await
                        } else {
                            let mut mcp_lock = mcp.lock().await;
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
                            &run_id,
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
        compaction_ref,
    };

    run_stage_loop(&mut ctx, &mut callbacks, &agent_id, &mut exec).await?;

    tool_registry.shutdown().await;
    Ok(())
}
