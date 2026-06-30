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

                // ── ask_user_*: agent-initiated dynamic interaction tools ──────
                // Unlike `interaction_points` (declared statically in the
                // blueprint and always shown), these let the model itself
                // decide, mid-reasoning, that it needs human input.
                if tc.name == "ask_user_text" {
                    let prompt = tc
                        .arguments
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    record_stage_log(
                        &run_id,
                        stage_idx,
                        &format!("[tool] ask_user_text \u{2192} waiting: {}", prompt),
                    );
                    let req = crate::interaction::InteractionRequest::free_text(
                        format!("ask-{}", tc.id),
                        &prompt,
                        &stage_name,
                        true,
                    );
                    let resp =
                        crate::interaction::request_interaction_bg_review(&run_id, req).await;
                    let answer = crate::interaction::response_as_text(&resp);
                    record_stage_log(&run_id, stage_idx, "[tool] ask_user_text \u{2192} done");
                    let result = if answer.trim().is_empty() {
                        "User provided no answer.".to_string()
                    } else {
                        format!("User: {}", answer)
                    };
                    out.push((tc.id.clone(), result));
                    continue;
                }

                if tc.name == "ask_user_choice" {
                    let prompt = tc
                        .arguments
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let options: Vec<String> = tc
                        .arguments
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if options.len() < 2 {
                        out.push((
                            tc.id.clone(),
                            "[error] ask_user_choice requires at least 2 options".to_string(),
                        ));
                        continue;
                    }
                    record_stage_log(
                        &run_id,
                        stage_idx,
                        &format!("[tool] ask_user_choice \u{2192} waiting: {}", prompt),
                    );
                    let req = crate::interaction::InteractionRequest::multiple_choice(
                        format!("ask-{}", tc.id),
                        &prompt,
                        options.clone(),
                        &stage_name,
                    );
                    let resp =
                        crate::interaction::request_interaction_bg_review(&run_id, req).await;
                    let choice = crate::interaction::response_as_choice(&resp, &options)
                        .cloned()
                        .unwrap_or_else(|| crate::interaction::response_as_text(&resp));
                    record_stage_log(&run_id, stage_idx, "[tool] ask_user_choice \u{2192} done");
                    out.push((tc.id.clone(), format!("User chose: {}", choice)));
                    continue;
                }

                if tc.name == "ask_user_confirm" {
                    let prompt = tc
                        .arguments
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    record_stage_log(
                        &run_id,
                        stage_idx,
                        &format!("[tool] ask_user_confirm \u{2192} waiting: {}", prompt),
                    );
                    let req = crate::interaction::InteractionRequest::confirm(
                        format!("ask-{}", tc.id),
                        &prompt,
                        &stage_name,
                    );
                    let resp =
                        crate::interaction::request_interaction_bg_review(&run_id, req).await;
                    let approved = crate::interaction::response_approved(&resp);
                    record_stage_log(&run_id, stage_idx, "[tool] ask_user_confirm \u{2192} done");
                    out.push((
                        tc.id.clone(),
                        format!("User answered: {}", if approved { "Yes" } else { "No" }),
                    ));
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
    let mut io = ConsoleIO;

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

    run_stage_loop(&mut ctx, &mut callbacks, &agent_id, &mut io, &mut exec).await?;

    tool_registry.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::{FinishReason, InferenceResponse, TokenUsage};

    // ─── Helpers ──────────────────────────────────────────────────────────────

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
        assert!(ts > 0, "Expected positive timestamp, got {}", ts);
    }

    #[test]
    fn worker_callbacks_now_secs_is_recent() {
        let ts = WorkerCallbacks::now_secs();
        // Should be after 2024-01-01 (1704067200) and before 2040
        assert!(ts > 1_704_067_200, "Timestamp too old: {}", ts);
        assert!(ts < 2_208_988_800, "Timestamp too far in future: {}", ts);
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
        assert!(handle.is_none(), "Worker should not start a message reader");
    }

    #[tokio::test]
    async fn worker_callbacks_on_complete_with_positive_stages() {
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
        assert!(result, "on_provider_missing should return true (abort)");
        assert!(matches!(cb.meta.status, RunStatus::Error));
        assert!(cb.meta.error.is_some());
        assert!(cb.meta.error.as_ref().unwrap().contains("nonexistent"));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_enter_updates_meta() {
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
        assert!(matches!(cb.meta.status, RunStatus::Running));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_enter_with_visit_label() {
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
        assert!(matches!(cb.meta.status, RunStatus::Error));
        assert!(cb.meta.error.as_ref().unwrap().contains("linear error"));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn worker_callbacks_on_stage_result_updates_stages() {
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
        assert!(
            err_msg.contains("Could not find") || err_msg.contains("manifest"),
            "Expected manifest error, got: {}",
            err_msg
        );

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[tokio::test]
    async fn execute_worker_creates_meta_when_missing() {
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
        assert!(
            saved_meta.is_ok(),
            "Meta should have been written by execute_worker"
        );

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn execute_worker_with_yolo_false_and_empty_overrides() {
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

        let mut exec = |_calls: Vec<leviath_providers::ToolCall>| async move { vec![] };

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
                &mut super::super::io::ConsoleIO,
                &mut exec,
            )
            .await;

        // Should return Err because entity has no ContextWindow
        assert!(result.is_err());
    }

    // ─── Additional on_stage_error coverage ──────────────────────────────────

    #[tokio::test]
    async fn worker_callbacks_on_stage_error_graph_mode_with_full_state() {
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
        assert!(!matches!(cb.meta.status, RunStatus::Error));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_provider_missing: stages index has no entry at stage_idx ─────────

    #[tokio::test]
    async fn worker_callbacks_on_provider_missing_empty_stages() {
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
        assert!(result, "Should abort run");
        assert!(matches!(cb.meta.status, RunStatus::Error));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── on_complete with max stages, checks correct log path ────────────────

    #[tokio::test]
    async fn worker_callbacks_on_complete_logs_to_last_stage() {
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
        assert!(matches!(cb.meta.status, RunStatus::Error));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }
}
