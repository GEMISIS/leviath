//! Foreground (inline, blocking) run mode.

use leviath_core::blueprint::{StageMode, StageResult};
use leviath_runtime::{AgentMessage, AgentPool, AgentState, ContextWindow};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::{Config, ToolPolicy};
use crate::tools::{resolve_policy, ToolRegistry};

use super::graph::{apply_edge_transform, is_graph_mode, resolve_transition};
use super::helpers::{initialize_context_window, swap_context_layout};
use super::manifest::{find_manifest, parse_manifest};
use super::session::{build_provider_registry, resolve_task};
use super::stages::{run_autonomous_stage, run_interactive_points_stage, run_interactive_stage};
use super::RunArgs;

/// Run an agent in the foreground (inline, blocking) — the original behavior.
pub async fn run_foreground(args: RunArgs) -> anyhow::Result<()> {
    let path = args.path.unwrap_or_else(|| ".".to_string());

    let manifest_path = find_manifest(&path)?;
    println!("Loading agent from: {}", manifest_path.display());

    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest(&manifest_content)?;

    let description = Some(blueprint.description.as_str());
    let task = resolve_task(&args.task, &blueprint.name, description)?;

    tracing::info!(path = %path, task = %task, "Running agent (foreground)");

    println!("Agent: {} v{}", blueprint.name, blueprint.version);
    println!("Task: {}", task);

    let config = Config::load()?;
    for warning in config.validate_keys() {
        println!("Warning: {}", warning);
    }

    let registry = build_provider_registry(&config);
    let mut engine = leviath_runtime::AgentEngine::with_providers(registry);

    let mut pool = AgentPool::new(blueprint.clone());
    let agent_id = pool.spawn_agent(engine.world_mut());
    let entity = pool
        .get_agent(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("Failed to get spawned agent entity"))?;

    let workdir = std::env::current_dir()?;
    initialize_context_window(&mut engine, entity, &blueprint, &task);

    let tool_registry = Arc::new(ToolRegistry::build(workdir, &config).await);

    // Build launch-level tool policy overrides from CLI flags
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

    // Session-level tool allows (populated when user chooses "Allow for this session")
    let session_allows: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    // Current stage's permissions (updated per stage below)
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
    let global_perms = config.tool_permissions.clone();

    // Current stage name for present_for_review interactions (updated per stage below)
    let current_stage_name: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // Build executor closure (Arcs cloned once here, then again per call)
    let builtins = tool_registry.builtins.clone();
    let mcp = tool_registry.mcp.clone();
    let builtin_names = tool_registry.builtin_names.clone();
    let launch_overrides_arc = Arc::new(launch_overrides);
    let exec_session_allows = session_allows.clone();
    let exec_stage_perms = current_stage_perms.clone();
    let exec_stage_name = current_stage_name.clone();
    let exec_agent_perms = agent_perms_arc.clone();
    let exec_global_perms = Arc::new(global_perms);
    let mut exec = move |calls: Vec<leviath_providers::ToolCall>| {
        let builtins = builtins.clone();
        let mcp = mcp.clone();
        let builtin_names = builtin_names.clone();
        let launch_ov = launch_overrides_arc.clone();
        let session_al = exec_session_allows.clone();
        let stage_pm = exec_stage_perms.clone();
        let stage_nm = exec_stage_name.clone();
        let agent_pm = exec_agent_perms.clone();
        let global_pm = exec_global_perms.clone();
        async move {
            let mut out: Vec<(String, String)> = Vec::new();
            for tc in calls {
                // ── present_for_review: print document to stdout, ask for feedback ──
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
                    let stage_name = stage_nm.lock().await.clone();

                    println!("\n{}", "─".repeat(60));
                    println!("  {}", title);
                    println!("{}", "─".repeat(60));
                    println!("{}", markdown);
                    println!("{}", "─".repeat(60));

                    use crate::interaction::{
                        request_interaction_stdin, response_as_text, InteractionRequest,
                    };
                    let req = InteractionRequest::review(
                        format!("fg-review-{}", tc.id),
                        &title,
                        &markdown,
                        &stage_name,
                    );
                    let resp = request_interaction_stdin(&req);
                    let user_feedback = response_as_text(&resp);
                    let result = if user_feedback.trim().is_empty() {
                        "User reviewed the document and acknowledged.".to_string()
                    } else {
                        format!("User feedback: {}", user_feedback)
                    };
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
                        format!("[denied] Tool '{}' is not permitted for this run.", tc.name)
                    }
                    ToolPolicy::Ask => {
                        // Foreground: ask via stdin
                        use crate::interaction::{
                            request_interaction_stdin, response_approved, ApprovalScope,
                            InteractionRequest,
                        };
                        let req = InteractionRequest::tool_approval(
                            format!("fg-{}", tc.id),
                            &tc.name,
                            tc.arguments.clone(),
                            "tool-call",
                        );
                        let resp = request_interaction_stdin(&req);
                        if response_approved(&resp) {
                            if resp.scope == Some(ApprovalScope::Session) {
                                session_al.lock().await.insert(tc.name.clone());
                            }
                            if is_builtin {
                                builtins.execute(&tc.name, tc.arguments.clone()).await
                            } else {
                                let mut mcp_lock = mcp.lock().await;
                                match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                                    Ok(r) if r.success => r.text,
                                    Ok(r) => format!("[error] {}", r.text),
                                    Err(e) => format!("[error] tool error: {}", e),
                                }
                            }
                        } else {
                            format!("[denied] User declined tool call '{}'.", tc.name)
                        }
                    }
                    ToolPolicy::Allow => {
                        if is_builtin {
                            builtins.execute(&tc.name, tc.arguments.clone()).await
                        } else {
                            let mut mcp_lock = mcp.lock().await;
                            match mcp_lock.execute(&tc.name, tc.arguments.clone()).await {
                                Ok(r) if r.success => r.text,
                                Ok(r) => format!("[error] {}", r.text),
                                Err(e) => format!("[error] tool error: {}", e),
                            }
                        }
                    }
                };
                out.push((tc.id.clone(), res));
            }
            out
        }
    };

    let compaction_config = blueprint.compaction_config.clone();
    let compaction_ref = compaction_config.as_ref();

    // ─── Graph stage loop ────────────────────────────────────────────────────
    let entry_name = blueprint.resolve_entry_stage_name();
    let mut current_stage_name_val = entry_name;
    let mut current_stage_idx = blueprint
        .stages
        .iter()
        .position(|s| s.name == current_stage_name_val)
        .unwrap_or(0);
    let mut visit_counts: HashMap<String, usize> = HashMap::new();

    loop {
        let stage = blueprint
            .find_stage(&current_stage_name_val)
            .ok_or_else(|| anyhow::anyhow!("Stage '{}' not found", current_stage_name_val))?;

        let provider_name = &stage.model.provider;
        let model_name = args.model.as_deref().unwrap_or(&stage.model.model);

        // Update current stage permissions and name for the executor closure
        {
            let mut sp = current_stage_perms.lock().await;
            *sp = stage.tool_permissions.clone();
        }
        {
            let mut sn = current_stage_name.lock().await;
            *sn = stage.name.clone();
        }

        if !engine.providers().has(provider_name) {
            println!(
                "\nProvider '{}' is not configured. Please set an API key in ~/.leviath/config.toml",
                provider_name
            );
            println!("\nExample config:");
            println!("  [providers]");
            println!("  anthropic_api_key = \"sk-ant-...\"");
            println!("\nOr set the ANTHROPIC_API_KEY environment variable.");
            println!("\nOr use Claude Code (no API key needed):");
            println!("  [stages.main]");
            println!("  model = {{ provider = \"claude-code\", model = \"claude-sonnet-4-6\" }}");
            return Ok(());
        }

        let visit_num = visit_counts.get(&stage.name).copied().unwrap_or(0);
        let visit_label = if visit_num > 0 {
            format!(" (visit {})", visit_num + 1)
        } else {
            String::new()
        };
        println!(
            "\n--- Stage {}: {} ({}:{}){} ---",
            current_stage_idx + 1,
            stage.name,
            provider_name,
            model_name,
            visit_label,
        );

        if provider_name == "claude-code" {
            println!("⚠️  This stage uses the claude-code provider.");
            println!("   Tool routing, per-stage filtering, and prompt caching are not available.");
            println!("   For full features, use provider = \"anthropic\" with an API key.");
            println!();
        }

        // Update accepts_messages on the agent state for this stage
        if let Some(mut state) = engine.world_mut().get_mut::<AgentState>(entity) {
            state.accepts_messages = stage.accepts_messages;
        }

        if stage.accepts_messages {
            println!("💬 Type a message and press Enter to send input to the agent while it runs.");
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

        // Spawn a background stdin reader to accept mid-run messages
        let stdin_handle = if stage.accepts_messages {
            let message_tx = engine.get_message_sender();
            let stdin_agent_id = agent_id.clone();
            Some(tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let stdin = tokio::io::stdin();
                let reader = tokio::io::BufReader::new(stdin);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let _ = message_tx.send(AgentMessage {
                        agent_id: stdin_agent_id.clone(),
                        content: trimmed,
                        target_region: None,
                        priority: 10,
                    });
                }
            }))
        } else {
            None
        };

        // Determine stage result for transition condition evaluation
        let stage_result_val: StageResult;

        match &stage.mode {
            StageMode::Interactive => {
                run_interactive_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    None, // foreground — use stdin
                    &stage.name,
                    &mut exec,
                )
                .await?;
                stage_result_val = StageResult::Success;
            }
            StageMode::InteractivePoints { points } => {
                run_interactive_points_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    routing_ref,
                    compaction_ref,
                    points,
                    None, // foreground — use stdin
                    &mut exec,
                )
                .await?;
                stage_result_val = StageResult::Success;
            }
            StageMode::Autonomous => {
                match run_autonomous_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    &effective_tools,
                    routing_ref,
                    compaction_ref,
                    &mut exec,
                )
                .await
                {
                    Ok(()) => {
                        // Check if we hit max_iterations by looking at iteration count
                        if let Some(state) = engine.world().get::<AgentState>(entity) {
                            if stage.max_iterations.is_some() && state.iteration >= max_iterations {
                                stage_result_val = StageResult::MaxIterations;
                            } else {
                                stage_result_val = StageResult::Success;
                            }
                        } else {
                            stage_result_val = StageResult::Success;
                        }
                    }
                    Err(e) => {
                        // Check if error edges exist; if so, route to error handler
                        if is_graph_mode(&blueprint) {
                            println!("Stage error: {} — checking error transitions", e);
                            stage_result_val = StageResult::Error;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        }

        // Cancel the stdin reader task now that the stage is complete
        if let Some(handle) = stdin_handle {
            handle.abort();
        }

        *visit_counts.entry(stage.name.clone()).or_default() += 1;

        // Resolve the next transition
        // We need to clone the stage name before passing blueprint as &mut
        let stage_name_owned = current_stage_name_val.clone();
        let stage_ref = blueprint.find_stage(&stage_name_owned).unwrap();
        let transition = resolve_transition(
            stage_ref,
            current_stage_idx,
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
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                    let marker = format!(
                        "[Stage complete: {}, transitioning to: {}]",
                        stage_name_owned, next_name
                    );
                    let tokens = marker.len() / 4 + 1;
                    let _ = window.add_to_region("conversation", marker, tokens);
                }

                // Apply edge transform
                apply_edge_transform(
                    &edge,
                    &visit_counts,
                    &mut engine,
                    entity,
                    &edge.target, // use target's provider — but we'll use current for simplicity
                    model_name,
                    compaction_ref,
                )
                .await;

                current_stage_name_val = next_name;
                current_stage_idx = next_idx;
            }
            None => {
                break; // terminal: no valid transitions
            }
        }
    }

    println!("\n[All stages complete]");
    tool_registry.shutdown().await;
    Ok(())
}
