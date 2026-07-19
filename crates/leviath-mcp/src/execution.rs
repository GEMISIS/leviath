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
    ///
    /// `MCPClient::shutdown` always returns `Ok` by design (it swallows
    /// subprocess errors so a dead server cannot block cleanup), so errors
    /// are discarded here too.
    pub async fn shutdown_all(&mut self) -> anyhow::Result<()> {
        tracing::info!("Shutting down all MCP clients");
        for client in self.clients.values_mut() {
            let _ = client.shutdown().await;
        }
        self.clients.clear();
        Ok(())
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
    use crate::test_support::always_on_tracing_guard;

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
        let _guard = always_on_tracing_guard();
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
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let result = executor
            .execute("nonexistent_tool", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    // ─── execute_on: unknown server ─────────────────────────────────────

    #[tokio::test]
    async fn test_execute_on_unknown_server() {
        let _guard = always_on_tracing_guard();
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
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let result = executor.shutdown_all().await;
        assert!(result.is_ok());
        assert_eq!(executor.server_count(), 0);
    }

    // ─── add_client / execute / execute_on with a live client ───────────
    //
    // Same Python-backed JSON-RPC stub approach used in client.rs/discovery.rs
    // tests. Note: MCPClient::shutdown() always returns Ok(()) by design (it
    // swallows failures so a dead server can't block cleanup) — so
    // shutdown_all()'s error-collection branch is intentionally left
    // uncovered here; there's no way to make client.shutdown() fail without
    // changing that documented "always succeeds" behavior.

    const STUB_INIT_LIST_AND_CALL: &str = r#"
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
        respond(id_, {"content": [{"type": "text", "text": "hello from tool"}], "isError": False})
    elif method == "notifications/cancelled":
        pass
    else:
        respond(id_, {"error": {"code": -32601, "message": "method not found"}})
"#;

    async fn spawn_ready_client() -> MCPClient {
        let mut client =
            MCPClient::spawn("python3", &["-c", STUB_INIT_LIST_AND_CALL], &HashMap::new())
                .await
                .expect("failed to spawn stub server");
        client.connect().await.expect("connect should succeed");
        client
            .list_tools()
            .await
            .expect("list_tools should succeed");
        client
    }

    #[tokio::test]
    async fn add_client_and_server_count_reflects_it() {
        let mut executor = ToolExecutor::new();
        let client = spawn_ready_client().await;
        executor.add_client("server1".to_string(), client);
        assert_eq!(executor.server_count(), 1);
    }

    #[tokio::test]
    async fn execute_finds_owning_server_and_calls_tool() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_ready_client().await;
        executor.add_client("server1".to_string(), client);

        let result = executor
            .execute("echo", serde_json::json!({"text": "hi"}))
            .await
            .expect("execute should succeed");
        assert!(result.success);
        assert_eq!(result.text, "hello from tool");
    }

    #[tokio::test]
    async fn execute_on_specific_server_calls_tool() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_ready_client().await;
        executor.add_client("server1".to_string(), client);

        let result = executor
            .execute_on("server1", "echo", serde_json::json!({}))
            .await
            .expect("execute_on should succeed");
        assert!(result.success);
        assert_eq!(result.text, "hello from tool");
    }

    #[tokio::test]
    async fn execute_filtered_allowed_tool_with_server_succeeds() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_ready_client().await;
        executor.add_client("server1".to_string(), client);

        let allowed = vec!["echo".to_string()];
        let result = executor
            .execute_filtered("echo", serde_json::json!({}), &allowed)
            .await
            .expect("execute_filtered should succeed");
        assert!(result.success);
    }

    #[tokio::test]
    async fn shutdown_all_with_live_client_succeeds_and_clears() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_ready_client().await;
        executor.add_client("server1".to_string(), client);

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

    // ─── execute_on: call_tool error propagation ────────────────────────
    //
    // Server returns a JSON-RPC error for tools/call, which causes
    // execute_on's `client.call_tool(...).await?` to propagate the error.

    const STUB_CALL_ERROR: &str = r#"
import sys, json

def respond(id, result):
    msg = json.dumps({"jsonrpc": "2.0", "id": id, "result": result})
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()

def error(id, message):
    msg = json.dumps({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}})
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
        respond(id_, {"capabilities": {"tools": {}}, "protocolVersion": "2024-11-05"})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "echo", "description": "echo", "inputSchema": {}}]})
    elif method == "tools/call":
        error(id_, "tool execution failed")
    elif method == "notifications/cancelled":
        pass
"#;

    #[tokio::test]
    async fn execute_on_propagates_call_tool_error() {
        let _guard = always_on_tracing_guard();
        let mut client = MCPClient::spawn("python3", &["-c", STUB_CALL_ERROR], &HashMap::new())
            .await
            .expect("spawn");
        client.connect().await.expect("connect");
        client.list_tools().await.expect("list_tools");

        let mut executor = ToolExecutor::new();
        executor.add_client("server1".to_string(), client);

        let result = executor
            .execute_on("server1", "echo", serde_json::json!({}))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("tool execution failed")
        );
    }
}
