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
            .find(|(_, client)| client.cached_tools().iter().any(|t| t.name == tool_name))
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

        let client = self
            .clients
            .get_mut(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found", server_name))?;

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

    // ─── ToolExecutor::default ──────────────────────────────────────────

    #[test]
    fn test_tool_executor_default() {
        let executor = ToolExecutor::default();
        assert_eq!(executor.server_count(), 0);
    }

    // ─── map_result: text content ───────────────────────────────────────

    #[test]
    fn test_map_result_text_content() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "Hello world".to_string(),
            }],
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert!(result.success);
        assert_eq!(result.text, "Hello world");
    }

    #[test]
    fn test_map_result_error() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "Something failed".to_string(),
            }],
            is_error: true,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert!(!result.success);
        assert_eq!(result.text, "Something failed");
    }

    #[test]
    fn test_map_result_empty_content() {
        let tool_result = ToolResult {
            content: vec![],
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert!(result.success);
        assert_eq!(result.text, "");
    }

    #[test]
    fn test_map_result_multiple_text() {
        let tool_result = ToolResult {
            content: vec![
                ToolResultContent::Text {
                    text: "line1".to_string(),
                },
                ToolResultContent::Text {
                    text: "line2".to_string(),
                },
            ],
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert_eq!(result.text, "line1\nline2");
    }

    #[test]
    fn test_map_result_image_excluded_from_text() {
        let tool_result = ToolResult {
            content: vec![
                ToolResultContent::Text {
                    text: "before".to_string(),
                },
                ToolResultContent::Image {
                    data: "base64data".to_string(),
                    mime_type: "image/png".to_string(),
                },
                ToolResultContent::Text {
                    text: "after".to_string(),
                },
            ],
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert_eq!(result.text, "before\nafter");
    }

    #[test]
    fn test_map_result_resource_with_text() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Resource {
                uri: "file:///test".to_string(),
                text: Some("resource content".to_string()),
            }],
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert_eq!(result.text, "resource content");
    }

    #[test]
    fn test_map_result_resource_without_text() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Resource {
                uri: "file:///test".to_string(),
                text: None,
            }],
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert_eq!(result.text, "");
    }

    #[test]
    fn test_map_result_data_is_json() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "hi".to_string(),
            }],
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert!(result.data.is_array());
    }

    // ─── execute_filtered: allowed tool ────────────────────────────────

    #[tokio::test]
    async fn test_execute_filtered_allowed_tool_but_no_server() {
        let mut executor = ToolExecutor::new();
        let allowed = vec!["read_file".to_string()];

        // Tool is allowed but no server has it
        let result = executor
            .execute_filtered("read_file", serde_json::json!({}), &allowed)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No MCP server"));
    }

    // ─── execute: no server ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_no_server_errors() {
        let mut executor = ToolExecutor::new();
        let result = executor
            .execute("nonexistent_tool", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    // ─── execute_on: unknown server ─────────────────────────────────────

    #[tokio::test]
    async fn test_execute_on_unknown_server() {
        let mut executor = ToolExecutor::new();
        let result = executor
            .execute_on("unknown_server", "tool", serde_json::json!({}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // ─── shutdown_all: empty executor ───────────────────────────────────

    #[tokio::test]
    async fn test_shutdown_all_empty() {
        let mut executor = ToolExecutor::new();
        let result = executor.shutdown_all().await;
        assert!(result.is_ok());
        assert_eq!(executor.server_count(), 0);
    }

    // ─── ExecutionResult ────────────────────────────────────────────────

    #[test]
    fn test_execution_result_clone() {
        let result = ExecutionResult {
            success: true,
            data: serde_json::json!("test"),
            text: "hello".to_string(),
        };
        let cloned = result.clone();
        assert!(cloned.success);
        assert_eq!(cloned.text, "hello");
    }

    #[test]
    fn test_execution_result_debug() {
        let result = ExecutionResult {
            success: false,
            data: Value::Null,
            text: "error".to_string(),
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("success"));
        assert!(debug.contains("false"));
    }
}
