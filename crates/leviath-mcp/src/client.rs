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
}
