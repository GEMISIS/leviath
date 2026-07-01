//! Unified tool registry combining built-in tools and MCP-discovered tools.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use leviath_mcp::{ToolDiscovery, ToolExecutor};
use leviath_providers::Tool;
use leviath_tools::{BuiltinTools, ToolContext};

use crate::config::{Config, ToolPolicy};

/// Combined tool registry: native built-in tools + MCP-discovered tools.
///
/// Cheap to clone (all fields are `Arc`s). The `call` method dispatches
/// to the appropriate executor.
pub struct ToolRegistry {
    pub builtins: Arc<BuiltinTools>,
    pub mcp: Arc<Mutex<ToolExecutor>>,
    pub mcp_tool_defs: Vec<Tool>,
    pub builtin_names: HashSet<String>,
    #[allow(dead_code)]
    pub subagent_names: HashSet<String>,
}

impl ToolRegistry {
    /// Build a registry, connecting MCP servers declared in config (non-fatal).
    pub async fn build(workdir: PathBuf, config: &Config) -> Self {
        let ctx = ToolContext::new(workdir);
        let builtins = Arc::new(BuiltinTools::new(ctx));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();

        let mut mcp_executor = ToolExecutor::new();
        let mut mcp_tool_defs = Vec::new();

        if !config.mcp_servers.is_empty() {
            let mut discovery = ToolDiscovery::new();
            for server_cfg in &config.mcp_servers {
                match discovery.discover_from_config(server_cfg).await {
                    Ok((tool_metas, client)) => {
                        mcp_executor.add_client(server_cfg.name.clone(), client);
                        for meta in tool_metas {
                            mcp_tool_defs.push(Tool {
                                name: meta.name,
                                description: meta.description,
                                parameters: meta.schema,
                            });
                        }
                        tracing::info!(server = %server_cfg.name, "Connected MCP server");
                    }
                    Err(e) => {
                        tracing::warn!(
                            server = %server_cfg.name,
                            error = %e,
                            "Failed to connect MCP server — skipping"
                        );
                    }
                }
            }
        }

        let subagent_names: HashSet<String> =
            BuiltinTools::subagent_tool_names().into_iter().collect();

        Self {
            builtins,
            mcp: Arc::new(Mutex::new(mcp_executor)),
            mcp_tool_defs,
            builtin_names,
            subagent_names,
        }
    }

    /// All tool definitions to advertise to the LLM (built-ins + MCP + sub-agent).
    pub fn all_tool_defs(&self) -> Vec<Tool> {
        let mut tools = self.builtins.tool_defs();
        tools.extend(BuiltinTools::subagent_tool_defs());
        tools.extend_from_slice(&self.mcp_tool_defs);
        tools
    }

    /// Execute a tool by name, dispatching to built-ins or MCP.
    #[allow(dead_code)]
    pub async fn call(&self, name: &str, arguments: serde_json::Value) -> String {
        if self.builtin_names.contains(name) {
            self.builtins.execute(name, arguments).await
        } else {
            let mut mcp = self.mcp.lock().await;
            match mcp.execute(name, arguments).await {
                Ok(r) if r.success => r.text,
                Ok(r) => format!("[error] tool '{}' failed: {}", name, r.text),
                Err(e) => format!("[error] tool '{}' error: {}", name, e),
            }
        }
    }

    /// Shut down all MCP connections.
    pub async fn shutdown(&self) {
        let mut mcp = self.mcp.lock().await;
        if let Err(e) = mcp.shutdown_all().await {
            tracing::warn!(error = %e, "Error shutting down MCP servers");
        }
    }
}

// ─── Sub-agent tool executor ─────────────────────────────────────────────────

use bevy_ecs::prelude::Entity;
use leviath_core::Blueprint;
use leviath_runtime::{
    AgentEngine, AgentPool, AgentState, AgentStatus, CancellationToken, ContextWindow, ParentRef,
    SubAgentChildren,
};
use tokio::sync::RwLock;

/// Shared state for sub-agent tool execution.
///
/// Wraps an `Arc<RwLock<AgentEngine>>` plus lookup tables so that the tool
/// executor closure can spawn/query/kill child agents.
#[derive(Clone)]
#[allow(dead_code)]
pub struct SubAgentExecutor {
    /// The shared engine — the tool executor needs mutable access to spawn
    /// entities. Using RwLock so multiple reads can happen concurrently.
    engine: Arc<RwLock<AgentEngine>>,

    /// Blueprint registry: name → blueprint, loaded from installed agents
    blueprints: Arc<std::sync::RwLock<HashMap<String, Blueprint>>>,

    /// Agent ID → Entity lookup (all agents, including sub-agents)
    agent_entities: Arc<std::sync::RwLock<HashMap<String, Entity>>>,

    /// Agent pools keyed by blueprint name for auto-numbering
    pools: Arc<std::sync::RwLock<HashMap<String, AgentPool>>>,
}

#[allow(dead_code)]
impl SubAgentExecutor {
    /// Create a new sub-agent executor.
    pub fn new(engine: Arc<RwLock<AgentEngine>>) -> Self {
        Self {
            engine,
            blueprints: Arc::new(std::sync::RwLock::new(HashMap::new())),
            agent_entities: Arc::new(std::sync::RwLock::new(HashMap::new())),
            pools: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Register a blueprint so sub-agents can be spawned from it.
    pub fn register_blueprint(&self, blueprint: Blueprint) {
        let name = blueprint.name.clone();
        {
            let mut pools = self.pools.write().unwrap();
            if !pools.contains_key(&name) {
                pools.insert(name.clone(), AgentPool::new(blueprint.clone()));
            }
        }
        self.blueprints.write().unwrap().insert(name, blueprint);
    }

    /// Register an existing agent entity (e.g. the root agent).
    pub fn register_agent(&self, agent_id: String, entity: Entity) {
        self.agent_entities
            .write()
            .unwrap()
            .insert(agent_id, entity);
    }

    /// Execute a sub-agent tool call, returning the result string.
    pub async fn execute(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        caller_agent_id: &str,
        caller_entity: Entity,
        caller_depth: usize,
        max_depth: usize,
    ) -> String {
        match tool_name {
            "spawn_agent" => {
                self.exec_spawn(
                    args,
                    caller_agent_id,
                    caller_entity,
                    caller_depth,
                    max_depth,
                )
                .await
            }
            "check_agent" => self.exec_check(args, caller_agent_id).await,
            "wait_for_agent" => self.exec_wait(args, caller_agent_id).await,
            "send_to_agent" => self.exec_send(args).await,
            "kill_agent" => self.exec_kill(args, caller_agent_id).await,
            _ => format!("[error] Unknown sub-agent tool: {}", tool_name),
        }
    }

    async fn exec_spawn(
        &self,
        args: &serde_json::Value,
        caller_agent_id: &str,
        caller_entity: Entity,
        caller_depth: usize,
        max_depth: usize,
    ) -> String {
        let blueprint_name = match args.get("blueprint").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => return "[error] missing 'blueprint' argument".to_string(),
        };
        let task = match args.get("task").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => return "[error] missing 'task' argument".to_string(),
        };
        let _wait = args.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);
        let seed_context = args
            .get("seed_context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Depth validation
        let child_depth = caller_depth + 1;
        if child_depth > max_depth {
            return format!(
                "[error] Cannot spawn sub-agent: depth {} exceeds max depth {}",
                child_depth, max_depth
            );
        }

        // Check blueprint exists
        let blueprint = {
            let bps = self.blueprints.read().unwrap();
            match bps.get(&blueprint_name) {
                Some(bp) => bp.clone(),
                None => {
                    return format!(
                        "[error] Blueprint '{}' not found. Register it first.",
                        blueprint_name
                    )
                }
            }
        };

        // Spawn the child agent entity
        let child_agent_id = {
            let mut engine = self.engine.write().await;
            let mut pools = self.pools.write().unwrap();
            let pool = pools
                .entry(blueprint_name.clone())
                .or_insert_with(|| AgentPool::new(blueprint.clone()));
            pool.spawn_agent(engine.world_mut())
        };

        let child_entity = {
            let pools = self.pools.read().unwrap();
            match pools
                .get(&blueprint_name)
                .and_then(|p| p.get_agent(&child_agent_id))
            {
                Some(e) => e,
                None => return "[error] Failed to get spawned child entity".to_string(),
            }
        };

        // Register in our lookup
        self.agent_entities
            .write()
            .unwrap()
            .insert(child_agent_id.clone(), child_entity);

        // Attach ParentRef and SubAgentChildren components
        {
            let mut engine = self.engine.write().await;

            // Add ParentRef to child
            engine
                .world_mut()
                .entity_mut(child_entity)
                .insert(ParentRef {
                    parent_entity: caller_entity,
                    parent_agent_id: caller_agent_id.to_string(),
                    depth: child_depth,
                });

            // Add/update SubAgentChildren on parent
            if engine
                .world()
                .get::<SubAgentChildren>(caller_entity)
                .is_some()
            {
                if let Some(mut children) = engine
                    .world_mut()
                    .get_mut::<SubAgentChildren>(caller_entity)
                {
                    children.children.push(child_entity);
                }
            } else {
                engine
                    .world_mut()
                    .entity_mut(caller_entity)
                    .insert(SubAgentChildren {
                        children: vec![child_entity],
                        max_child_depth: max_depth,
                    });
            }

            // Update parent's AgentState
            if let Some(mut state) = engine.world_mut().get_mut::<AgentState>(caller_entity) {
                state.spawned_children_ids.push(child_agent_id.clone());
            }

            // Inject seed context if provided
            if let Some(seed) = &seed_context {
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(child_entity)
                {
                    let tokens = seed.len() / 4 + 1;
                    let pinned_name = window
                        .regions
                        .iter()
                        .find(|r| matches!(r.kind, leviath_core::RegionKind::Pinned))
                        .map(|r| r.name.clone());
                    if let Some(name) = pinned_name {
                        let _ = window.add_to_region(&name, seed.clone(), tokens);
                    }
                }
            }

            // Set child as Active
            if let Some(mut state) = engine.world_mut().get_mut::<AgentState>(child_entity) {
                state.status = AgentStatus::Active;
            }
        }

        tracing::info!(
            parent = %caller_agent_id,
            child = %child_agent_id,
            blueprint = %blueprint_name,
            depth = child_depth,
            "Spawned sub-agent"
        );

        let _ = task; // Task is used by the caller to set up the child's context
        format!(
            "Spawned sub-agent '{}' (blueprint: {}, depth: {})",
            child_agent_id, blueprint_name, child_depth
        )
    }

    async fn exec_check(&self, args: &serde_json::Value, _caller_agent_id: &str) -> String {
        let agent_id = match args.get("agent_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return "[error] missing 'agent_id' argument".to_string(),
        };

        let entity = {
            let entities = self.agent_entities.read().unwrap();
            match entities.get(agent_id) {
                Some(e) => *e,
                None => return format!("[error] Agent '{}' not found", agent_id),
            }
        };

        let engine = self.engine.read().await;
        let world = engine.world();

        let status = match world.get::<AgentState>(entity) {
            Some(state) => match &state.status {
                AgentStatus::Active => "active".to_string(),
                AgentStatus::Waiting => "waiting".to_string(),
                AgentStatus::Complete => "complete".to_string(),
                AgentStatus::Error { message } => format!("error: {}", message),
                AgentStatus::Cancelled => "cancelled".to_string(),
                AgentStatus::Idle => "idle".to_string(),
            },
            None => return format!("[error] Agent '{}' entity has no state", agent_id),
        };

        // If complete, try to get the last response from context
        let result = if status == "complete" {
            if let Some(window) = world.get::<ContextWindow>(entity) {
                window
                    .get_region("conversation")
                    .and_then(|r| r.content.last())
                    .map(|e| e.content.clone())
            } else {
                None
            }
        } else {
            None
        };

        match result {
            Some(content) => format!("Status: {}\nResult: {}", status, content),
            None => format!("Status: {}", status),
        }
    }

    async fn exec_wait(&self, args: &serde_json::Value, caller_agent_id: &str) -> String {
        let agent_id = match args.get("agent_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return "[error] missing 'agent_id' argument".to_string(),
        };

        let entity = {
            let entities = self.agent_entities.read().unwrap();
            match entities.get(&agent_id) {
                Some(e) => *e,
                None => return format!("[error] Agent '{}' not found", agent_id),
            }
        };

        // Set the parent's pending_wait
        {
            let caller_entity = {
                let entities = self.agent_entities.read().unwrap();
                entities.get(caller_agent_id).copied()
            };
            if let Some(ce) = caller_entity {
                let mut engine = self.engine.write().await;
                if let Some(mut state) = engine.world_mut().get_mut::<AgentState>(ce) {
                    state.pending_wait = Some(agent_id.clone());
                }
            }
        }

        // Poll until child completes (check every 500ms)
        loop {
            {
                let engine = self.engine.read().await;
                let world = engine.world();
                if let Some(state) = world.get::<AgentState>(entity) {
                    match &state.status {
                        AgentStatus::Complete => {
                            // Clear pending_wait on parent
                            drop(engine);
                            let caller_entity = {
                                let entities = self.agent_entities.read().unwrap();
                                entities.get(caller_agent_id).copied()
                            };
                            if let Some(ce) = caller_entity {
                                let mut eng = self.engine.write().await;
                                if let Some(mut pstate) = eng.world_mut().get_mut::<AgentState>(ce)
                                {
                                    pstate.pending_wait = None;
                                }
                            }

                            // Get final result
                            let eng = self.engine.read().await;
                            let result = eng
                                .world()
                                .get::<ContextWindow>(entity)
                                .and_then(|w| {
                                    w.get_region("conversation")
                                        .and_then(|r| r.content.last())
                                        .map(|e| e.content.clone())
                                })
                                .unwrap_or_else(|| "(no result)".to_string());
                            return format!("Agent '{}' completed.\nResult: {}", agent_id, result);
                        }
                        AgentStatus::Error { message } => {
                            return format!("Agent '{}' failed with error: {}", agent_id, message);
                        }
                        AgentStatus::Cancelled => {
                            return format!("Agent '{}' was cancelled", agent_id);
                        }
                        _ => {}
                    }
                } else {
                    return format!("[error] Agent '{}' entity no longer exists", agent_id);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn exec_send(&self, args: &serde_json::Value) -> String {
        let agent_id = match args.get("agent_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return "[error] missing 'agent_id' argument".to_string(),
        };
        let message = match args.get("message").and_then(|v| v.as_str()) {
            Some(m) => m.to_string(),
            None => return "[error] missing 'message' argument".to_string(),
        };
        let target_region = args
            .get("target_region")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let engine = self.engine.read().await;
        let msg = leviath_runtime::AgentMessage {
            agent_id: agent_id.clone(),
            content: message,
            target_region,
            priority: 0,
        };
        match engine.send_message(msg) {
            Ok(()) => format!("Message sent to '{}'", agent_id),
            Err(e) => format!("[error] Failed to send message: {}", e),
        }
    }

    async fn exec_kill(&self, args: &serde_json::Value, _caller_agent_id: &str) -> String {
        let agent_id = match args.get("agent_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return "[error] missing 'agent_id' argument".to_string(),
        };

        let entity = {
            let entities = self.agent_entities.read().unwrap();
            match entities.get(&agent_id) {
                Some(e) => *e,
                None => return format!("[error] Agent '{}' not found", agent_id),
            }
        };

        // Cascade kill: collect all descendants first
        let mut to_kill = vec![entity];
        {
            let engine = self.engine.read().await;
            let world = engine.world();
            let mut i = 0;
            while i < to_kill.len() {
                let e = to_kill[i];
                if let Some(children) = world.get::<SubAgentChildren>(e) {
                    to_kill.extend_from_slice(&children.children);
                }
                i += 1;
            }
        }

        // Cancel all
        {
            let mut engine = self.engine.write().await;
            for e in &to_kill {
                if let Some(token) = engine.world().get::<CancellationToken>(*e) {
                    token.cancel();
                }
                if let Some(mut state) = engine.world_mut().get_mut::<AgentState>(*e) {
                    state.status = AgentStatus::Cancelled;
                }
            }
        }

        let count = to_kill.len();
        if count == 1 {
            format!("Killed agent '{}'", agent_id)
        } else {
            format!(
                "Killed agent '{}' and {} descendant(s)",
                agent_id,
                count - 1
            )
        }
    }
}

// ─── Tool policy resolution ───────────────────────────────────────────────────

/// Built-in Claude Code-style defaults: read-only tools auto-allow, everything
/// else requires approval.
pub fn default_tool_policy(tool_name: &str, is_builtin: bool) -> ToolPolicy {
    match tool_name {
        "read_file" | "list_dir" => ToolPolicy::Allow,
        "write_file" | "edit_file" | "bash" => ToolPolicy::Ask,
        // These tools ARE the human-in-the-loop mechanism — gating them behind
        // a separate tool-approval prompt would mean asking the user "may I
        // ask you something?" before actually asking them.
        "ask_user_text" | "ask_user_choice" | "ask_user_confirm" => ToolPolicy::Allow,
        _ => {
            // All other tools (built-in or MCP) default to Ask
            let _ = is_builtin;
            ToolPolicy::Ask
        }
    }
}

/// Resolve the effective policy for a tool call, narrowest scope first.
///
/// Precedence (first match wins):
/// 1. `launch_overrides` — from `--allow`/`--ask`/`--deny` / `--yolo` flags
/// 2. `stage_permissions` — `[stages.x.tool_permissions]` in agent.leviath
/// 3. `agent_permissions` — `[tool_permissions]` in agent.leviath
/// 4. `global_permissions` — `[tool_permissions]` in `~/.leviath/config.toml`
/// 5. Built-in defaults
pub fn resolve_policy(
    tool_name: &str,
    is_builtin: bool,
    launch_overrides: &HashMap<String, ToolPolicy>,
    stage_permissions: &HashMap<String, String>,
    agent_permissions: &HashMap<String, String>,
    global_permissions: &HashMap<String, ToolPolicy>,
) -> ToolPolicy {
    // 1. Launch overrides (highest priority)
    if let Some(p) = launch_overrides.get(tool_name) {
        return *p;
    }
    // Wildcard launch allow ("--yolo")
    if let Some(p) = launch_overrides.get("*") {
        return *p;
    }

    // 2. Stage-level (from blueprint string map "allow"/"ask"/"deny")
    if let Some(s) = stage_permissions.get(tool_name) {
        return parse_policy_str(s);
    }

    // 3. Agent-level
    if let Some(s) = agent_permissions.get(tool_name) {
        return parse_policy_str(s);
    }

    // 4. Global config
    if let Some(p) = global_permissions.get(tool_name) {
        return *p;
    }

    // 5. Built-in defaults
    default_tool_policy(tool_name, is_builtin)
}

fn parse_policy_str(s: &str) -> ToolPolicy {
    match s.to_lowercase().as_str() {
        "allow" => ToolPolicy::Allow,
        "deny" => ToolPolicy::Deny,
        _ => ToolPolicy::Ask,
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn test_default_policy_read_file() {
        assert_eq!(default_tool_policy("read_file", true), ToolPolicy::Allow);
        assert_eq!(default_tool_policy("list_dir", true), ToolPolicy::Allow);
    }

    #[test]
    fn test_default_policy_write_tools() {
        assert_eq!(default_tool_policy("write_file", true), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("edit_file", true), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("bash", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_ask_user_tools_allow_by_default() {
        // These tools ARE the human-in-the-loop mechanism — they must not
        // require a separate approval prompt before asking the user.
        assert_eq!(
            default_tool_policy("ask_user_text", true),
            ToolPolicy::Allow
        );
        assert_eq!(
            default_tool_policy("ask_user_choice", true),
            ToolPolicy::Allow
        );
        assert_eq!(
            default_tool_policy("ask_user_confirm", true),
            ToolPolicy::Allow
        );
    }

    #[test]
    fn test_resolve_policy_launch_override_wins() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_yolo_wins() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_stage_beats_global() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "allow".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &global,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_falls_through_to_default() {
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    // ─── Additional default_tool_policy tests ──────────────────────────────

    #[test]
    fn test_default_policy_unknown_tools() {
        assert_eq!(default_tool_policy("unknown_tool", false), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("mcp_tool", false), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("custom_thing", true), ToolPolicy::Ask);
    }

    // ─── resolve_policy additional scenarios ───────────────────────────────

    #[test]
    fn test_resolve_policy_agent_beats_global() {
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "allow".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &global,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_launch_override_specific_beats_wildcard() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Deny);
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        // Specific tool match checked before wildcard
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_global_overrides_default() {
        let mut global = HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &global,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_stage_deny() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_stage_ask() {
        let mut stage = HashMap::new();
        stage.insert("read_file".to_string(), "ask".to_string());
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn test_resolve_policy_unknown_stage_string_defaults_to_ask() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "unknown_policy".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    // ─── parse_policy_str ──────────────────────────────────────────────────

    #[test]
    fn test_parse_policy_str_values() {
        assert_eq!(parse_policy_str("allow"), ToolPolicy::Allow);
        assert_eq!(parse_policy_str("Allow"), ToolPolicy::Allow);
        assert_eq!(parse_policy_str("ALLOW"), ToolPolicy::Allow);
        assert_eq!(parse_policy_str("deny"), ToolPolicy::Deny);
        assert_eq!(parse_policy_str("Deny"), ToolPolicy::Deny);
        assert_eq!(parse_policy_str("ask"), ToolPolicy::Ask);
        assert_eq!(parse_policy_str("Ask"), ToolPolicy::Ask);
        assert_eq!(parse_policy_str("anything_else"), ToolPolicy::Ask);
        assert_eq!(parse_policy_str(""), ToolPolicy::Ask);
    }

    // ─── ToolRegistry construction ─────────────────────────────────────────

    #[tokio::test]
    async fn test_tool_registry_build_no_mcp() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // Should have built-in tools
        assert!(!registry.builtin_names.is_empty());
        // Should have no MCP tools
        assert!(registry.mcp_tool_defs.is_empty());
    }

    #[tokio::test]
    async fn test_tool_registry_all_tool_defs() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        let all_defs = registry.all_tool_defs();
        assert!(!all_defs.is_empty());

        // Should include known built-in tools
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"read_file"),
            "Expected read_file in tool defs: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_tool_registry_builtin_names_consistent() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // builtin_names should come from builtins.names()
        let names_from_builtins: HashSet<String> = registry.builtins.names().into_iter().collect();
        assert_eq!(
            registry.builtin_names, names_from_builtins,
            "builtin_names should match builtins.names()"
        );
    }

    // ─── resolve_policy full precedence chain ─────────────────────────────

    #[test]
    fn test_resolve_policy_launch_overrides_stage() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &stage,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_stage_overrides_agent() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "allow".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &agent,
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_agent_overrides_global() {
        let mut agent = HashMap::new();
        agent.insert("write_file".to_string(), "deny".to_string());
        let mut global = HashMap::new();
        global.insert("write_file".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "write_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &global,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_wildcard_launch_with_missing_specific() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        // unknown_tool has no specific override, should match wildcard
        let policy = resolve_policy(
            "unknown_tool",
            false,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_mcp_tool_defaults_to_ask() {
        let policy = resolve_policy(
            "mcp_custom_tool",
            false,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn test_resolve_policy_read_file_default_is_allow() {
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_list_dir_default_is_allow() {
        let policy = resolve_policy(
            "list_dir",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_write_file_default_is_ask() {
        let policy = resolve_policy(
            "write_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn test_resolve_policy_edit_file_default_is_ask() {
        let policy = resolve_policy(
            "edit_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    // ─── SubAgentExecutor ─────────────────────────────────────────────────

    #[test]
    fn test_subagent_executor_construction() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);
        // Should be able to clone
        let _clone = exec.clone();
    }

    #[test]
    fn test_subagent_executor_register_blueprint() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let bp = leviath_core::Blueprint::new(
            "test-bp".to_string(),
            "test desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 0),
        );
        exec.register_blueprint(bp.clone());
        // Registering again should not panic
        exec.register_blueprint(bp);
    }

    #[test]
    fn test_subagent_executor_register_agent() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let entity = bevy_ecs::prelude::Entity::from_raw(42);
        exec.register_agent("agent-1".to_string(), entity);
    }

    // ─── ToolRegistry call dispatch ───────────────────────────────────────

    #[tokio::test]
    async fn test_tool_registry_call_builtin() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // list_dir is a builtin tool
        let result = registry
            .call("list_dir", serde_json::json!({"path": "."}))
            .await;
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn test_tool_registry_call_unknown_mcp() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // Unknown MCP tool should return error
        let result = registry
            .call("nonexistent_mcp_tool", serde_json::json!({}))
            .await;
        assert!(result.contains("[error]"));
    }

    #[tokio::test]
    async fn test_tool_registry_shutdown_no_panic() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn test_tool_registry_all_defs_includes_subagent() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        let all_defs = registry.all_tool_defs();
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();
        // Should include subagent tools
        assert!(
            names.contains(&"spawn_agent"),
            "Expected spawn_agent in tool defs: {:?}",
            names
        );
    }

    // ─── default_tool_policy for all known builtin tools ──────────────────

    #[test]
    fn test_default_policy_search_is_ask() {
        assert_eq!(default_tool_policy("search", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_glob_is_ask() {
        assert_eq!(default_tool_policy("glob", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_http_request_is_ask() {
        assert_eq!(default_tool_policy("http_request", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_read_file_not_builtin_still_allow() {
        // Even if is_builtin is false, the name-based lookup should still match
        assert_eq!(default_tool_policy("read_file", false), ToolPolicy::Allow);
    }

    #[test]
    fn test_default_policy_list_dir_not_builtin_still_allow() {
        assert_eq!(default_tool_policy("list_dir", false), ToolPolicy::Allow);
    }

    // ─── resolve_policy: agent-level deny ─────────────────────────────────

    #[test]
    fn test_resolve_policy_agent_deny() {
        let mut agent = HashMap::new();
        agent.insert("read_file".to_string(), "deny".to_string());
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    // ─── resolve_policy: unknown agent-level string defaults to ask ───────

    #[test]
    fn test_resolve_policy_agent_unknown_string_defaults_to_ask() {
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "foobar".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &HashMap::new(),
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    // ─── resolve_policy: global allows override default ───────────────────

    #[test]
    fn test_resolve_policy_global_allow_overrides_default_ask() {
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &global,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    // ─── SubAgentExecutor tool definitions ────────────────────────────────

    #[tokio::test]
    async fn test_tool_registry_all_defs_includes_all_subagent_tools() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        let all_defs = registry.all_tool_defs();
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();

        for expected in &[
            "spawn_agent",
            "check_agent",
            "wait_for_agent",
            "send_to_agent",
            "kill_agent",
        ] {
            assert!(
                names.contains(expected),
                "Expected '{}' in tool defs, found: {:?}",
                expected,
                names
            );
        }
    }

    // ─── ToolRegistry.builtin_names includes known builtins ───────────────

    #[tokio::test]
    async fn test_tool_registry_builtin_names_has_expected_tools() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // These should be in builtin_names
        for name in &["read_file", "list_dir"] {
            assert!(
                registry.builtin_names.contains(*name),
                "Expected '{}' in builtin_names",
                name
            );
        }

        // Subagent tools should NOT be in builtin_names
        assert!(
            !registry.builtin_names.contains("spawn_agent"),
            "spawn_agent should not be in builtin_names"
        );
    }

    // ─── ToolRegistry.all_tool_defs does not duplicate ────────────────────

    #[tokio::test]
    async fn test_tool_registry_all_defs_no_mcp_when_none_configured() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        assert!(registry.mcp_tool_defs.is_empty());

        // Total defs = builtins + subagent tools
        let all_defs = registry.all_tool_defs();
        let builtin_count = registry.builtins.tool_defs().len();
        let subagent_count = leviath_tools::BuiltinTools::subagent_tool_defs().len();
        assert_eq!(all_defs.len(), builtin_count + subagent_count);
    }

    // ─── resolve_policy full chain: all four levels present ───────────────

    #[test]
    fn test_resolve_policy_full_chain_launch_highest() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "deny".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);

        let policy = resolve_policy("bash", true, &launch, &stage, &agent, &global);
        assert_eq!(policy, ToolPolicy::Allow);
    }

    // ─── SubAgentExecutor execute unknown tool ────────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_execute_unknown_tool() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "nonexistent_tool",
                &serde_json::json!({}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("Unknown sub-agent tool"));
    }

    // ─── SubAgentExecutor spawn missing blueprint ─────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_spawn_missing_blueprint() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "nonexistent", "task": "do stuff"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("[error]"));
        assert!(result.contains("not found"));
    }

    // ─── SubAgentExecutor spawn missing args ──────────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_spawn_missing_args() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        // Missing blueprint
        let result = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"task": "do stuff"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("missing 'blueprint'"));

        // Missing task
        let result = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "test"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("missing 'task'"));
    }

    // ─── SubAgentExecutor spawn exceeds max depth ─────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_spawn_exceeds_max_depth() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let bp = leviath_core::Blueprint::new(
            "test-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 0),
        );
        exec.register_blueprint(bp);

        // Depth 3, max_depth 3 -> child_depth = 4 > 3
        let result = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "test-bp", "task": "do stuff"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                3,
                3,
            )
            .await;
        assert!(result.contains("exceeds max depth"));
    }

    // ─── SubAgentExecutor check missing agent_id arg ──────────────────────

    #[tokio::test]
    async fn test_subagent_executor_check_missing_arg() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("missing 'agent_id'"));
    }

    // ─── SubAgentExecutor check nonexistent agent ─────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_check_nonexistent_agent() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": "ghost"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("not found"));
    }

    // ─── SubAgentExecutor send_to_agent missing args ──────────────────────

    #[tokio::test]
    async fn test_subagent_executor_send_missing_args() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        // Missing agent_id
        let result = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({"message": "hello"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("missing 'agent_id'"));

        // Missing message
        let result = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({"agent_id": "target"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("missing 'message'"));
    }

    // ─── SubAgentExecutor kill missing args ───────────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_kill_missing_arg() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "kill_agent",
                &serde_json::json!({}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("missing 'agent_id'"));
    }

    // ─── SubAgentExecutor kill nonexistent agent ──────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_kill_nonexistent() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "kill_agent",
                &serde_json::json!({"agent_id": "ghost"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("not found"));
    }

    // ─── SubAgentExecutor wait_for_agent missing arg ──────────────────────

    #[tokio::test]
    async fn test_subagent_executor_wait_missing_arg() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("missing 'agent_id'"));
    }

    // ─── SubAgentExecutor wait_for_agent nonexistent ──────────────────────

    #[tokio::test]
    async fn test_subagent_executor_wait_nonexistent() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine_arc);

        let result = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": "ghost"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("not found"));
    }

    // ─── ToolRegistry build with failing MCP server ────────────────────────
    // Exercises the Err branch (lines 52-58): a bad command fails to connect.

    #[tokio::test]
    async fn test_tool_registry_build_with_failing_mcp_server() {
        use leviath_mcp::MCPServerConfig;
        use std::collections::HashMap as StdHashMap;

        let bad_server = MCPServerConfig {
            name: "bad-server".to_string(),
            command: "/nonexistent/binary/that/does/not/exist".to_string(),
            args: vec![],
            env: StdHashMap::new(),
        };
        let config = Config {
            mcp_servers: vec![bad_server],
            ..Config::default()
        };

        let workdir = std::env::current_dir().unwrap();
        // Should not panic; the error branch is non-fatal (just a tracing::warn)
        let registry = ToolRegistry::build(workdir, &config).await;

        // MCP tool defs should be empty because connection failed
        assert!(registry.mcp_tool_defs.is_empty());
        // Built-ins should still be present
        assert!(!registry.builtin_names.is_empty());
    }

    // ─── exec_spawn success path ──────────────────────────────────────────
    // Register a blueprint, spawn a caller entity in the world, then call spawn.
    // Uses multi_thread flavor because exec_spawn internally calls blocking_write().

    #[tokio::test]
    async fn test_subagent_executor_spawn_success() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        // Spawn a caller entity in the world so entity_mut(caller_entity) works
        let caller_entity = {
            let mut eng = engine_arc.write().await;
            eng.world_mut().spawn(()).id()
        };

        let bp = leviath_core::Blueprint::new(
            "spawn-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );
        exec.register_blueprint(bp);

        let result = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "spawn-bp", "task": "do work"}),
                "caller-agent",
                caller_entity,
                0,
                3,
            )
            .await;
        assert!(
            result.contains("Spawned sub-agent"),
            "Expected spawn success, got: {}",
            result
        );
        assert!(result.contains("spawn-bp"));
    }

    #[tokio::test]
    async fn test_subagent_executor_spawn_with_seed_context() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let caller_entity = {
            let mut eng = engine_arc.write().await;
            eng.world_mut().spawn(()).id()
        };

        let bp = leviath_core::Blueprint::new(
            "seed-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );
        exec.register_blueprint(bp);

        // spawn_agent with seed_context
        let result = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({
                    "blueprint": "seed-bp",
                    "task": "seed task",
                    "seed_context": "Initial context for the agent"
                }),
                "caller-seed",
                caller_entity,
                0,
                3,
            )
            .await;
        assert!(
            result.contains("Spawned sub-agent"),
            "Expected spawn success, got: {}",
            result
        );
    }

    // ─── exec_check with a real spawned agent ─────────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_check_registered_agent() {
        use leviath_runtime::AgentPool;

        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let bp = leviath_core::Blueprint::new(
            "check-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );

        // Spawn agent into world directly through pool
        let agent_id = {
            let mut eng = engine_arc.write().await;
            let mut pool = AgentPool::new(bp);
            let id = pool.spawn_agent(eng.world_mut());
            // Register entity in exec's lookup
            let entity = pool.get_agent(&id).unwrap();
            exec.register_agent(id.clone(), entity);
            id
        };

        // check_agent — should return status
        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(
            result.contains("Status:"),
            "Expected status output, got: {}",
            result
        );
    }

    /// Spawn an agent, register it, set its AgentState.status, and return
    /// (executor, agent_id) for exec_check-style tests.
    async fn spawn_agent_with_status(
        status: leviath_runtime::AgentStatus,
    ) -> (SubAgentExecutor, String) {
        use leviath_runtime::AgentPool;

        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let bp = leviath_core::Blueprint::new(
            "status-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );

        let agent_id = {
            let mut eng = engine_arc.write().await;
            let mut pool = AgentPool::new(bp);
            let id = pool.spawn_agent(eng.world_mut());
            let entity = pool.get_agent(&id).unwrap();
            exec.register_agent(id.clone(), entity);
            if let Some(mut state) = eng.world_mut().get_mut::<AgentState>(entity) {
                state.status = status;
            }
            id
        };

        (exec, agent_id)
    }

    #[tokio::test]
    async fn test_exec_check_reports_waiting_status() {
        let (exec, agent_id) = spawn_agent_with_status(AgentStatus::Waiting).await;
        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert_eq!(result, "Status: waiting");
    }

    #[tokio::test]
    async fn test_exec_check_reports_error_status_with_message() {
        let (exec, agent_id) = spawn_agent_with_status(AgentStatus::Error {
            message: "boom".to_string(),
        })
        .await;
        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert_eq!(result, "Status: error: boom");
    }

    #[tokio::test]
    async fn test_exec_check_reports_cancelled_status() {
        let (exec, agent_id) = spawn_agent_with_status(AgentStatus::Cancelled).await;
        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert_eq!(result, "Status: cancelled");
    }

    #[tokio::test]
    async fn test_exec_check_reports_idle_status() {
        let (exec, agent_id) = spawn_agent_with_status(AgentStatus::Idle).await;
        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert_eq!(result, "Status: idle");
    }

    #[tokio::test]
    async fn test_exec_check_complete_status_includes_last_conversation_entry() {
        let (exec, agent_id) = spawn_agent_with_status(AgentStatus::Complete).await;
        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        // No conversation region content was added, so it should still say
        // "Status: complete" without a "Result:" line — but must not panic.
        assert!(result.starts_with("Status: complete"));
    }

    #[tokio::test]
    async fn test_exec_check_entity_no_longer_exists() {
        // Register an agent_id pointing to an Entity that doesn't exist in
        // the world at all (never spawned).
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));
        exec.register_agent(
            "ghost-agent".to_string(),
            bevy_ecs::prelude::Entity::from_raw(9999),
        );

        let result = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": "ghost-agent"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("no state") || result.contains("[error]"));
    }

    // ─── exec_wait: completes, errors, cancelled ────────────────────────────

    #[tokio::test]
    async fn test_exec_wait_returns_when_agent_completes() {
        use leviath_runtime::AgentPool;

        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let bp = leviath_core::Blueprint::new(
            "wait-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );

        let (agent_id, entity) = {
            let mut eng = engine_arc.write().await;
            let mut pool = AgentPool::new(bp);
            let id = pool.spawn_agent(eng.world_mut());
            let entity = pool.get_agent(&id).unwrap();
            exec.register_agent(id.clone(), entity);
            (id, entity)
        };

        // Flip to Complete after a short delay, from a background task.
        let engine_arc2 = Arc::clone(&engine_arc);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let mut eng = engine_arc2.write().await;
            if let Some(mut state) = eng.world_mut().get_mut::<AgentState>(entity) {
                state.status = AgentStatus::Complete;
            }
        });

        let result = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("completed"), "got: {}", result);
    }

    #[tokio::test]
    async fn test_exec_wait_returns_when_agent_errors() {
        use leviath_runtime::AgentPool;

        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let bp = leviath_core::Blueprint::new(
            "wait-err-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );

        let (agent_id, entity) = {
            let mut eng = engine_arc.write().await;
            let mut pool = AgentPool::new(bp);
            let id = pool.spawn_agent(eng.world_mut());
            let entity = pool.get_agent(&id).unwrap();
            exec.register_agent(id.clone(), entity);
            (id, entity)
        };

        let engine_arc2 = Arc::clone(&engine_arc);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let mut eng = engine_arc2.write().await;
            if let Some(mut state) = eng.world_mut().get_mut::<AgentState>(entity) {
                state.status = AgentStatus::Error {
                    message: "oops".to_string(),
                };
            }
        });

        let result = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("failed"), "got: {}", result);
        assert!(result.contains("oops"), "got: {}", result);
    }

    #[tokio::test]
    async fn test_exec_wait_returns_when_agent_cancelled() {
        use leviath_runtime::AgentPool;

        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let bp = leviath_core::Blueprint::new(
            "wait-cancel-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );

        let (agent_id, entity) = {
            let mut eng = engine_arc.write().await;
            let mut pool = AgentPool::new(bp);
            let id = pool.spawn_agent(eng.world_mut());
            let entity = pool.get_agent(&id).unwrap();
            exec.register_agent(id.clone(), entity);
            (id, entity)
        };

        let engine_arc2 = Arc::clone(&engine_arc);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let mut eng = engine_arc2.write().await;
            if let Some(mut state) = eng.world_mut().get_mut::<AgentState>(entity) {
                state.status = AgentStatus::Cancelled;
            }
        });

        let result = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("cancelled"), "got: {}", result);
    }

    #[tokio::test]
    async fn test_exec_wait_entity_no_longer_exists() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));
        exec.register_agent(
            "ghost-wait".to_string(),
            bevy_ecs::prelude::Entity::from_raw(9999),
        );

        let result = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": "ghost-wait"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(result.contains("no longer exists"), "got: {}", result);
    }

    // ─── exec_send success path ────────────────────────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_send_success() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        // send_to_agent just sends to the channel; should succeed even if agent
        // doesn't exist in the world
        let result = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({"agent_id": "some-agent", "message": "hello!"}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(
            result.contains("Message sent"),
            "Expected message sent, got: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_subagent_executor_send_with_target_region() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let result = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({
                    "agent_id": "target-agent",
                    "message": "important update",
                    "target_region": "conversation"
                }),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(
            result.contains("Message sent"),
            "Expected message sent, got: {}",
            result
        );
    }

    // ─── exec_kill success path ────────────────────────────────────────────

    #[tokio::test]
    async fn test_subagent_executor_kill_registered_agent() {
        use leviath_runtime::AgentPool;

        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        let bp = leviath_core::Blueprint::new(
            "kill-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );

        let agent_id = {
            let mut eng = engine_arc.write().await;
            let mut pool = AgentPool::new(bp);
            let id = pool.spawn_agent(eng.world_mut());
            let entity = pool.get_agent(&id).unwrap();
            exec.register_agent(id.clone(), entity);
            id
        };

        let result = exec
            .execute(
                "kill_agent",
                &serde_json::json!({"agent_id": agent_id}),
                "caller",
                bevy_ecs::prelude::Entity::from_raw(0),
                0,
                3,
            )
            .await;
        assert!(
            result.contains("Killed agent"),
            "Expected kill confirmation, got: {}",
            result
        );
        // Single agent killed (no descendants)
        assert!(
            result.contains(&agent_id),
            "Result should include agent ID: {}",
            result
        );
    }

    // ─── exec_spawn second spawn (SubAgentChildren already present) ───────

    #[tokio::test]
    async fn test_subagent_executor_spawn_second_child_updates_children() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let engine = leviath_runtime::AgentEngine::with_providers(registry);
        let engine_arc = Arc::new(tokio::sync::RwLock::new(engine));
        let exec = SubAgentExecutor::new(Arc::clone(&engine_arc));

        // Pre-spawn a caller entity that has AgentState (for spawned_children_ids update)
        let caller_entity = {
            let mut eng = engine_arc.write().await;
            eng.world_mut().spawn(()).id()
        };

        let bp = leviath_core::Blueprint::new(
            "multi-spawn-bp".to_string(),
            "desc".to_string(),
            vec![],
            leviath_core::ContextLayout::new(vec![], 128_000),
        );
        exec.register_blueprint(bp);

        // First spawn
        let r1 = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "multi-spawn-bp", "task": "task 1"}),
                "caller-multi",
                caller_entity,
                0,
                3,
            )
            .await;
        assert!(r1.contains("Spawned"), "First spawn failed: {}", r1);

        // Second spawn — exercises the `SubAgentChildren already exists` branch
        let r2 = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "multi-spawn-bp", "task": "task 2"}),
                "caller-multi",
                caller_entity,
                0,
                3,
            )
            .await;
        assert!(r2.contains("Spawned"), "Second spawn failed: {}", r2);
    }
}
