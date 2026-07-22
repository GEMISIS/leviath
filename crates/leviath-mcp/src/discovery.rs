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
    /// Parameter schema (JSON Schema).
    ///
    /// Defaults to an empty object schema rather than `Value::Null`: the spec
    /// requires `inputSchema` to be a valid JSON Schema object and never null,
    /// and providers reject a tool whose parameter schema is null.
    #[serde(rename = "inputSchema", default = "empty_object_schema")]
    pub schema: serde_json::Value,
}

/// `{"type": "object"}` — the fallback parameter schema for a tool whose
/// server omitted `inputSchema`.
fn empty_object_schema() -> serde_json::Value {
    serde_json::json!({"type": "object"})
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

        // Bind `tool_count` in a plain `let` (rather than as an inline
        // `tool_count = tools.len()` field) so the length is computed
        // unconditionally: as an inline field it's only evaluated when a
        // tracing subscriber is active, which made this region's coverage
        // depend on test ordering / the ambient global subscriber and so
        // differ across OSes.
        let tool_count = tools.len();
        tracing::info!(server = %server_name, tool_count, "Discovered tools");

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
    use crate::test_support::always_on_tracing_guard;

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
        // Never `Value::Null`: the spec requires a JSON Schema *object*, and
        // providers reject a tool whose parameter schema is null.
        assert_eq!(meta.schema, serde_json::json!({"type": "object"}));
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

    // --- discover_from_client / discover_from_config ---
    //
    // Same Python-backed JSON-RPC stub approach used in client.rs's tests
    // (a minimal in-process server reading one request per line from stdin).

    const STUB_INIT_AND_LIST: &str = r#"
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
    else:
        respond(id_, {"error": {"code": -32601, "message": "method not found"}})
"#;

    #[tokio::test]
    async fn discover_from_client_returns_tools_and_indexes_by_server_name() {
        let _guard = always_on_tracing_guard();
        let mut client = MCPClient::spawn("python3", &["-c", STUB_INIT_AND_LIST], &HashMap::new())
            .await
            .expect("failed to spawn stub server");
        client.connect().await.expect("connect should succeed");

        let mut discovery = ToolDiscovery::new();
        let tools = discovery
            .discover_from_client("server1", &mut client)
            .await
            .expect("discovery should succeed");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(discovery.server_count(), 1);
        assert_eq!(discovery.all_tools().len(), 1);

        let (server_name, tool) = discovery.find_tool("echo").expect("tool should be found");
        assert_eq!(server_name, "server1");
        assert_eq!(tool.name, "echo");

        let server_tools = discovery
            .server_tools("server1")
            .expect("server tools should be present");
        assert_eq!(server_tools.len(), 1);
    }

    #[tokio::test]
    async fn discover_from_client_missing_tool_returns_none() {
        let mut client = MCPClient::spawn("python3", &["-c", STUB_INIT_AND_LIST], &HashMap::new())
            .await
            .expect("failed to spawn stub server");
        client.connect().await.expect("connect should succeed");

        let mut discovery = ToolDiscovery::new();
        discovery
            .discover_from_client("server1", &mut client)
            .await
            .expect("discovery should succeed");

        assert!(discovery.find_tool("nonexistent").is_none());
        assert!(discovery.server_tools("nonexistent").is_none());
    }

    #[tokio::test]
    async fn discover_from_config_spawns_connects_and_discovers() {
        let config = MCPServerConfig {
            name: "configured-server".to_string(),
            command: "python3".to_string(),
            args: vec!["-c".to_string(), STUB_INIT_AND_LIST.to_string()],
            env: HashMap::new(),
        };

        let mut discovery = ToolDiscovery::new();
        let (tools, mut client) = discovery
            .discover_from_config(&config)
            .await
            .expect("discover_from_config should succeed");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(discovery.server_count(), 1);
        assert!(discovery.find_tool("echo").is_some());

        // The returned client is live and connected (capabilities were parsed).
        assert!(client.capabilities().is_some());
        let _ = client.shutdown().await;
    }

    #[tokio::test]
    async fn discover_from_config_invalid_command_propagates_error() {
        let config = MCPServerConfig {
            name: "bad-server".to_string(),
            command: "this-command-does-not-exist-anywhere".to_string(),
            args: vec![],
            env: HashMap::new(),
        };

        let mut discovery = ToolDiscovery::new();
        let result = discovery.discover_from_config(&config).await;
        assert!(result.is_err());
        assert_eq!(discovery.server_count(), 0);
    }

    /// Responds with a JSON-RPC error to "initialize", so `connect()` fails.
    const STUB_INIT_ERRORS: &str = r#"
import sys, json

def respond(id, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": id}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        respond(id_, error={"code": -32000, "message": "initialize failed"})
    else:
        respond(id_, error={"code": -32601, "message": "method not found"})
"#;

    /// Initializes successfully but responds with a JSON-RPC error to
    /// "tools/list", so `list_tools()` (and therefore `discover_from_client`)
    /// fails.
    const STUB_INIT_OK_LIST_ERRORS: &str = r#"
import sys, json

def respond(id, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": id}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {}, "protocolVersion": "2024-11-05"})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(id_, error={"code": -32000, "message": "listing failed"})
    else:
        respond(id_, error={"code": -32601, "message": "method not found"})
"#;

    #[tokio::test]
    async fn discover_from_client_list_tools_error_propagates() {
        let mut client = MCPClient::spawn(
            "python3",
            &["-c", STUB_INIT_OK_LIST_ERRORS],
            &HashMap::new(),
        )
        .await
        .expect("failed to spawn stub server");
        client.connect().await.expect("connect should succeed");

        let mut discovery = ToolDiscovery::new();
        let result = discovery.discover_from_client("server1", &mut client).await;
        assert!(result.is_err());
        assert_eq!(discovery.server_count(), 0);
    }

    #[tokio::test]
    async fn discover_from_config_connect_error_propagates() {
        let config = MCPServerConfig {
            name: "server1".to_string(),
            command: "python3".to_string(),
            args: vec!["-c".to_string(), STUB_INIT_ERRORS.to_string()],
            env: HashMap::new(),
        };

        let mut discovery = ToolDiscovery::new();
        let result = discovery.discover_from_config(&config).await;
        assert!(result.is_err());
        assert_eq!(discovery.server_count(), 0);
    }

    #[tokio::test]
    async fn discover_from_config_discover_error_propagates() {
        let config = MCPServerConfig {
            name: "server1".to_string(),
            command: "python3".to_string(),
            args: vec!["-c".to_string(), STUB_INIT_OK_LIST_ERRORS.to_string()],
            env: HashMap::new(),
        };

        let mut discovery = ToolDiscovery::new();
        let result = discovery.discover_from_config(&config).await;
        assert!(result.is_err());
        assert_eq!(discovery.server_count(), 0);
    }
}
