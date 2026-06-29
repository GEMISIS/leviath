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

use leviath_runtime::{
    AgentEngine, AgentPool, AgentState, AgentStatus, CancellationToken, ContextWindow,
    SubAgentChildren, ParentRef,
};
use leviath_core::Blueprint;
use bevy_ecs::prelude::Entity;
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
                self.exec_spawn(args, caller_agent_id, caller_entity, caller_depth, max_depth)
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
        let _wait = args
            .get("wait")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
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
            let mut pools = self.pools.write().unwrap();
            let pool = pools
                .entry(blueprint_name.clone())
                .or_insert_with(|| AgentPool::new(blueprint.clone()));
            let mut engine = self.engine.blocking_write();
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
            let mut engine = self.engine.blocking_write();

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
                if let Some(mut children) =
                    engine.world_mut().get_mut::<SubAgentChildren>(caller_entity)
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
                if let Some(mut window) =
                    engine.world_mut().get_mut::<ContextWindow>(child_entity)
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

        let engine = self.engine.blocking_read();
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
                let mut engine = self.engine.blocking_write();
                if let Some(mut state) = engine.world_mut().get_mut::<AgentState>(ce) {
                    state.pending_wait = Some(agent_id.clone());
                }
            }
        }

        // Poll until child completes (check every 500ms)
        loop {
            {
                let engine = self.engine.blocking_read();
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
                                let mut eng = self.engine.blocking_write();
                                if let Some(mut pstate) =
                                    eng.world_mut().get_mut::<AgentState>(ce)
                                {
                                    pstate.pending_wait = None;
                                }
                            }

                            // Get final result
                            let eng = self.engine.blocking_read();
                            let result = eng
                                .world()
                                .get::<ContextWindow>(entity)
                                .and_then(|w| {
                                    w.get_region("conversation")
                                        .and_then(|r| r.content.last())
                                        .map(|e| e.content.clone())
                                })
                                .unwrap_or_else(|| "(no result)".to_string());
                            return format!(
                                "Agent '{}' completed.\nResult: {}",
                                agent_id, result
                            );
                        }
                        AgentStatus::Error { message } => {
                            return format!(
                                "Agent '{}' failed with error: {}",
                                agent_id, message
                            );
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

        let engine = self.engine.blocking_read();
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
            let engine = self.engine.blocking_read();
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
            let mut engine = self.engine.blocking_write();
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
}
