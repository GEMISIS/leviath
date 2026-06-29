//! Tool discovery via MCP.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::client::MCPClient;

/// Discovered tool metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// Tool name
    pub name: String,
    /// Tool description
    #[serde(default)]
    pub description: String,
    /// Parameter schema (JSON Schema)
    #[serde(rename = "inputSchema", default)]
    pub schema: serde_json::Value,
}

/// Configuration for an MCP server connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPServerConfig {
    /// Server name (used as an identifier)
    pub name: String,
    /// Command to launch the server
    pub command: String,
    /// Arguments to pass to the command
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Tool discovery service that aggregates tools from multiple MCP servers.
pub struct ToolDiscovery {
    /// Discovered tools from all connected servers, keyed by server name
    servers: HashMap<String, Vec<ToolMetadata>>,
}

impl ToolDiscovery {
    /// Create a new tool discovery service.
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    /// Discover tools from an already-connected MCP client.
    pub async fn discover_from_client(
        &mut self,
        server_name: &str,
        client: &mut MCPClient,
    ) -> anyhow::Result<Vec<ToolMetadata>> {
        tracing::info!(server = %server_name, "Discovering tools from MCP client");

        let tools = client.list_tools().await?;
        self.servers.insert(server_name.to_string(), tools.clone());

        tracing::info!(
            server = %server_name,
            tool_count = tools.len(),
            "Discovered tools"
        );

        Ok(tools)
    }

    /// Discover tools by spawning a client from a server config.
    ///
    /// Spawns the server process, connects, and discovers tools.
    /// Returns the tools and the live client (caller keeps it alive for tool calls).
    pub async fn discover_from_config(
        &mut self,
        config: &MCPServerConfig,
    ) -> anyhow::Result<(Vec<ToolMetadata>, MCPClient)> {
        tracing::info!(server = %config.name, command = %config.command, "Spawning MCP server from config");

        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        let mut client = MCPClient::spawn(&config.command, &args, &config.env).await?;
        client.connect().await?;

        let tools = self.discover_from_client(&config.name, &mut client).await?;
        Ok((tools, client))
    }

    /// Return all discovered tools across all servers.
    pub fn all_tools(&self) -> Vec<&ToolMetadata> {
        self.servers
            .values()
            .flat_map(|tools| tools.iter())
            .collect()
    }

    /// Find a tool by name, returning the server name and tool metadata.
    pub fn find_tool(&self, name: &str) -> Option<(&str, &ToolMetadata)> {
        for (server_name, tools) in &self.servers {
            if let Some(tool) = tools.iter().find(|t| t.name == name) {
                return Some((server_name.as_str(), tool));
            }
        }
        None
    }

    /// Get tools for a specific server.
    pub fn server_tools(&self, server_name: &str) -> Option<&[ToolMetadata]> {
        self.servers.get(server_name).map(|v| v.as_slice())
    }

    /// Get the number of servers registered.
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }
}

impl Default for ToolDiscovery {
    fn default() -> Self {
        Self::new()
    }
}
