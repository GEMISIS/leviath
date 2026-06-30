//! Background worker run mode.

use leviath_core::blueprint::{StageMode, StageResult};
use leviath_runtime::{AgentPool, AgentState, ContextWindow};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::{Config, ToolPolicy};
use crate::runstate::{self, RunMeta, RunStatus, StageRecord, StageRunStatus};
use crate::tools::{resolve_policy, ToolRegistry};

use super::graph::{apply_edge_transform, is_graph_mode, resolve_transition};
use super::helpers::{
    build_context_snapshot, generate_title, initialize_context_window, record_stage_log,
    record_stage_output, swap_context_layout, write_context_snapshot_if_bg,
};
use super::manifest::{find_manifest, parse_manifest};
use super::session::build_provider_registry;
use super::stages::{run_interactive_points_stage, run_interactive_stage};
use super::WorkerArgs;

/// Tracks the current stage index for tool-activity logging from the executor closure.
type CurrentStageIdx = Arc<Mutex<usize>>;

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
                            "[tool] present_for_review → waiting for user review: {}",
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
                    record_stage_log(&run_id, stage_idx, "[tool] present_for_review → done");
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
                            &format!("[tool] {} → denied", tc.name),
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
                                format!("{}…", &result[..120])
                            } else {
                                result.clone()
                            };
                            record_stage_log(
                                &run_id,
                                stage_idx,
                                &format!("[tool] {} → {}", tc.name, short_result),
                            );
                            result
                        } else {
                            record_stage_log(
                                &run_id,
                                stage_idx,
                                &format!("[tool] {} → declined by user", tc.name),
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
                            format!("{}…", &result[..120])
                        } else {
                            result.clone()
                        };
                        record_stage_log(
                            &run_id,
                            stage_idx,
                            &format!("[tool] {} → {}", tc.name, short_result),
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

    // ─── Graph stage loop (worker) ──────────────────────────────────────────
    let entry_name = blueprint.resolve_entry_stage_name();
    let mut current_stage_name_val = entry_name;
    let mut current_stage_idx_val = blueprint
        .stages
        .iter()
        .position(|s| s.name == current_stage_name_val)
        .unwrap_or(0);
    let mut visit_counts: HashMap<String, usize> = HashMap::new();

    loop {
        let stage = blueprint
            .find_stage(&current_stage_name_val)
            .ok_or_else(|| anyhow::anyhow!("Stage '{}' not found", current_stage_name_val))?;

        let stage_idx = current_stage_idx_val;
        let provider_name = &stage.model.provider;
        let model_name = args.model.as_deref().unwrap_or(&stage.model.model);

        // Update current stage permissions + index + name for the executor closure
        {
            let mut sp = current_stage_perms.lock().await;
            *sp = stage.tool_permissions.clone();
        }
        {
            let mut si = current_stage_idx.lock().await;
            *si = stage_idx;
        }
        {
            let mut sn = current_stage_name.lock().await;
            *sn = stage.name.clone();
        }

        if !engine.providers().has(provider_name) {
            let msg = format!("Provider '{}' is not configured", provider_name);
            println!("\n{}", msg);
            record_stage_log(&args.run_id, stage_idx, &format!("[error] {}", msg));
            {
                let mut stages = runstate::read_stages_index(&args.run_id);
                if let Some(r) = stages.get_mut(stage_idx) {
                    r.status = StageRunStatus::Error;
                }
                let _ = runstate::write_stages_index(&args.run_id, &stages);
            }
            meta.status = RunStatus::Error;
            meta.error = Some(msg);
            meta.touch();
            let _ = runstate::write_meta(meta);
            return Ok(());
        }

        let visit_num = visit_counts.get(&stage.name).copied().unwrap_or(0);
        let visit_label = if visit_num > 0 {
            format!(" (visit {})", visit_num + 1)
        } else {
            String::new()
        };
        let stage_header = format!(
            "Stage {}: {} ({}:{}){}",
            stage_idx + 1,
            stage.name,
            provider_name,
            model_name,
            visit_label,
        );
        println!("\n--- {} ---", stage_header);
        record_stage_log(
            &args.run_id,
            stage_idx,
            &format!("--- {} ---", stage_header),
        );

        if provider_name == "claude-code" {
            let warn = "⚠️  Using claude-code provider: tool routing, per-stage filtering, and prompt caching are not available.";
            println!("{}", warn);
            record_stage_log(&args.run_id, stage_idx, warn);
        }

        // Mark stage as active and update stages.json
        let stage_started_at = {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        };
        {
            let mut stages = runstate::read_stages_index(&args.run_id);
            if let Some(r) = stages.get_mut(stage_idx) {
                r.status = StageRunStatus::Active;
                r.started_at = Some(stage_started_at);
            }
            let _ = runstate::write_stages_index(&args.run_id, &stages);
        }

        meta.current_stage = stage.name.clone();
        meta.stage_index = stage_idx;
        meta.status = RunStatus::Running;
        meta.touch();
        let _ = runstate::write_meta(meta);

        // Update accepts_messages on the agent state for this stage
        if let Some(mut state) = engine.world_mut().get_mut::<AgentState>(entity) {
            state.accepts_messages = stage.accepts_messages;
        }

        if let Some(ref stage_layout) = stage.context_layout {
            swap_context_layout(&mut engine, entity, stage_layout);
        }

        // Inject per-stage system prompt into context
        if let Some(sp) = stage.config.get("system_prompt").and_then(|v| v.as_str()) {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = sp.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("[Stage instructions: {}]", sp),
                    tokens,
                );
            }
        }

        let all_tools = tool_registry.all_tool_defs();
        let effective_tools: Vec<leviath_providers::Tool> = if stage.available_tools.is_empty() {
            Vec::new()
        } else {
            all_tools
                .into_iter()
                .filter(|t| stage.available_tools.iter().any(|f| f == &t.name))
                .collect()
        };

        let routing_config =
            stage
                .tool_result_routing
                .as_ref()
                .map(|r| leviath_runtime::ToolResultRoutingConfig {
                    default_region: r.default_region.clone(),
                    tool_overrides: r.tool_overrides.clone(),
                    persist: r.persist,
                    max_result_tokens: r.max_result_tokens,
                });
        let routing_ref = routing_config.as_ref();
        let max_iterations = stage.max_iterations.unwrap_or(20);

        // Determine stage result for transition condition evaluation
        let stage_result_val: StageResult;

        // Workers now support interactive stage modes via the file-based IPC channel.
        let stage_run_result: anyhow::Result<Option<leviath_providers::InferenceResponse>> =
            match &stage.mode {
                StageMode::Interactive => run_interactive_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    Some((&args.run_id, meta)),
                    &stage.name,
                    &mut exec,
                )
                .await
                .map(|_| None),
                StageMode::InteractivePoints { points } => {
                    let pts = points.clone();
                    run_interactive_points_stage(
                        &mut engine,
                        entity,
                        provider_name,
                        model_name,
                        max_iterations,
                        &effective_tools,
                        routing_ref,
                        compaction_ref,
                        &pts,
                        Some((&args.run_id, meta)),
                        &mut exec,
                    )
                    .await
                    .map(|_| None)
                }
                StageMode::Autonomous => engine
                    .run_inference_loop_filtered(
                        entity,
                        provider_name,
                        model_name,
                        effective_tools,
                        max_iterations,
                        None,
                        routing_ref,
                        compaction_ref,
                        &mut exec,
                    )
                    .await
                    .map(Some)
                    .map_err(|e| anyhow::anyhow!("{}", e)),
            };

        let stage_ended_at = {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        };

        match stage_run_result {
            Ok(resp_opt) => {
                if let Some(resp) = resp_opt {
                    // Route the readable agent response to both stdout (legacy) and per-stage output
                    println!("{}", resp.content);
                    record_stage_output(&args.run_id, stage_idx, &resp.content);

                    let token_line = format!(
                        "[Tokens: {} in, {} out]",
                        resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                    );
                    println!("\n{}", token_line);
                    record_stage_log(&args.run_id, stage_idx, &token_line);

                    meta.prompt_tokens += resp.tokens_used.prompt_tokens;
                    meta.completion_tokens += resp.tokens_used.completion_tokens;
                    meta.cached_tokens += resp.tokens_used.cached_tokens;

                    // Carry the final response forward so the next stage sees the previous stage's output
                    if !resp.content.is_empty() {
                        if let Some(mut window) =
                            engine.world_mut().get_mut::<ContextWindow>(entity)
                        {
                            let tokens = resp.content.len() / 4 + 1;
                            let _ = window.add_to_region(
                                "conversation",
                                format!("Assistant ({}): {}", stage.name, resp.content),
                                tokens,
                            );
                        }
                    }
                }

                // Determine if max_iterations was hit
                if let Some(state) = engine.world().get::<AgentState>(entity) {
                    if stage.max_iterations.is_some() && state.iteration >= max_iterations {
                        stage_result_val = StageResult::MaxIterations;
                    } else {
                        stage_result_val = StageResult::Success;
                    }
                } else {
                    stage_result_val = StageResult::Success;
                }

                // Mark stage complete
                {
                    let mut stages = runstate::read_stages_index(&args.run_id);
                    if let Some(r) = stages.get_mut(stage_idx) {
                        r.status = StageRunStatus::Complete;
                        r.ended_at = Some(stage_ended_at);
                        r.prompt_tokens = meta.prompt_tokens;
                        r.completion_tokens = meta.completion_tokens;
                        r.cached_tokens = meta.cached_tokens;
                    }
                    let _ = runstate::write_stages_index(&args.run_id, &stages);
                }
            }
            Err(e) => {
                // In graph mode, errors can route to error-handler stages
                if is_graph_mode(&blueprint) {
                    let msg = format!(
                        "Stage '{}' error: {} — checking error transitions",
                        stage.name, e
                    );
                    println!("{}", msg);
                    record_stage_log(&args.run_id, stage_idx, &format!("[error] {}", msg));
                    stage_result_val = StageResult::Error;

                    // Mark stage as errored but don't abort
                    {
                        let mut stages = runstate::read_stages_index(&args.run_id);
                        if let Some(r) = stages.get_mut(stage_idx) {
                            r.status = StageRunStatus::Error;
                            r.ended_at = Some(stage_ended_at);
                        }
                        let _ = runstate::write_stages_index(&args.run_id, &stages);
                    }
                } else {
                    let msg = format!("Stage '{}' inference error: {}", stage.name, e);
                    println!("{}", msg);
                    record_stage_log(&args.run_id, stage_idx, &format!("[error] {}", msg));
                    // Mark stage error
                    {
                        let mut stages = runstate::read_stages_index(&args.run_id);
                        if let Some(r) = stages.get_mut(stage_idx) {
                            r.status = StageRunStatus::Error;
                            r.ended_at = Some(stage_ended_at);
                        }
                        let _ = runstate::write_stages_index(&args.run_id, &stages);
                    }
                    meta.status = RunStatus::Error;
                    meta.error = Some(msg);
                    meta.touch();
                    let _ = runstate::write_meta(meta);
                    return Ok(());
                }
            }
        }

        *visit_counts.entry(stage.name.clone()).or_default() += 1;

        if let Some(state) = engine.world().get::<AgentState>(entity) {
            meta.iteration = state.iteration;
        }
        meta.touch();
        let _ = runstate::write_meta(meta);
        // Write context snapshot to both legacy path and per-stage path
        write_context_snapshot_if_bg(&engine, entity, &stage.name, &Some(args.run_id.clone()));
        if let Some(snap) = build_context_snapshot(&engine, entity, &stage.name) {
            let _ = runstate::write_stage_context(&args.run_id, stage_idx, &snap);
        }

        // Resolve the next transition
        let stage_name_owned = current_stage_name_val.clone();
        let stage_ref = blueprint.find_stage(&stage_name_owned).unwrap();
        let transition = resolve_transition(
            stage_ref,
            current_stage_idx_val,
            &blueprint,
            &visit_counts,
            &stage_result_val,
            &mut engine,
            entity,
            provider_name,
            model_name,
        )
        .await;

        match transition {
            Some((edge, next_idx)) => {
                let next_name = edge.target.clone();
                let marker = format!(
                    "[Stage complete: {}, transitioning to: {}]",
                    stage_name_owned, next_name
                );
                record_stage_log(&args.run_id, stage_idx, &marker);
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                    let tokens = marker.len() / 4 + 1;
                    let _ = window.add_to_region("conversation", marker, tokens);
                }

                // Apply edge transform
                apply_edge_transform(
                    &edge,
                    &visit_counts,
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    compaction_ref,
                )
                .await;

                current_stage_name_val = next_name;
                current_stage_idx_val = next_idx;
            }
            None => break, // terminal: no valid transitions
        }
    }

    let done_msg = "[All stages complete]";
    println!("\n{}", done_msg);
    // Log the completion message to the last stage's log
    if !blueprint.stages.is_empty() {
        record_stage_log(&args.run_id, current_stage_idx_val, done_msg);
    }
    tool_registry.shutdown().await;
    Ok(())
}
