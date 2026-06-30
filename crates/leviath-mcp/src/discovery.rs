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

#[cfg(test)]
mod tests {
    use super::*;

    // --- ToolMetadata serde ---

    #[test]
    fn tool_metadata_serde_roundtrip() {
        let meta = ToolMetadata {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: ToolMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "test_tool");
        assert_eq!(deserialized.description, "A test tool");
        assert_eq!(deserialized.schema, serde_json::json!({"type": "object"}));
    }

    #[test]
    fn tool_metadata_deserialize_with_defaults() {
        let json = r#"{"name": "minimal"}"#;
        let meta: ToolMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.name, "minimal");
        assert_eq!(meta.description, "");
        assert_eq!(meta.schema, serde_json::Value::Null);
    }

    #[test]
    fn tool_metadata_input_schema_rename() {
        let json = r#"{"name": "t", "inputSchema": {"type": "string"}}"#;
        let meta: ToolMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.schema, serde_json::json!({"type": "string"}));

        // Serializes back as inputSchema
        let serialized = serde_json::to_value(&meta).unwrap();
        assert!(serialized.get("inputSchema").is_some());
        assert!(serialized.get("schema").is_none());
    }

    // --- MCPServerConfig serde ---

    #[test]
    fn mcp_server_config_serde_roundtrip() {
        let config = MCPServerConfig {
            name: "server1".to_string(),
            command: "node".to_string(),
            args: vec!["index.js".to_string()],
            env: HashMap::from([("KEY".to_string(), "VAL".to_string())]),
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MCPServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "server1");
        assert_eq!(deserialized.command, "node");
        assert_eq!(deserialized.args, vec!["index.js"]);
        assert_eq!(deserialized.env.get("KEY").unwrap(), "VAL");
    }

    #[test]
    fn mcp_server_config_defaults_for_args_and_env() {
        let json = r#"{"name": "s", "command": "cmd"}"#;
        let config: MCPServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "s");
        assert_eq!(config.command, "cmd");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
    }

    // --- ToolDiscovery ---

    #[test]
    fn new_server_count_is_zero() {
        let discovery = ToolDiscovery::new();
        assert_eq!(discovery.server_count(), 0);
    }

    #[test]
    fn new_all_tools_is_empty() {
        let discovery = ToolDiscovery::new();
        assert!(discovery.all_tools().is_empty());
    }

    #[test]
    fn new_find_tool_returns_none() {
        let discovery = ToolDiscovery::new();
        assert!(discovery.find_tool("x").is_none());
    }

    #[test]
    fn new_server_tools_returns_none() {
        let discovery = ToolDiscovery::new();
        assert!(discovery.server_tools("x").is_none());
    }

    #[test]
    fn default_same_as_new() {
        let d1 = ToolDiscovery::new();
        let d2 = ToolDiscovery::default();
        assert_eq!(d1.server_count(), d2.server_count());
        assert!(d1.all_tools().is_empty());
        assert!(d2.all_tools().is_empty());
    }
}
