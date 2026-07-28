//! MCP client: the protocol layer, over any transport.
//!
//! Framing and connection lifecycle live in [`crate::transport`]; this module
//! owns the MCP conversation itself — the handshake, tool discovery, tool
//! calls, and the wire types they exchange.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::discovery::{MCPServerConfig, ResolvedTransport, ToolMetadata};
use crate::transport::http::HttpTransport;
use crate::transport::stdio::StdioTransport;
use crate::transport::{
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, JsonRpcRequest, Transport,
};

/// Upper bound on `tools/list` pages followed, so a server that always returns
/// a `nextCursor` can't spin the discovery loop forever.
const MAX_TOOL_PAGES: usize = 100;

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
    /// Content items in the result.
    ///
    /// Defaulted: a server returning only `structuredContent` is still parsed
    /// rather than rejected.
    #[serde(default)]
    pub content: Vec<ToolResultContent>,
    /// Structured output conforming to the tool's `outputSchema`, when the
    /// server provides one (MCP 2025-06-18 and later).
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    /// Whether the result represents a *tool execution* error (as opposed to a
    /// JSON-RPC protocol error, which never reaches this type).
    ///
    /// The wire name is `isError`. Without the rename this never matched, so
    /// every failing tool call was silently reported to the model as a success.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// The resource payload carried by an embedded `resource` content block.
///
/// The spec nests this under a `resource` key rather than inlining `uri`/`text`
/// on the content block itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmbeddedResource {
    /// URI identifying the resource.
    pub uri: String,
    /// Text contents, for text resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 contents, for binary resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// MIME type of the resource.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Content item in a tool result.
///
/// Every wire field name here is the spec's camelCase spelling (`mimeType`),
/// not a Rust-style snake_case one — mismatches made whole tool results fail to
/// deserialize.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ToolResultContent {
    /// Text content
    #[serde(rename = "text")]
    Text { text: String },
    /// Image content (base64 encoded)
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Audio content (base64 encoded)
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// A link to a resource the client may fetch or subscribe to.
    #[serde(rename = "resource_link")]
    ResourceLink {
        uri: String,
        #[serde(default)]
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    /// An embedded resource, whose payload is nested under `resource`.
    #[serde(rename = "resource")]
    Resource { resource: EmbeddedResource },
    /// Any content block this client does not model.
    ///
    /// Internally-tagged enums can't carry the original payload in a catch-all
    /// arm, so the block's data is dropped — but that is the point: without
    /// this variant a single unrecognized block (a future content type, or a
    /// vendor extension) fails the *entire* tool result. Callers skip these and
    /// warn.
    #[serde(other)]
    Unknown,
}
/// MCP protocol revisions this client understands, newest first.
///
/// The client offers the newest and adopts whatever the server echoes back.
/// Pinning a single old revision — as this used to, with `2024-11-05`
/// hardcoded — locks every connection to the oldest dialect and, on HTTP,
/// actively misdeclares the connection: the streamable transport postdates
/// that revision entirely.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision offered in `initialize`.
pub const PREFERRED_PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// Client for communicating with MCP tool providers over any transport.
pub struct MCPClient {
    /// The underlying message channel (stdio or HTTP).
    transport: Box<dyn Transport>,
    /// Next request ID
    next_id: AtomicU64,
    /// How long to wait for a response to an ordinary request.
    request_timeout: Duration,
    /// Server capabilities after initialization
    capabilities: Option<ServerCapabilities>,
    /// The protocol revision agreed during `initialize`.
    protocol_version: Option<String>,
    /// Cached tool list from the server
    cached_tools: Vec<ToolMetadata>,
}

impl MCPClient {
    /// Build a client over an already-constructed transport.
    pub(crate) fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            next_id: AtomicU64::new(1),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            capabilities: None,
            protocol_version: None,
            cached_tools: Vec::new(),
        }
    }

    /// Spawn an MCP server as a child process and talk to it over stdio.
    pub async fn spawn(
        command: &str,
        args: &[&str],
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        let transport = StdioTransport::spawn(command, args, env).await?;
        Ok(Self::new(Box::new(transport)))
    }

    /// Connect to an MCP server over HTTP.
    pub fn connect_http(
        url: &str,
        headers: &HashMap<String, String>,
        allow_env: &[String],
    ) -> anyhow::Result<Self> {
        let transport = HttpTransport::new(url, headers, allow_env)?;
        Ok(Self::new(Box::new(transport)))
    }

    /// Attach a refresher used to re-authenticate on a mid-session `401`.
    ///
    /// Only the HTTP transport acts on it; stdio ignores it. The daemon sets
    /// this for an OAuth-backed HTTP server so a long run whose token expires
    /// keeps working.
    pub fn set_refresher(
        &mut self,
        refresher: std::sync::Arc<dyn crate::transport::BearerRefresher>,
    ) {
        self.transport.set_bearer_refresher(refresher);
    }

    /// Build a client for a configured server, over whichever transport the
    /// entry describes.
    ///
    /// Callers above this point — discovery, execution, the tool registry —
    /// never learn which one it turned out to be.
    pub async fn from_config(config: &MCPServerConfig) -> anyhow::Result<Self> {
        Self::from_config_with_auth(config, None, &[]).await
    }

    /// [`Self::from_config`] with a resolved `Authorization` header injected for
    /// an HTTP server.
    ///
    /// `auth_header` is the `(name, value)` pair from
    /// [`crate::OAuthClient::authorization_header`]; it is layered on top of the
    /// config's static headers (and wins on a clash, since a live token should
    /// override a stale hard-coded one). Ignored for stdio servers, which carry
    /// no HTTP headers.
    pub async fn from_config_with_auth(
        config: &MCPServerConfig,
        auth_header: Option<(String, String)>,
        allow_env: &[String],
    ) -> anyhow::Result<Self> {
        match config.resolve()? {
            ResolvedTransport::Stdio { command, args, env } => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                Self::spawn(command, &args, env).await
            }
            ResolvedTransport::Http { url, headers } => {
                let mut headers = headers.clone();
                if let Some((name, value)) = auth_header {
                    headers.insert(name, value);
                }
                Self::connect_http(url, &headers, allow_env)
            }
        }
    }

    /// Connect to the MCP server by sending initialize and initialized messages.
    pub async fn connect(&mut self) -> anyhow::Result<()> {
        tracing::info!("Initializing MCP connection");

        let init_params = serde_json::json!({
            "protocolVersion": PREFERRED_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "leviath",
                "version": env!("CARGO_PKG_VERSION")
            }
        });

        // Tighter than an ordinary request: a server that hasn't finished its
        // handshake in this long is broken, not busy, and it is holding up an
        // agent's startup.
        let result = self
            .request_with_timeout("initialize", init_params, DEFAULT_CONNECT_TIMEOUT)
            .await?;

        // Parse server capabilities
        let capabilities: ServerCapabilities = if let Some(caps) = result.get("capabilities") {
            serde_json::from_value(caps.clone()).unwrap_or_default()
        } else {
            ServerCapabilities::default()
        };
        self.capabilities = Some(capabilities);
        let version = negotiated_version(result.get("protocolVersion"));
        self.protocol_version = Some(version.clone());

        // Send initialized notification
        self.send_notification("notifications/initialized", serde_json::json!({}))
            .await?;

        tracing::info!(version = %version, "MCP connection established");
        Ok(())
    }

    /// List available tools from the server, following `nextCursor` pagination.
    ///
    /// `tools/list` is a paginated operation. Reading only the first response
    /// silently exposes just the first page of a large server's catalogue, so
    /// this loops until the server stops returning a cursor, bounded by an
    /// internal page limit so a server that returns a cursor forever can't spin
    /// the loop.
    pub async fn list_tools(&mut self) -> anyhow::Result<Vec<ToolMetadata>> {
        tracing::debug!("Listing MCP tools");

        let mut tools: Vec<ToolMetadata> = Vec::new();
        let mut cursor: Option<String> = None;

        for page in 0..MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(c) => serde_json::json!({ "cursor": c }),
                None => serde_json::json!({}),
            };
            let result = self.send_request("tools/list", params).await?;

            let tools_value = result.get("tools").cloned().unwrap_or(Value::Array(vec![]));
            let page_tools: Vec<ToolMetadata> = serde_json::from_value(tools_value)
                .map_err(|e| anyhow::anyhow!("Failed to parse tools list: {}", e))?;
            tools.extend(page_tools);

            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
            if page + 1 == MAX_TOOL_PAGES {
                tracing::warn!(
                    pages = MAX_TOOL_PAGES,
                    "MCP server still returned a tools/list cursor at the page \
                     limit — stopping; some tools may be missing"
                );
            }
        }

        self.cached_tools = tools.clone();
        // Bound in a plain `let` rather than inlined as a tracing field: as a
        // field it is only evaluated when a subscriber is installed, so its
        // coverage would depend on test ordering (see discovery.rs).
        let count = tools.len();
        tracing::debug!(count, "Discovered MCP tools");
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
    ///
    /// Always succeeds: a dead or unresponsive server must never block cleanup.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        tracing::info!("Shutting down MCP server");
        let _ = self.transport.close().await;
        Ok(())
    }

    /// Get the server capabilities (available after connect).
    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }

    /// The protocol revision agreed with the server (available after connect).
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// Get the cached tool list.
    pub fn cached_tools(&self) -> &[ToolMetadata] {
        &self.cached_tools
    }

    /// Send a JSON-RPC request and wait for a response.
    async fn send_request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.request_with_timeout(method, params, self.request_timeout)
            .await
    }

    /// [`Self::send_request`] with an explicit deadline.
    async fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::request(id, method, params);
        self.transport
            .send_request(&request, timeout)
            .await?
            .into_result()
    }

    /// Send a JSON-RPC notification (fire-and-forget, no response expected).
    async fn send_notification(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let request = JsonRpcRequest::notification(method, params);
        self.transport.send_notification(&request).await
    }
}

/// Decide which protocol revision is in force after `initialize`.
///
/// A server that echoes a revision we know wins outright. One that echoes
/// something unrecognized is *still* honored rather than rejected: the value
/// only feeds the `MCP-Protocol-Version` header, and refusing to talk to a
/// server that speaks a newer revision than this client was compiled against
/// would break connections that otherwise work fine.
fn negotiated_version(echoed: Option<&Value>) -> String {
    match echoed.and_then(Value::as_str) {
        Some(version) => {
            if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
                tracing::warn!(
                    version = %version,
                    "MCP server negotiated an unrecognized protocol revision — continuing"
                );
            }
            version.to_string()
        }
        None => {
            // Servers predating version negotiation omit the field.
            tracing::debug!("MCP server echoed no protocolVersion — assuming the offered one");
            PREFERRED_PROTOCOL_VERSION.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::always_on_tracing_guard;

    // ── Additional coverage tests ──────────────────────────────────────────

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
            structured_content: None,
            is_error: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("Hello"));
        // Wire name is `isError`, not `is_error` — a mismatch here meant every
        // failing tool call deserialized as a success.
        assert!(json.contains("\"isError\":false"), "got: {json}");
        assert!(!json.contains("is_error"), "got: {json}");
    }

    #[test]
    fn test_tool_result_with_error() {
        let result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "Something went wrong".to_string(),
            }],
            structured_content: None,
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
            resource: EmbeddedResource {
                uri: "file:///tmp/test.txt".to_string(),
                text: Some("file contents".to_string()),
                blob: None,
                mime_type: None,
            },
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("file:///tmp/test.txt"));
        assert!(json.contains("file contents"));
    }

    #[test]
    fn test_tool_result_content_resource_no_text() {
        let content = ToolResultContent::Resource {
            resource: EmbeddedResource {
                uri: "file:///tmp/test.txt".to_string(),
                text: None,
                blob: None,
                mime_type: None,
            },
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
            structured_content: None,
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
            structured_content: None,
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
            structured_content: None,
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
                    resource: EmbeddedResource {
                        uri: "file:///test".to_string(),
                        text: Some("content".to_string()),
                        blob: None,
                        mime_type: None,
                    },
                },
            ],
            structured_content: None,
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
        assert_eq!(
            content,
            ToolResultContent::Text {
                text: "hello world".to_string()
            }
        );
    }

    #[test]
    fn test_tool_result_content_image_deserialization() {
        let json = r#"{"type":"image","data":"abc123","mimeType":"image/png"}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        assert_eq!(
            content,
            ToolResultContent::Image {
                data: "abc123".to_string(),
                mime_type: "image/png".to_string(),
            }
        );
    }

    #[test]
    fn test_tool_result_content_resource_deserialization() {
        // The spec nests the payload under `resource`; it is not inlined on the
        // content block.
        let json = r#"{"type":"resource","resource":{"uri":"file:///tmp/x","text":"data"}}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        assert_eq!(
            content,
            ToolResultContent::Resource {
                resource: EmbeddedResource {
                    uri: "file:///tmp/x".to_string(),
                    text: Some("data".to_string()),
                    blob: None,
                    mime_type: None,
                },
            }
        );
    }

    // ─── filter_env additional ────────────────────────────────────────────

    // ─── JsonRpcRequest serialization ───────────────────────────────────

    // ─── JsonRpcResponse deserialization ─────────────────────────────────

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
                    resource: EmbeddedResource {
                        uri: "file:///x".to_string(),
                        text: Some("data".to_string()),
                        blob: None,
                        mime_type: None,
                    },
                },
            ],
            structured_content: None,
            is_error: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content.len(), 3);
        assert!(!back.is_error);
    }

    // ─── filter_env: mixed sensitive and safe ───────────────────────────

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
        assert_eq!(
            back,
            ToolResultContent::Text {
                text: "".to_string()
            }
        );
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
            resource: EmbeddedResource {
                uri: "".to_string(),
                text: None,
                blob: None,
                mime_type: None,
            },
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

    // Script that responds to initialize then closes its own stdin fd so the parent's
    // notification flush gets EPIPE. The process stays alive (sleeping) so the
    // process itself doesn't exit before our write attempt.
    //
    // Ordering note: os.close(0) MUST happen BEFORE the response is written to
    // stdout, not after. Closing after flushing the response is NOT a
    // happens-before relationship from our side: our client can finish reading
    // the response and race ahead to the notification write while the child is
    // still merely *about* to execute the next line, before the close(0)
    // syscall has actually completed. That race is flaky (passes on some CI
    // runners, fails on others, depending on process-scheduling speed).
    // Closing stdin first guarantees
    // the read end is fully closed before the response bytes can even be
    // sent, which our client necessarily observes only after they're sent --
    // so by the time we read the response and attempt the notification
    // write, the close has unconditionally already happened.
    const STUB_INIT_THEN_CLOSE_STDIN: &str = r#"
import sys, json, os
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req.get("method") == "initialize":
        id_ = req.get("id")
        os.close(0)
        result = {"capabilities": {}, "protocolVersion": "2024-11-05"}
        msg = json.dumps({"jsonrpc": "2.0", "id": id_, "result": result})
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()
        import time; time.sleep(10)
        break
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
    async fn test_connect_fails_when_notification_write_errors() {
        let _guard = always_on_tracing_guard();
        // Server responds to initialize then closes its stdin, causing our
        // notification flush to fail with EPIPE. This covers the `?` error
        // propagation path in connect() after send_notification.
        let mut client = spawn_stub_client(STUB_INIT_THEN_CLOSE_STDIN).await;
        let result = client.connect().await;
        assert!(result.is_err());
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
        assert_eq!(
            result.content[0],
            ToolResultContent::Text {
                text: "hello from tool".to_string()
            }
        );
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

    #[tokio::test]
    async fn test_mcp_client_server_error_propagates() {
        let mut client = spawn_stub_client(STUB_ERROR_SERVER).await;
        // initialize sends a request and the error server will return an error response
        let err = client.connect().await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("server error"));
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
        assert!(tools.is_empty());
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

    // ─── Spec wire-format conformance ─────────────────────────────────────
    //
    // Each test here pins a field name or shape that the client previously got
    // wrong. The failures they guard against were mostly *silent* (a tool error
    // read as success) or total (one unrecognized block failing an entire
    // result), which is what made real servers appear not to work.

    #[test]
    fn tool_result_reads_is_error_from_camel_case_wire_name() {
        // The regression that motivated all of this: the wire field is
        // `isError`. Reading `is_error` never matched, so `#[serde(default)]`
        // silently produced `false` and every failed tool call was handed to
        // the model as a success.
        let json = r#"{"content":[{"type":"text","text":"boom"}],"isError":true}"#;
        let result: ToolResult = serde_json::from_str(json).unwrap();
        assert!(result.is_error, "isError:true must deserialize as an error");
    }

    #[test]
    fn tool_result_snake_case_is_error_is_not_honored() {
        // Guards the inverse mistake: `is_error` is *not* a wire name, so a
        // payload using it must fall back to the default rather than silently
        // re-introducing the old behavior.
        let json = r#"{"content":[],"is_error":true}"#;
        let result: ToolResult = serde_json::from_str(json).unwrap();
        assert!(!result.is_error);
    }

    #[test]
    fn tool_result_content_defaults_to_empty() {
        let json = r#"{"structuredContent":{"ok":true}}"#;
        let result: ToolResult = serde_json::from_str(json).unwrap();
        assert!(result.content.is_empty());
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"ok": true}))
        );
    }

    #[test]
    fn tool_result_structured_content_absent_is_none_and_omitted() {
        let result = ToolResult {
            content: vec![],
            structured_content: None,
            is_error: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("structuredContent"), "got: {json}");
    }

    #[test]
    fn tool_result_content_image_uses_mime_type_wire_name() {
        let content = ToolResultContent::Image {
            data: "abc".to_string(),
            mime_type: "image/png".to_string(),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("\"mimeType\":\"image/png\""), "got: {json}");
        assert!(!json.contains("mime_type"), "got: {json}");
    }

    #[test]
    fn tool_result_content_audio_roundtrip() {
        let json = r#"{"type":"audio","data":"YWJj","mimeType":"audio/wav"}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        assert_eq!(
            content,
            ToolResultContent::Audio {
                data: "YWJj".to_string(),
                mime_type: "audio/wav".to_string(),
            }
        );
        let back: ToolResultContent =
            serde_json::from_str(&serde_json::to_string(&content).unwrap()).unwrap();
        assert_eq!(back, content);
    }

    #[test]
    fn tool_result_content_resource_link_full() {
        let json = r#"{"type":"resource_link","uri":"file:///m.rs","name":"m.rs",
                       "description":"entry point","mimeType":"text/x-rust"}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        assert_eq!(
            content,
            ToolResultContent::ResourceLink {
                uri: "file:///m.rs".to_string(),
                name: "m.rs".to_string(),
                description: Some("entry point".to_string()),
                mime_type: Some("text/x-rust".to_string()),
            }
        );
    }

    #[test]
    fn tool_result_content_resource_link_minimal_omits_optionals() {
        let content: ToolResultContent =
            serde_json::from_str(r#"{"type":"resource_link","uri":"file:///x"}"#).unwrap();
        assert_eq!(
            content,
            ToolResultContent::ResourceLink {
                uri: "file:///x".to_string(),
                name: String::new(),
                description: None,
                mime_type: None,
            }
        );
        let json = serde_json::to_string(&content).unwrap();
        assert!(!json.contains("description"), "got: {json}");
        assert!(!json.contains("mimeType"), "got: {json}");
    }

    #[test]
    fn tool_result_content_embedded_resource_is_nested() {
        let json = r#"{"type":"resource","resource":{"uri":"file:///m.rs",
                       "mimeType":"text/x-rust","text":"fn main() {}"}}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        assert_eq!(
            content,
            ToolResultContent::Resource {
                resource: EmbeddedResource {
                    uri: "file:///m.rs".to_string(),
                    text: Some("fn main() {}".to_string()),
                    blob: None,
                    mime_type: Some("text/x-rust".to_string()),
                }
            }
        );
    }

    #[test]
    fn tool_result_content_embedded_resource_binary_blob() {
        let json = r#"{"type":"resource","resource":{"uri":"file:///a.png","blob":"YWJj"}}"#;
        let content: ToolResultContent = serde_json::from_str(json).unwrap();
        assert_eq!(
            content,
            ToolResultContent::Resource {
                resource: EmbeddedResource {
                    uri: "file:///a.png".to_string(),
                    text: None,
                    blob: Some("YWJj".to_string()),
                    mime_type: None,
                }
            }
        );
        // Absent optionals stay absent on the way back out.
        let json = serde_json::to_string(&content).unwrap();
        assert!(!json.contains("text"), "got: {json}");
        assert!(!json.contains("mimeType"), "got: {json}");
    }

    #[test]
    fn unknown_content_type_degrades_instead_of_failing_the_result() {
        // Without the catch-all arm a single unrecognized block — a future
        // content type or a vendor extension — makes the *whole* tool result
        // fail to parse, taking the usable blocks down with it.
        let json = r#"{"content":[
            {"type":"text","text":"keep me"},
            {"type":"hologram","payload":{"deeply":["nested"]}}
        ],"isError":false}"#;
        let result: ToolResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.content.len(), 2);
        assert_eq!(
            result.content[0],
            ToolResultContent::Text {
                text: "keep me".to_string()
            }
        );
        assert_eq!(result.content[1], ToolResultContent::Unknown);
    }

    #[test]
    fn unknown_content_serializes_without_panicking() {
        // The payload is unrecoverable by construction (internally-tagged
        // enums can't carry data in a catch-all), so this only has to be
        // lossy, not lossless.
        let json = serde_json::to_string(&ToolResultContent::Unknown).unwrap();
        assert!(json.contains("Unknown"), "got: {json}");
    }

    // ─── tools/list pagination ────────────────────────────────────────────

    /// Serves `tools/list` in `pages` pages of one tool each, returning a
    /// `nextCursor` on every page but the last.
    fn paginated_stub(pages: usize) -> String {
        format!(
            r#"
import sys, json
PAGES = {pages}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method, id_ = req.get("method", ""), req.get("id")
    if method == "initialize":
        res = {{"capabilities": {{}}, "protocolVersion": "2024-11-05"}}
    elif method == "tools/list":
        cursor = int((req.get("params") or {{}}).get("cursor", "0"))
        res = {{"tools": [{{"name": "tool%d" % cursor, "inputSchema": {{}}}}]}}
        if cursor + 1 < PAGES:
            res["nextCursor"] = str(cursor + 1)
    else:
        continue
    sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": id_, "result": res}}) + "\n")
    sys.stdout.flush()
"#
        )
    }

    #[tokio::test]
    async fn list_tools_follows_next_cursor_across_pages() {
        let _guard = always_on_tracing_guard();
        let script = paginated_stub(3);
        let mut client = spawn_stub_client(&script).await;
        client.connect().await.unwrap();

        let tools = client
            .list_tools()
            .await
            .expect("list_tools should succeed");
        // Reading only the first response would have returned 1 of 3.
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["tool0", "tool1", "tool2"]);
        assert_eq!(client.cached_tools().len(), 3);
    }

    #[tokio::test]
    async fn list_tools_stops_at_the_page_limit() {
        let _guard = always_on_tracing_guard();
        // Always returns a cursor: without the bound this never terminates.
        let script = paginated_stub(MAX_TOOL_PAGES + 10);
        let mut client = spawn_stub_client(&script).await;
        client.connect().await.unwrap();

        let tools = client
            .list_tools()
            .await
            .expect("list_tools should succeed");
        assert_eq!(tools.len(), MAX_TOOL_PAGES);
    }

    #[tokio::test]
    async fn transport_failure_propagates_out_of_a_request() {
        let _guard = always_on_tracing_guard();
        // A server that exits immediately: the transport itself fails, as
        // distinct from a server that answers with a JSON-RPC `error` member.
        // Both must surface as errors, by different paths.
        let mut client = spawn_stub_client("import sys\nsys.exit(0)\n").await;
        assert!(client.connect().await.is_err());
    }

    // ─── protocol version negotiation ─────────────────────────────────────

    #[test]
    fn preferred_version_is_the_newest_supported() {
        assert_eq!(PREFERRED_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS[0]);
        // Sorted newest-first, so [0] really is the newest.
        let mut sorted = SUPPORTED_PROTOCOL_VERSIONS.to_vec();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(sorted, SUPPORTED_PROTOCOL_VERSIONS);
    }

    #[test]
    fn negotiation_adopts_a_recognized_echo() {
        let _guard = always_on_tracing_guard();
        let echoed = serde_json::json!("2025-06-18");
        assert_eq!(negotiated_version(Some(&echoed)), "2025-06-18");
    }

    #[test]
    fn negotiation_honors_an_unrecognized_echo() {
        let _guard = always_on_tracing_guard();
        // Refusing a revision newer than this client was compiled against
        // would break connections that otherwise work perfectly well.
        let echoed = serde_json::json!("2099-01-01");
        assert_eq!(negotiated_version(Some(&echoed)), "2099-01-01");
    }

    #[test]
    fn negotiation_falls_back_when_the_server_omits_the_field() {
        let _guard = always_on_tracing_guard();
        assert_eq!(negotiated_version(None), PREFERRED_PROTOCOL_VERSION);
    }

    #[test]
    fn negotiation_falls_back_when_the_echo_is_not_a_string() {
        let _guard = always_on_tracing_guard();
        let echoed = serde_json::json!(20251125);
        assert_eq!(
            negotiated_version(Some(&echoed)),
            PREFERRED_PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn connect_offers_the_preferred_version_and_records_the_echo() {
        let _guard = always_on_tracing_guard();
        // Echoes back a *different* supported revision than the one offered,
        // which is exactly what a server one step behind does.
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req.get("method") == "initialize":
        offered = req["params"]["protocolVersion"]
        assert offered == "2025-11-25", offered
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req.get("id"),
            "result": {"capabilities": {}, "protocolVersion": "2025-03-26"}}) + "\n")
        sys.stdout.flush()
"#;
        let mut client = spawn_stub_client(script).await;
        client.connect().await.expect("connect should succeed");
        assert_eq!(client.protocol_version(), Some("2025-03-26"));
    }

    #[tokio::test]
    async fn protocol_version_is_none_before_connect() {
        let client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        assert!(client.protocol_version().is_none());
    }

    // ─── construction over HTTP ───────────────────────────────────────────

    #[test]
    fn connect_http_builds_a_client_without_touching_the_network() {
        // No server is listening; construction must still succeed, because
        // the first request is what connects.
        assert!(MCPClient::connect_http("http://127.0.0.1:1/mcp", &HashMap::new(), &[]).is_ok());
    }

    struct NoopRefresher;
    #[async_trait::async_trait]
    impl crate::transport::BearerRefresher for NoopRefresher {
        async fn refresh(&self) -> anyhow::Result<String> {
            Ok("Bearer x".to_string())
        }
    }

    #[tokio::test]
    async fn set_refresher_on_stdio_is_a_noop() {
        // stdio has no bearer, so the transport's default no-op handles it; this
        // drives MCPClient::set_refresher and the default trait method.
        let mut client = spawn_stub_client(STUB_INIT_LIST_CALL).await;
        client.set_refresher(std::sync::Arc::new(NoopRefresher));
    }

    #[tokio::test]
    async fn set_refresher_on_http_is_accepted() {
        let mut client =
            MCPClient::connect_http("http://127.0.0.1:1/mcp", &HashMap::new(), &[]).unwrap();
        // Exercise the refresher itself so its body is covered.
        use crate::transport::BearerRefresher as _;
        assert_eq!(NoopRefresher.refresh().await.unwrap(), "Bearer x");
        client.set_refresher(std::sync::Arc::new(NoopRefresher));
    }

    #[test]
    fn connect_http_rejects_an_unparseable_url() {
        assert!(MCPClient::connect_http("not a url", &HashMap::new(), &[]).is_err());
    }

    #[tokio::test]
    async fn from_config_builds_the_stdio_transport() {
        let _guard = always_on_tracing_guard();
        let config = MCPServerConfig::stdio(
            "s",
            "python3",
            vec!["-c".into(), STUB_INIT_LIST_CALL.into()],
        );
        let mut client = MCPClient::from_config(&config)
            .await
            .expect("stdio config should connect");
        client.connect().await.expect("handshake should succeed");
        assert_eq!(client.list_tools().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn from_config_builds_the_http_transport() {
        let config = MCPServerConfig::http("s", "http://127.0.0.1:1/mcp");
        assert!(MCPClient::from_config(&config).await.is_ok());
    }

    #[tokio::test]
    async fn from_config_with_auth_injects_a_bearer_for_http() {
        // The header-injection arm: construction succeeds without a network
        // round-trip (connecting happens later).
        let config = MCPServerConfig::http("s", "http://127.0.0.1:1/mcp");
        let header = Some(("Authorization".to_string(), "Bearer tok".to_string()));
        assert!(
            MCPClient::from_config_with_auth(&config, header, &[])
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn from_config_rejects_an_unresolvable_entry() {
        let config = MCPServerConfig {
            name: "broken".to_string(),
            ..Default::default()
        };
        let err = MCPClient::from_config(&config)
            .await
            .err()
            .expect("an entry with neither command nor url cannot connect");
        assert!(err.to_string().contains("either a `command`"), "got: {err}");
    }
}
