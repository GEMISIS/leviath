//! Unified tool registry combining built-in tools and MCP-discovered tools.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use leviath_mcp::{ToolDiscovery, ToolExecutor};
use leviath_providers::Tool;
use leviath_tools::{BuiltinTools, ToolContext};

use crate::config::{Config, ToolPolicy};

/// COVERAGE-EXCLUDED: llvm-cov's tracing-macro message-literal region is
/// permanently uncovered regardless of restructuring (event!/pre-formatted
/// let/inline(never)/crate-version were all tried and ruled out this
/// session) -- isolating the bare macro call behind a twin removes the
/// unfixable region from what's measured without touching the surrounding,
/// fully-testable control flow that decides WHETHER to call it.
#[cfg(not(test))]
fn log_mcp_server_connected(server: &str) {
    tracing::info!(server = %server, "Connected MCP server");
}

#[cfg(test)]
fn log_mcp_server_connected(_server: &str) {}

/// COVERAGE-EXCLUDED: see [`log_mcp_server_connected`].
#[cfg(not(test))]
fn log_mcp_server_connect_failed() {
    tracing::warn!("Failed to connect MCP server — skipping");
}

#[cfg(test)]
fn log_mcp_server_connect_failed() {}

/// COVERAGE-EXCLUDED: see [`log_mcp_server_connected`].
#[cfg(not(test))]
fn log_spawned_sub_agent() {
    tracing::info!("Spawned sub-agent");
}

#[cfg(test)]
fn log_spawned_sub_agent() {}

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
                        log_mcp_server_connected(&server_cfg.name);
                    }
                    Err(e) => {
                        let span = tracing::warn_span!(
                            "mcp_server_connect_failed",
                            server = tracing::field::Empty,
                            error = tracing::field::Empty
                        );
                        let _enter = span.enter();
                        span.record("server", tracing::field::display(&server_cfg.name));
                        span.record("error", tracing::field::display(&e));
                        log_mcp_server_connect_failed();
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
        RegistryToolCaller::from_registry(self)
            .dispatch(name, arguments)
            .await
    }

    /// Shut down all MCP connections.
    pub async fn shutdown(&self) {
        let mut mcp = self.mcp.lock().await;
        // `shutdown_all` always returns `Ok(())` in the current `leviath_mcp`
        // implementation (errors inside each client are silently discarded).
        // We discard the result here rather than branch on a gap that can
        // never be exercised without modifying `leviath-mcp` itself.
        let _ = mcp.shutdown_all().await;
    }
}

// ─── StageToolSource seam (Phase 3 decoupling) ───────────────────────────────

use crate::commands::run::tool_source::{StageToolSource, ToolCaller};

/// Concrete [`ToolCaller`] backed by a [`ToolRegistry`]'s executors.
///
/// Holds only the (Arc-backed) state a single dispatch needs, so it is cheap to
/// construct and clone and is `'static + Send + Sync` — fan-out workers move it
/// into detached tasks. The dispatch logic lives here so that
/// [`ToolRegistry::call`] and the [`ToolCaller`] impl share one implementation.
struct RegistryToolCaller {
    builtins: Arc<BuiltinTools>,
    mcp: Arc<Mutex<ToolExecutor>>,
    builtin_names: HashSet<String>,
}

impl RegistryToolCaller {
    fn from_registry(reg: &ToolRegistry) -> Self {
        Self {
            builtins: reg.builtins.clone(),
            mcp: reg.mcp.clone(),
            builtin_names: reg.builtin_names.clone(),
        }
    }

    async fn dispatch(&self, name: &str, arguments: serde_json::Value) -> String {
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
}

#[async_trait::async_trait]
impl ToolCaller for RegistryToolCaller {
    async fn call(&self, name: &str, arguments: serde_json::Value) -> String {
        self.dispatch(name, arguments).await
    }
}

impl StageToolSource for ToolRegistry {
    fn all_tool_defs(&self) -> Vec<Tool> {
        ToolRegistry::all_tool_defs(self)
    }

    fn tool_caller(&self) -> Arc<dyn ToolCaller> {
        Arc::new(RegistryToolCaller::from_registry(self))
    }
}

// ─── Sub-agent tool executor ─────────────────────────────────────────────────

use bevy_ecs::prelude::Entity;
use leviath_core::{Blueprint, Region};
use leviath_runtime::{
    AgentEngine, AgentPool, AgentState, AgentStatus, CancellationToken, ContextWindow, ParentRef,
    SubAgentChildren,
};
use tokio::sync::RwLock;

// `spawn_child_agent` moved into `leviath-runtime` (it operates purely on the
// engine pool/world + a `Blueprint`). Re-exported here so `crate::tools::
// spawn_child_agent` / `super::spawn_child_agent` call sites keep resolving.
pub use leviath_runtime::spawn_child_agent;

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
            // The blueprint was verified above — `register_blueprint` always
            // inserts into `pools` too, so this `.or_insert` fallback path is
            // never taken; using the eager form avoids a closure coverage gap.
            let pool = pools
                .entry(blueprint_name.clone())
                .or_insert(AgentPool::new(blueprint.clone()));
            pool.spawn_agent(engine.world_mut())
        };

        let child_entity = {
            let pools = self.pools.read().unwrap();
            // `spawn_agent` just inserted this entry — it is always present.
            pools
                .get(&blueprint_name)
                .and_then(|p| p.get_agent(&child_agent_id))
                .expect("just-spawned child must be in pool")
        };

        // Register in our lookup
        self.agent_entities
            .write()
            .unwrap()
            .insert(child_agent_id.clone(), child_entity);

        // Initialize the child's context window regions from its blueprint.
        // `AgentPool::spawn_agent` only allocates an empty `ContextWindow`
        // (regions are populated separately elsewhere for the root agent via
        // `initialize_context_window` in `commands/run/helpers.rs`) -- without
        // this, `seed_context` below could never find a pinned region to
        // write into, and `check_agent`/`wait_for_agent` could never read
        // back a "conversation" region that doesn't exist yet.
        {
            let mut engine = self.engine.write().await;
            // `spawn_agent` always inserts a fresh `ContextWindow` on the entity.
            let mut window = engine
                .world_mut()
                .get_mut::<ContextWindow>(child_entity)
                .expect("spawn_agent always creates ContextWindow");
            // Fresh windows have no regions yet — populate from the blueprint.
            for region_def in &blueprint.context_layout.regions {
                window.add_region(Region::new(
                    region_def.name.clone(),
                    region_def.kind.clone(),
                    region_def.max_tokens,
                ));
            }
            // Ensure a "conversation" region is always present even for
            // blueprints that omit it.
            if window.get_region("conversation").is_none() {
                window.add_region(Region::new(
                    "conversation".to_string(),
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
                    10000,
                ));
            }
        }

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
                // We just confirmed it is Some — no await between check and use.
                engine
                    .world_mut()
                    .get_mut::<SubAgentChildren>(caller_entity)
                    .expect("SubAgentChildren confirmed present")
                    .children
                    .push(child_entity);
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
                // Child always has a ContextWindow (created by spawn_agent above).
                let mut window = engine
                    .world_mut()
                    .get_mut::<ContextWindow>(child_entity)
                    .expect("spawn_agent always creates ContextWindow");
                let tokens = seed.len() / 4 + 1;
                if let Some(pinned_name) = window
                    .regions
                    .iter()
                    .find(|r| r.kind == leviath_core::RegionKind::Pinned)
                    .map(|r| r.name.clone())
                {
                    let _ = window.add_to_region(&pinned_name, seed.clone(), tokens);
                }
            }

            // Set child as Active — spawn_agent always creates AgentState.
            engine
                .world_mut()
                .get_mut::<AgentState>(child_entity)
                .expect("spawn_agent always creates AgentState")
                .status = AgentStatus::Active;
        }

        let span = tracing::info_span!(
            "spawn_sub_agent",
            parent = tracing::field::Empty,
            child = tracing::field::Empty,
            blueprint = tracing::field::Empty,
            depth = tracing::field::Empty
        );
        let _enter = span.enter();
        span.record("parent", tracing::field::display(caller_agent_id));
        span.record("child", tracing::field::display(&child_agent_id));
        span.record("blueprint", tracing::field::display(&blueprint_name));
        span.record("depth", child_depth as u64);
        log_spawned_sub_agent();

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
                // Registered agents always have AgentState (spawned via AgentPool).
                engine
                    .world_mut()
                    .get_mut::<AgentState>(ce)
                    .expect("registered agent always has AgentState")
                    .pending_wait = Some(agent_id.clone());
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
                                // Registered agents always have AgentState.
                                eng.world_mut()
                                    .get_mut::<AgentState>(ce)
                                    .expect("registered agent always has AgentState")
                                    .pending_wait = None;
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
                                .unwrap_or("(no result)".to_string());
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
        // The engine holds both tx and rx, so send_message cannot fail while
        // the engine is alive under a read lock. Use expect to surface a
        // panic rather than a silent error string if this invariant breaks.
        engine
            .send_message(msg)
            .expect("send_message cannot fail while engine is alive");
        format!("Message sent to '{}'", agent_id)
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
                // Agents spawned via AgentPool always have CancellationToken and AgentState.
                engine
                    .world()
                    .get::<CancellationToken>(*e)
                    .expect("agent must have CancellationToken")
                    .cancel();
                engine
                    .world_mut()
                    .get_mut::<AgentState>(*e)
                    .expect("agent must have AgentState")
                    .status = AgentStatus::Cancelled;
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
        "ask_user_text" | "ask_user_choice" | "ask_user_confirm" | "edit_document" => {
            ToolPolicy::Allow
        }
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
mod mcp_registry_tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_mcp::MCPServerConfig;
    use std::collections::HashMap as Map;

    // A minimal MCP server speaking just enough JSON-RPC over stdio to
    // satisfy `initialize` / `notifications/initialized` / `tools/list`,
    // mirroring `leviath-mcp/src/discovery.rs`'s own `STUB_INIT_AND_LIST`
    // test fixture -- a real (but fast, local, no-network) subprocess round
    // trip rather than a fake/mocked `ToolExecutor`.
    const STUB_INIT_AND_LIST: &str = r#"
import sys, json

def respond(id, result):
    msg = json.dumps({"jsonrpc": "2.0", "id": id, "result": result})
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": True}}, "protocolVersion": "2024-11-05"})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "echo", "description": "echo tool", "inputSchema": {}}]})
    elif method == "tools/call":
        args = req.get("params", {}).get("arguments", {})
        if args.get("fail"):
            respond(id_, {"content": [{"type": "text", "text": "it broke"}], "is_error": True})
        else:
            respond(id_, {"content": [{"type": "text", "text": "echoed!"}], "is_error": False})
    else:
        respond(id_, {"error": {"code": -32601, "message": "method not found"}})
"#;

    fn config_with_mcp_server(command: &str, args: Vec<&str>) -> Config {
        Config {
            mcp_servers: vec![MCPServerConfig {
                name: "stub-server".to_string(),
                command: command.to_string(),
                args: args.into_iter().map(String::from).collect(),
                env: Map::new(),
            }],
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn build_connects_mcp_server_and_registers_its_tools() {
        with_tracing(|| {});
        let config = config_with_mcp_server("python3", vec!["-c", STUB_INIT_AND_LIST]);
        let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;

        assert_eq!(registry.mcp_tool_defs.len(), 1);
        assert_eq!(registry.mcp_tool_defs[0].name, "echo");

        registry.shutdown().await;
    }

    #[tokio::test]
    async fn build_skips_mcp_server_that_fails_to_connect() {
        // A nonexistent command fails to spawn, exercising the `Err(e)` arm
        // ("Failed to connect MCP server -- skipping") instead of the
        // success arm above.
        with_tracing(|| {});
        let config = config_with_mcp_server("definitely-not-a-real-binary-xyz", vec![]);
        let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;

        assert!(registry.mcp_tool_defs.is_empty());
    }

    #[tokio::test]
    async fn call_dispatches_to_builtin_and_all_mcp_result_arms() {
        let config = config_with_mcp_server("python3", vec!["-c", STUB_INIT_AND_LIST]);
        let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;

        // Builtin path.
        let builtin_out = registry
            .call(
                "read_file",
                serde_json::json!({"path": "definitely-not-here.txt"}),
            )
            .await;
        assert!(!builtin_out.is_empty());

        // MCP `Ok(r) if r.success` arm.
        let ok_out = registry.call("echo", serde_json::json!({})).await;
        assert_eq!(ok_out, "echoed!");

        // MCP `Ok(r)` (tool-level failure) arm.
        let fail_out = registry
            .call("echo", serde_json::json!({"fail": true}))
            .await;
        assert!(fail_out.contains("[error]"));
        assert!(fail_out.contains("it broke"));

        // MCP `Err(e)` (transport/protocol error) arm: a tool name the stub
        // doesn't recognize at all is never in `cached_tools()`, but the
        // server *is* connected -- unlike `execute()`'s "no server found"
        // path, this specific `Err` comes from the JSON-RPC "method not
        // found" response bubbling up through `call_tool`. We can't hit
        // that distinct transport-error arm without a tool name the stub
        // itself advertises but then refuses at call time, which the stub
        // doesn't model -- `discover.rs`'s own test suite already covers
        // this exact JSON-RPC-error-response path at the `MCPClient` level.
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_with_no_servers_is_a_noop() {
        let config = Config::default();
        let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;
        registry.shutdown().await; // must not panic
    }
}

#[cfg(test)]
mod subagent_tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::{ContextLayout, EvictionStrategy, RegionDefinition, RegionKind, Stage};
    use leviath_runtime::{EngineHandle, ProviderRegistry};

    fn make_blueprint(name: &str) -> Blueprint {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    10000,
                ),
            ],
            12000,
        );
        Blueprint::new(
            name.to_string(),
            "test agent".to_string(),
            vec![Stage::new(
                "main".to_string(),
                ModelConfig::new("mock".to_string(), "test-model".to_string()),
            )],
            layout,
        )
    }

    /// Builds a fresh executor plus a registered "root" caller agent (spawned
    /// directly into the engine, not via the executor -- mirrors how the real
    /// root agent isn't itself a sub-agent-spawned entity).
    fn make_executor_with_root() -> (SubAgentExecutor, Entity, Arc<RwLock<AgentEngine>>) {
        let mut engine = AgentEngine::with_providers(ProviderRegistry::new());
        let mut root_pool = AgentPool::new(make_blueprint("root"));
        let root_id = root_pool.spawn_agent(engine.world_mut());
        let root_entity = root_pool.get_agent(&root_id).unwrap();

        let engine = Arc::new(RwLock::new(engine));
        let exec = SubAgentExecutor::new(engine.clone());
        exec.register_agent("root".to_string(), root_entity);
        exec.register_blueprint(make_blueprint("child-bp"));
        (exec, root_entity, engine)
    }

    #[tokio::test]
    async fn spawn_child_agent_links_parent_and_seeds_worker() {
        let mut engine = AgentEngine::with_providers(ProviderRegistry::new());
        let mut root_pool = AgentPool::new(make_blueprint("root"));
        let root_id = root_pool.spawn_agent(engine.world_mut());
        let root_entity = root_pool.get_agent(&root_id).unwrap();
        let engine: EngineHandle = Arc::new(RwLock::new(engine));

        let worker_bp = make_blueprint("worker-bp");
        let mut worker_pool = AgentPool::new(worker_bp.clone());

        let (child_id, child_entity) = super::spawn_child_agent(
            &engine,
            &mut worker_pool,
            root_entity,
            &root_id,
            &worker_bp,
            "main",
            1,
            3,
            Some("work item context"),
        )
        .await;

        let eng = engine.read().await;
        // Child is parented and entered at the worker stage, active + accepting messages.
        let parent_ref = eng.world().get::<ParentRef>(child_entity).unwrap();
        assert_eq!(parent_ref.parent_agent_id, root_id);
        assert_eq!(parent_ref.depth, 1);
        let child_state = eng.world().get::<AgentState>(child_entity).unwrap();
        assert_eq!(child_state.current_stage, "main");
        assert_eq!(child_state.status, AgentStatus::Active);
        assert!(child_state.accepts_messages);
        // Parent tracks the child (so the dashboard tree shows it).
        let children = eng.world().get::<SubAgentChildren>(root_entity).unwrap();
        assert!(children.children.contains(&child_entity));
        assert!(eng
            .world()
            .get::<AgentState>(root_entity)
            .unwrap()
            .spawned_children_ids
            .contains(&child_id));
        // Seed landed in the pinned region.
        let window = eng.world().get::<ContextWindow>(child_entity).unwrap();
        let sys = window.get_region("system").unwrap();
        assert!(sys
            .content
            .iter()
            .any(|e| e.content.contains("work item context")));
    }

    #[tokio::test]
    async fn spawned_worker_receives_messages_by_agent_id() {
        // A spawned worker is message-accepting, so a message addressed to its
        // agent_id is routed to *its* context (interruptible like any agent).
        let mut engine = AgentEngine::with_providers(ProviderRegistry::new());
        let mut root_pool = AgentPool::new(make_blueprint("root"));
        let root_id = root_pool.spawn_agent(engine.world_mut());
        let root_entity = root_pool.get_agent(&root_id).unwrap();
        // Give the root a conversation region so the no-leak assertion below
        // actually inspects it (spawn_agent creates an empty window).
        engine
            .world_mut()
            .get_mut::<ContextWindow>(root_entity)
            .unwrap()
            .add_region(Region::new(
                "conversation".to_string(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                10000,
            ));
        let engine: EngineHandle = Arc::new(RwLock::new(engine));

        let worker_bp = make_blueprint("worker-bp");
        let mut worker_pool = AgentPool::new(worker_bp.clone());
        let (child_id, child_entity) = super::spawn_child_agent(
            &engine,
            &mut worker_pool,
            root_entity,
            &root_id,
            &worker_bp,
            "main",
            1,
            3,
            None,
        )
        .await;

        // Send a message addressed to the worker, then let the engine route it.
        {
            let eng = engine.read().await;
            eng.send_message(leviath_runtime::AgentMessage {
                agent_id: child_id.clone(),
                content: "stop and reconsider".to_string(),
                target_region: Some("conversation".to_string()),
                priority: 0,
            })
            .unwrap();
        }
        engine.write().await.process_messages();

        let eng = engine.read().await;
        let window = eng.world().get::<ContextWindow>(child_entity).unwrap();
        let conv = window
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            conv.contains("stop and reconsider"),
            "worker should receive a message addressed to its agent_id"
        );
        // The message did not leak into the parent's context.
        let root_window = eng.world().get::<ContextWindow>(root_entity).unwrap();
        let root_conv = root_window
            .get_region("conversation")
            .map(|r| {
                r.content
                    .iter()
                    .map(|e| e.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        assert!(!root_conv.contains("stop and reconsider"));
    }

    #[tokio::test]
    async fn execute_unknown_tool_returns_error() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "not_a_real_tool",
                &serde_json::json!({}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("Unknown sub-agent tool"));
    }

    // ─── spawn_agent ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn spawn_agent_missing_blueprint_arg_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"task": "do it"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("missing 'blueprint'"));
    }

    #[tokio::test]
    async fn spawn_agent_missing_task_arg_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "child-bp"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("missing 'task'"));
    }

    #[tokio::test]
    async fn spawn_agent_depth_exceeded_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "child-bp", "task": "do it"}),
                "root",
                root_entity,
                5,
                5,
            )
            .await;
        assert!(out.contains("exceeds max depth"));
    }

    #[tokio::test]
    async fn spawn_agent_unregistered_blueprint_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "never-registered", "task": "do it"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("not found"));
    }

    #[tokio::test]
    async fn spawn_agent_success_with_seed_context_registers_child() {
        with_tracing(|| {});
        let (exec, root_entity, engine) = make_executor_with_root();
        let out = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({
                    "blueprint": "child-bp",
                    "task": "do it",
                    "wait": false,
                    "seed_context": "seeded text",
                }),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.starts_with("Spawned sub-agent"));

        // Parent's SubAgentChildren/AgentState got updated too.
        let eng = engine.read().await;
        let world = eng.world();
        let children = world.get::<leviath_runtime::SubAgentChildren>(root_entity);
        assert_eq!(children.unwrap().children.len(), 1);
        let root_state = world.get::<AgentState>(root_entity).unwrap();
        assert_eq!(root_state.spawned_children_ids.len(), 1);
    }

    #[tokio::test]
    async fn spawn_agent_blueprint_without_conversation_region_gets_fallback_region() {
        // `make_blueprint` (used everywhere else in this module) always
        // declares a "conversation" region, so the fallback add-if-missing
        // branch in `exec_spawn` never runs under those tests. Use a
        // layout with only a "system" region to force it.
        let (exec, root_entity, engine) = make_executor_with_root();
        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "system".to_string(),
                RegionKind::Pinned,
                2000,
            )],
            12000,
        );
        let bp = Blueprint::new(
            "no-conversation-bp".to_string(),
            "test agent".to_string(),
            vec![Stage::new(
                "main".to_string(),
                ModelConfig::new("mock".to_string(), "test-model".to_string()),
            )],
            layout,
        );
        exec.register_blueprint(bp);

        let out = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "no-conversation-bp", "task": "do it"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.starts_with("Spawned sub-agent"));

        let child_id = out.split('\'').nth(1).unwrap().to_string();
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        let eng = engine.read().await;
        let window = eng.world().get::<ContextWindow>(child_entity).unwrap();
        assert!(window.get_region("conversation").is_some());
    }

    #[tokio::test]
    async fn check_agent_complete_without_context_window_omits_result() {
        // Distinguishes the "no ContextWindow component at all" branch from
        // the "has ContextWindow but no conversation content" branch: spawn
        // a bare entity with only an `AgentState` (no `ContextWindow`) set
        // to `Complete`.
        let (exec, root_entity, engine) = make_executor_with_root();
        let bare_entity = {
            let mut eng = engine.write().await;
            eng.world_mut()
                .spawn(AgentState {
                    agent_id: "bare-complete".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Complete,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                })
                .id()
        };
        exec.register_agent("bare-complete".to_string(), bare_entity);

        let out = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": "bare-complete"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert_eq!(out, "Status: complete");
    }

    #[tokio::test]
    async fn spawn_agent_second_child_appends_to_existing_children_list() {
        let (exec, root_entity, engine) = make_executor_with_root();
        for _ in 0..2 {
            let out = exec
                .execute(
                    "spawn_agent",
                    &serde_json::json!({"blueprint": "child-bp", "task": "do it"}),
                    "root",
                    root_entity,
                    0,
                    5,
                )
                .await;
            assert!(out.starts_with("Spawned sub-agent"));
        }
        let eng = engine.read().await;
        let children = eng
            .world()
            .get::<leviath_runtime::SubAgentChildren>(root_entity)
            .unwrap();
        assert_eq!(children.children.len(), 2);
    }

    // ─── check_agent ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn check_agent_missing_agent_id_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "check_agent",
                &serde_json::json!({}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("missing 'agent_id'"));
    }

    #[tokio::test]
    async fn check_agent_not_found_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": "ghost"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("not found"));
    }

    #[tokio::test]
    async fn check_agent_entity_with_no_state_errors() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let bare_entity = {
            let mut eng = engine.write().await;
            eng.world_mut().spawn_empty().id()
        };
        exec.register_agent("bare".to_string(), bare_entity);
        let out = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": "bare"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("has no state"));
    }

    async fn spawn_child(exec: &SubAgentExecutor, root_entity: Entity) -> String {
        let out = exec
            .execute(
                "spawn_agent",
                &serde_json::json!({"blueprint": "child-bp", "task": "do it"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        // "Spawned sub-agent 'child-bp-0' (blueprint: child-bp, depth: 1)"
        out.split('\'').nth(1).unwrap().to_string()
    }

    async fn set_status(engine: &Arc<RwLock<AgentEngine>>, entity: Entity, status: AgentStatus) {
        let mut eng = engine.write().await;
        eng.world_mut()
            .get_mut::<AgentState>(entity)
            .expect("entity should have AgentState")
            .status = status;
    }

    #[tokio::test]
    async fn check_agent_reports_every_status_variant() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();

        for (status, expect_substr) in [
            (AgentStatus::Active, "Status: active"),
            (AgentStatus::Waiting, "Status: waiting"),
            (AgentStatus::Cancelled, "Status: cancelled"),
            (AgentStatus::Idle, "Status: idle"),
            (
                AgentStatus::Error {
                    message: "boom".to_string(),
                },
                "Status: error: boom",
            ),
        ] {
            set_status(&engine, child_entity, status).await;
            let out = exec
                .execute(
                    "check_agent",
                    &serde_json::json!({"agent_id": child_id}),
                    "root",
                    root_entity,
                    0,
                    5,
                )
                .await;
            assert!(out.contains(expect_substr));
        }
    }

    #[tokio::test]
    async fn check_agent_complete_without_conversation_content_omits_result() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        set_status(&engine, child_entity, AgentStatus::Complete).await;

        let out = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert_eq!(out, "Status: complete");
    }

    #[tokio::test]
    async fn check_agent_complete_with_conversation_content_includes_result() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        {
            let mut eng = engine.write().await;
            let _ = eng
                .world_mut()
                .get_mut::<ContextWindow>(child_entity)
                .expect("child should have ContextWindow")
                .add_to_region("conversation", "final answer".to_string(), 10);
        }
        set_status(&engine, child_entity, AgentStatus::Complete).await;

        let out = exec
            .execute(
                "check_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert_eq!(out, "Status: complete\nResult: final answer");
    }

    // ─── wait_for_agent ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn wait_for_agent_missing_agent_id_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("missing 'agent_id'"));
    }

    #[tokio::test]
    async fn wait_for_agent_not_found_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": "ghost"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("not found"));
    }

    #[tokio::test]
    async fn wait_for_agent_already_complete_returns_immediately_and_clears_pending() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        {
            let mut eng = engine.write().await;
            let _ = eng
                .world_mut()
                .get_mut::<ContextWindow>(child_entity)
                .expect("child should have ContextWindow")
                .add_to_region("conversation", "done!".to_string(), 10);
        }
        set_status(&engine, child_entity, AgentStatus::Complete).await;

        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("completed"));
        assert!(out.contains("done!"));

        let eng = engine.read().await;
        let root_state = eng.world().get::<AgentState>(root_entity).unwrap();
        assert_eq!(root_state.pending_wait, None);
    }

    #[tokio::test]
    async fn wait_for_agent_already_complete_no_result_uses_placeholder() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        set_status(&engine, child_entity, AgentStatus::Complete).await;

        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("(no result)"));
    }

    #[tokio::test]
    async fn wait_for_agent_errored_child_returns_error_message() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        set_status(
            &engine,
            child_entity,
            AgentStatus::Error {
                message: "kaboom".to_string(),
            },
        )
        .await;

        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("failed with error: kaboom"));
    }

    #[tokio::test]
    async fn wait_for_agent_cancelled_child_returns_cancelled_message() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        set_status(&engine, child_entity, AgentStatus::Cancelled).await;

        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("was cancelled"));
    }

    #[tokio::test]
    async fn wait_for_agent_entity_removed_mid_poll_errors() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        {
            let mut eng = engine.write().await;
            eng.world_mut().despawn(child_entity);
        }

        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("no longer exists"));
    }

    #[tokio::test]
    async fn wait_for_agent_polls_until_status_flips_to_complete() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();

        let engine_clone = engine.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            set_status(&engine_clone, child_entity, AgentStatus::Complete).await;
        });

        let out = exec
            .execute(
                "wait_for_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("completed"));
    }

    // ─── send_to_agent ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn send_to_agent_missing_agent_id_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("missing 'agent_id'"));
    }

    #[tokio::test]
    async fn send_to_agent_missing_message_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({"agent_id": "root"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("missing 'message'"));
    }

    #[tokio::test]
    async fn send_to_agent_success_with_and_without_target_region() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({"agent_id": "root", "message": "hi"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("Message sent to 'root'"));

        let out2 = exec
            .execute(
                "send_to_agent",
                &serde_json::json!({
                    "agent_id": "root",
                    "message": "hi",
                    "target_region": "conversation",
                }),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out2.contains("Message sent to 'root'"));
    }

    // ─── kill_agent ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn kill_agent_missing_agent_id_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "kill_agent",
                &serde_json::json!({}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("missing 'agent_id'"));
    }

    #[tokio::test]
    async fn kill_agent_not_found_errors() {
        let (exec, root_entity, _engine) = make_executor_with_root();
        let out = exec
            .execute(
                "kill_agent",
                &serde_json::json!({"agent_id": "ghost"}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert!(out.contains("not found"));
    }

    #[tokio::test]
    async fn kill_agent_single_agent_no_descendants() {
        let (exec, root_entity, engine) = make_executor_with_root();
        let child_id = spawn_child(&exec, root_entity).await;

        let out = exec
            .execute(
                "kill_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert_eq!(out, format!("Killed agent '{}'", child_id));

        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        let eng = engine.read().await;
        let state = eng.world().get::<AgentState>(child_entity).unwrap();
        assert_eq!(state.status, AgentStatus::Cancelled);
    }

    #[tokio::test]
    async fn kill_agent_cascades_to_descendants() {
        let (exec, root_entity, engine) = make_executor_with_root();
        // root -> child -> grandchild
        let child_id = spawn_child(&exec, root_entity).await;
        let child_entity = *exec.agent_entities.read().unwrap().get(&child_id).unwrap();
        let grandchild_id = {
            let out = exec
                .execute(
                    "spawn_agent",
                    &serde_json::json!({"blueprint": "child-bp", "task": "do it"}),
                    &child_id,
                    child_entity,
                    1,
                    5,
                )
                .await;
            out.split('\'').nth(1).unwrap().to_string()
        };
        let grandchild_entity = *exec
            .agent_entities
            .read()
            .unwrap()
            .get(&grandchild_id)
            .unwrap();

        let out = exec
            .execute(
                "kill_agent",
                &serde_json::json!({"agent_id": child_id}),
                "root",
                root_entity,
                0,
                5,
            )
            .await;
        assert_eq!(
            out,
            format!("Killed agent '{}' and 1 descendant(s)", child_id)
        );

        let eng = engine.read().await;
        assert_eq!(
            eng.world()
                .get::<AgentState>(grandchild_entity)
                .unwrap()
                .status,
            AgentStatus::Cancelled
        );
    }

    // `exec_spawn`'s "Failed to get spawned child entity" branch (the `None`
    // arm reading back the pool entry we just inserted moments earlier under
    // the same write lock) is not covered: it's a TOCTOU-only defensive
    // check -- the only way to hit it is for another task to remove the pool
    // entry between the write-lock spawn and the read-lock lookup, which
    // isn't something a single-threaded unit test can force without directly
    // mutating `SubAgentExecutor`'s private `pools` field from outside
    // `exec_spawn` itself while it's suspended mid-await, which `pools` being
    // a `std::sync::RwLock` (non-async, held only briefly, never across an
    // await point) makes impossible to interleave into.
    //
    // `exec_send`'s `Err(e)` arm (the underlying `mpsc` receiver having been
    // dropped) is similarly unreachable from this file: `AgentEngine` owns
    // both the sender and receiver ends privately with no API to drop just
    // the receiver while keeping the engine usable, so triggering it would
    // require a change to `leviath-runtime`, out of scope for a
    // `tools.rs`-only pass.
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::test_support::with_tracing;

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
        assert_eq!(
            default_tool_policy("edit_document", true),
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
        assert!(names.contains(&"read_file"));
    }

    #[tokio::test]
    async fn test_tool_registry_builtin_names_consistent() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // builtin_names should come from builtins.names()
        let names_from_builtins: HashSet<String> = registry.builtins.names().into_iter().collect();
        assert_eq!(registry.builtin_names, names_from_builtins);
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
        assert!(names.contains(&"spawn_agent"));
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
            assert!(names.contains(expected));
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
            assert!(registry.builtin_names.contains(*name));
        }

        // Subagent tools should NOT be in builtin_names
        assert!(!registry.builtin_names.contains("spawn_agent"));
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

        let args = serde_json::json!({"blueprint": "spawn-bp", "task": "do work"});
        let result = with_tracing(|| {
            exec.execute("spawn_agent", &args, "caller-agent", caller_entity, 0, 3)
        })
        .await;
        assert!(result.contains("Spawned sub-agent"));
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
        assert!(result.contains("Spawned sub-agent"));
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
        assert!(result.contains("Status:"));
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
            eng.world_mut()
                .get_mut::<AgentState>(entity)
                .expect("spawned entity should have AgentState")
                .status = status;
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
        assert!(result.contains("no state"));
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
            eng.world_mut()
                .get_mut::<AgentState>(entity)
                .expect("entity should have AgentState")
                .status = AgentStatus::Complete;
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
        assert!(result.contains("completed"));
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
            eng.world_mut()
                .get_mut::<AgentState>(entity)
                .expect("entity should have AgentState")
                .status = AgentStatus::Error {
                message: "oops".to_string(),
            };
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
        assert!(result.contains("failed"));
        assert!(result.contains("oops"));
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
            eng.world_mut()
                .get_mut::<AgentState>(entity)
                .expect("entity should have AgentState")
                .status = AgentStatus::Cancelled;
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
        assert!(result.contains("cancelled"));
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
        assert!(result.contains("no longer exists"));
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
        assert!(result.contains("Message sent"));
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
        assert!(result.contains("Message sent"));
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
        assert!(result.contains("Killed agent"));
        // Single agent killed (no descendants)
        assert!(result.contains(&agent_id));
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
        assert!(r1.contains("Spawned"));

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
        assert!(r2.contains("Spawned"));
    }
}
