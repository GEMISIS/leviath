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
//! deliberately *not* done here: this executor is ECS-free (no context window),
//! so the shared world's `collect_tools` applies the agent's `file_tracking`
//! config to these results downstream — where the window is available — via the
//! same path top-level agents use. Every daemon agent, sub-agent included, gets
//! file-tracking whenever its blueprint declares it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use bevy_ecs::entity::Entity;
use leviath_core::interaction::{ApprovalScope, InteractionRequest};
use leviath_providers::ToolCall;
use leviath_runtime::dynamic_interaction::{
    InteractionBackend, UnattendedInteraction, dispatch_dynamic_interaction,
};
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
    /// `--yolo`: nobody is watching this run, so `ask_user_*` /
    /// `present_for_review` / `edit_document` are answered by
    /// [`UnattendedInteraction`] rather than parked on the hub forever.
    pub unattended: bool,
    /// The current stage name, for tagging interactions (re-synced on stage change).
    pub stage_name: Arc<StdMutex<String>>,
    /// Handle for the sub-agent tools (spawn/check/wait/send/kill), or `None`
    /// when this agent can't reach the host (e.g. in unit tests).
    pub subagent: Option<crate::daemon::subagent::SubAgentHandle>,
    /// The agent's sandbox manager, or `None` when no stage is sandboxed. Held
    /// here so `sync_stage` can point it at the entered stage's sandbox; the same
    /// `Arc` is also an ECS component (for teardown at reap) and is wired into
    /// `builtins` as the shell tool's executor.
    pub sandbox: Option<std::sync::Arc<crate::daemon::sandbox_manager::SandboxManager>>,
    /// The agent's discovered Rhai script tools (issue #97), compiled at spawn.
    /// Behind a mutex so a `dynamic_tools` agent's mid-run re-scan can swap the
    /// set in place; static agents never mutate it.
    pub script_tools: Arc<StdMutex<leviath_scripting::ScriptToolSet>>,
    /// Names of the script tools, for routing dispatch to the Rhai executor.
    /// Mutable alongside `script_tools` on a dynamic re-scan.
    pub script_tool_names: Arc<StdMutex<HashSet<String>>>,
    /// The host functions script tools call, with `[tool_script_permissions]`
    /// enforcement (Layer 3) already baked in.
    pub script_host: Arc<dyn leviath_scripting::ScriptHost>,
    /// Present only for `dynamic_tools` agents: everything needed to re-discover
    /// and re-advertise this agent's tools mid-run (issue #97).
    pub dynamic: Option<Arc<DynamicToolCtx>>,
}

/// Re-resolution inputs for a `dynamic_tools` agent — held so [`CliToolService`]
/// can re-scan its `tools/` directories and re-filter its stage tool defs mid-run.
pub struct DynamicToolCtx {
    /// `tools/` directories to re-scan (agent dir, run workdir, global), in order.
    pub scan_dirs: Vec<PathBuf>,
    /// Names reserved by built-in / sub-agent / MCP tools (collision-drop set).
    pub reserved_names: HashSet<String>,
    /// Static (non-script) tool defs: built-in + sub-agent + MCP.
    pub static_defs: Vec<leviath_providers::Tool>,
    /// Each stage's `available_tools` (Layer-1 allowlist), by stage index.
    pub stage_available: Vec<Vec<String>>,
    /// Set when the agent writes a tool file; drained by `wants_refresh`.
    pub dirty: Arc<AtomicBool>,
}

/// Execute a single (non-context) tool call against the script-tool, built-in,
/// or MCP executor. Script tools are checked first so a discovered `.rhai` tool
/// dispatches to the Rhai engine; the compiled script and permission-enforcing
/// host run on a blocking thread (the engine is synchronous).
async fn execute_tool(state: &AgentToolState, is_builtin: bool, tc: &ToolCall) -> String {
    if state
        .script_tool_names
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&tc.name)
    {
        return execute_script_tool(state, tc).await;
    }
    if is_builtin {
        let result = state.builtins.execute(&tc.name, tc.arguments.clone()).await;
        mark_dirty_on_tool_write(state, tc);
        result
    } else {
        let mut mcp = state.mcp.lock().await;
        match mcp.execute(&tc.name, tc.arguments.clone()).await {
            Ok(r) if r.success => r.text,
            Ok(r) => format!("[error] {}", r.text),
            Err(e) => format!("[error] tool error: {e}"),
        }
    }
}

/// For a `dynamic_tools` agent, flag its tool set dirty after it writes a `.rhai`
/// file (via `write_file`/`edit_file`), so the next tick re-scans + re-advertises.
/// A no-op for static agents. The path lives in the tool args; the actual
/// discovery is workdir-confined, so an off-`tools/` write just yields a no-op
/// re-scan.
fn mark_dirty_on_tool_write(state: &AgentToolState, tc: &ToolCall) {
    let Some(ctx) = &state.dynamic else { return };
    let writes = matches!(
        leviath_tools::canonical_tool_name(&tc.name),
        "write_file" | "edit_file"
    );
    let is_rhai = tc
        .arguments
        .get("path")
        .and_then(|p| p.as_str())
        .is_some_and(|p| p.ends_with(".rhai"));
    if writes && is_rhai {
        ctx.dirty.store(true, Ordering::SeqCst);
    }
}

/// Run a Rhai script tool on a blocking thread and return its result string.
async fn execute_script_tool(state: &AgentToolState, tc: &ToolCall) -> String {
    let Some(tool) = state
        .script_tools
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&tc.name)
        .cloned()
    else {
        // Name was in `script_tool_names` but the tool is gone — treat as unknown.
        return format!("[error] unknown script tool: {}", tc.name);
    };
    let host = state.script_host.clone();
    let args = tc.arguments.clone();
    tokio::task::spawn_blocking(move || leviath_scripting::execute_script_tool(&tool, args, host))
        .await
        .unwrap_or_else(script_tool_join_failed)
}

/// Last-resort net for a script tool: a panic that escaped the script engine's
/// own native-function guards, or a task cancelled by runtime shutdown, becomes
/// a tool error rather than taking the daemon (and every other run) with it.
///
/// A free function applied via `unwrap_or_else` — not a `match` arm — because
/// the arm can no longer be reached from a test now that panics are contained
/// inside `leviath_scripting` (issue #109), while this body is directly
/// unit-testable with a real `JoinError`. Mirrors
/// `leviath_providers::rhai_provider`'s `task_failed`.
fn script_tool_join_failed(e: tokio::task::JoinError) -> String {
    format!("[error] script tool panicked: {e}")
}

/// Resolve policy, handle approvals / dynamic interactions, and execute a batch
/// of tool calls, returning `(tool_call_id, result)` pairs in call order.
///
/// Two passes so tool calls within one batch run in parallel where it is safe:
/// 1. **Sequential resolution** — dynamic interactions (`ask_user_*`), sub-agent
///    tools, and `ask` approval prompts are inherently interactive and are
///    resolved one at a time, in order (a user answers one prompt at a time, and
///    a `Session`-scope approval must be visible to later calls in the batch).
///    Each call ends up either fully resolved or queued for execution.
/// 2. **Parallel execution** — every queued call runs concurrently (`join_all`),
///    then results are stitched back into the original call order.
pub async fn dispatch_tools(
    state: Arc<AgentToolState>,
    calls: Vec<ToolCall>,
) -> Vec<(String, String)> {
    let stage_name = state
        .stage_name
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();

    // Pass 1: sequential resolution. `slots[i].1 == None` means "execute in pass
    // 2"; the queued `(slot_index, is_builtin, call)` records what to run.
    let mut slots: Vec<(String, Option<String>)> = Vec::with_capacity(calls.len());
    let mut queued: Vec<(usize, bool, ToolCall)> = Vec::new();
    for tc in calls {
        let slot = slots.len();
        // ask_user_* / present_for_review are handled by the interaction backend
        // — the hub (a real person answers) or, for an unattended `--yolo` run,
        // the auto-answering one.
        let interaction: &dyn InteractionBackend = match state.unattended {
            true => &UnattendedInteraction,
            false => &state.interaction,
        };
        if let Some(result) =
            dispatch_dynamic_interaction(interaction, &tc.name, &tc.id, &tc.arguments, &stage_name)
                .await
        {
            slots.push((tc.id, Some(result)));
            continue;
        }

        // Sub-agent tools (spawn/check/wait/send/kill) reach the world through
        // the host, not the builtin/MCP executors.
        if crate::daemon::subagent::is_subagent_tool(&tc.name) {
            let result = match &state.subagent {
                Some(handle) => crate::daemon::subagent::handle(handle, &tc).await,
                None => "[error] sub-agent tools are unavailable for this agent".to_string(),
            };
            slots.push((tc.id, Some(result)));
            continue;
        }

        let is_builtin = state.builtin_names.contains(&tc.name);
        // What a session-scoped approval for *this specific call* would be
        // remembered under. For a shell call that is the command's leading words,
        // not the bare tool name — see `session_approval_key`.
        let approval_key = crate::tools::session_approval_key(&tc.name, &tc.arguments);
        let session_approved = match &approval_key {
            Some(key) => state.session_allows.lock().await.contains(key),
            // A call with no reusable key can never match an earlier grant.
            None => false,
        };
        let policy = if session_approved {
            ToolPolicy::Allow
        } else {
            let stage_snap = state
                .stage_perms
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            resolve_policy(
                &tc.name,
                is_builtin,
                &state.launch_overrides,
                &stage_snap,
                &state.agent_perms,
                &state.global_perms,
            )
        };

        match policy {
            ToolPolicy::Deny => {
                slots.push((
                    tc.id.clone(),
                    Some(format!("[denied] Tool '{}' is not permitted.", tc.name)),
                ));
            }
            ToolPolicy::Ask => {
                let req = InteractionRequest::tool_approval(
                    format!("approve-{}", tc.id),
                    &tc.name,
                    tc.arguments.clone(),
                    &stage_name,
                );
                let response = state.interaction.ask(req).await;
                if response.approved.unwrap_or(false) {
                    // Record the grant under the key that describes what was
                    // actually approved. `None` means this call is not reusable
                    // (a chained shell command), so "for this session" degrades
                    // to "this once" — the safe direction, and the only honest
                    // one when the command's leading words don't characterize it.
                    if response.scope == Some(ApprovalScope::Session)
                        && let Some(key) = &approval_key
                    {
                        state.session_allows.lock().await.insert(key.clone());
                    }
                    slots.push((tc.id.clone(), None));
                    queued.push((slot, is_builtin, tc));
                } else {
                    slots.push((
                        tc.id.clone(),
                        Some(format!("[denied] User declined tool call '{}'.", tc.name)),
                    ));
                }
            }
            ToolPolicy::Allow => {
                slots.push((tc.id.clone(), None));
                queued.push((slot, is_builtin, tc));
            }
        }
    }

    // Pass 2: run the approved/allowed calls concurrently, then fill their slots.
    let executed = futures::future::join_all(
        queued
            .iter()
            .map(|(_, is_builtin, tc)| execute_tool(&state, *is_builtin, tc)),
    )
    .await;
    for ((slot, _, _), result) in queued.iter().zip(executed) {
        slots[*slot].1 = Some(result);
    }

    slots
        .into_iter()
        .map(|(id, result)| (id, result.unwrap_or_default()))
        .collect()
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
        self.states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(entity, state);
    }

    /// Drop an agent's tool state (called when the agent is reaped).
    pub fn unregister(&self, entity: Entity) {
        self.states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&entity);
    }

    /// Remove an agent's tool state and return it, so the caller can run any
    /// teardown it holds (e.g. sandbox destruction) before it is dropped. Used
    /// by the daemon's reap hook.
    pub fn take(&self, entity: Entity) -> Option<Arc<AgentToolState>> {
        self.states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&entity)
    }

    /// Reap an agent: drop its tool state (fixing the prior leak) and tear down
    /// its sandbox (destroying any containers it started). Called from the
    /// daemon's reap hook just before the entity is despawned.
    pub fn reap(&self, entity: Entity) {
        if let Some(state) = self.take(entity)
            && let Some(sandbox) = &state.sandbox
        {
            sandbox.destroy_all();
        }
    }
}

impl ToolService for CliToolService {
    fn sync_stage(&self, entity: Entity, stage_index: usize, stage_name: &str) {
        // Take a handle and drop the `states` guard before touching anything
        // else. `states` is the process-wide map of *every* agent's tool state,
        // and the work below reaches three more mutexes (including the sandbox
        // manager's); holding the global guard across all of that means one
        // agent's panic poisons the map every other agent depends on (#109).
        let Some(state) = self
            .states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&entity)
            .cloned()
        else {
            return;
        };
        if let Some(perms) = state.stage_perms_by_index.get(stage_index) {
            *state
                .stage_perms
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = perms.clone();
        }
        *state
            .stage_name
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = stage_name.to_string();
        // Point the shell tool at this stage's sandbox (per-stage override).
        if let Some(sandbox) = &state.sandbox {
            sandbox.set_stage(stage_index);
        }
    }

    fn exec_for(&self, entity: Entity, calls: Vec<ToolCall>) -> BoxedToolExec {
        let state = self
            .states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&entity)
            .cloned();
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

    fn wants_refresh(&self, entity: Entity) -> bool {
        // Drain the per-agent dirty flag (set when a dynamic agent wrote a .rhai).
        self.states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&entity)
            .and_then(|s| s.dynamic.as_ref())
            .map(|ctx| ctx.dirty.swap(false, Ordering::SeqCst))
            .unwrap_or(false)
    }

    fn refresh_tools(
        &self,
        entity: Entity,
        stage_index: usize,
    ) -> Option<Vec<leviath_providers::Tool>> {
        let state = self
            .states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&entity)
            .cloned()?;
        let ctx = state.dynamic.as_ref()?;
        // Re-discover the agent's script tools from disk and swap them into the
        // live set so a new tool is both advertised *and* dispatchable.
        let (set, names, script_defs) =
            crate::daemon::spawn::discover_script_tools_in(&ctx.scan_dirs, &ctx.reserved_names);
        *state
            .script_tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = set;
        *state
            .script_tool_names
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = names;
        // Re-filter this stage's advertised tools = static defs + fresh script defs.
        let available = ctx.stage_available.get(stage_index)?;
        let mut all = ctx.static_defs.clone();
        all.extend(script_defs);
        Some(crate::daemon::spawn::filter_tools_by_available(
            &all, available,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::interaction::{ApprovalScope, InteractionResponse};
    use leviath_runtime::interaction_hub::InteractionHub;

    /// The three script-tool fields of [`AgentToolState`], as a tuple.
    type ScriptFields = (
        Arc<StdMutex<leviath_scripting::ScriptToolSet>>,
        Arc<StdMutex<HashSet<String>>>,
        Arc<dyn leviath_scripting::ScriptHost>,
    );

    /// Empty script-tool fields (no discovered tools, a deny-all host) for tests
    /// that don't exercise script tools.
    fn no_script_fields() -> ScriptFields {
        let allow = crate::daemon::script_host::ScriptAllow {
            http_get: false,
            http_post: false,
            shell: false,
            read_file: false,
            write_file: false,
            env_var: false,
        };
        (
            Arc::new(StdMutex::new(leviath_scripting::ScriptToolSet::default())),
            Arc::new(StdMutex::new(HashSet::new())),
            Arc::new(crate::daemon::script_host::DaemonScriptHost::new(
                allow,
                std::env::temp_dir(),
            )),
        )
    }

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
        let (script_tools, script_tool_names, script_host) = no_script_fields();
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
            unattended: false,
            stage_name: Arc::new(StdMutex::new("main".to_string())),
            subagent: None,
            sandbox: None,
            script_tools,
            script_tool_names,
            script_host,
            dynamic: None,
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

    /// Build a state whose script tools come from `sources` (name → rhai body,
    /// with a `// @tool <name>` header prepended) and whose script host is
    /// `host`. All other layers permit the tool by default via `global`.
    fn script_state(
        hub: &InteractionHub,
        sources: &[(&str, &str)],
        script_tool_names: HashSet<String>,
        host: Arc<dyn leviath_scripting::ScriptHost>,
        global: HashMap<String, ToolPolicy>,
    ) -> (Arc<AgentToolState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in sources {
            std::fs::write(
                dir.path().join(format!("{name}.rhai")),
                format!("// @tool {name}\n{body}"),
            )
            .unwrap();
        }
        let (set, _skipped) =
            leviath_scripting::ScriptToolSet::discover(&[dir.path().to_path_buf()]);
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
            stage_perms_by_index: Arc::new(Vec::new()),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(global),
            interaction: hub.backend_for("agent-a"),
            unattended: false,
            stage_name: Arc::new(StdMutex::new("main".to_string())),
            subagent: None,
            sandbox: None,
            script_tools: Arc::new(StdMutex::new(set)),
            script_tool_names: Arc::new(StdMutex::new(script_tool_names)),
            script_host: host,
            dynamic: None,
        });
        (state, dir)
    }

    #[tokio::test]
    async fn script_tool_allow_executes() {
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("echo".to_string(), ToolPolicy::Allow);
        let names: HashSet<String> = ["echo".to_string()].into_iter().collect();
        let (state, _dir) = script_state(
            &hub,
            &[("echo", "params.text.to_upper()")],
            names,
            no_script_fields().2,
            allow,
        );
        let out = dispatch_tools(
            state,
            vec![call("c1", "echo", serde_json::json!({"text": "hi"}))],
        )
        .await;
        assert_eq!(out[0].0, "c1");
        assert_eq!(out[0].1, "HI");
    }

    // ── dynamic_tools (issue #97) ──

    fn tool_def(name: &str) -> leviath_providers::Tool {
        leviath_providers::Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    /// A state with a `DynamicToolCtx` scanning `scan_dir`, over `workdir`.
    fn dynamic_state(
        workdir: PathBuf,
        scan_dir: PathBuf,
        static_defs: Vec<leviath_providers::Tool>,
        stage_available: Vec<Vec<String>>,
    ) -> Arc<AgentToolState> {
        let hub = InteractionHub::new();
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(workdir),
        ));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let mut allow = HashMap::new();
        // Both write tools default to Ask; allow them so tests don't block on an
        // approval prompt no one answers.
        allow.insert("write_file".to_string(), ToolPolicy::Allow);
        allow.insert("edit_file".to_string(), ToolPolicy::Allow);
        Arc::new(AgentToolState {
            builtins,
            mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            builtin_names,
            launch_overrides: Arc::new(HashMap::new()),
            session_allows: Arc::new(Mutex::new(HashSet::new())),
            stage_perms: Arc::new(StdMutex::new(HashMap::new())),
            stage_perms_by_index: Arc::new(Vec::new()),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(allow),
            interaction: hub.backend_for("a"),
            unattended: false,
            stage_name: Arc::new(StdMutex::new("main".to_string())),
            subagent: None,
            sandbox: None,
            script_tools: Arc::new(StdMutex::new(leviath_scripting::ScriptToolSet::default())),
            script_tool_names: Arc::new(StdMutex::new(HashSet::new())),
            script_host: no_script_fields().2,
            dynamic: Some(Arc::new(DynamicToolCtx {
                scan_dirs: vec![scan_dir],
                reserved_names: HashSet::new(),
                static_defs,
                stage_available,
                dirty: Arc::new(AtomicBool::new(false)),
            })),
        })
    }

    #[test]
    fn refresh_tools_rediscovers_and_filters() {
        let workdir = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        std::fs::write(tools.path().join("echo.rhai"), "// @tool echo\nparams.x").unwrap();
        let state = dynamic_state(
            workdir.path().to_path_buf(),
            tools.path().to_path_buf(),
            vec![tool_def("read_file")],
            vec![vec!["read_file".to_string(), "echo".to_string()]],
        );
        let svc = CliToolService::new();
        let e = Entity::from_raw_u32(1).expect("a small literal index is always a valid entity id");
        svc.register(e, state.clone());

        let defs = svc.refresh_tools(e, 0).unwrap();
        let mut names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["echo", "read_file"]);
        // The live script set + names now include the freshly discovered tool.
        assert!(state.script_tool_names.lock().unwrap().contains("echo"));
        assert!(state.script_tools.lock().unwrap().contains("echo"));
    }

    #[test]
    fn a_poisoned_state_map_does_not_wedge_every_other_agent() {
        // `states` holds *every* agent's tool state. A panic while holding it
        // used to poison it, so from then on `.lock().unwrap()` panicked for all
        // agents — one bad agent taking the whole daemon's tool dispatch with it
        // (issue #109). Recovering the guard keeps the map usable.
        let svc = CliToolService::new();
        let e = Entity::from_raw_u32(1).expect("a small literal index is always a valid entity id");
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the deliberate panic
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = svc.states.lock().expect("fresh lock");
            panic!("a panic while holding the global state map");
        }));
        std::panic::set_hook(prev);
        assert!(poisoned.is_err());
        assert!(svc.states.is_poisoned(), "the lock really is poisoned");

        // Every entry point still works over the poisoned lock.
        let hub = InteractionHub::new();
        svc.register(
            e,
            state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new()),
        );
        assert!(svc.take(e).is_some());
        svc.unregister(e);
        svc.sync_stage(e, 0, "stage"); // unregistered ⇒ no-op, must not panic
        assert!(!svc.wants_refresh(e));
    }

    #[test]
    fn refresh_tools_none_for_out_of_range_stage() {
        let workdir = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let state = dynamic_state(
            workdir.path().to_path_buf(),
            tools.path().to_path_buf(),
            vec![],
            vec![vec![]], // only stage 0 exists
        );
        let svc = CliToolService::new();
        let e = Entity::from_raw_u32(2).expect("a small literal index is always a valid entity id");
        svc.register(e, state);
        assert!(svc.refresh_tools(e, 9).is_none());
    }

    #[test]
    fn refresh_and_wants_refresh_none_for_non_dynamic_or_unregistered() {
        let hub = InteractionHub::new();
        let svc = CliToolService::new();
        // Non-dynamic agent → both are inert.
        let e = Entity::from_raw_u32(3).expect("a small literal index is always a valid entity id");
        svc.register(
            e,
            state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new()),
        );
        assert!(svc.refresh_tools(e, 0).is_none());
        assert!(!svc.wants_refresh(e));
        // Unregistered entity → both are inert.
        let ghost =
            Entity::from_raw_u32(99).expect("a small literal index is always a valid entity id");
        assert!(svc.refresh_tools(ghost, 0).is_none());
        assert!(!svc.wants_refresh(ghost));
    }

    #[test]
    fn wants_refresh_drains_dirty_flag() {
        let workdir = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let state = dynamic_state(
            workdir.path().to_path_buf(),
            tools.path().to_path_buf(),
            vec![],
            vec![vec![]],
        );
        state
            .dynamic
            .as_ref()
            .unwrap()
            .dirty
            .store(true, Ordering::SeqCst);
        let svc = CliToolService::new();
        let e = Entity::from_raw_u32(4).expect("a small literal index is always a valid entity id");
        svc.register(e, state);
        assert!(svc.wants_refresh(e)); // reads true...
        assert!(!svc.wants_refresh(e)); // ...and drained it to false
    }

    #[tokio::test]
    async fn dynamic_agent_marks_dirty_only_on_rhai_write() {
        let workdir = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let state = dynamic_state(
            workdir.path().to_path_buf(),
            tools.path().to_path_buf(),
            vec![],
            vec![vec![]],
        );
        let dirty = state.dynamic.as_ref().unwrap().dirty.clone();
        // Writing a non-.rhai file does not flag a re-scan.
        dispatch_tools(
            state.clone(),
            vec![call(
                "c1",
                "write_file",
                serde_json::json!({"path": "note.txt", "content": "x"}),
            )],
        )
        .await;
        assert!(!dirty.load(Ordering::SeqCst));
        // Writing a .rhai file flags a re-scan.
        dispatch_tools(
            state.clone(),
            vec![call(
                "c2",
                "write_file",
                serde_json::json!({"path": "t.rhai", "content": "// @tool t\n1"}),
            )],
        )
        .await;
        assert!(dirty.load(Ordering::SeqCst));
        // Editing a .rhai file also flags it (the `edit_file` match arm).
        dirty.store(false, Ordering::SeqCst);
        dispatch_tools(
            state.clone(),
            vec![call(
                "c3",
                "edit_file",
                serde_json::json!({"path": "t.rhai", "old_str": "1", "new_str": "2"}),
            )],
        )
        .await;
        assert!(dirty.load(Ordering::SeqCst));
        // A non-write builtin (list_dir, default Allow) exercises the
        // `writes == false` short-circuit — no flag.
        dirty.store(false, Ordering::SeqCst);
        dispatch_tools(
            state,
            vec![call("c4", "list_dir", serde_json::json!({"path": "."}))],
        )
        .await;
        assert!(!dirty.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn static_agent_write_is_a_noop_for_dirty() {
        // A non-dynamic agent (dynamic: None) never flags dirty on a .rhai write.
        let workdir = tempfile::tempdir().unwrap();
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("write_file".to_string(), ToolPolicy::Allow);
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(workdir.path().to_path_buf()),
        ));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let (script_tools, script_tool_names, script_host) = no_script_fields();
        let state = Arc::new(AgentToolState {
            builtins,
            mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            builtin_names,
            launch_overrides: Arc::new(HashMap::new()),
            session_allows: Arc::new(Mutex::new(HashSet::new())),
            stage_perms: Arc::new(StdMutex::new(HashMap::new())),
            stage_perms_by_index: Arc::new(Vec::new()),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(allow),
            interaction: hub.backend_for("a"),
            unattended: false,
            stage_name: Arc::new(StdMutex::new("main".to_string())),
            subagent: None,
            sandbox: None,
            script_tools,
            script_tool_names,
            script_host,
            dynamic: None,
        });
        // Must not panic (the mark_dirty early-return path).
        let out = dispatch_tools(
            state,
            vec![call(
                "c1",
                "write_file",
                serde_json::json!({"path": "t.rhai", "content": "x"}),
            )],
        )
        .await;
        assert!(out[0].1.contains("Successfully wrote"));
    }

    #[tokio::test]
    async fn script_tool_denied_host_fn_surfaces_denied() {
        // The script calls env_var, but the (deny-all) host blocks it → [denied].
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("readenv".to_string(), ToolPolicy::Allow);
        let names: HashSet<String> = ["readenv".to_string()].into_iter().collect();
        let (state, _dir) = script_state(
            &hub,
            &[("readenv", "env_var(\"HOME\")")],
            names,
            no_script_fields().2, // deny-all host
            allow,
        );
        let out = dispatch_tools(state, vec![call("c1", "readenv", serde_json::json!({}))]).await;
        assert!(out[0].1.contains("[denied]"));
    }

    #[tokio::test]
    async fn script_tool_ask_declined_is_denied() {
        let hub = InteractionHub::new();
        let mut ask = HashMap::new();
        ask.insert("echo".to_string(), ToolPolicy::Ask);
        let names: HashSet<String> = ["echo".to_string()].into_iter().collect();
        let (state, _dir) =
            script_state(&hub, &[("echo", "\"x\"")], names, no_script_fields().2, ask);
        let out = dispatch_answering(
            state,
            vec![call("c1", "echo", serde_json::json!({}))],
            |req| InteractionResponse::approval(&req.id, false, ApprovalScope::Once),
            hub,
        )
        .await;
        assert!(out[0].1.contains("User declined"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn script_tool_panic_is_caught() {
        // A host function that panics is stopped at the Rhai native-function
        // boundary and surfaced as an ordinary tool error. It must never unwind
        // through the engine: rhai's `ArgBackup` destructor asserts during
        // unwinding, which double-panics and aborts the whole daemon (#109).
        struct PanicHost;
        impl leviath_scripting::ScriptHost for PanicHost {
            fn http_get(
                &self,
                _u: &str,
                _h: std::collections::BTreeMap<String, String>,
            ) -> Result<String, String> {
                Ok(String::new())
            }
            fn http_post(
                &self,
                _u: &str,
                _b: &str,
                _h: std::collections::BTreeMap<String, String>,
            ) -> Result<String, String> {
                Ok(String::new())
            }
            fn shell(&self, _c: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn read_file(&self, _p: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn write_file(&self, _p: &str, _c: &str) -> Result<String, String> {
                Ok(String::new())
            }
            fn env_var(&self, _n: &str) -> Result<String, String> {
                panic!("boom in host");
            }
        }
        use leviath_scripting::ScriptHost as _;
        let host = Arc::new(PanicHost);
        // Exercise the non-panicking host methods directly (only env_var is
        // reached via the script below).
        assert!(
            host.http_get("u", std::collections::BTreeMap::new())
                .is_ok()
        );
        assert!(
            host.http_post("u", "b", std::collections::BTreeMap::new())
                .is_ok()
        );
        assert!(host.shell("c").is_ok());
        assert!(host.read_file("p").is_ok());
        assert!(host.write_file("p", "c").is_ok());
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("boom".to_string(), ToolPolicy::Allow);
        let names: HashSet<String> = ["boom".to_string()].into_iter().collect();
        let (state, _dir) = script_state(&hub, &[("boom", "env_var(\"X\")")], names, host, allow);
        let out = dispatch_tools(state, vec![call("c1", "boom", serde_json::json!({}))]).await;
        let result = &out[0].1;
        assert!(result.contains("env_var panicked"), "got: {result}");
        assert!(result.contains("boom in host"), "got: {result}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn script_tool_join_failure_becomes_a_tool_error() {
        // The blocking-task net beneath the engine's own guards: whatever kills
        // the task (a panic that slipped past them, or runtime shutdown) must
        // read back as a tool error, not take the daemon down.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the expected panic
        let join_err = tokio::task::spawn_blocking(|| panic!("kaboom"))
            .await
            .expect_err("the blocking task must fail");
        std::panic::set_hook(prev);
        let out = script_tool_join_failed(join_err);
        assert!(
            out.starts_with("[error] script tool panicked:"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn script_tool_name_without_compiled_tool_errors() {
        // `script_tool_names` claims "ghost" but the set has no such tool.
        let hub = InteractionHub::new();
        let mut allow = HashMap::new();
        allow.insert("ghost".to_string(), ToolPolicy::Allow);
        let names: HashSet<String> = ["ghost".to_string()].into_iter().collect();
        let (state, _dir) = script_state(&hub, &[], names, no_script_fields().2, allow);
        let out = dispatch_tools(state, vec![call("c1", "ghost", serde_json::json!({}))]).await;
        assert!(out[0].1.contains("unknown script tool"));
    }

    #[tokio::test]
    async fn batch_mixes_denied_and_executed_in_call_order() {
        // A batch with a denied call between two allowed reads: results must come
        // back in the original call order even though pass 2 runs them in parallel.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "AAA").unwrap();
        std::fs::write(dir.path().join("b.txt"), "BBB").unwrap();
        let hub = InteractionHub::new();
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(dir.path().to_path_buf()),
        ));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let mut global = HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Allow);
        global.insert("write_file".to_string(), ToolPolicy::Deny);
        let (script_tools, script_tool_names, script_host) = no_script_fields();
        let state = Arc::new(AgentToolState {
            builtins,
            mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            builtin_names,
            launch_overrides: Arc::new(HashMap::new()),
            session_allows: Arc::new(Mutex::new(HashSet::new())),
            stage_perms: Arc::new(StdMutex::new(HashMap::new())),
            stage_perms_by_index: Arc::new(Vec::new()),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(global),
            interaction: hub.backend_for("agent-a"),
            unattended: false,
            stage_name: Arc::new(StdMutex::new("main".to_string())),
            subagent: None,
            sandbox: None,
            script_tools,
            script_tool_names,
            script_host,
            dynamic: None,
        });
        let out = dispatch_tools(
            state,
            vec![
                call("c1", "read_file", serde_json::json!({"path": "a.txt"})),
                call(
                    "c2",
                    "write_file",
                    serde_json::json!({"path": "x", "content": "y"}),
                ),
                call("c3", "read_file", serde_json::json!({"path": "b.txt"})),
            ],
        )
        .await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], ("c1".to_string(), "AAA".to_string()));
        assert!(out[1].0 == "c2" && out[1].1.contains("[denied]"));
        assert_eq!(out[2], ("c3".to_string(), "BBB".to_string()));
    }

    #[tokio::test]
    async fn exec_for_without_state_errors() {
        let service = CliToolService::new();
        let exec = service.exec_for(
            Entity::from_raw_u32(1).expect("a small literal index is always a valid entity id"),
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
        let e = Entity::from_raw_u32(5).expect("a small literal index is always a valid entity id");
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
        let e = Entity::from_raw_u32(9).expect("a small literal index is always a valid entity id");
        let mut deny = HashMap::new();
        deny.insert("bash".to_string(), "deny".to_string());
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(std::env::temp_dir()),
        ));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let (script_tools, script_tool_names, script_host) = no_script_fields();
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
            unattended: false,
            stage_name: Arc::new(StdMutex::new("main".to_string())),
            subagent: None,
            sandbox: None,
            script_tools,
            script_tool_names,
            script_host,
            dynamic: None,
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
        service.sync_stage(
            Entity::from_raw_u32(123).expect("a small literal index is always a valid entity id"),
            0,
            "x",
        );
    }

    #[test]
    fn sync_stage_points_sandbox_at_the_entered_stage() {
        use leviath_core::sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig};
        let hub = InteractionHub::new();
        let service = CliToolService::new();
        let e =
            Entity::from_raw_u32(11).expect("a small literal index is always a valid entity id");
        // Two namespace-warn stages → a manager builds on any platform without a
        // runtime, so this exercises `sync_stage`'s per-stage sandbox branch.
        let ns = ToolSandboxConfig {
            kind: SandboxKind::Namespace,
            on_unavailable: OnUnavailable::Warn,
            ..Default::default()
        };
        let mgr = crate::daemon::sandbox_manager::SandboxManager::build(
            "r",
            vec![ns.clone(), ns],
            &std::env::temp_dir().to_string_lossy(),
            0,
        )
        .unwrap()
        .expect("active sandbox yields a manager");
        let mut state = state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new());
        Arc::get_mut(&mut state).unwrap().sandbox = Some(Arc::new(mgr));
        service.register(e, state);
        // Entering stage 1 drives the sandbox branch (set_stage) without panic.
        service.sync_stage(e, 1, "s2");
        assert!(service.take(e).unwrap().sandbox.is_some());
    }

    #[test]
    fn reap_drops_state_and_tears_down_sandbox() {
        use leviath_core::sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig};
        let hub = InteractionHub::new();
        let service = CliToolService::new();

        // With a sandbox: reap removes the state and tears the sandbox down
        // (namespace → destroy_all is a no-op, so no runtime is needed).
        let e =
            Entity::from_raw_u32(21).expect("a small literal index is always a valid entity id");
        let ns = ToolSandboxConfig {
            kind: SandboxKind::Namespace,
            on_unavailable: OnUnavailable::Warn,
            ..Default::default()
        };
        let mgr = crate::daemon::sandbox_manager::SandboxManager::build(
            "r",
            vec![ns],
            &std::env::temp_dir().to_string_lossy(),
            0,
        )
        .unwrap()
        .unwrap();
        let mut state = state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new());
        Arc::get_mut(&mut state).unwrap().sandbox = Some(Arc::new(mgr));
        service.register(e, state);
        service.reap(e);
        assert!(service.take(e).is_none(), "reap removed the state");

        // Without a sandbox: reap still drops the state (the leak fix path).
        let e2 =
            Entity::from_raw_u32(22).expect("a small literal index is always a valid entity id");
        service.register(
            e2,
            state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new()),
        );
        service.reap(e2);
        assert!(service.take(e2).is_none());
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
    async fn subagent_tool_without_a_handle_reports_unavailable() {
        let hub = InteractionHub::new();
        // state_with leaves `subagent: None`.
        let state = state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new());
        let out = dispatch_tools(
            state,
            vec![call(
                "c1",
                "spawn_agent",
                serde_json::json!({ "blueprint": "x", "task": "t" }),
            )],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("unavailable"));
    }

    #[tokio::test]
    async fn subagent_tool_with_a_handle_is_routed_to_the_handler() {
        let hub = InteractionHub::new();
        // A handle whose host is already gone: routing succeeds but the send
        // fails, so the handler reports "shutting down" — which proves the call
        // reached `subagent::handle` (the Some branch), not the None fallback.
        // Drop the receiver explicitly (a `_rx` binding would outlive the send
        // and hang the handler on the never-answered oneshot reply).
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let handle = crate::daemon::subagent::SubAgentHandle {
            sender: tx,
            parent_run_id: "parent".to_string(),
            workdir: "/tmp".to_string(),
            max_depth: 3,
            no_seed_commands: false,
        };
        let builtins = Arc::new(leviath_tools::BuiltinTools::new(
            leviath_tools::ToolContext::new(std::env::temp_dir()),
        ));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let (script_tools, script_tool_names, script_host) = no_script_fields();
        let state = Arc::new(AgentToolState {
            builtins,
            mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            builtin_names,
            launch_overrides: Arc::new(HashMap::new()),
            session_allows: Arc::new(Mutex::new(HashSet::new())),
            stage_perms: Arc::new(StdMutex::new(HashMap::new())),
            stage_perms_by_index: Arc::new(Vec::new()),
            agent_perms: Arc::new(HashMap::new()),
            global_perms: Arc::new(HashMap::new()),
            interaction: hub.backend_for("agent-a"),
            unattended: false,
            stage_name: Arc::new(StdMutex::new("main".to_string())),
            subagent: Some(handle),
            sandbox: None,
            script_tools,
            script_tool_names,
            script_host,
            dynamic: None,
        });
        let out = dispatch_tools(
            state,
            vec![call(
                "c1",
                "kill_agent",
                serde_json::json!({ "agent_id": "c" }),
            )],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert!(out[0].1.contains("shutting down"));
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
    async fn unattended_run_answers_ask_user_itself_instead_of_opening_a_prompt() {
        // `--yolo` sets `unattended`, so `ask_user_confirm` resolves inline. With
        // a live hub and nobody answering, the attended path would block here
        // forever — this test finishing at all is the assertion (#107).
        let hub = InteractionHub::new();
        let mut state =
            (*state_with(&hub, leviath_mcp::ToolExecutor::new(), HashMap::new())).clone();
        state.unattended = true;
        let out = dispatch_tools(
            Arc::new(state),
            vec![call(
                "c1",
                "ask_user_confirm",
                serde_json::json!({"prompt": "proceed?"}),
            )],
        )
        .await;
        assert_eq!(out[0].1, "User answered: Yes");
        assert!(hub.pending().is_empty(), "no prompt was opened");
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

    /// Returns a tool *execution* error. The error flag's wire name is
    /// `isError`; this stub previously wrote `is_error`, which only worked
    /// because the client read the same wrong name — so the pair agreed and
    /// the bug stayed invisible here while every real server's tool errors
    /// were being reported to the model as successes.
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
        respond(id_, {"content": [{"type": "text", "text": "boom"}], "isError": True})
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
