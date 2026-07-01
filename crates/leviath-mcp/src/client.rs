//! MCP client for connecting to tool providers via JSON-RPC 2.0 over stdin/stdout.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::discovery::ToolMetadata;

/// Server capabilities returned after initialization.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerCapabilities {
    /// Tool-related capabilities
    pub tools: Option<ToolsCapability>,
}

/// Tool capabilities advertised by the server.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsCapability {
    /// Whether the server supports list_changed notifications
    #[serde(rename = "listChanged")]
    pub list_changed: Option<bool>,
}

/// Result returned from a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Content items in the result
    pub content: Vec<ToolResultContent>,
    /// Whether the result represents an error
    #[serde(default)]
    pub is_error: bool,
}

/// Content item in a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolResultContent {
    /// Text content
    #[serde(rename = "text")]
    Text { text: String },
    /// Image content (base64 encoded)
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
    /// Resource content
    #[serde(rename = "resource")]
    Resource { uri: String, text: Option<String> },
}

/// Client for communicating with MCP tool providers via JSON-RPC 2.0 over stdin/stdout.
pub struct MCPClient {
    /// Child process handle
    child: Child,
    /// Stdin writer for sending JSON-RPC requests
    writer: BufWriter<ChildStdin>,
    /// Reader for receiving JSON-RPC responses
    reader: BufReader<ChildStdout>,
    /// Next request ID
    next_id: AtomicU64,
    /// Server capabilities after initialization
    capabilities: Option<ServerCapabilities>,
    /// Cached tool list from the server
    cached_tools: Vec<ToolMetadata>,
}

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: Option<Value>,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

impl MCPClient {
    /// Spawn an MCP server as a child process.
    pub async fn spawn(
        command: &str,
        args: &[&str],
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        tracing::info!(command = %command, "Spawning MCP server process");

        let mut cmd = Command::new(command);

        // Build clean environment - strip sensitive keys from parent env
        let sensitive_patterns = [
            "API_KEY",
            "API_SECRET",
            "SECRET_KEY",
            "ACCESS_TOKEN",
            "AUTH_TOKEN",
            "PRIVATE_KEY",
            "PASSWORD",
        ];
        cmd.env_clear();
        for (key, value) in std::env::vars() {
            let key_upper = key.to_uppercase();
            let is_sensitive = sensitive_patterns.iter().any(|p| key_upper.contains(p));
            if !is_sensitive {
                cmd.env(&key, &value);
            }
        }
        // Add explicitly configured env vars (intentional, from MCP config)
        cmd.envs(env);

        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn MCP server '{}': {}", command, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdin of MCP server"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout of MCP server"))?;

        Ok(Self {
            child,
            writer: BufWriter::new(stdin),
            reader: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
            capabilities: None,
            cached_tools: Vec::new(),
        })
    }

    /// Connect to the MCP server by sending initialize and initialized messages.
    pub async fn connect(&mut self) -> anyhow::Result<()> {
        tracing::info!("Initializing MCP connection");

        let init_params = serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "leviath",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        let result = self.send_request("initialize", init_params).await?;

        // Parse server capabilities
        let capabilities: ServerCapabilities = if let Some(caps) = result.get("capabilities") {
            serde_json::from_value(caps.clone()).unwrap_or_default()
        } else {
            ServerCapabilities::default()
        };
        self.capabilities = Some(capabilities);

        // Send initialized notification
        self.send_notification("notifications/initialized", serde_json::json!({}))
            .await?;

        tracing::info!("MCP connection established");
        Ok(())
    }

    /// List available tools from the server.
    pub async fn list_tools(&mut self) -> anyhow::Result<Vec<ToolMetadata>> {
        tracing::debug!("Listing MCP tools");

        let result = self
            .send_request("tools/list", serde_json::json!({}))
            .await?;

        let tools_value = result.get("tools").cloned().unwrap_or(Value::Array(vec![]));
        let tools: Vec<ToolMetadata> = serde_json::from_value(tools_value)
            .map_err(|e| anyhow::anyhow!("Failed to parse tools list: {}", e))?;

        self.cached_tools = tools.clone();
        tracing::debug!(count = tools.len(), "Discovered MCP tools");
        Ok(tools)
    }

    /// Call a tool on the server.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> anyhow::Result<ToolResult> {
        tracing::debug!(tool = %name, "Calling MCP tool");

        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
        });

        let result = self.send_request("tools/call", params).await?;

        let tool_result: ToolResult = serde_json::from_value(result)
            .map_err(|e| anyhow::anyhow!("Failed to parse tool result: {}", e))?;

        Ok(tool_result)
    }

    /// Shutdown the MCP server.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        tracing::info!("Shutting down MCP server");

        // Try to send a cancellation, but don't fail if the process is already gone
        let _ = self
            .send_notification("notifications/cancelled", serde_json::json!({}))
            .await;
        let _ = self.child.kill().await;

        Ok(())
    }

    /// Get the server capabilities (available after connect).
    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }

    /// Get the cached tool list.
    pub fn cached_tools(&self) -> &[ToolMetadata] {
        &self.cached_tools
    }

    /// Send a JSON-RPC request and wait for a response.
    async fn send_request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: Some(id),
            method: method.to_string(),
            params: Some(params),
        };

        let request_json = serde_json::to_string(&request)
            .map_err(|e| anyhow::anyhow!("Failed to serialize request: {}", e))?;

        tracing::trace!(method = %method, id = id, "Sending JSON-RPC request");

        // Write request line
        self.writer
            .write_all(request_json.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write to MCP server stdin: {}", e))?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write newline: {}", e))?;
        self.writer
            .flush()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to flush MCP server stdin: {}", e))?;

        // Read response line
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read from MCP server stdout: {}", e))?;

        if line.is_empty() {
            return Err(anyhow::anyhow!("MCP server closed connection unexpectedly"));
        }

        let response: JsonRpcResponse = serde_json::from_str(line.trim())
            .map_err(|e| anyhow::anyhow!("Failed to parse JSON-RPC response: {}", e))?;

        if let Some(error) = response.error {
            return Err(anyhow::anyhow!("MCP server error: {}", error.message));
        }

        response
            .result
            .ok_or_else(|| anyhow::anyhow!("MCP server returned no result"))
    }

    /// Filter environment variables, stripping sensitive keys.
    ///
    /// Used internally by `spawn()` to build a clean environment for child processes.
    pub fn filter_env(vars: &[(String, String)]) -> HashMap<String, String> {
        let sensitive_patterns = [
            "API_KEY",
            "API_SECRET",
            "SECRET_KEY",
            "ACCESS_TOKEN",
            "AUTH_TOKEN",
            "PRIVATE_KEY",
            "PASSWORD",
        ];
        vars.iter()
            .filter(|(key, _)| {
                let key_upper = key.to_uppercase();
                !sensitive_patterns.iter().any(|p| key_upper.contains(p))
            })
            .cloned()
            .collect()
    }

    /// Send a JSON-RPC notification (fire-and-forget, no response expected).
    async fn send_notification(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: None,
            method: method.to_string(),
            params: Some(params),
        };

        let request_json = serde_json::to_string(&request)
            .map_err(|e| anyhow::anyhow!("Failed to serialize notification: {}", e))?;

        tracing::trace!(method = %method, "Sending JSON-RPC notification");

        self.writer
            .write_all(request_json.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write notification: {}", e))?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write newline: {}", e))?;
        self.writer
            .flush()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to flush notification: {}", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A no-op `tracing::Subscriber` that reports every callsite/level as
    /// enabled. Without a registered subscriber, `tracing`'s macros
    /// short-circuit field-expression evaluation before the "is this level
    /// enabled" check even runs -- so a line like
    /// `tracing::info!(command = %command, "...")` shows a nonzero hit count
    /// (the macro call itself executes) while the `%command` field-expansion
    /// sub-region shows zero, even though the enclosing branch genuinely
    /// ran. Setting this as the default subscriber for a test's duration
    /// (via the `_guard` binding kept alive across `.await` points on a
    /// single-threaded runtime) makes those field expressions actually
    /// evaluate.
    struct AlwaysOnSubscriber;
    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn always_on_tracing_guard() -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(AlwaysOnSubscriber)
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        let _guard = always_on_tracing_guard();
        let span = tracing::info_span!("test-span", field = 1);
        span.record("field", 2);
        span.follows_from(&span);
        span.in_scope(|| {
            tracing::info!("inside span");
        });
    }

    #[test]
    fn test_filter_env_strips_api_keys() {
        let vars = vec![
            ("HOME".to_string(), "/home/user".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "sk-ant-secret".to_string()),
            ("OPENAI_API_KEY".to_string(), "sk-secret".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("MY_PASSWORD".to_string(), "hunter2".to_string()),
            ("DB_ACCESS_TOKEN".to_string(), "tok123".to_string()),
            ("SOME_AUTH_TOKEN".to_string(), "auth456".to_string()),
            ("SSH_PRIVATE_KEY".to_string(), "key789".to_string()),
            ("MY_API_SECRET".to_string(), "sec000".to_string()),
            ("SECRET_KEY_BASE".to_string(), "skb111".to_string()),
        ];

        let filtered = MCPClient::filter_env(&vars);

        assert_eq!(filtered.get("HOME"), Some(&"/home/user".to_string()));
        assert_eq!(filtered.get("PATH"), Some(&"/usr/bin".to_string()));
        assert!(!filtered.contains_key("ANTHROPIC_API_KEY"));
        assert!(!filtered.contains_key("OPENAI_API_KEY"));
        assert!(!filtered.contains_key("MY_PASSWORD"));
        assert!(!filtered.contains_key("DB_ACCESS_TOKEN"));
        assert!(!filtered.contains_key("SOME_AUTH_TOKEN"));
        assert!(!filtered.contains_key("SSH_PRIVATE_KEY"));
        assert!(!filtered.contains_key("MY_API_SECRET"));
        assert!(!filtered.contains_key("SECRET_KEY_BASE"));
    }

    #[test]
    fn test_filter_env_keeps_safe_vars() {
        let vars = vec![
            ("EDITOR".to_string(), "vim".to_string()),
            ("RUST_LOG".to_string(), "debug".to_string()),
            ("TERM".to_string(), "xterm".to_string()),
        ];

        let filtered = MCPClient::filter_env(&vars);
        assert_eq!(filtered.len(), 3);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_filter_env_empty_input() {
        let vars: Vec<(String, String)> = vec![];
        let filtered = MCPClient::filter_env(&vars);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_env_case_insensitive_matching() {
        let vars = vec![
            ("my_api_key".to_string(), "secret".to_string()),
            ("My_Password".to_string(), "pass".to_string()),
        ];
        let filtered = MCPClient::filter_env(&vars);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_env_partial_match() {
        // "API_KEY" pattern should match anything containing it
        let vars = vec![
            ("CUSTOM_API_KEY_VALUE".to_string(), "val".to_string()),
            ("SAFE_VAR".to_string(), "ok".to_string()),
        ];
        let filtered = MCPClient::filter_env(&vars);
        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains_key("SAFE_VAR"));
    }

    #[test]
    fn test_server_capabilities_default() {
        let caps = ServerCapabilities::default();
        assert!(caps.tools.is_none());
    }

    #[test]
    fn test_tools_capability_default() {
        let cap = ToolsCapability::default();
        assert!(cap.list_changed.is_none());
    }

    #[test]
    fn test_server_capabilities_serialization() {
        let caps = ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: Some(true),
            }),
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("listChanged"));
        assert!(json.contains("true"));

        let deserialized: ServerCapabilities = serde_json::from_str(&json).unwrap();
        assert!(deserialized.tools.unwrap().list_changed.unwrap());
    }

    #[test]
    fn test_tool_result_serialization() {
        let result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "Hello".to_string(),
            }],
            is_error: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("Hello"));
        assert!(json.contains("\"is_error\":false"));
    }

    #[test]
    fn test_tool_result_with_error() {
        let result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "Something went wrong".to_string(),
            }],
            is_error: true,
        };
        assert!(result.is_error);
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn test_tool_result_content_text() {
        let content = ToolResultContent::Text {
            text: "result text".to_string(),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("result text"));
        assert!(json.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_tool_result_content_image() {
        let content = ToolResultContent::Image {
            data: "base64data".to_string(),
            mime_type: "image/png".to_string(),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("base64data"));
        assert!(json.contains("image/png"));
    }

    #[test]
    fn test_tool_result_content_resource() {
        let content = ToolResultContent::Resource {
            uri: "file:///tmp/test.txt".to_string(),
            text: Some("file contents".to_string()),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("file:///tmp/test.txt"));
        assert!(json.contains("file contents"));
    }

    #[test]
    fn test_tool_result_content_resource_no_text() {
        let content = ToolResultContent::Resource {
            uri: "file:///tmp/test.txt".to_string(),
            text: None,
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("file:///tmp/test.txt"));
    }

    #[test]
    fn test_tool_result_deserialization() {
        let json = r#"{"content":[{"type":"text","text":"Hello"}],"is_error":false}"#;
        let result: ToolResult = serde_json::from_str(json).unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn test_tool_result_deserialization_missing_is_error() {
        let json = r#"{"content":[{"type":"text","text":"Hello"}]}"#;
        let result: ToolResult = serde_json::from_str(json).unwrap();
        assert!(!result.is_error); // defaults to false
    }

    #[test]
    fn test_tool_result_multiple_content() {
        let result = ToolResult {
            content: vec![
                ToolResultContent::Text {
                    text: "line 1".to_string(),
                },
                ToolResultContent::Text {
                    text: "line 2".to_string(),
                },
            ],
            is_error: false,
        };
        assert_eq!(result.content.len(), 2);
    }

    #[test]
    fn test_tool_result_clone() {
        let result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "test".to_string(),
            }],
            is_error: true,
        };
        let cloned = result.clone();
        assert!(cloned.is_error);
        assert_eq!(cloned.content.len(), 1);
    }

    #[test]
    fn test_server_capabilities_clone() {
        let caps = ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: Some(true),
            }),
        };
        let cloned = caps.clone();
        assert!(cloned.tools.unwrap().list_changed.unwrap());
    }

    // ─── ToolResult additional tests ──────────────────────────────────────

    #[test]
    fn test_tool_result_empty_content() {
        let result = ToolResult {
            content: vec![],
            is_error: false,
        };
        assert!(result.content.is_empty());
        assert!(!result.is_error);
    }

    #[test]
    fn test_tool_result_mixed_content_types() {
        let result = ToolResult {
            content: vec![
                ToolResultContent::Text {
                    text: "hello".to_string(),
                },
                ToolResultContent::Image {
                    data: "base64".to_string(),
                    mime_type: "image/jpeg".to_string(),
                },
                ToolResultContent::Resource {
                    uri: "file:///test".to_string(),
                    text: Some("content".to_string()),
                },
            ],
            is_error: false,
        };
        assert_eq!(result.content.len(), 3);
        let json = serde_json::to_string(&result).unwrap();
        let back: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content.len(), 3);
    }

    // ─── ServerCapabilities deserialization ────────────────────────────────

    #[test]
    fn test_server_capabilities_from_empty_json() {
        let caps: ServerCapabilities = serde_json::from_str("{}").unwrap();
        assert!(caps.tools.is_none());
    }

    #[test]
    fn test_server_capabilities_with_tools() {
        let json = r#"{"tools":{"listChanged":false}}"#;
        let caps: ServerCapabilities = serde_json::from_str(json).unwrap();
        let tools = caps.tools.unwrap();
        assert_eq!(tools.list_changed, Some(false));
    }

    #[test]
    fn test_server_capabilities_with_null_tools() {
        let json = r#"{"tools":null}"#;
        let caps: ServerCapabilities = serde_json::from_str(json).unwrap();
        assert!(caps.tools.is_none());
    }

    // ─── ToolsCapability serde ────────────────────────────────────────────

    #[test]
    fn test_tools_capability_with_list_changed_true() {
        let cap = ToolsCapability {
            list_changed: Some(true),
        };
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains("true"));
        let back: ToolsCapability = serde_json::from_str(&json).unwrap();
        assert_eq!(back.list_changed, Some(true));
    }

    #[test]
    fn test_tools_capability_no_list_changed() {
        let cap = ToolsCapability { list_changed: None };
        let json = serde_json::to_string(&cap).unwrap();
        let back: ToolsCapability = serde_json::from_str(&json).unwrap();
        assert!(back.list_changed.is_none());
    }

    // ─── ToolResultContent deserialization ─────────────────────────────────

    #[test]
    fn test_tool_result_content_text_deserialization() {
        let json = r#"{"type":"text","text":"hello world"}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        match content {
            ToolResultContent::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_tool_result_content_image_deserialization() {
        let json = r#"{"type":"image","data":"abc123","mime_type":"image/png"}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        match content {
            ToolResultContent::Image { data, mime_type } => {
                assert_eq!(data, "abc123");
                assert_eq!(mime_type, "image/png");
            }
            _ => panic!("Expected Image variant"),
        }
    }

    #[test]
    fn test_tool_result_content_resource_deserialization() {
        let json = r#"{"type":"resource","uri":"file:///tmp/x","text":"data"}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        match content {
            ToolResultContent::Resource { uri, text } => {
                assert_eq!(uri, "file:///tmp/x");
                assert_eq!(text, Some("data".to_string()));
            }
            _ => panic!("Expected Resource variant"),
        }
    }

    // ─── filter_env additional ────────────────────────────────────────────

    #[test]
    fn test_filter_env_preserves_path_and_home() {
        let vars = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("HOME".to_string(), "/Users/test".to_string()),
            ("SHELL".to_string(), "/bin/zsh".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ];
        let filtered = MCPClient::filter_env(&vars);
        assert_eq!(filtered.len(), 4);
    }

    #[test]
    fn test_filter_env_all_sensitive() {
        let vars = vec![
            ("API_KEY".to_string(), "key".to_string()),
            ("API_SECRET".to_string(), "secret".to_string()),
            ("SECRET_KEY".to_string(), "sk".to_string()),
            ("ACCESS_TOKEN".to_string(), "at".to_string()),
            ("AUTH_TOKEN".to_string(), "auth".to_string()),
            ("PRIVATE_KEY".to_string(), "pk".to_string()),
            ("PASSWORD".to_string(), "pass".to_string()),
        ];
        let filtered = MCPClient::filter_env(&vars);
        assert!(filtered.is_empty());
    }

    // ─── JsonRpcRequest serialization ───────────────────────────────────

    #[test]
    fn test_jsonrpc_request_serialization_with_id() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: Some(42),
            method: "tools/list".to_string(),
            params: Some(serde_json::json!({})),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"method\":\"tools/list\""));
    }

    #[test]
    fn test_jsonrpc_request_notification_no_id() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: None,
            method: "notifications/initialized".to_string(),
            params: Some(serde_json::json!({})),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"id\""));
    }

    #[test]
    fn test_jsonrpc_request_no_params() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: Some(1),
            method: "test".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("\"params\""));
    }

    // ─── JsonRpcResponse deserialization ─────────────────────────────────

    #[test]
    fn test_jsonrpc_response_with_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_jsonrpc_response_with_error() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().message, "Invalid request");
    }

    #[test]
    fn test_jsonrpc_response_null_id() {
        let json = r#"{"jsonrpc":"2.0","id":null,"result":"ok"}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_some());
    }

    // ─── ToolResult complex cases ───────────────────────────────────────

    #[test]
    fn test_tool_result_json_roundtrip() {
        let result = ToolResult {
            content: vec![
                ToolResultContent::Text {
                    text: "line1".to_string(),
                },
                ToolResultContent::Image {
                    data: "abc".to_string(),
                    mime_type: "image/png".to_string(),
                },
                ToolResultContent::Resource {
                    uri: "file:///x".to_string(),
                    text: Some("data".to_string()),
                },
            ],
            is_error: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content.len(), 3);
        assert!(!back.is_error);
    }

    // ─── filter_env: mixed sensitive and safe ───────────────────────────

    #[test]
    fn test_filter_env_mixed_with_duplicates() {
        let vars = vec![
            ("SAFE_VAR".to_string(), "safe".to_string()),
            ("MY_API_KEY".to_string(), "secret".to_string()),
            ("ANOTHER_SAFE".to_string(), "ok".to_string()),
            ("DB_PASSWORD".to_string(), "pass".to_string()),
            ("THIRD_SAFE".to_string(), "fine".to_string()),
        ];
        let filtered = MCPClient::filter_env(&vars);
        assert_eq!(filtered.len(), 3);
        assert!(filtered.contains_key("SAFE_VAR"));
        assert!(filtered.contains_key("ANOTHER_SAFE"));
        assert!(filtered.contains_key("THIRD_SAFE"));
    }

    // ─── ServerCapabilities with empty tools ────────────────────────────

    #[test]
    fn test_server_capabilities_empty_tools_object() {
        let json = r#"{"tools":{}}"#;
        let caps: ServerCapabilities = serde_json::from_str(json).unwrap();
        let tools = caps.tools.unwrap();
        assert!(tools.list_changed.is_none());
    }

    // ─── ToolResultContent: edge cases ──────────────────────────────────

    #[test]
    fn test_tool_result_content_text_empty() {
        let content = ToolResultContent::Text {
            text: "".to_string(),
        };
        let json = serde_json::to_string(&content).unwrap();
        let back: ToolResultContent = serde_json::from_str(&json).unwrap();
        match back {
            ToolResultContent::Text { text } => assert_eq!(text, ""),
            _ => panic!("Expected Text"),
        }
    }

    #[test]
    fn test_tool_result_content_image_empty_data() {
        let content = ToolResultContent::Image {
            data: "".to_string(),
            mime_type: "".to_string(),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("\"type\":\"image\""));
    }

    #[test]
    fn test_tool_result_content_resource_empty_uri() {
        let content = ToolResultContent::Resource {
            uri: "".to_string(),
            text: None,
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("\"type\":\"resource\""));
    }

    // ─── MCPClient spawn / connect / list_tools / call_tool / shutdown ────────
    // We use Python as a minimal in-process JSON-RPC 2.0 stub server that reads
    // one request per line and responds with a canned reply.

    /// Spawn a Python-backed stub MCP server.  The script reads JSON-RPC lines
    /// from stdin and writes canned responses to stdout.
    async fn spawn_stub_client(script: &str) -> MCPClient {
        MCPClient::spawn("python3", &["-c", script], &HashMap::new())
            .await
            .expect("Failed to spawn stub MCP server")
    }

    // Python script that answers initialize then tools/list then tools/call
    const STUB_INIT_LIST_CALL: &str = r#"
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
        pass  # notification -- no response
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "echo", "description": "echo tool", "inputSchema": {}}]})
    elif method == "tools/call":
        respond(id_, {"content": [{"type": "text", "text": "hello from tool"}], "isError": False})
    elif method == "notifications/cancelled":
        pass
    else:
        respond(id_, {"error": {"code": -32601, "message": "method not found"}})
"#;

    // Script that always returns a JSON-RPC error for every request
    const STUB_ERROR_SERVER: &str = r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    id_ = req.get("id")
    if id_ is not None:
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "error": {"code": -32600, "message": "server error"}})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
"#;

    // Script that closes stdout immediately (simulates process exit mid-request)
    const STUB_CLOSE_IMMEDIATELY: &str = r#"
import sys
sys.stdout.close()
"#;

    #[tokio::test]
    async fn test_mcp_client_spawn_succeeds() {
        let _guard = always_on_tracing_guard();
        let _client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        // If we got here, spawn worked
    }

    #[tokio::test]
    async fn test_mcp_client_connect_parses_capabilities() {
        let _guard = always_on_tracing_guard();
        let mut client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        client.connect().await.expect("connect should succeed");

        let caps = client.capabilities().expect("should have capabilities");
        assert!(caps.tools.is_some());
        assert_eq!(caps.tools.as_ref().unwrap().list_changed, Some(true));
    }

    #[tokio::test]
    async fn test_mcp_client_capabilities_before_connect_is_none() {
        let client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        // Before connect, capabilities should be None
        assert!(client.capabilities().is_none());
    }

    #[tokio::test]
    async fn test_mcp_client_cached_tools_before_list_is_empty() {
        let client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        assert!(client.cached_tools().is_empty());
    }

    #[tokio::test]
    async fn test_mcp_client_list_tools_returns_tools() {
        let _guard = always_on_tracing_guard();
        let mut client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        client.connect().await.unwrap();

        let tools = client
            .list_tools()
            .await
            .expect("list_tools should succeed");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        // cached_tools() should now return them too
        assert_eq!(client.cached_tools().len(), 1);
        assert_eq!(client.cached_tools()[0].name, "echo");
    }

    #[tokio::test]
    async fn test_mcp_client_call_tool_returns_result() {
        let _guard = always_on_tracing_guard();
        let mut client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        client.connect().await.unwrap();
        // Consume the tools/list response first
        client.list_tools().await.unwrap();

        let result = client
            .call_tool("echo", serde_json::json!({"msg": "hi"}))
            .await
            .expect("call_tool should succeed");

        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            ToolResultContent::Text { text } => assert_eq!(text, "hello from tool"),
            other => panic!("Expected Text content, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_mcp_client_shutdown_succeeds() {
        let _guard = always_on_tracing_guard();
        let mut client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        client.connect().await.unwrap();
        // Shutdown should not fail even if the process is still running
        client.shutdown().await.expect("shutdown should succeed");
    }

    #[tokio::test]
    async fn test_mcp_client_shutdown_with_dead_process() {
        let mut client = spawn_stub_client(STUB_CLOSE_IMMEDIATELY).await;
        // Process closed stdout immediately; shutdown should still succeed
        client
            .shutdown()
            .await
            .expect("shutdown should be graceful");
    }

    // ─── send_request/send_notification: real write/flush I/O errors ───────
    //
    // `writer` is a `BufWriter`, so a small `write_all` just appends to its
    // in-memory buffer without touching the OS -- the *real* write only
    // happens on `flush()`, which is where a broken pipe actually surfaces.
    // Killing and reaping the child first guarantees the pipe's read end is
    // gone, so these are deterministic, not racy. A payload large enough to
    // exceed `BufWriter`'s default 8KB capacity forces `write_all` itself to
    // bypass buffering and write directly, surfacing the error there instead.

    async fn spawn_and_kill_stub_client() -> MCPClient {
        let mut client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        client.child.kill().await.expect("kill should succeed");
        let _ = client.child.wait().await; // reap so the pipe's read end is fully gone
                                           // Empirically, a *tiny* buffered write's subsequent flush() doesn't
                                           // reliably surface EPIPE immediately after reaping on this platform
                                           // (unlike a >8KB write, which bypasses BufWriter's buffer and hits
                                           // the OS directly in write_all() itself -- see the write_all tests
                                           // below, which are deterministic). A short delay lets the kernel
                                           // fully settle the closed pipe state before flush() is attempted.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        client
    }

    #[tokio::test]
    async fn test_send_notification_flush_after_child_killed_returns_error() {
        let mut client = spawn_and_kill_stub_client().await;
        let result = client
            .send_notification("notifications/test", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_notification_write_all_after_child_killed_returns_error() {
        let mut client = spawn_and_kill_stub_client().await;
        // >8KB payload exceeds BufWriter's default capacity, forcing write_all
        // to write directly rather than buffer.
        let huge = "x".repeat(20_000);
        let result = client
            .send_notification("notifications/test", serde_json::json!({"data": huge}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_request_flush_after_child_killed_returns_error() {
        let mut client = spawn_and_kill_stub_client().await;
        let result = client.connect().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_request_write_all_after_child_killed_returns_error() {
        let mut client = spawn_and_kill_stub_client().await;
        let huge = "x".repeat(20_000);
        let result = client
            .call_tool("echo", serde_json::json!({"data": huge}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mcp_client_server_error_propagates() {
        let mut client = spawn_stub_client(STUB_ERROR_SERVER).await;
        // initialize sends a request and the error server will return an error response
        let err = client.connect().await;
        assert!(err.is_err(), "Expected error from error server");
        assert!(
            err.unwrap_err().to_string().contains("server error"),
            "Expected server error message"
        );
    }

    #[tokio::test]
    async fn test_mcp_client_empty_response_is_error() {
        // A script that writes nothing (closes stdout) causes "closed connection" error
        let script = r#"
import sys
# Read one line then close -- simulates EOF during request
for line in sys.stdin:
    break
sys.stdout.close()
"#;
        let mut client = spawn_stub_client(script).await;
        let err = client.connect().await;
        assert!(err.is_err(), "Expected error when server closes connection");
    }

    #[tokio::test]
    async fn test_mcp_client_malformed_json_response_is_error() {
        let script = r#"
import sys
for line in sys.stdin:
    sys.stdout.write("this is not json\n")
    sys.stdout.flush()
    break
"#;
        let mut client = spawn_stub_client(script).await;
        let err = client.connect().await;
        assert!(err.is_err(), "Expected error for malformed JSON");
        assert!(
            err.unwrap_err().to_string().contains("parse"),
            "Expected parse error"
        );
    }

    #[tokio::test]
    async fn test_mcp_client_spawn_invalid_command_fails() {
        let result = MCPClient::spawn(
            "/nonexistent/command/that/does/not/exist",
            &[],
            &HashMap::new(),
        )
        .await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("Failed to spawn"), "err: {}", err_msg);
    }

    #[tokio::test]
    async fn test_mcp_client_connect_with_no_capabilities_field() {
        // Server returns initialize result without a "capabilities" key
        let script = r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": {"protocolVersion": "2024-11-05"}})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "notifications/cancelled":
        pass
"#;
        let mut client = spawn_stub_client(script).await;
        // Should succeed with default (empty) capabilities
        client.connect().await.expect("connect should succeed");
        let caps = client.capabilities().unwrap();
        assert!(caps.tools.is_none());
    }

    #[tokio::test]
    async fn test_mcp_client_list_tools_missing_tools_key_returns_empty() {
        // Server returns initialize ok and tools/list without "tools" key -> empty list
        let script = r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": {"capabilities": {}}})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        # Return result without "tools" key
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": {}})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
    elif method == "notifications/cancelled":
        pass
"#;
        let mut client = spawn_stub_client(script).await;
        client.connect().await.unwrap();
        let tools = client
            .list_tools()
            .await
            .expect("list_tools should succeed");
        assert!(
            tools.is_empty(),
            "Expected empty list when tools key absent"
        );
    }

    #[tokio::test]
    async fn test_mcp_client_list_tools_malformed_response_is_error() {
        // Server returns valid initialize but tools/list with bad tools array
        let script = r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": {"capabilities": {}}})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        # Return tools as a string instead of an array -- triggers parse error
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": {"tools": "not_an_array"}})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
    elif method == "notifications/cancelled":
        pass
"#;
        let mut client = spawn_stub_client(script).await;
        client.connect().await.unwrap();
        let err = client.list_tools().await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("parse"));
    }

    #[tokio::test]
    async fn test_mcp_client_call_tool_malformed_result_is_error() {
        let script = r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": {"capabilities": {}}})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/call":
        # Return a result that can't be parsed as ToolResult
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": "bad_tool_result"})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
    elif method == "notifications/cancelled":
        pass
"#;
        let mut client = spawn_stub_client(script).await;
        client.connect().await.unwrap();
        let err = client.call_tool("broken", serde_json::json!({})).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("parse"));
    }

    #[tokio::test]
    async fn test_mcp_client_response_with_no_result_is_error() {
        // Server returns a valid JSON-RPC response with neither result nor error
        let script = r#"
import sys, json

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    id_ = req.get("id")
    if id_ is not None:
        # No "result", no "error"
        msg = json.dumps({"jsonrpc": "2.0", "id": id_})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
"#;
        let mut client = spawn_stub_client(script).await;
        let err = client.connect().await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("no result"));
    }
}
