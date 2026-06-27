//! Unified tool registry combining built-in tools and MCP-discovered tools.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use leviath_mcp::{ToolDiscovery, ToolExecutor};
use leviath_providers::Tool;
use leviath_tools::{BuiltinTools, ToolContext};

use crate::config::Config;

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
