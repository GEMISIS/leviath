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

        Self {
            builtins,
            mcp: Arc::new(Mutex::new(mcp_executor)),
            mcp_tool_defs,
            builtin_names,
        }
    }

    /// All tool definitions to advertise to the LLM (built-ins + MCP).
    pub fn all_tool_defs(&self) -> Vec<Tool> {
        let mut tools = self.builtins.tool_defs();
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
        let policy = resolve_policy("bash", true, &launch, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_yolo_wins() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy("bash", true, &launch, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_stage_beats_global() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "allow".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy("bash", true, &HashMap::new(), &stage, &HashMap::new(), &global);
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_falls_through_to_default() {
        let policy = resolve_policy("bash", true, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(policy, ToolPolicy::Ask);
    }
}
