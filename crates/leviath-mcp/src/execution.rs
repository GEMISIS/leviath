//! Tool execution via MCP.

use serde_json::Value;
use std::collections::HashMap;

use crate::client::{MCPClient, ToolResult, ToolResultContent};

/// Result of a tool execution, with convenience fields.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Result data as JSON (contains the content array)
    pub data: Value,
    /// Concatenated text content for convenience
    pub text: String,
}

/// Tool execution service that routes tool calls to the correct MCP server.
pub struct ToolExecutor {
    /// Active MCP clients, keyed by server name
    clients: HashMap<String, MCPClient>,
}

impl ToolExecutor {
    /// Create a new tool executor.
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Register an MCP client for a server.
    pub fn add_client(&mut self, server_name: String, client: MCPClient) {
        self.clients.insert(server_name, client);
    }

    /// Execute a tool by looking up which server owns it.
    ///
    /// Searches all connected servers' cached tool lists to find the right one.
    pub async fn execute(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<ExecutionResult> {
        tracing::info!(tool = %tool_name, "Executing tool");

        // Find which server owns this tool
        let server_name = self
            .clients
            .iter()
            .find(|(_, client)| {
                client.cached_tools().iter().any(|t| t.name == tool_name)
            })
            .map(|(name, _)| name.clone());

        let server_name = server_name.ok_or_else(|| {
            anyhow::anyhow!(
                "No MCP server found with tool '{}'. Available servers: {:?}",
                tool_name,
                self.clients.keys().collect::<Vec<_>>()
            )
        })?;

        self.execute_on(&server_name, tool_name, arguments).await
    }

    /// Execute a tool on a specific server.
    pub async fn execute_on(
        &mut self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<ExecutionResult> {
        tracing::info!(server = %server_name, tool = %tool_name, "Executing tool on server");

        let client = self.clients.get_mut(server_name).ok_or_else(|| {
            anyhow::anyhow!("MCP server '{}' not found", server_name)
        })?;

        let tool_result = client.call_tool(tool_name, arguments).await?;
        Ok(Self::map_result(tool_result))
    }

    /// Execute a tool only if it is in the allowed list.
    ///
    /// Returns an error if the tool is not in the allowed_tools list.
    pub async fn execute_filtered(
        &mut self,
        tool_name: &str,
        arguments: Value,
        allowed_tools: &[String],
    ) -> anyhow::Result<ExecutionResult> {
        if !allowed_tools.iter().any(|t| t == tool_name) {
            return Ok(ExecutionResult {
                success: false,
                data: Value::Null,
                text: format!(
                    "Tool '{}' is not allowed in the current stage. Allowed tools: {:?}",
                    tool_name, allowed_tools
                ),
            });
        }
        self.execute(tool_name, arguments).await
    }

    /// Shutdown all connected MCP clients.
    pub async fn shutdown_all(&mut self) -> anyhow::Result<()> {
        tracing::info!("Shutting down all MCP clients");

        let mut errors = Vec::new();
        for (name, client) in self.clients.iter_mut() {
            if let Err(e) = client.shutdown().await {
                tracing::warn!(server = %name, error = %e, "Failed to shutdown MCP client");
                errors.push(format!("{}: {}", name, e));
            }
        }
        self.clients.clear();

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Some MCP clients failed to shutdown: {}",
                errors.join(", ")
            ))
        }
    }

    /// Get the number of connected servers.
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    /// Map a ToolResult into an ExecutionResult.
    fn map_result(tool_result: ToolResult) -> ExecutionResult {
        let text = tool_result
            .content
            .iter()
            .filter_map(|c| match c {
                ToolResultContent::Text { text } => Some(text.as_str()),
                ToolResultContent::Resource { text, .. } => text.as_deref(),
                ToolResultContent::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let data = serde_json::to_value(&tool_result.content).unwrap_or(Value::Null);

        ExecutionResult {
            success: !tool_result.is_error,
            data,
            text,
        }
    }
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_filtered_rejects_disallowed_tool() {
        let mut executor = ToolExecutor::new();
        let allowed = vec!["read_file".to_string(), "write_file".to_string()];

        let result = executor
            .execute_filtered("delete_file", serde_json::json!({}), &allowed)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.text.contains("not allowed"));
    }

    #[test]
    fn test_tool_executor_creation() {
        let executor = ToolExecutor::new();
        assert_eq!(executor.server_count(), 0);
    }
}
