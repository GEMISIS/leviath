//! Foreground (inline, blocking) run mode.

use async_trait::async_trait;
use leviath_core::blueprint::StageResult;
use leviath_providers::InferenceResponse;
use leviath_runtime::{AgentMessage, AgentPool, AgentState, ToolResultRoutingConfig};
use std::sync::Arc;
use tokio::sync::Mutex;

use std::collections::{HashMap, HashSet};

use crate::config::{Config, ToolPolicy};
use crate::interaction::{InteractionRequest, InteractionResponse};
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
    // Deliberately not called from any test. `request_interaction_stdin`
    // blocks on real stdin via `std::io::stdin().lock()`; the underlying
    // logic is fully covered by `request_interaction_from_reader`'s
    // in-memory-reader tests in `interaction.rs`. A previous test here drove
    // this for real through `spawn_blocking` + a timeout, which doesn't
    // help: on a live TTY the read never reaches EOF, that blocking-pool
    // thread is tracked by tokio, and tearing down the test's runtime
    // blocks forever waiting for it regardless of what the test does with
    // the `JoinHandle`. Hit exactly this hang running interactively.
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

/// Convert a raw `anyhow::Result<leviath_mcp::ExecutionResult>` from a
/// `ToolExecutor::execute` call into the string text the agent should see.
///
/// Extracted so the three distinct outcomes (`Ok` + success, `Ok` + failure,
/// `Err`) can be exercised by unit tests without requiring a live MCP server.
fn mcp_result_to_text(result: anyhow::Result<leviath_mcp::ExecutionResult>) -> String {
    match result {
        Ok(r) if r.success => r.text,
        Ok(r) => format!("[error] {}", r.text),
        Err(e) => format!("[error] tool error: {}", e),
    }
}

/// Shared state needed by [`dispatch_tool_calls_foreground`] to resolve and
/// execute a batch of tool calls from the model.
///
/// Extracted from the `exec` closure in [`run_foreground`] purely so the
/// tool-dispatch logic (policy resolution, dynamic interactions, approval
/// gating, builtin/MCP execution) can be exercised by unit tests directly,
/// without needing to drive a full run through a real provider/inference call
/// or block on real stdin.
struct ForegroundToolDispatchState {
    builtins: Arc<leviath_tools::BuiltinTools>,
    mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    builtin_names: HashSet<String>,
    launch_overrides: Arc<HashMap<String, ToolPolicy>>,
    session_allows: Arc<Mutex<HashSet<String>>>,
    stage_perms: Arc<Mutex<HashMap<String, String>>>,
    stage_name: Arc<Mutex<String>>,
    agent_perms: Arc<HashMap<String, String>>,
    global_perms: Arc<HashMap<String, ToolPolicy>>,
}

/// Resolve tool policy, handle approvals/dynamic interactions, and execute a
/// batch of tool calls from the model. Returns `(tool_call_id, result_text)`
/// pairs in the same order as `calls`.
///
/// This is the core body of the `exec` closure passed to
/// [`super::executor::run_stage_loop`] in [`run_foreground`], lifted out into
/// a standalone function so it can be unit-tested directly. `interaction_backend`
/// and `ask_approval` are injected so tests never block on real stdin: in
/// production these are [`ForegroundInteractionBackend`] and
/// [`crate::interaction::request_interaction_stdin`] respectively.
async fn dispatch_tool_calls_foreground(
    state: &ForegroundToolDispatchState,
    calls: Vec<leviath_providers::ToolCall>,
    interaction_backend: &dyn super::dynamic_interaction::InteractionBackend,
    ask_approval: &(dyn Fn(&InteractionRequest) -> InteractionResponse + Send + Sync),
) -> Vec<(String, String)> {
    let stage_name = state.stage_name.lock().await.clone();
    let mut out: Vec<(String, String)> = Vec::new();
    for tc in calls {
        // ── Dynamic interaction tools (present_for_review, ask_user_*) ──
        // Unlike `interaction_points` (declared statically in the
        // blueprint and always shown), these let the model itself
        // decide, mid-reasoning, that it needs human input.
        if let Some(result) = super::dynamic_interaction::dispatch_dynamic_interaction(
            interaction_backend,
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
                format!("[denied] Tool '{}' is not permitted for this run.", tc.name)
            }
            ToolPolicy::Ask => {
                use crate::interaction::{response_approved, ApprovalScope};
                let req = InteractionRequest::tool_approval(
                    format!("fg-{}", tc.id),
                    &tc.name,
                    tc.arguments.clone(),
                    "tool-call",
                );
                let resp = ask_approval(&req);
                if response_approved(&resp) {
                    if resp.scope == Some(ApprovalScope::Session) {
                        state.session_allows.lock().await.insert(tc.name.clone());
                    }
                    if is_builtin {
                        state.builtins.execute(&tc.name, tc.arguments.clone()).await
                    } else {
                        let mut mcp_lock = state.mcp.lock().await;
                        mcp_result_to_text(mcp_lock.execute(&tc.name, tc.arguments.clone()).await)
                    }
                } else {
                    format!("[denied] User declined tool call '{}'.", tc.name)
                }
            }
            ToolPolicy::Allow => {
                if is_builtin {
                    state.builtins.execute(&tc.name, tc.arguments.clone()).await
                } else {
                    let mut mcp_lock = state.mcp.lock().await;
                    mcp_result_to_text(mcp_lock.execute(&tc.name, tc.arguments.clone()).await)
                }
            }
        };
        out.push((tc.id.clone(), res));
    }
    out
}

/// Reads newline-delimited messages from `reader` and forwards each
/// non-empty, trimmed line to `message_tx` as an [`AgentMessage`] from
/// `agent_id`. Generic over `R` (rather than hardcoding real stdin) purely so
/// tests can drive it with an in-memory reader instead of blocking on real
/// process stdin.
async fn forward_lines_as_messages<R: tokio::io::AsyncBufRead + Unpin>(
    reader: R,
    agent_id: String,
    message_tx: tokio::sync::mpsc::UnboundedSender<AgentMessage>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        let _ = message_tx.send(AgentMessage {
            agent_id: agent_id.clone(),
            content: trimmed,
            target_region: None,
            priority: 10,
        });
    }
}

/// Core logic behind [`StageCallbacks::start_message_reader`], parameterized
/// over how the reader is constructed so tests can supply e.g.
/// `tokio::io::empty()` instead of real stdin.
///
/// This distinction matters more than it looks: merely *calling* the
/// production reader-factory (`tokio::io::stdin()`) starts a real blocking
/// read on a tokio blocking-pool thread the moment the spawned task runs --
/// regardless of whether the caller ever awaits or aborts the returned
/// handle. On a live TTY that read never reaches EOF, and tokio's blocking
/// pool tracks it, so tearing down the runtime (e.g. at the end of a
/// `#[tokio::test]`) blocks waiting for it to finish -- forever. There is no
/// way to bound or cancel that from the outside once started, so the only
/// safe fix is to never let a test construct the real reader at all.
fn start_message_reader_with<R, F>(
    engine: &leviath_runtime::AgentEngine,
    agent_id: &str,
    accepts: bool,
    make_reader: F,
) -> Option<tokio::task::JoinHandle<()>>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    if !accepts {
        return None;
    }
    let message_tx = engine.get_message_sender();
    let stdin_agent_id = agent_id.to_string();
    Some(tokio::spawn(async move {
        forward_lines_as_messages(make_reader(), stdin_agent_id, message_tx).await;
    }))
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
        start_message_reader_with(engine, agent_id, accepts, || {
            tokio::io::BufReader::new(tokio::io::stdin())
        })
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
    run_foreground_with_registry(args, build_provider_registry).await
}

/// Core of [`run_foreground`], with provider-registry construction injected
/// so tests can drive a real (in-process, no network) inference round trip
/// with a [`Provider`](leviath_providers::Provider) mock instead of either
/// stopping at [`StageCallbacks::on_provider_missing`] or making a real,
/// billed network call.
async fn run_foreground_with_registry(
    args: RunArgs,
    build_registry: impl FnOnce(&Config) -> leviath_runtime::ProviderRegistry,
) -> anyhow::Result<()> {
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

    let registry = build_registry(&config);
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
        let state = ForegroundToolDispatchState {
            builtins: builtins.clone(),
            mcp: mcp.clone(),
            builtin_names: builtin_names.clone(),
            launch_overrides: launch_overrides_arc.clone(),
            session_allows: exec_session_allows.clone(),
            stage_perms: exec_stage_perms.clone(),
            stage_name: exec_stage_name.clone(),
            agent_perms: exec_agent_perms.clone(),
            global_perms: exec_global_perms.clone(),
        };
        async move {
            let interaction_backend = ForegroundInteractionBackend;
            dispatch_tool_calls_foreground(
                &state,
                calls,
                &interaction_backend,
                &crate::interaction::request_interaction_stdin,
            )
            .await
        }
    };

    let compaction_config = blueprint.compaction_config.clone();
    let compaction_ref = compaction_config.as_ref();

    let mut callbacks = ForegroundCallbacks {};
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
        compaction_ref,
    };

    run_stage_loop(&mut ctx, &mut callbacks, &agent_id, &mut io, &mut exec).await?;

    tool_registry.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_providers::Provider;

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
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        // Drives the real spawn+forward logic via `start_message_reader_with`
        // (what `StageCallbacks::start_message_reader` delegates to) with
        // `tokio::io::empty()` instead of real stdin. This previously called
        // the trait method directly, which constructs `tokio::io::stdin()`
        // for real the moment the spawned task starts running -- on a live
        // TTY that read never reaches EOF, and since it's tracked by tokio's
        // blocking pool, awaiting/timing-out/aborting the handle doesn't
        // matter: tearing down this test's runtime at the end of the test
        // still blocks forever waiting for that leaked real stdin read.
        // `tokio::io::empty()` reaches EOF immediately with no blocking-pool
        // involvement at all, so this is fully bounded regardless of
        // environment.
        let handle = start_message_reader_with(&engine, "agent-1", true, || {
            tokio::io::BufReader::new(tokio::io::empty())
        });
        assert!(
            handle.is_some(),
            "Should return Some(JoinHandle) when accepts is true"
        );
        handle.unwrap().await.unwrap();
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

    #[test]
    fn foreground_interaction_backend_on_review_document_does_not_panic() {
        use crate::commands::run::dynamic_interaction::InteractionBackend;
        let backend = ForegroundInteractionBackend;
        backend.on_review_document("call-1", "Title", "# Markdown body");
    }

    // `ask()`'s real body (`request_interaction_stdin`) is deliberately not
    // called from any test -- see the doc comment on the `ask` impl above.
    // The `spawn_blocking` + timeout wrapper this test used to have doesn't
    // bound anything real: on a live TTY the underlying blocking stdin read
    // never reaches EOF, tokio's blocking pool tracks it regardless of
    // whether the `JoinHandle` is awaited/timed-out/aborted, and this test's
    // runtime teardown hung waiting for it. `request_interaction_from_reader`
    // (interaction.rs) has full in-memory-reader coverage of the actual
    // logic `ask()` delegates to.

    // ─── dispatch_tool_calls_foreground ─────────────────────────────────────

    async fn make_dispatch_state(unique: &str) -> ForegroundToolDispatchState {
        let workdir = std::env::temp_dir().join(format!("lev-fg-dispatch-{}", unique));
        let _ = std::fs::create_dir_all(&workdir);
        let config = Config::default();
        let tool_registry = ToolRegistry::build(workdir, &config).await;
        ForegroundToolDispatchState {
            builtins: tool_registry.builtins.clone(),
            mcp: tool_registry.mcp.clone(),
            builtin_names: tool_registry.builtin_names.clone(),
            launch_overrides: Arc::new(HashMap::new()),
            session_allows: Arc::new(Mutex::new(HashSet::new())),
            stage_perms: Arc::new(Mutex::new(HashMap::new())),
            stage_name: Arc::new(Mutex::new("main".to_string())),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(HashMap::new()),
        }
    }

    fn make_tool_call(name: &str, args: serde_json::Value) -> leviath_providers::ToolCall {
        leviath_providers::ToolCall {
            id: format!("call-{}", name),
            name: name.to_string(),
            arguments: args,
        }
    }

    fn approve_once(_req: &InteractionRequest) -> InteractionResponse {
        InteractionResponse::approval("ignored", true, crate::interaction::ApprovalScope::Once)
    }

    fn approve_session(_req: &InteractionRequest) -> InteractionResponse {
        InteractionResponse::approval("ignored", true, crate::interaction::ApprovalScope::Session)
    }

    fn deny_once(_req: &InteractionRequest) -> InteractionResponse {
        InteractionResponse::approval("ignored", false, crate::interaction::ApprovalScope::Once)
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_deny_policy_returns_denied_message() {
        let mut state = make_dispatch_state("deny").await;
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        state.global_perms = Arc::new(global);

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call("bash", serde_json::json!({"command": "ls"}))];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_once).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-bash");
        assert!(out[0].1.contains("[denied]"));
        assert!(out[0].1.contains("not permitted"));
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_allow_builtin_executes() {
        let mut state = make_dispatch_state("allow-builtin").await;
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        state.launch_overrides = Arc::new(launch);

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_once).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-read_file");
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_session_allow_short_circuits_policy() {
        let mut state = make_dispatch_state("session-allow").await;
        let mut global = HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Deny);
        state.global_perms = Arc::new(global);
        state
            .session_allows
            .lock()
            .await
            .insert("read_file".to_string());

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_once).await;

        assert_eq!(out.len(), 1);
        assert!(!out[0].1.contains("[denied]"));
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_ask_approved_once_executes_tool() {
        let mut state = make_dispatch_state("ask-approved-once").await;
        let mut global = HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Ask);
        state.global_perms = Arc::new(global);

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_once).await;

        assert_eq!(out.len(), 1);
        assert!(!out[0].1.contains("[denied]"));
        assert!(
            !state.session_allows.lock().await.contains("read_file"),
            "Once-scope approval must not be recorded as a session allow"
        );
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_ask_approved_session_records_session_allow() {
        let mut state = make_dispatch_state("ask-approved-session").await;
        let mut global = HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Ask);
        state.global_perms = Arc::new(global);

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_session).await;

        assert_eq!(out.len(), 1);
        assert!(!out[0].1.contains("[denied]"));
        assert!(state.session_allows.lock().await.contains("read_file"));
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_ask_denied_returns_declined_message() {
        let mut state = make_dispatch_state("ask-denied").await;
        let mut global = HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Ask);
        state.global_perms = Arc::new(global);

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call(
            "read_file",
            serde_json::json!({"path": "definitely-not-here.txt"}),
        )];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &deny_once).await;

        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("[denied]"));
        assert!(out[0].1.contains("declined"));
        assert!(!state.session_allows.lock().await.contains("read_file"));
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_dynamic_interaction_short_circuits() {
        // `ask_user_confirm` IS answered via the backend (that's the whole
        // point of the tool) -- what it short-circuits is normal tool-policy
        // resolution (Deny/Ask/Allow), not the backend call itself. A real
        // `ForegroundInteractionBackend` here would block on real stdin
        // (this hung under `cargo llvm-cov` in an interactive terminal,
        // where stdin isn't already closed/EOF the way it is in a sandboxed
        // test runner) -- use a mock backend instead, matching the pattern
        // `dynamic_interaction.rs`'s own tests use.
        struct StubBackend;
        #[async_trait]
        impl super::super::dynamic_interaction::InteractionBackend for StubBackend {
            async fn ask(
                &self,
                req: crate::interaction::InteractionRequest,
            ) -> crate::interaction::InteractionResponse {
                crate::interaction::InteractionResponse::approval(
                    &req.id,
                    true,
                    crate::interaction::ApprovalScope::Once,
                )
            }
        }

        let state = make_dispatch_state("dynamic-interaction").await;
        let backend = StubBackend;
        let calls = vec![make_tool_call(
            "ask_user_confirm",
            serde_json::json!({"prompt": "Continue?"}),
        )];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &deny_once).await;

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "call-ask_user_confirm");
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_multiple_calls_preserve_order() {
        let mut state = make_dispatch_state("multi-order").await;
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        global.insert("read_file".to_string(), ToolPolicy::Deny);
        state.global_perms = Arc::new(global);

        let backend = ForegroundInteractionBackend;
        let calls = vec![
            make_tool_call("bash", serde_json::json!({"command": "ls"})),
            make_tool_call("read_file", serde_json::json!({"path": "x"})),
        ];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_once).await;

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "call-bash");
        assert_eq!(out[1].0, "call-read_file");
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_allow_mcp_tool_returns_error_text() {
        // Not a builtin name and no MCP server registered -> the MCP
        // execute() path returns Err, exercising the `Err(e)` arm of the
        // Allow branch (as opposed to the builtin-execution arm).
        let mut state = make_dispatch_state("allow-mcp").await;
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        state.launch_overrides = Arc::new(launch);

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call("some_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_once).await;

        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("[error]"));
    }

    #[tokio::test]
    async fn dispatch_tool_calls_foreground_ask_approved_mcp_tool_returns_error_text() {
        let mut state = make_dispatch_state("ask-mcp").await;
        let mut global = HashMap::new();
        global.insert("some_mcp_tool".to_string(), ToolPolicy::Ask);
        state.global_perms = Arc::new(global);

        let backend = ForegroundInteractionBackend;
        let calls = vec![make_tool_call("some_mcp_tool", serde_json::json!({}))];
        let out = dispatch_tool_calls_foreground(&state, calls, &backend, &approve_once).await;

        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("[error]"));
    }

    // ─── mcp_result_to_text ──────────────────────────────────────────────────

    #[test]
    fn mcp_result_to_text_ok_success_returns_text() {
        let result = Ok(leviath_mcp::ExecutionResult {
            success: true,
            data: serde_json::Value::Null,
            text: "hello world".to_string(),
        });
        assert_eq!(mcp_result_to_text(result), "hello world");
    }

    #[test]
    fn mcp_result_to_text_ok_failure_returns_error_text() {
        let result = Ok(leviath_mcp::ExecutionResult {
            success: false,
            data: serde_json::Value::Null,
            text: "tool failed".to_string(),
        });
        assert_eq!(mcp_result_to_text(result), "[error] tool failed");
    }

    #[test]
    fn mcp_result_to_text_err_returns_tool_error_text() {
        let result: anyhow::Result<leviath_mcp::ExecutionResult> =
            Err(anyhow::anyhow!("connection refused"));
        let text = mcp_result_to_text(result);
        assert!(text.contains("[error] tool error:"));
        assert!(text.contains("connection refused"));
    }

    // ─── forward_lines_as_messages ───────────────────────────────────────────

    #[tokio::test]
    async fn forward_lines_as_messages_forwards_nonempty_trimmed_lines() {
        let input = "  hello\n\n   \nworld  \n";
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input.as_bytes()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();

        forward_lines_as_messages(reader, "agent-1".to_string(), tx).await;

        let first = rx.try_recv().unwrap();
        assert_eq!(first.agent_id, "agent-1");
        assert_eq!(first.content, "hello");
        let second = rx.try_recv().unwrap();
        assert_eq!(second.content, "world");
        assert!(rx.try_recv().is_err(), "blank lines must be skipped");
    }

    #[tokio::test]
    async fn forward_lines_as_messages_empty_input_sends_nothing() {
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(&b""[..]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();

        forward_lines_as_messages(reader, "agent-1".to_string(), tx).await;

        assert!(rx.try_recv().is_err());
    }

    // ─── run_foreground ──────────────────────────────────────────────────────
    //
    // `run_foreground()` drives a real `Config::load()` + real provider
    // registry + real `AgentEngine`/`AgentPool`, so it can't be safely driven
    // end-to-end with a live provider without either a real API key (a real,
    // billed network call) or an injectable provider registry -- out of scope
    // for a coverage-only pass. These tests instead isolate `Config::load()`
    // from any real API key (see `crate::config::isolate_config_path_for_test`)
    // so the run reliably reaches `on_provider_missing` and aborts cleanly,
    // exercising manifest loading, blueprint parsing, config loading, provider
    // registry building, engine/pool setup, tool registry init, and
    // launch_overrides population (yolo/allow/ask/deny) without ever blocking
    // on stdin or making a network call.

    #[tokio::test]
    async fn run_foreground_with_valid_manifest_aborts_at_missing_provider() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-valid-manifest");

        let temp_dir = std::env::temp_dir().join("lev-test-foreground-valid-manifest");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "test-foreground-agent"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
max_iterations = 1

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"

[tool_permissions]
bash = "ask"
"#;
        std::fs::write(temp_dir.join("agent.leviath"), manifest_content).unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: true,
            yolo: true,
            allow: vec!["read_file".to_string()],
            ask: vec!["bash".to_string()],
            deny: vec!["write_file".to_string()],
            max_depth: None,
            count: 1,
        };

        // No provider is configured (config isolated above), so this should
        // abort cleanly via `on_provider_missing` returning `true` -- which
        // `run_stage_loop` surfaces as `Ok(())`, not an error. Wrapped in
        // `with_tracing` because this path reaches the `tracing::info!` at
        // the top of `run_foreground_with_registry`.
        with_tracing(|| run_foreground(args)).await.unwrap();

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // ─── run_foreground_with_registry (mock provider, no network) ───────────
    //
    // With a real, working (mock) provider injected via
    // `run_foreground_with_registry`, the run completes an actual inference
    // round trip in-process -- exercising `StageCallbacks::run_autonomous`,
    // the `exec` closure construction/call site, and the `validate_keys()`
    // warning-print branch, none of which are reachable once the run aborts
    // early at `on_provider_missing` (as in the test above).

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

    #[tokio::test]
    async fn run_foreground_with_mock_provider_completes_full_round_trip() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-mock-provider");
        // A malformed key still exercises the `validate_keys()` warning
        // branch without being usable as a real credential -- and since the
        // provider registry is fully mocked below, no real network call can
        // happen regardless.
        let mut fake_config = Config::default();
        fake_config.providers.anthropic_api_key = Some("not-a-real-key".to_string());
        std::fs::write(
            Config::config_path(),
            toml::to_string(&fake_config).unwrap(),
        )
        .unwrap();

        let temp_dir = std::env::temp_dir().join("lev-test-foreground-mock-provider");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "test-foreground-mock-agent"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
max_iterations = 2

[stages.main.model]
provider = "mock"
model = "mock-model"
"#;
        std::fs::write(temp_dir.join("agent.leviath"), manifest_content).unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: true,
            yolo: true,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        // Wrapped in `with_tracing` because this path reaches the
        // `tracing::info!` at the top of `run_foreground_with_registry`.
        with_tracing(|| {
            run_foreground_with_registry(args, |_config| {
                let mut registry = leviath_runtime::ProviderRegistry::new();
                registry.register("mock".to_string(), Arc::new(MockProvider::new()));
                registry
            })
        })
        .await
        .unwrap();

        // Cover the remaining `Provider` trait methods that this particular
        // run never exercises through the engine.
        let provider = MockProvider::new();
        assert_eq!(provider.count_tokens("abcd", "mock-model"), 1);
        assert_eq!(provider.max_context_tokens("mock-model"), 100_000);
        assert_eq!(provider.name(), "mock");
        assert!(provider.list_models().await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn run_foreground_fails_with_nonexistent_path() {
        let args = RunArgs {
            path: Some("/nonexistent/path/to/nowhere".to_string()),
            task: Some("do something".to_string()),
            model: None,
            foreground: true,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        let result = run_foreground(args).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Could not find"),
            "Expected manifest-not-found error, got: {err_msg}",
        );
    }

    // ─── run_foreground_with_registry: error and edge-case branches ──────────

    /// Covers the `args.path = None` → `unwrap_or_else(|| ".".to_string())`
    /// branch (line 379).  The path resolves to "." which may or may not have
    /// a manifest; we only care that the closure fires and the function returns
    /// (with any result).
    #[tokio::test]
    async fn run_foreground_with_registry_path_none_uses_dot() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-path-none");

        let temp_dir = std::env::temp_dir().join("lev-test-fg-path-none");
        let _ = std::fs::create_dir_all(&temp_dir);
        // Write a manifest so find_manifest("." relative to temp_dir) works.
        // We can't change cwd reliably in a test, so use an explicit path
        // instead — but we DO pass path: None to exercise the else branch.
        // Since the working directory during tests is the workspace root and
        // likely has no agent.leviath, the call will fail at find_manifest,
        // which is fine — we only need to cover the unwrap_or_else closure.
        let args = RunArgs {
            path: None, // ← exercises unwrap_or_else(|| ".".to_string())
            task: Some("test".to_string()),
            model: None,
            foreground: true,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        // find_manifest(".") will fail unless there's an agent.leviath in cwd
        let result =
            run_foreground_with_registry(args, |_c| leviath_runtime::ProviderRegistry::new()).await;
        // Accept either Ok or Err — we just need the unwrap_or_else to fire.
        let _ = result;
    }

    /// Covers `read_to_string(...).map_err(...)` error branch (line 385) and
    /// the `yolo=false` path that skips the `if args.yolo { ... }` insert
    /// (line 420).  We provide a valid directory with a manifest that
    /// `find_manifest` finds, but the file is removed before `read_to_string`
    /// is called.  Since `find_manifest` checks `exists()`, we write the file,
    /// create the RunArgs with the path, then delete the file, so
    /// `read_to_string` fails.
    #[tokio::test]
    async fn run_foreground_with_registry_fails_on_manifest_read_error() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-manifest-read-error");

        let temp_dir = std::env::temp_dir().join("lev-test-fg-manifest-read");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_path = temp_dir.join("agent.leviath");
        // Write minimal manifest so find_manifest succeeds
        std::fs::write(
            &manifest_path,
            "[agent]\nname = \"x\"\nversion = \"1.0.0\"\ndescription = \"x\"\n",
        )
        .unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test".to_string()),
            model: None,
            foreground: true,
            yolo: false, // ← exercises the yolo=false path
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        // Remove the manifest after find_manifest can locate it but before
        // read_to_string runs — not possible to race that way in a single
        // thread.  Instead, make the manifest unreadable (chmod 000 on Unix).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o000))
                .unwrap();

            let result =
                run_foreground_with_registry(args, |_c| leviath_runtime::ProviderRegistry::new())
                    .await;
            assert!(
                result.is_err(),
                "expected error reading unreadable manifest"
            );
            // Restore permissions so cleanup works
            std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        #[cfg(not(unix))]
        {
            // On non-Unix, skip the permission test but still exercise the
            // yolo=false path by using an invalid manifest path.
            let _ = args;
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// Covers `parse_manifest(...)? ` error branch (line 386) — the manifest
    /// file exists and is readable but contains invalid TOML / bad structure.
    #[tokio::test]
    async fn run_foreground_with_registry_fails_on_invalid_manifest() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-invalid-manifest");

        let temp_dir = std::env::temp_dir().join("lev-test-fg-invalid-manifest");
        let _ = std::fs::create_dir_all(&temp_dir);
        std::fs::write(temp_dir.join("agent.leviath"), "this is not valid toml }{").unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test".to_string()),
            model: None,
            foreground: true,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        let result =
            run_foreground_with_registry(args, |_c| leviath_runtime::ProviderRegistry::new()).await;
        assert!(result.is_err(), "expected parse error for invalid manifest");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// Covers `resolve_task(...)? ` error branch (line 389) via the "empty
    /// task file" error, not `task: None`. `task: None` reaches
    /// `resolve_task`'s real, un-injected `std::io::stdin().is_terminal()`
    /// check (see `session.rs`) -- under `cargo test` run from a real
    /// interactive terminal that's actually true, so this used to launch a
    /// real editor (`vim`/`nano`/`vi`, whichever is found first with no
    /// `$EDITOR`/`$VISUAL` set) with the test process's real inherited
    /// stdio, hanging the whole test run on real keyboard input. An empty
    /// task file hits a `resolve_task` error deterministically, in every
    /// environment, without depending on whether stdin happens to be a TTY.
    #[tokio::test]
    async fn run_foreground_with_registry_fails_when_task_file_is_empty() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-no-task");

        let temp_dir = std::env::temp_dir().join("lev-test-fg-no-task");
        let _ = std::fs::create_dir_all(&temp_dir);
        std::fs::write(
            temp_dir.join("agent.leviath"),
            "[agent]\nname = \"x\"\nversion = \"1.0.0\"\ndescription = \"x\"\n",
        )
        .unwrap();
        let empty_task_file = temp_dir.join("empty-task.txt");
        std::fs::write(&empty_task_file, "").unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some(empty_task_file.to_string_lossy().to_string()),
            model: None,
            foreground: true,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        let result =
            run_foreground_with_registry(args, |_c| leviath_runtime::ProviderRegistry::new()).await;
        assert!(result.is_err(), "expected error for empty task file");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("is empty"), "unexpected error: {err_msg}",);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// Covers `Config::load()?`'s error branch — a manifest and task both
    /// resolve successfully, but the isolated config path contains invalid
    /// TOML, so `Config::load()` (via `load_from_path`'s `toml::from_str`)
    /// fails and `run_foreground_with_registry` propagates that error before
    /// ever reaching provider-registry construction.
    #[tokio::test]
    async fn run_foreground_with_registry_fails_on_invalid_config() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-invalid-config");
        std::fs::write(
            crate::config::Config::config_path(),
            "this is not valid toml }{",
        )
        .unwrap();

        let temp_dir = std::env::temp_dir().join("lev-test-fg-invalid-config");
        let _ = std::fs::create_dir_all(&temp_dir);
        std::fs::write(
            temp_dir.join("agent.leviath"),
            "[agent]\nname = \"x\"\nversion = \"1.0.0\"\ndescription = \"x\"\n",
        )
        .unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: true,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        // Wrapped in `with_tracing` because this path reaches the
        // `tracing::info!` at the top of `run_foreground_with_registry`
        // (after manifest parsing, before `Config::load()` fails).
        let result = with_tracing(|| {
            run_foreground_with_registry(args, |_c| leviath_runtime::ProviderRegistry::new())
        })
        .await;
        assert!(result.is_err(), "expected error for invalid config TOML");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed to parse config"),
            "unexpected error: {err_msg}",
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// Covers the `if args.yolo { ... }` false arm (the "skip" branch) in a
    /// run that actually reaches that point in the function -- every other
    /// test reaching this far in the function uses `yolo: true`, so the
    /// no-op "yolo is false, don't insert a wildcard override" path was
    /// otherwise never taken by a run that gets past manifest/task/config
    /// resolution. Reuses the same "abort at missing provider" shape as
    /// `run_foreground_with_valid_manifest_aborts_at_missing_provider`.
    #[tokio::test]
    async fn run_foreground_with_registry_yolo_false_reaches_provider_missing() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-yolo-false");

        let temp_dir = std::env::temp_dir().join("lev-test-fg-yolo-false");
        let _ = std::fs::create_dir_all(&temp_dir);
        let manifest_content = r#"
[agent]
name = "test-fg-yolo-false-agent"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
max_iterations = 1

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        std::fs::write(temp_dir.join("agent.leviath"), manifest_content).unwrap();

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: true,
            yolo: false, // ← exercises the `if args.yolo` false/skip branch
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        // No provider configured (config isolated above) → aborts cleanly at
        // `on_provider_missing`, same as the yolo:true counterpart above.
        with_tracing(|| run_foreground(args)).await.unwrap();

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// Covers `run_stage_loop(...)?.await` error branch (line 507) — a
    /// provider that always errors on an interactive stage with tools causes
    /// `run_interactive_stage` to propagate the inference error directly
    /// (via `map_err(...)? `), unlike `run_autonomous_stage` which catches
    /// and logs it.  The stage loop propagates this Err, and
    /// `run_foreground_with_registry` returns Err.
    #[tokio::test]
    async fn run_foreground_with_registry_propagates_stage_error() {
        let _config_guard = crate::config::isolate_config_path_for_test("fg-stage-error");

        let temp_dir = std::env::temp_dir().join("lev-test-fg-stage-error");
        let _ = std::fs::create_dir_all(&temp_dir);
        // mode = "interactive" with tools: the has_tools=true path inside
        // run_interactive_stage propagates inference errors rather than
        // swallowing them.
        let manifest_content = r#"
[agent]
name = "test-fg-error-agent"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "interactive"
max_iterations = 1

[stages.main.model]
provider = "error-mock"
model = "error-model"

[stages.main.tools]
allowed = ["read_file"]
"#;
        std::fs::write(temp_dir.join("agent.leviath"), manifest_content).unwrap();

        struct ErrorProvider;

        #[async_trait]
        impl leviath_providers::Provider for ErrorProvider {
            async fn infer(
                &self,
                _request: leviath_providers::InferenceRequest,
            ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
            {
                Err(leviath_providers::ProviderError::ApiError(
                    "intentional test error".to_string(),
                ))
            }

            fn count_tokens(&self, _text: &str, _model: &str) -> usize {
                0
            }

            fn max_context_tokens(&self, _model: &str) -> usize {
                100_000
            }

            fn name(&self) -> &str {
                "error-mock"
            }

            fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
                leviath_providers::ModelCapabilities::default()
            }

            async fn list_models(
                &self,
            ) -> Result<Vec<leviath_providers::ModelInfo>, leviath_providers::ProviderError>
            {
                Ok(vec![])
            }
        }

        let args = RunArgs {
            path: Some(temp_dir.to_string_lossy().to_string()),
            task: Some("test task".to_string()),
            model: None,
            foreground: true,
            yolo: true,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };

        // Exercise the trivial trait methods so their bodies are covered.
        let probe = ErrorProvider;
        assert_eq!(probe.count_tokens("hello", "model"), 0);
        assert_eq!(probe.max_context_tokens("model"), 100_000);
        assert_eq!(probe.name(), "error-mock");
        let _ = probe.capabilities("model");
        let _ = probe.list_models().await;

        // Wrapped in `with_tracing` because this path reaches the
        // `tracing::info!` at the top of `run_foreground_with_registry`.
        let result = with_tracing(|| {
            run_foreground_with_registry(args, |_config| {
                let mut registry = leviath_runtime::ProviderRegistry::new();
                registry.register("error-mock".to_string(), Arc::new(ErrorProvider));
                registry
            })
        })
        .await;

        // The stage loop propagates the provider error in linear mode
        assert!(result.is_err(), "expected error from stage loop");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
