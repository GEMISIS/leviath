//! The real [`ToolService`] for the shared world: bridges an agent's tool calls
//! to the built-in and MCP executors, applying the same policy / approval /
//! interaction flow the imperative worker used — but with interactions routed
//! through the in-memory [`leviath_runtime::interaction_hub`] instead of file
//! polling.
//!
//! The pipeline already applies `context_*` tools inline (they need ECS-window
//! access), so those never reach here. Every other call is resolved against the
//! agent's policy layers and executed; `ask_user_*` / `present_for_review` are
//! handled by [`dispatch_dynamic_interaction`]. File-tracking result rewriting is
//! not yet applied here (an opt-in blueprint feature that also needs window
//! access — a follow-up).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use bevy_ecs::entity::Entity;
use leviath_core::interaction::{ApprovalScope, InteractionRequest};
use leviath_providers::ToolCall;
use leviath_runtime::dynamic_interaction::{InteractionBackend, dispatch_dynamic_interaction};
use leviath_runtime::interaction_hub::HubInteractionBackend;
use leviath_runtime::pipeline::ToolService;
use leviath_runtime::tool_bridge::BoxedToolExec;
use tokio::sync::Mutex;

use crate::config::ToolPolicy;
use crate::tools::resolve_policy;

/// Everything one agent needs to execute a tool call: the executors, its policy
/// layers, and its interaction backend. All fields are cheap `Arc`s so a clone is
/// moved into each `exec_for` closure. The stage-scoped fields
/// (`stage_perms`/`stage_name`) are shared handles the host updates as the agent
/// changes stage.
#[derive(Clone)]
pub struct AgentToolState {
    /// Built-in tool executor (holds the agent's workdir).
    pub builtins: Arc<leviath_tools::BuiltinTools>,
    /// MCP tool executor.
    pub mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    /// Names of the built-in tools (dispatch routes builtin vs MCP).
    pub builtin_names: HashSet<String>,
    /// `--yolo` / `--allow` / `--ask` / `--deny` launch overrides.
    pub launch_overrides: Arc<HashMap<String, ToolPolicy>>,
    /// Tools the user allowed for the whole run (grows on "allow for session").
    pub session_allows: Arc<Mutex<HashSet<String>>>,
    /// The current stage's `tool_permissions` — re-synced by `sync_stage` on each
    /// stage change (a `std` mutex so the sync system can update it synchronously).
    pub stage_perms: Arc<StdMutex<HashMap<String, String>>>,
    /// Every stage's `tool_permissions`, indexed by stage index; `sync_stage`
    /// copies the entered stage's map into `stage_perms`.
    pub stage_perms_by_index: Arc<Vec<HashMap<String, String>>>,
    /// Blueprint-level `[tool_permissions]`.
    pub agent_perms: Arc<HashMap<String, String>>,
    /// Config-level tool permissions.
    pub global_perms: Arc<HashMap<String, ToolPolicy>>,
    /// The agent's interaction backend (ask_user + tool approvals).
    pub interaction: HubInteractionBackend,
    /// The current stage name, for tagging interactions (re-synced on stage change).
    pub stage_name: Arc<StdMutex<String>>,
}

/// Execute a single (non-context) tool call against the built-in or MCP executor.
async fn execute_tool(state: &AgentToolState, is_builtin: bool, tc: &ToolCall) -> String {
    if is_builtin {
        state.builtins.execute(&tc.name, tc.arguments.clone()).await
    } else {
        let mut mcp = state.mcp.lock().await;
        match mcp.execute(&tc.name, tc.arguments.clone()).await {
            Ok(r) if r.success => r.text,
            Ok(r) => format!("[error] {}", r.text),
            Err(e) => format!("[error] tool error: {e}"),
        }
    }
}

/// Resolve policy, handle approvals / dynamic interactions, and execute a batch
/// of tool calls, returning `(tool_call_id, result)` pairs in call order.
pub async fn dispatch_tools(
    state: Arc<AgentToolState>,
    calls: Vec<ToolCall>,
) -> Vec<(String, String)> {
    let stage_name = state.stage_name.lock().unwrap().clone();
    let mut out = Vec::with_capacity(calls.len());
    for tc in calls {
        // ask_user_* / present_for_review are handled by the interaction backend.
        if let Some(result) = dispatch_dynamic_interaction(
            &state.interaction,
            &tc.name,
            &tc.id,
            &tc.arguments,
            &stage_name,
        )
        .await
        {
            out.push((tc.id, result));
            continue;
        }

        let is_builtin = state.builtin_names.contains(&tc.name);
        let policy = if state.session_allows.lock().await.contains(&tc.name) {
            ToolPolicy::Allow
        } else {
            let stage_snap = state.stage_perms.lock().unwrap().clone();
            resolve_policy(
                &tc.name,
                is_builtin,
                &state.launch_overrides,
                &stage_snap,
                &state.agent_perms,
                &state.global_perms,
            )
        };

        let result = match policy {
            ToolPolicy::Deny => format!("[denied] Tool '{}' is not permitted.", tc.name),
            ToolPolicy::Ask => {
                let req = InteractionRequest::tool_approval(
                    format!("approve-{}", tc.id),
                    &tc.name,
                    tc.arguments.clone(),
                    &stage_name,
                );
                let response = state.interaction.ask(req).await;
                if response.approved.unwrap_or(false) {
                    if response.scope == Some(ApprovalScope::Session) {
                        state.session_allows.lock().await.insert(tc.name.clone());
                    }
                    execute_tool(&state, is_builtin, &tc).await
                } else {
                    format!("[denied] User declined tool call '{}'.", tc.name)
                }
            }
            ToolPolicy::Allow => execute_tool(&state, is_builtin, &tc).await,
        };
        out.push((tc.id, result));
    }
    out
}

/// The shared-world tool service: maps entities to their [`AgentToolState`] and
/// builds a per-call executor closure.
#[derive(Default)]
pub struct CliToolService {
    states: StdMutex<HashMap<Entity, Arc<AgentToolState>>>,
}

impl CliToolService {
    /// A fresh, empty service.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an agent's tool state (called when the agent is spawned).
    pub fn register(&self, entity: Entity, state: Arc<AgentToolState>) {
        self.states.lock().unwrap().insert(entity, state);
    }

    /// Drop an agent's tool state (called when the agent is reaped).
    pub fn unregister(&self, entity: Entity) {
        self.states.lock().unwrap().remove(&entity);
    }
}

impl ToolService for CliToolService {
    fn sync_stage(&self, entity: Entity, stage_index: usize, stage_name: &str) {
        if let Some(state) = self.states.lock().unwrap().get(&entity) {
            if let Some(perms) = state.stage_perms_by_index.get(stage_index) {
                *state.stage_perms.lock().unwrap() = perms.clone();
            }
            *state.stage_name.lock().unwrap() = stage_name.to_string();
        }
    }

    fn exec_for(&self, entity: Entity, calls: Vec<ToolCall>) -> BoxedToolExec {
        let state = self.states.lock().unwrap().get(&entity).cloned();
        Box::new(move || {
            Box::pin(async move {
                match state {
                    Some(state) => dispatch_tools(state, calls).await,
                    // A tool batch for an unregistered agent (never spawned via
                    // the CLI, or already reaped): fail each call, don't panic.
                    None => calls
                        .into_iter()
                        .map(|c| (c.id, "[error] agent has no tool state".to_string()))
                        .collect(),
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::interaction::{ApprovalScope, InteractionResponse};
    use leviath_runtime::interaction_hub::InteractionHub;

    /// A tool state with real built-ins over a temp workdir and an (initially
    /// empty) MCP executor, wired to `hub`.
    fn state_with(
        hub: &InteractionHub,
        mcp: leviath_mcp::ToolExecutor,
        global: HashMap<String, ToolPolicy>,
    ) -> Arc<AgentToolState> {
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(std::env::temp_dir()),
        ));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        Arc::new(AgentToolState {
            builtins,
            mcp: Arc::new(Mutex::new(mcp)),
            builtin_names,
            launch_overrides: Arc::new(HashMap::new()),
            session_allows: Arc::new(Mutex::new(HashSet::new())),
            stage_perms: Arc::new(StdMutex::new(HashMap::new())),
            stage_perms_by_index: Arc::new(Vec::new()),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(global),
            interaction: hub.backend_for("agent-a"),
            stage_name: Arc::new(StdMutex::new("main".to_string())),
        })
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }
    }

    /// Run `dispatch_tools` while answering the single interaction it raises.
    async fn dispatch_answering(
        state: Arc<AgentToolState>,
        calls: Vec<ToolCall>,
        answer: impl Fn(&InteractionRequest) -> InteractionResponse + Send + 'static,
        hub: InteractionHub,
    ) -> Vec<(String, String)> {
        let task = tokio::spawn(async move { dispatch_tools(state, calls).await });
        // Wait for the interaction to register, answer it, then collect.
        let response = loop {
            let pending = hub.pending();
            if let Some((_, req)) = pending.first() {
                break answer(req);
            }
            tokio::task::yield_now().await;
        };
        assert!(hub.answer(response));
        task.await.unwrap()
    }

    #[tokio::test]
    async fn exec_for_without_state_errors() {
        let service = CliToolService::new();
        let exec = service.exec_for(
            Entity::from_raw(1),
            vec![call("c1", "read_file", serde_json::json!({}))],
        );
        let results = exec().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("no tool state"));
    }

    #[tokio::test]
    async fn register_routes_to_state_and_unregister_removes_it() {
        let hub = InteractionHub::new();
        let mut deny = HashMap::new();
        deny.insert("bash".to_string(), ToolPolicy::Deny);
        let service = CliToolService::new();
        let e = Entity::from_raw(5);
        service.register(e, state_with(&hub, leviath_mcp::ToolExecutor::new(), deny));

        let out = service.exec_for(
            e,
            vec![call("c1", "bash", serde_json::json!({"command": "ls"}))],
        )()
        .await;
        assert!(out[0].1.contains("[denied]"));

        service.unregister(e);
        let out2 = service.exec_for(e, vec![call("c1", "bash", serde_json::json!({}))])().await;
        assert!(out2[0].1.contains("no tool state"));
    }

    #[test]
    fn sync_stage_swaps_perms_and_name() {
        let hub = InteractionHub::new();
        let service = CliToolService::new();
        let e = Entity::from_raw(9);
        let mut deny = HashMap::new();
        deny.insert("bash".to_string(), "deny".to_string());
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(std::env::temp_dir()),
        ));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let state = Arc::new(AgentToolState {
            builtins,
            mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            builtin_names,
            launch_overrides: Arc::new(HashMap::new()),
            session_allows: Arc::new(Mutex::new(HashSet::new())),
            stage_perms: Arc::new(StdMutex::new(HashMap::new())),
            stage_perms_by_index: Arc::new(vec![HashMap::new(), deny.clone()]),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(HashMap::new()),
            interaction: hub.backend_for("a"),
            stage_name: Arc::new(StdMutex::new("main".to_string())),
        });
        service.register(e, state.clone());

        // Entering stage 1 swaps in that stage's perms + name.
        service.sync_stage(e, 1, "review");
        assert_eq!(*state.stage_perms.lock().unwrap(), deny);
        assert_eq!(*state.stage_name.lock().unwrap(), "review");

        // An out-of-range index leaves perms as-is but still updates the name.
        service.sync_stage(e, 99, "ghost");
        assert_eq!(*state.stage_perms.lock().unwrap(), deny);
        assert_eq!(*state.stage_name.lock().unwrap(), "ghost");

        // An unregistered entity is a no-op (must not panic).
        service.sync_stage(Entity::from_raw(123), 0, "x");
    }

    #[tokio::test]
    async fn allow_builtin_executes() {
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("read_file".to_string(), ToolPolicy::Allow);
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), allow);
        // A nonexistent file: builtins return an error string, but the builtin
        // execution path is exercised and a result is produced.
        let out = dispatch_tools(
            state,
            vec![call(
                "c1",
                "read_file",
                serde_json::json!({"path": "/no/such/file"}),
            )],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "c1");
    }

    #[tokio::test]
    async fn session_allows_short_circuits_to_allow() {
        let hub = InteractionHub::new();
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new());
        state
            .session_allows
            .lock()
            .await
            .insert("read_file".to_string());
        let out = dispatch_tools(
            state,
            vec![call(
                "c1",
                "read_file",
                serde_json::json!({"path": "/no/such"}),
            )],
        )
        .await;
        assert_eq!(out.len(), 1); // executed, not asked
    }

    #[tokio::test]
    async fn dynamic_interaction_is_handled() {
        let hub = InteractionHub::new();
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new());
        let out = dispatch_answering(
            state,
            vec![call(
                "c1",
                "ask_user_text",
                serde_json::json!({"prompt": "name?"}),
            )],
            |req| InteractionResponse::text(&req.id, "Ada"),
            hub,
        )
        .await;
        assert_eq!(out[0].0, "c1");
        assert!(out[0].1.contains("Ada"));
    }

    #[tokio::test]
    async fn ask_approved_once_executes() {
        let hub = InteractionHub::new();
        let mut ask = HashMap::new();
        ask.insert("read_file".to_string(), ToolPolicy::Ask);
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), ask);
        let out = dispatch_answering(
            state.clone(),
            vec![call(
                "c1",
                "read_file",
                serde_json::json!({"path": "/no/such"}),
            )],
            |req| InteractionResponse::approval(&req.id, true, ApprovalScope::Once),
            hub,
        )
        .await;
        assert_eq!(out[0].0, "c1");
        // Once-scope approval does not persist.
        assert!(!state.session_allows.lock().await.contains("read_file"));
    }

    #[tokio::test]
    async fn ask_approved_session_persists() {
        let hub = InteractionHub::new();
        let mut ask = HashMap::new();
        ask.insert("read_file".to_string(), ToolPolicy::Ask);
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), ask);
        let out = dispatch_answering(
            state.clone(),
            vec![call(
                "c1",
                "read_file",
                serde_json::json!({"path": "/no/such"}),
            )],
            |req| InteractionResponse::approval(&req.id, true, ApprovalScope::Session),
            hub,
        )
        .await;
        assert_eq!(out[0].0, "c1");
        assert!(state.session_allows.lock().await.contains("read_file"));
    }

    #[tokio::test]
    async fn ask_declined_is_denied() {
        let hub = InteractionHub::new();
        let mut ask = HashMap::new();
        ask.insert("read_file".to_string(), ToolPolicy::Ask);
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), ask);
        let out = dispatch_answering(
            state,
            vec![call("c1", "read_file", serde_json::json!({}))],
            |req| InteractionResponse::approval(&req.id, false, ApprovalScope::Once),
            hub,
        )
        .await;
        assert!(out[0].1.contains("User declined"));
    }

    // ── MCP execution branches (real python3 JSON-RPC stub) ──

    const MCP_STUB_SUCCESS: &str = r#"
import sys, json
def respond(id_, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); method = req.get("method", ""); id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": False}}, "protocolVersion": "2024-11-05"})
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "stub_mcp_tool", "description": "s", "inputSchema": {"type": "object", "properties": {}}}]})
    elif method == "tools/call":
        respond(id_, {"content": [{"type": "text", "text": "ok result"}], "isError": False})
    elif method != "notifications/initialized" and method != "notifications/cancelled":
        respond(id_, {})
"#;

    const MCP_STUB_ERROR: &str = r#"
import sys, json
def respond(id_, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id_, "result": result}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); method = req.get("method", ""); id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": False}}, "protocolVersion": "2024-11-05"})
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "stub_mcp_tool", "description": "s", "inputSchema": {"type": "object", "properties": {}}}]})
    elif method == "tools/call":
        respond(id_, {"content": [{"type": "text", "text": "boom"}], "is_error": True})
    elif method != "notifications/initialized" and method != "notifications/cancelled":
        respond(id_, {})
"#;

    async fn mcp_with_stub(stub: &str) -> leviath_mcp::ToolExecutor {
        let mut client = leviath_mcp::MCPClient::spawn("python3", &["-c", stub], &HashMap::new())
            .await
            .expect("spawn stub");
        client.connect().await.expect("connect");
        client.list_tools().await.expect("list_tools");
        let mut executor = leviath_mcp::ToolExecutor::new();
        executor.add_client("stub".to_string(), client);
        executor
    }

    #[tokio::test]
    async fn mcp_allow_ok_success_returns_text() {
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("stub_mcp_tool".to_string(), ToolPolicy::Allow);
        let state = state_with(&hub, mcp_with_stub(MCP_STUB_SUCCESS).await, allow);
        let out = dispatch_tools(
            state,
            vec![call("c1", "stub_mcp_tool", serde_json::json!({}))],
        )
        .await;
        assert_eq!(out[0].1, "ok result");
    }

    #[tokio::test]
    async fn mcp_allow_ok_error_result_is_prefixed() {
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("stub_mcp_tool".to_string(), ToolPolicy::Allow);
        let state = state_with(&hub, mcp_with_stub(MCP_STUB_ERROR).await, allow);
        let out = dispatch_tools(
            state,
            vec![call("c1", "stub_mcp_tool", serde_json::json!({}))],
        )
        .await;
        assert!(out[0].1.contains("[error]") && out[0].1.contains("boom"));
    }

    #[tokio::test]
    async fn mcp_allow_err_is_reported() {
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("ghost_mcp".to_string(), ToolPolicy::Allow);
        // Empty executor: no server has the tool → execute returns Err.
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), allow);
        let out = dispatch_tools(state, vec![call("c1", "ghost_mcp", serde_json::json!({}))]).await;
        assert!(out[0].1.contains("[error] tool error"));
    }
}
