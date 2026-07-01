//! Foreground (inline, blocking) run mode.

use async_trait::async_trait;
use leviath_core::blueprint::StageResult;
use leviath_providers::InferenceResponse;
use leviath_runtime::{AgentMessage, AgentPool, AgentState, ToolResultRoutingConfig};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::{Config, ToolPolicy};
use crate::runstate::RunMeta;
use crate::tools::{resolve_policy, ToolRegistry};

use super::executor::{run_stage_loop, StageCallbacks, StageContext};
use super::helpers::initialize_context_window;
use super::io::{ConsoleIO, RunIO};
use super::manifest::{find_manifest, parse_manifest};
use super::session::{build_provider_registry, resolve_task};
use super::stages::run_autonomous_stage;
use super::RunArgs;

/// Foreground [`InteractionBackend`](super::dynamic_interaction::InteractionBackend):
/// answers via stdin and prints the review document directly (no per-stage
/// log file to persist to in foreground mode).
struct ForegroundInteractionBackend;

#[async_trait]
impl super::dynamic_interaction::InteractionBackend for ForegroundInteractionBackend {
    async fn ask(
        &self,
        req: crate::interaction::InteractionRequest,
    ) -> crate::interaction::InteractionResponse {
        crate::interaction::request_interaction_stdin(&req)
    }

    fn on_review_document(&self, _tool_call_id: &str, title: &str, markdown: &str) {
        println!("\n{}", "\u{2500}".repeat(60));
        println!("  {}", title);
        println!("{}", "\u{2500}".repeat(60));
        println!("{}", markdown);
        println!("{}", "\u{2500}".repeat(60));
    }
}

/// Foreground-specific callbacks for the unified stage loop.
struct ForegroundCallbacks {}

#[async_trait]
impl StageCallbacks for ForegroundCallbacks {
    async fn on_provider_missing(&mut self, provider: &str, _stage_idx: usize) -> bool {
        println!(
            "\nProvider '{}' is not configured. Please set an API key in ~/.leviath/config.toml",
            provider
        );
        println!("\nExample config:");
        println!("  [providers]");
        println!("  anthropic_api_key = \"sk-ant-...\"");
        println!("\nOr set the ANTHROPIC_API_KEY environment variable.");
        println!("\nOr use Claude Code (no API key needed):");
        println!("  [stages.main]");
        println!("  model = {{ provider = \"claude-code\", model = \"claude-sonnet-4-6\" }}");
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
        println!(
            "\n--- Stage {}: {} ({}:{}){} ---",
            stage_idx + 1,
            stage_name,
            provider,
            model,
            visit_label,
        );
    }

    async fn on_claude_code_warning(&mut self, _stage_idx: usize) {
        println!("\u{26a0}\u{fe0f}  This stage uses the claude-code provider.");
        println!("   Tool routing, per-stage filtering, and prompt caching are not available.");
        println!("   For full features, use provider = \"anthropic\" with an API key.");
        println!();
    }

    fn start_message_reader(
        &mut self,
        engine: &leviath_runtime::AgentEngine,
        agent_id: &str,
        accepts: bool,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !accepts {
            return None;
        }
        let message_tx = engine.get_message_sender();
        let stdin_agent_id = agent_id.to_string();
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
    }

    fn get_run_context(&mut self) -> Option<(&str, &mut RunMeta)> {
        None // foreground uses stdin
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
        io: &mut dyn RunIO,
        executor: &mut F,
    ) -> anyhow::Result<(StageResult, Option<InferenceResponse>)>
    where
        F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
        Fut: std::future::Future<Output = Vec<(String, String)>> + Send,
    {
        run_autonomous_stage(
            engine,
            entity,
            provider,
            model,
            max_iterations,
            &tools,
            routing,
            compaction,
            io,
            executor,
        )
        .await?;

        // Determine stage result
        let result = if let Some(state) = engine.world().get::<AgentState>(entity) {
            if state.iteration >= max_iterations {
                StageResult::MaxIterations
            } else {
                StageResult::Success
            }
        } else {
            StageResult::Success
        };

        Ok((result, None))
    }

    async fn on_stage_result(
        &mut self,
        _stage_name: &str,
        _stage_idx: usize,
        _result: &StageResult,
        _response: Option<&InferenceResponse>,
        _engine: &mut leviath_runtime::AgentEngine,
        _entity: bevy_ecs::prelude::Entity,
    ) {
        // Foreground: output is printed during execution; no-op here
    }

    async fn on_stage_error(
        &mut self,
        _stage_name: &str,
        _stage_idx: usize,
        error: &anyhow::Error,
        is_graph_mode: bool,
    ) -> Option<StageResult> {
        if is_graph_mode {
            println!("Stage error: {} \u{2014} checking error transitions", error);
            Some(StageResult::Error)
        } else {
            None // propagate error
        }
    }

    async fn on_transition(&mut self, _from_stage: &str, _to_stage: &str, _stage_idx: usize) {
        // Foreground: no-op
    }

    async fn on_complete(&mut self, _last_stage_idx: usize) {
        println!("\n[All stages complete]");
    }

    async fn on_post_stage(
        &mut self,
        _engine: &leviath_runtime::AgentEngine,
        _entity: bevy_ecs::prelude::Entity,
        _stage_name: &str,
    ) {
        // Foreground: no-op
    }
}

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
    // Current stage index (shared with executor)
    let current_stage_idx: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

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
                // ── Dynamic interaction tools (present_for_review, ask_user_*) ──
                // Unlike `interaction_points` (declared statically in the
                // blueprint and always shown), these let the model itself
                // decide, mid-reasoning, that it needs human input.
                let stage_name = stage_nm.lock().await.clone();
                let interaction_backend = ForegroundInteractionBackend;
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

    let mut callbacks = ForegroundCallbacks {};
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

    #[test]
    fn foreground_callbacks_construction() {
        let _cb = ForegroundCallbacks {};
    }

    #[tokio::test]
    async fn foreground_on_provider_missing_returns_true() {
        let mut cb = ForegroundCallbacks {};
        let result = cb.on_provider_missing("nonexistent", 0).await;
        assert!(result, "on_provider_missing should return true (abort)");
    }

    #[tokio::test]
    async fn foreground_on_stage_enter_does_not_panic() {
        let mut cb = ForegroundCallbacks {};
        cb.on_stage_enter("plan", 0, "anthropic", "claude-sonnet-4-6", "")
            .await;
        cb.on_stage_enter("code", 1, "openai", "gpt-5", " (visit 2)")
            .await;
    }

    #[tokio::test]
    async fn foreground_on_claude_code_warning_does_not_panic() {
        let mut cb = ForegroundCallbacks {};
        cb.on_claude_code_warning(0).await;
    }

    #[test]
    fn foreground_get_run_context_returns_none() {
        let mut cb = ForegroundCallbacks {};
        assert!(cb.get_run_context().is_none());
    }

    #[tokio::test]
    async fn foreground_on_stage_result_is_noop() {
        let mut cb = ForegroundCallbacks {};
        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(leviath_core::Blueprint::new(
            "test".to_string(),
            "test".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 0),
        ));
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        cb.on_stage_result("main", 0, &StageResult::Success, None, &mut engine, entity)
            .await;
    }

    #[tokio::test]
    async fn foreground_on_stage_error_graph_mode() {
        let mut cb = ForegroundCallbacks {};
        let err = anyhow::anyhow!("test error");
        let result = cb.on_stage_error("main", 0, &err, true).await;
        assert_eq!(result, Some(StageResult::Error));
    }

    #[tokio::test]
    async fn foreground_on_stage_error_linear_mode() {
        let mut cb = ForegroundCallbacks {};
        let err = anyhow::anyhow!("test error");
        let result = cb.on_stage_error("main", 0, &err, false).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn foreground_on_transition_is_noop() {
        let mut cb = ForegroundCallbacks {};
        cb.on_transition("plan", "code", 0).await;
    }

    #[tokio::test]
    async fn foreground_on_complete_does_not_panic() {
        let mut cb = ForegroundCallbacks {};
        cb.on_complete(2).await;
    }

    #[tokio::test]
    async fn foreground_on_post_stage_is_noop() {
        let mut cb = ForegroundCallbacks {};
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let entity = bevy_ecs::prelude::Entity::from_raw(0);
        cb.on_post_stage(&engine, entity, "main").await;
    }

    #[test]
    fn foreground_start_message_reader_returns_none_when_not_accepts() {
        let mut cb = ForegroundCallbacks {};
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let handle = cb.start_message_reader(&engine, "agent-1", false);
        assert!(handle.is_none(), "Should return None when accepts is false");
    }

    #[tokio::test]
    async fn foreground_on_stage_enter_with_visit_label() {
        let mut cb = ForegroundCallbacks {};
        // Should not panic
        cb.on_stage_enter("review", 2, "openai", "gpt-5", " (visit 3)")
            .await;
    }

    #[tokio::test]
    async fn foreground_on_complete_multiple_stages() {
        let mut cb = ForegroundCallbacks {};
        cb.on_complete(5).await;
    }

    #[tokio::test]
    async fn foreground_on_transition_is_noop_different_stages() {
        let mut cb = ForegroundCallbacks {};
        cb.on_transition("plan", "implement", 0).await;
        cb.on_transition("implement", "review", 1).await;
    }

    #[tokio::test]
    async fn foreground_on_stage_result_no_response_is_noop() {
        let mut cb = ForegroundCallbacks {};
        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(leviath_core::Blueprint::new(
            "test".to_string(),
            "test".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 0),
        ));
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        cb.on_stage_result(
            "main",
            0,
            &StageResult::MaxIterations,
            None,
            &mut engine,
            entity,
        )
        .await;
    }

    #[tokio::test]
    async fn foreground_start_message_reader_returns_handle_when_accepts() {
        let rt = tokio::runtime::Handle::current();
        let _ = rt; // ensure we are in async context

        let mut cb = ForegroundCallbacks {};
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        // When accepts=true, a JoinHandle is spawned for stdin reading
        let handle = cb.start_message_reader(&engine, "agent-1", true);
        assert!(
            handle.is_some(),
            "Should return Some(JoinHandle) when accepts is true"
        );
        // Abort it immediately to avoid blocking
        if let Some(h) = handle {
            h.abort();
        }
    }

    #[tokio::test]
    async fn foreground_on_stage_error_various_stages() {
        let mut cb = ForegroundCallbacks {};
        // Linear mode: returns None
        let err = anyhow::anyhow!("stage failed");
        assert!(cb.on_stage_error("plan", 0, &err, false).await.is_none());
        assert!(cb.on_stage_error("code", 1, &err, false).await.is_none());
        // Graph mode: returns Some(Error)
        assert_eq!(
            cb.on_stage_error("review", 2, &err, true).await,
            Some(StageResult::Error)
        );
    }

    #[tokio::test]
    async fn foreground_callbacks_complete_sequence() {
        let mut cb = ForegroundCallbacks {};

        // Simulate a complete stage lifecycle
        cb.on_stage_enter("plan", 0, "anthropic", "claude-sonnet-4-6", "")
            .await;
        cb.on_claude_code_warning(0).await;

        let registry = leviath_runtime::ProviderRegistry::new();
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(leviath_core::Blueprint::new(
            "test".to_string(),
            "test".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 0),
        ));
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();

        cb.on_stage_result("plan", 0, &StageResult::Success, None, &mut engine, entity)
            .await;
        cb.on_transition("plan", "code", 0).await;
        cb.on_stage_enter("code", 1, "anthropic", "claude-sonnet-4-6", "")
            .await;
        cb.on_stage_result("code", 1, &StageResult::Success, None, &mut engine, entity)
            .await;
        cb.on_complete(1).await;
        cb.on_post_stage(&engine, entity, "code").await;
    }

    #[test]
    fn foreground_callbacks_all_provider_missing_messages() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut cb = ForegroundCallbacks {};
            // Test with various provider names
            for provider in ["anthropic", "openai", "google", "openrouter", "ollama"] {
                let result = cb.on_provider_missing(provider, 0).await;
                assert!(result, "on_provider_missing should always return true");
            }
        });
    }

    // ─── ForegroundInteractionBackend ───────────────────────────────────────
    //
    // `ask()` delegates to `request_interaction_stdin`, which blocks reading
    // real stdin — there's no seam to mock that without either piping real
    // input into the test process or a deeper refactor of `interaction.rs`
    // itself, so it's intentionally not covered here (it never has been,
    // even before this code was extracted from the inline exec closure).

    #[test]
    fn foreground_interaction_backend_on_review_document_does_not_panic() {
        use crate::commands::run::dynamic_interaction::InteractionBackend;
        let backend = ForegroundInteractionBackend;
        backend.on_review_document("call-1", "Title", "# Markdown body");
    }
}
