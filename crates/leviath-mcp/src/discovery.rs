//! Tool discovery via MCP.

use serde::{Deserialize, Serialize};

/// Discovered tool metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// Tool name
    pub name: String,
    /// Tool description
    pub description: String,
    /// Parameter schema
    pub schema: serde_json::Value,
}

/// Tool discovery service.
pub struct ToolDiscovery {
    /// Available tools
    tools: Vec<ToolMetadata>,
}

impl ToolDiscovery {
    /// Create a new tool discovery service.
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Discover available tools from MCP servers.
    pub async fn discover(&mut self) -> anyhow::Result<Vec<ToolMetadata>> {
        // TODO: Implement discovery
        tracing::info!("Discovering MCP tools");
        Ok(self.tools.clone())
    }
}

impl Default for ToolDiscovery {
    fn default() -> Self {
        Self::new()
    }
}
