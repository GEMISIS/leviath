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

/// Which transport reaches a configured MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MCPTransport {
    /// Spawn the server as a child process and speak over its pipes.
    Stdio,
    /// Reach the server over HTTP.
    Http,
}

/// Configuration for an MCP server connection.
///
/// ```toml
/// [[mcp_servers]]              # stdio
/// name = "local"
/// command = "npx"
/// args = ["-y", "@my/mcp-server"]
///
/// [[mcp_servers]]              # http
/// name = "remote"
/// url = "https://mcp.example.com/mcp"
/// headers = { Authorization = "Bearer ${MY_TOKEN}" }
/// ```
///
/// `transport` may be omitted whenever exactly one of `command`/`url` is set;
/// see [`MCPServerConfig::resolve`].
///
/// Field order matters: the scalar and array fields are declared before the
/// map fields because this struct is round-tripped through
/// `toml::to_string_pretty` when the CLI rewrites the config, and TOML rejects
/// bare values emitted after a table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MCPServerConfig {
    /// Server name (used as an identifier)
    pub name: String,
    /// Transport to use. Inferred from `command`/`url` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<MCPTransport>,
    /// Command to launch the server (stdio transport)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Endpoint to connect to (http transport)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Arguments to pass to the command
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set for the spawned command
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Extra HTTP headers to send. Values may reference environment variables
    /// as `${NAME}`, so a token need not be written into the config file.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// A validated [`MCPServerConfig`], narrowed to the fields its transport uses.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolvedTransport<'a> {
    /// Spawn `command` with `args` and `env`.
    Stdio {
        command: &'a str,
        args: &'a [String],
        env: &'a HashMap<String, String>,
    },
    /// Connect to `url` with `headers`.
    Http {
        url: &'a str,
        headers: &'a HashMap<String, String>,
    },
}

impl MCPServerConfig {
    /// A stdio server definition.
    pub fn stdio(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            command: Some(command.into()),
            args,
            ..Self::default()
        }
    }

    /// An HTTP server definition.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: Some(url.into()),
            ..Self::default()
        }
    }

    /// Decide which transport this entry describes, rejecting ambiguity.
    ///
    /// With `transport` omitted the choice is inferred from whichever of
    /// `command`/`url` is present — but only when exactly one is. Guessing
    /// when both or neither are set would silently connect somewhere the user
    /// did not intend, so those are errors.
    pub fn resolve(&self) -> anyhow::Result<ResolvedTransport<'_>> {
        let stdio = || match self.command.as_deref() {
            Some(command) => Ok(ResolvedTransport::Stdio {
                command,
                args: &self.args,
                env: &self.env,
            }),
            None => Err(anyhow::anyhow!(
                "transport = \"stdio\" requires a `command`"
            )),
        };
        let http = || match self.url.as_deref() {
            Some(url) => Ok(ResolvedTransport::Http {
                url,
                headers: &self.headers,
            }),
            None => Err(anyhow::anyhow!("transport = \"http\" requires a `url`")),
        };

        match (self.transport, self.command.is_some(), self.url.is_some()) {
            (Some(MCPTransport::Stdio), _, _) => stdio(),
            (Some(MCPTransport::Http), _, _) => http(),
            (None, true, false) => stdio(),
            (None, false, true) => http(),
            (None, true, true) => Err(anyhow::anyhow!(
                "has both `command` and `url`; set `transport` to \"stdio\" or \
                 \"http\" to say which one to use"
            )),
            (None, false, false) => Err(anyhow::anyhow!(
                "needs either a `command` (stdio) or a `url` (http)"
            )),
        }
    }

    /// Validate this entry, discarding the resolved value.
    ///
    /// For callers that only want to know whether the config is usable — a
    /// broken entry should be caught when the config loads, not at the first
    /// tool call.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.resolve()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("mcp_servers entry '{}' {}", self.name, e))
    }
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
        self.discover_from_config_with_auth(config, None).await
    }

    /// [`Self::discover_from_config`] with a resolved `Authorization` header for
    /// an HTTP server (see [`MCPClient::from_config_with_auth`]).
    pub async fn discover_from_config_with_auth(
        &mut self,
        config: &MCPServerConfig,
        auth_header: Option<(String, String)>,
    ) -> anyhow::Result<(Vec<ToolMetadata>, MCPClient)> {
        tracing::info!(server = %config.name, "Connecting MCP server from config");

        let mut client = MCPClient::from_config_with_auth(config, auth_header).await?;
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
            command: Some("node".to_string()),
            args: vec!["index.js".to_string()],
            env: HashMap::from([("KEY".to_string(), "VAL".to_string())]),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MCPServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "server1");
        assert_eq!(deserialized.command.as_deref(), Some("node"));
        assert_eq!(deserialized.args, vec!["index.js"]);
        assert_eq!(deserialized.env.get("KEY").unwrap(), "VAL");
    }

    #[test]
    fn mcp_server_config_defaults_for_args_and_env() {
        let json = r#"{"name": "s", "command": "cmd"}"#;
        let config: MCPServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "s");
        assert_eq!(config.command.as_deref(), Some("cmd"));
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
            command: Some("python3".to_string()),
            args: vec!["-c".to_string(), STUB_INIT_AND_LIST.to_string()],
            env: HashMap::new(),
            ..Default::default()
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
            command: Some("this-command-does-not-exist-anywhere".to_string()),
            args: vec![],
            env: HashMap::new(),
            ..Default::default()
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
            command: Some("python3".to_string()),
            args: vec!["-c".to_string(), STUB_INIT_ERRORS.to_string()],
            env: HashMap::new(),
            ..Default::default()
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
            command: Some("python3".to_string()),
            args: vec!["-c".to_string(), STUB_INIT_OK_LIST_ERRORS.to_string()],
            env: HashMap::new(),
            ..Default::default()
        };

        let mut discovery = ToolDiscovery::new();
        let result = discovery.discover_from_config(&config).await;
        assert!(result.is_err());
        assert_eq!(discovery.server_count(), 0);
    }

    // ─── transport resolution ─────────────────────────────────────────────
    //
    // Every row of the inference table. Getting this wrong means silently
    // connecting somewhere the user did not ask for, so ambiguity is an error
    // rather than a guess.

    fn cfg(
        transport: Option<MCPTransport>,
        command: Option<&str>,
        url: Option<&str>,
    ) -> MCPServerConfig {
        MCPServerConfig {
            name: "s".to_string(),
            transport,
            command: command.map(str::to_string),
            url: url.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn command_alone_infers_stdio() {
        let config = cfg(None, Some("npx"), None);
        assert_eq!(
            config.resolve().unwrap(),
            ResolvedTransport::Stdio {
                command: "npx",
                args: &[],
                env: &HashMap::new(),
            }
        );
    }

    #[test]
    fn url_alone_infers_http() {
        let config = cfg(None, None, Some("https://e.com/mcp"));
        assert_eq!(
            config.resolve().unwrap(),
            ResolvedTransport::Http {
                url: "https://e.com/mcp",
                headers: &HashMap::new(),
            }
        );
    }

    #[test]
    fn both_command_and_url_is_ambiguous() {
        let err = cfg(None, Some("npx"), Some("https://e.com/mcp"))
            .resolve()
            .expect_err("ambiguous config must not resolve");
        assert!(err.to_string().contains("set `transport`"), "got: {err}");
    }

    #[test]
    fn neither_command_nor_url_is_incomplete() {
        let err = cfg(None, None, None)
            .resolve()
            .expect_err("empty config must not resolve");
        assert!(err.to_string().contains("either a `command`"), "got: {err}");
    }

    #[test]
    fn explicit_stdio_wins_over_a_url() {
        // The explicit setting is exactly how a user disambiguates.
        let config = cfg(
            Some(MCPTransport::Stdio),
            Some("npx"),
            Some("https://e.com"),
        );
        assert!(matches!(
            config.resolve().unwrap(),
            ResolvedTransport::Stdio { command, .. } if command == "npx"
        ));
    }

    #[test]
    fn explicit_http_wins_over_a_command() {
        let config = cfg(Some(MCPTransport::Http), Some("npx"), Some("https://e.com"));
        assert!(matches!(
            config.resolve().unwrap(),
            ResolvedTransport::Http { url, .. } if url == "https://e.com"
        ));
    }

    #[test]
    fn explicit_stdio_without_a_command_is_an_error() {
        let err = cfg(Some(MCPTransport::Stdio), None, Some("https://e.com"))
            .resolve()
            .expect_err("stdio needs a command");
        assert!(
            err.to_string().contains("requires a `command`"),
            "got: {err}"
        );
    }

    #[test]
    fn explicit_http_without_a_url_is_an_error() {
        let err = cfg(Some(MCPTransport::Http), Some("npx"), None)
            .resolve()
            .expect_err("http needs a url");
        assert!(err.to_string().contains("requires a `url`"), "got: {err}");
    }

    #[test]
    fn validate_names_the_offending_server() {
        // The message has to identify *which* entry is broken; a config can
        // hold many.
        let err = cfg(None, None, None)
            .validate()
            .expect_err("empty config must not validate");
        assert!(err.to_string().contains("'s'"), "got: {err}");
    }

    #[test]
    fn validate_accepts_a_usable_entry() {
        assert!(cfg(None, Some("npx"), None).validate().is_ok());
    }

    // ─── constructors and serde shape ─────────────────────────────────────

    #[test]
    fn stdio_constructor_sets_only_stdio_fields() {
        let config = MCPServerConfig::stdio("local", "npx", vec!["-y".to_string()]);
        assert_eq!(config.command.as_deref(), Some("npx"));
        assert_eq!(config.args, vec!["-y"]);
        assert!(config.url.is_none());
        assert!(config.transport.is_none());
    }

    #[test]
    fn http_constructor_sets_only_http_fields() {
        let config = MCPServerConfig::http("remote", "https://e.com/mcp");
        assert_eq!(config.url.as_deref(), Some("https://e.com/mcp"));
        assert!(config.command.is_none());
    }

    #[test]
    fn transport_serializes_as_a_lowercase_string() {
        assert_eq!(
            serde_json::to_string(&MCPTransport::Stdio).unwrap(),
            "\"stdio\""
        );
        assert_eq!(
            serde_json::to_string(&MCPTransport::Http).unwrap(),
            "\"http\""
        );
        let parsed: MCPTransport = serde_json::from_str("\"http\"").unwrap();
        assert_eq!(parsed, MCPTransport::Http);
    }

    #[test]
    fn absent_optional_fields_are_omitted_when_serialized() {
        // These round-trip through `toml::to_string_pretty` when the CLI
        // rewrites the config; emitting explicit nulls would not parse back.
        let json = serde_json::to_string(&MCPServerConfig::stdio("s", "npx", vec![])).unwrap();
        assert!(!json.contains("url"), "got: {json}");
        assert!(!json.contains("transport"), "got: {json}");
    }

    #[test]
    fn an_http_entry_parses_from_toml() {
        let config: MCPServerConfig = toml::from_str(
            r#"
name = "remote"
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer tok" }
"#,
        )
        .unwrap();
        assert_eq!(config.url.as_deref(), Some("https://mcp.example.com/mcp"));
        assert_eq!(config.headers.get("Authorization").unwrap(), "Bearer tok");
        assert_eq!(
            config.resolve().unwrap(),
            ResolvedTransport::Http {
                url: "https://mcp.example.com/mcp",
                headers: &config.headers,
            }
        );
    }

    #[test]
    fn a_config_round_trips_through_toml() {
        // Field order matters: TOML rejects bare values emitted after a table,
        // so the scalar fields must be declared before `env`/`headers`.
        let mut config = MCPServerConfig::http("remote", "https://e.com/mcp");
        config
            .headers
            .insert("Authorization".to_string(), "Bearer tok".to_string());
        let text = toml::to_string_pretty(&config).expect("must serialize");
        let back: MCPServerConfig = toml::from_str(&text).expect("must parse back");
        assert_eq!(back.url, config.url);
        assert_eq!(back.headers, config.headers);
    }
}
