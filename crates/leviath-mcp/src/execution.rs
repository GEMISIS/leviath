//! Tool execution via MCP.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::client::{MCPClient, ToolResult, ToolResultContent};
use crate::discovery::ToolMetadata;

/// The advertised-name rule, which the taint gate and the dashboard's tool
/// chooser need to reach the same answer as this module. It lives in
/// `leviath-core` so there is one of it; see [`leviath_core::mcp_names`].
pub use leviath_core::mcp_names::sanitize_tool_name;

/// Result of a tool execution, with convenience fields.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Result data as JSON (contains the content array)
    pub data: Value,
    /// Concatenated text content for convenience
    pub text: String,
    /// The binary blocks the server returned (`image`, `audio`, and a
    /// `resource` carrying a `blob`), decoded, each typed by the server's
    /// `mimeType`. The caller stores them; the executor has nowhere to.
    pub blobs: Vec<leviath_core::mime::Blob>,
}

/// A registered server's client, shared with every call in flight to it.
///
/// One lock per server rather than one around the executor: a `tools/call`
/// holds its client for the whole round trip, and a batch that names two
/// servers should not have the fast one wait behind the slow one. Calls to
/// the same server still run one at a time, in the order they took the lock.
pub type SharedClient = Arc<tokio::sync::Mutex<MCPClient>>;

/// One connection the executor holds.
struct Connection {
    /// The server's name, which its tools are qualified with.
    server: String,
    /// The client, shared with every call in flight to it.
    client: SharedClient,
    /// Each tool it offers: what the model is shown, and the name the server
    /// itself expects on a `tools/call`. In the order the server listed them.
    tools: Vec<(ToolMetadata, String)>,
}

impl Connection {
    /// The server's own name for the tool advertised as `advertised`.
    fn original_of(&self, advertised: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|(meta, _)| meta.name == advertised)
            .map(|(_, original)| original.as_str())
    }
}

/// Tool execution service that routes tool calls to the correct MCP server.
///
/// Connections are stored under a *key*, not under the server's name. Two
/// runs may each declare a server called `docs` that starts a different
/// process, so the daemon keys a connection by the server's whole launch spec
/// ([`crate::MCPServerConfig::connection_key`]) and each run routes through
/// the keys of the servers it declared ([`route_in`](Self::route_in)). The
/// servers every run shares, the global `[[mcp_servers]]`, are also
/// *published*: their tools route by advertised name alone
/// ([`route`](Self::route)).
pub struct ToolExecutor {
    /// Every connection, by key.
    connections: HashMap<String, Connection>,
    /// The published routing table: advertised tool name → (connection key,
    /// original tool name).
    ///
    /// The name advertised to the LLM is sanitized to the provider's character
    /// rule and made unique across published servers; this maps it back to the
    /// connection and the original name the server itself expects on a
    /// `tools/call`.
    aliases: HashMap<String, (String, String)>,
}

impl ToolExecutor {
    /// Create a new tool executor.
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    /// Register a client under its server's name and publish its tools; see
    /// [`add_published`](Self::add_published).
    pub fn add_client_advertised(
        &mut self,
        server_name: String,
        client: MCPClient,
        reserved: &HashSet<String>,
    ) -> Vec<ToolMetadata> {
        self.add_published(server_name.clone(), server_name, client, reserved)
    }

    /// Register a client under `key` and publish its tools, so they route by
    /// advertised name for every caller. What the global servers get.
    pub fn add_published(
        &mut self,
        key: String,
        server_name: String,
        client: MCPClient,
        reserved: &HashSet<String>,
    ) -> Vec<ToolMetadata> {
        self.add_connection(key.clone(), server_name, client, reserved);
        self.publish(&key, reserved)
    }

    /// Register a client under `key` without publishing it, and return its
    /// tools under *advertised* names. Only a caller routing through `key`
    /// ([`route_in`](Self::route_in)) reaches it.
    ///
    /// Every name is [`leviath_core::mcp_names::advertised_name`] and nothing
    /// else, so a blueprint can name a tool before the server has ever been
    /// reached. A tool whose name is reserved (a built-in's), or which lands
    /// on the same advertised name as one of this server's other tools, is
    /// **refused, not renamed**. Registering the same key again replaces the
    /// connection, and drops whatever it had published.
    pub fn add_connection(
        &mut self,
        key: String,
        server_name: String,
        client: MCPClient,
        reserved: &HashSet<String>,
    ) -> Vec<ToolMetadata> {
        // A replaced connection's published names would otherwise route this
        // key's names to tools the new client may not offer.
        self.aliases.retain(|_, (k, _)| *k != key);
        let mut tools: Vec<(ToolMetadata, String)> = Vec::new();
        for tool in client.cached_tools() {
            let name = leviath_core::mcp_names::advertised_name(&server_name, &tool.name);
            let mine = tools.iter().find(|(meta, _)| meta.name == name);
            if let Some((_, other)) = mine {
                self.refuse_tool(
                    &server_name,
                    &tool.name,
                    &name,
                    format!("{server_name}'s {other}"),
                );
                continue;
            }
            if reserved.contains(&name) {
                self.refuse_tool(
                    &server_name,
                    &tool.name,
                    &name,
                    "a built-in tool".to_string(),
                );
                continue;
            }
            // Unconditional: every advertised name is server-qualified, so it
            // never equals the original and there is nothing to compare
            // against. The line is what ties the name the model sees back to
            // the name the server knows.
            tracing::debug!(
                server = %server_name,
                original = %tool.name,
                advertised = %name,
                "advertising MCP tool under its server-qualified name"
            );
            let meta = ToolMetadata {
                name,
                description: tool.description.clone(),
                schema: tool.schema.clone(),
            };
            tools.push((meta, tool.name.clone()));
        }
        let advertised = tools.iter().map(|(meta, _)| meta.clone()).collect();
        self.connections.insert(
            key,
            Connection {
                server: server_name,
                client: Arc::new(tokio::sync::Mutex::new(client)),
                tools,
            },
        );
        advertised
    }

    /// Route `key`'s tools by advertised name alone, for every caller, and
    /// return the ones that now do. Nothing when no connection has that key.
    ///
    /// Publishing replaces whatever was published under the same server name
    /// (a global server edited in place) and the key's own earlier entries.
    /// A name some other published server already holds, or a reserved one,
    /// is **refused, not renamed**: that tool is left out and a warning says
    /// which two names ran into each other.
    pub fn publish(&mut self, key: &str, reserved: &HashSet<String>) -> Vec<ToolMetadata> {
        let Some(server) = self.connections.get(key).map(|c| c.server.clone()) else {
            return Vec::new();
        };
        let replaced: HashSet<String> = self
            .connections
            .iter()
            .filter(|(k, c)| k.as_str() == key || c.server == server)
            .map(|(k, _)| k.clone())
            .collect();
        self.aliases.retain(|_, (k, _)| !replaced.contains(k));
        let tools = self
            .connections
            .get(key)
            .map(|c| c.tools.clone())
            .unwrap_or_default();
        let mut published = Vec::new();
        for (meta, original) in tools {
            if reserved.contains(&meta.name) || self.aliases.contains_key(&meta.name) {
                let owner = self.owner_of(&meta.name);
                self.refuse_tool(&server, &original, &meta.name, owner);
                continue;
            }
            self.aliases
                .insert(meta.name.clone(), (key.to_string(), original));
            published.push(meta);
        }
        published
    }

    /// Who holds the published name `advertised`, for a refusal's message.
    fn owner_of(&self, advertised: &str) -> String {
        match self.aliases.get(advertised) {
            Some((key, other_tool)) => {
                let server = self
                    .connections
                    .get(key)
                    .map_or(key.as_str(), |c| c.server.as_str());
                format!("{server}'s {other_tool}")
            }
            None => "a built-in tool".to_string(),
        }
    }

    /// Say out loud that a tool is not being offered, and why.
    ///
    /// A suffix such as `_2` would rename the tool to something no blueprint,
    /// `[mcp_overrides]` key or grant could predict, and which would change
    /// with the order servers connected in. Refusing the one tool keeps every
    /// other name exactly `<server>__<tool>`.
    ///
    /// Two ways to get here, and the message has to serve both. Two servers
    /// whose names collide is the operator's to fix, by renaming one in
    /// `config.toml`. A single server offering two tools that collide only
    /// after the 64-character limit is not, so the message says which names
    /// ran into each other rather than implying a fix. `owner` is whoever
    /// holds the name.
    fn refuse_tool(&self, server: &str, original: &str, advertised: &str, owner: String) {
        tracing::warn!(
            server = %server,
            tool = %original,
            advertised = %advertised,
            conflicts_with = %owner,
            "not offering an MCP tool: its name is already taken. Two names that differ only \
             outside [A-Za-z0-9_-], or that match within the first 64 characters, land on the \
             same advertised name. Rename the server in config.toml if the other name is a \
             different server's"
        );
    }

    /// Take the connection under `key` back out of the executor, dropping the
    /// aliases that routed to it. The caller owns the returned client and is
    /// expected to shut it down - removal alone does not kill a stdio server's
    /// child process. `None` if no such connection is registered.
    ///
    /// The pool's idle-disconnect uses this: a per-agent server whose last
    /// leasing run ended has no caller left, and its connection and child
    /// process would otherwise live until the daemon exits. It names the
    /// connection by key, so another run's server of the same name stays.
    pub fn remove_client(&mut self, key: &str) -> Option<MCPClient> {
        let connection = self.connections.remove(key)?;
        // A call still in flight holds a clone of the `Arc`. Removing the
        // server out from under it would end a `tools/call` mid-flight, so the
        // client goes back and the caller is told there is nothing to take;
        // the pool's idle-disconnect only removes a server no run is leasing,
        // so in practice this arm is a race that was lost by a hair.
        let Connection {
            server,
            client,
            tools,
        } = connection;
        let client = match Arc::try_unwrap(client) {
            Ok(mutex) => mutex.into_inner(),
            Err(client) => {
                tracing::debug!(server = %server, "a call is in flight; keeping the server");
                self.connections.insert(
                    key.to_string(),
                    Connection {
                        server,
                        client,
                        tools,
                    },
                );
                return None;
            }
        };
        self.aliases.retain(|_, (k, _)| k != key);
        Some(client)
    }

    /// Which server advertises each tool: advertised name -> server name.
    ///
    /// The routing table read the other way round: a blueprint that grants a
    /// whole server needs to turn that server's name into the set of tools it
    /// covers.
    ///
    /// Advertised names are server-qualified, so they carry the answer in the
    /// string, but not in a form that can be parsed back out. A server name
    /// may itself contain `_`, so `a__b__c` is either `a`'s `b__c` or `a__b`'s
    /// `c`. Splitting on `__` is a guess where this table is a fact.
    ///
    /// Covers the published tools, the ones [`route`](Self::route) reaches.
    pub fn tool_owners(&self) -> HashMap<String, String> {
        self.aliases
            .iter()
            .filter_map(|(advertised, (key, _))| {
                self.connections
                    .get(key)
                    .map(|c| (advertised.clone(), c.server.clone()))
            })
            .collect()
    }

    /// Execute a tool by its advertised name, routing to the owning server.
    pub async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<ExecutionResult> {
        let (client, original) = self.route(tool_name)?;
        Self::call_routed(&client, &original, arguments).await
    }

    /// Resolve an advertised name to its server's client and the name that
    /// server knows the tool by.
    ///
    /// Split from [`execute`](Self::execute) so a caller holding a lock around
    /// the executor can let it go before the call: the route is a map lookup,
    /// the call is a network round trip, and only the first needs the executor.
    pub fn route(&self, tool_name: &str) -> anyhow::Result<(SharedClient, String)> {
        tracing::info!(tool = %tool_name, "Executing tool");
        // The advertised → (key, original) alias is the authoritative route.
        // Aliases and connections are inserted and removed together, so an
        // alias whose connection is missing cannot happen; folding the two
        // lookups into one answer keeps that from being a branch of its own.
        self.aliases
            .get(tool_name)
            .and_then(|(key, original)| {
                self.connections
                    .get(key)
                    .map(|c| (c.client.clone(), original.clone()))
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No MCP server found with tool '{}'. Available tools: {:?}",
                    tool_name,
                    self.aliases.keys().collect::<Vec<_>>()
                )
            })
    }

    /// [`route`](Self::route) for a caller that declared its own servers:
    /// the connections under `scope` are looked in first, in order, and the
    /// published table after them.
    ///
    /// This is how one run reaches its own `docs` server while another run
    /// reaches a different `docs`: both advertise `docs__search`, and only
    /// the key in each run's scope tells the two apart.
    pub fn route_in(
        &self,
        scope: &[String],
        tool_name: &str,
    ) -> anyhow::Result<(SharedClient, String)> {
        let own = scope.iter().find_map(|key| {
            let connection = self.connections.get(key)?;
            let original = connection.original_of(tool_name)?;
            Some((connection.client.clone(), original.to_string()))
        });
        match own {
            Some(routed) => {
                tracing::info!(tool = %tool_name, "Executing tool");
                Ok(routed)
            }
            None => self.route(tool_name),
        }
    }

    /// Execute a tool on the connection registered under `key` (a global
    /// server registered with [`add_client_advertised`](Self::add_client_advertised)
    /// is keyed by its name).
    pub async fn execute_on(
        &self,
        key: &str,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<ExecutionResult> {
        tracing::info!(server = %key, tool = %tool_name, "Executing tool on server");
        let client = self.shared_client(key)?;
        Self::call_routed(&client, tool_name, arguments).await
    }

    /// The call itself, on a client already resolved by [`route`](Self::route)
    /// or [`execute_on`](Self::execute_on). Holds only that server's lock.
    pub async fn call_routed(
        client: &SharedClient,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<ExecutionResult> {
        let tool_result = client.lock().await.call_tool(tool_name, arguments).await?;
        Ok(Self::map_result(tool_result))
    }

    fn shared_client(&self, key: &str) -> anyhow::Result<SharedClient> {
        self.connections
            .get(key)
            .map(|c| c.client.clone())
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found", key))
    }

    /// Shutdown all connected MCP clients.
    ///
    /// `MCPClient::shutdown` always returns `Ok` by design (it swallows
    /// subprocess errors so a dead server cannot block cleanup), so errors
    /// are discarded here too.
    pub async fn shutdown_all(&mut self) -> anyhow::Result<()> {
        tracing::info!("Shutting down all MCP clients");
        for connection in self.connections.values() {
            let _ = connection.client.lock().await.shutdown().await;
        }
        self.connections.clear();
        self.aliases.clear();
        Ok(())
    }

    /// Whether a connection is registered under `key`, busy or not.
    ///
    /// The distinction [`remove_client`](Self::remove_client) cannot make on
    /// its own: `None` from it means either "no such server" or "a call is in
    /// flight, so it stayed". A caller that wants to try again later needs to
    /// know which.
    pub fn has_client(&self, key: &str) -> bool {
        self.connections.contains_key(key)
    }

    /// Map a ToolResult into an ExecutionResult.
    ///
    /// The model-readable blocks contribute to `text`; the binary payloads
    /// (image, audio, a resource carrying a blob) are decoded into `blobs`
    /// for the caller to store; a bare resource link is described in the
    /// text, since its bytes were never sent; and an unmodelled block is
    /// skipped with a warning rather than failing the call.
    fn map_result(tool_result: ToolResult) -> ExecutionResult {
        let mut parts: Vec<String> = Vec::new();
        let mut blobs = Vec::new();
        for content in &tool_result.content {
            match content {
                ToolResultContent::Text { text } => parts.push(text.clone()),
                ToolResultContent::Resource { resource } => {
                    if let Some(text) = resource.text.as_deref() {
                        parts.push(text.to_string());
                    }
                    if let Some(blob) = resource.blob.as_deref() {
                        let name = resource
                            .uri
                            .rsplit('/')
                            .find(|s| !s.is_empty())
                            .unwrap_or(&resource.uri)
                            .to_string();
                        push_blob(&mut blobs, blob, resource.mime_type.as_deref(), Some(name));
                    }
                }
                ToolResultContent::Image { data, mime_type }
                | ToolResultContent::Audio { data, mime_type } => {
                    push_blob(&mut blobs, data, Some(mime_type), None);
                }
                ToolResultContent::ResourceLink {
                    uri,
                    name,
                    mime_type,
                    ..
                } => {
                    let label = match name.is_empty() {
                        true => uri.clone(),
                        false => format!("{name} ({uri})"),
                    };
                    parts.push(match mime_type {
                        Some(t) => format!("[link: {label}, {t}]"),
                        None => format!("[link: {label}]"),
                    });
                }
                ToolResultContent::Unknown => {
                    tracing::warn!("Skipping unrecognized MCP content block in tool result");
                }
            }
        }
        let mut text = parts.join("\n");

        // A structured-only result would otherwise reach the model as an empty
        // string. Servers *should* also mirror it into a text block, but not
        // all do.
        if text.is_empty()
            && let Some(structured) = &tool_result.structured_content
        {
            text = structured.to_string();
        }

        let data = serde_json::to_value(&tool_result.content).unwrap_or(Value::Null);

        ExecutionResult {
            success: !tool_result.is_error,
            data,
            text,
            blobs,
        }
    }
}

/// Decode one base64 payload into `blobs`, typed by the server's `mimeType`
/// (or `application/octet-stream` when it sent none or nonsense). A payload
/// that is not base64 is dropped with a warning: the server's bug, and not a
/// reason to fail a call whose text may still be useful.
fn push_blob(
    blobs: &mut Vec<leviath_core::mime::Blob>,
    data: &str,
    mime_type: Option<&str>,
    name: Option<String>,
) {
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data.trim()) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Skipping an MCP binary block that is not base64: {e}");
            return;
        }
    };
    let mime_type = mime_type
        .and_then(|t| leviath_core::mime::MimeType::parse(t).ok())
        .unwrap_or_else(leviath_core::mime::octet_stream);
    let mut blob = leviath_core::mime::Blob::new(mime_type, bytes);
    if let Some(name) = name {
        blob = blob.named(name);
    }
    blobs.push(blob);
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::EmbeddedResource;
    use crate::test_support::{
        McpStub, always_on_tracing_guard, echo_tool_stub, spawn_ready_client,
    };
    use std::sync::Arc;

    #[test]
    fn test_tool_executor_creation() {
        let executor = ToolExecutor::new();
        assert!(!executor.has_client("server1"));
    }

    // ─── ToolExecutor::default ──────────────────────────────────────────

    #[test]
    fn test_tool_executor_default() {
        let executor = ToolExecutor::default();
        assert!(!executor.has_client("server1"));
    }

    // ─── map_result: text content ───────────────────────────────────────

    #[test]
    fn test_map_result_text_content() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "Hello world".to_string(),
            }],
            structured_content: None,
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
            structured_content: None,
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
            structured_content: None,
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
            structured_content: None,
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
            structured_content: None,
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert_eq!(result.text, "before\nafter");
    }

    #[test]
    fn test_map_result_resource_with_text() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Resource {
                resource: EmbeddedResource {
                    uri: "file:///test".to_string(),
                    text: Some("resource content".to_string()),
                    blob: None,
                    mime_type: None,
                },
            }],
            structured_content: None,
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert_eq!(result.text, "resource content");
    }

    #[test]
    fn test_map_result_resource_without_text() {
        let tool_result = ToolResult {
            content: vec![ToolResultContent::Resource {
                resource: EmbeddedResource {
                    uri: "file:///test".to_string(),
                    text: None,
                    blob: None,
                    mime_type: None,
                },
            }],
            structured_content: None,
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
            structured_content: None,
            is_error: false,
        };
        let result = ToolExecutor::map_result(tool_result);
        assert!(result.data.is_array());
    }

    // ─── execute: no server ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_no_server_errors() {
        let _guard = always_on_tracing_guard();
        let executor = ToolExecutor::new();
        let result = executor
            .execute("nonexistent_tool", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    // ─── execute_on: unknown server ─────────────────────────────────────

    #[tokio::test]
    async fn test_execute_on_unknown_server() {
        let _guard = always_on_tracing_guard();
        let executor = ToolExecutor::new();
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
    }

    // ─── one lock per server ─────────────────────────────────────────────

    /// A stub whose `tools/call` sleeps `secs` before answering, so a call's
    /// duration is observable.
    fn slow_stub(secs: f64) -> String {
        echo_tool_stub().source().replace(
            "    elif method == \"tools/call\":\n",
            &format!(
                "    elif method == \"tools/call\":\n        import time; time.sleep({secs})\n"
            ),
        )
    }

    /// Two servers, one slow: a batch that names both takes as long as the
    /// slow one, not the sum. One lock per server is what allows that; a lock
    /// around the executor would serialise every call behind whichever is in
    /// flight.
    #[tokio::test]
    async fn calls_to_different_servers_overlap() {
        let mut executor = ToolExecutor::new();
        let _ = executor.add_client_advertised(
            "slow".to_string(),
            spawn_ready_client(&slow_stub(2.0)).await,
            &HashSet::new(),
        );
        let _ = executor.add_client_advertised(
            "fast".to_string(),
            spawn_ready_client(&echo_tool_stub().source()).await,
            &HashSet::new(),
        );
        let executor = Arc::new(executor);
        let started = std::time::Instant::now();
        let mut tasks = Vec::new();
        for i in 0..10 {
            let executor = executor.clone();
            let server = if i == 0 { "slow" } else { "fast" };
            tasks.push(tokio::spawn(async move {
                executor
                    .execute(&format!("{server}__echo"), serde_json::json!({}))
                    .await
                    .expect("the call succeeds")
            }));
        }
        for task in tasks {
            let result = task.await.expect("the task completes");
            assert!(result.success);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(3500),
            "ten calls, one of them 2 s, took {elapsed:?}: the fast server waited on the slow one"
        );
    }

    /// Two calls to the same server run one after the other: the second
    /// cannot overlap the first on one stdio pipe.
    #[tokio::test]
    async fn calls_to_the_same_server_stay_in_order() {
        let mut executor = ToolExecutor::new();
        let _ = executor.add_client_advertised(
            "slow".to_string(),
            spawn_ready_client(&slow_stub(0.5)).await,
            &HashSet::new(),
        );
        let executor = Arc::new(executor);
        let started = std::time::Instant::now();
        let a = {
            let executor = executor.clone();
            tokio::spawn(async move { executor.execute("slow__echo", serde_json::json!({})).await })
        };
        let b = {
            let executor = executor.clone();
            tokio::spawn(async move { executor.execute("slow__echo", serde_json::json!({})).await })
        };
        a.await.expect("task").expect("first call");
        b.await.expect("task").expect("second call");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(900),
            "two half-second calls on one server serialise"
        );
    }

    /// A server with a call in flight is not taken away mid-call: `remove_client`
    /// says there was nothing to take, and the server stays registered.
    #[tokio::test]
    async fn remove_client_keeps_a_server_with_a_call_in_flight() {
        let mut executor = ToolExecutor::new();
        let _ = executor.add_client_advertised(
            "slow".to_string(),
            spawn_ready_client(&slow_stub(1.0)).await,
            &HashSet::new(),
        );
        let (client, original) = executor.route("slow__echo").expect("routes");
        let in_flight = tokio::spawn(async move {
            ToolExecutor::call_routed(&client, &original, serde_json::json!({})).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            executor.remove_client("slow").is_none(),
            "a busy server is kept"
        );
        assert!(
            executor.has_client("slow"),
            "kept, which is how a caller tells a busy server from an unknown one"
        );
        in_flight
            .await
            .expect("task")
            .expect("the call still completes");
        let mut taken = executor.remove_client("slow").expect("idle now, so taken");
        assert!(!executor.has_client("slow"));
        taken.shutdown().await.expect("best-effort shutdown");
    }

    // ─── add_client / execute / execute_on with a live client ───────────
    //
    // Same Python-backed JSON-RPC stub approach used in client.rs/discovery.rs
    // tests.

    async fn spawn_echo_client() -> MCPClient {
        spawn_ready_client(&echo_tool_stub().source()).await
    }

    #[tokio::test]
    async fn add_client_registers_the_server() {
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());
        assert!(executor.has_client("server1"));
    }

    /// Removing a server takes back its client and drops the aliases that
    /// routed to it, so a later `execute` of its tools misses cleanly - and an
    /// unknown name removes nothing.
    #[tokio::test]
    async fn remove_client_takes_the_server_and_its_aliases() {
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());
        assert!(executor.has_client("server1"));

        assert!(executor.remove_client("nope").is_none());
        let mut taken = executor.remove_client("server1").expect("was registered");
        assert!(!executor.has_client("server1"));
        let result = executor
            .execute("server1__echo", serde_json::json!({}))
            .await;
        assert!(result.is_err(), "the removed server's tools route nowhere");
        // The caller owns the shutdown; this is what ends the child process.
        taken.shutdown().await.expect("shutdown is best-effort Ok");
    }

    #[tokio::test]
    async fn execute_finds_owning_server_and_calls_tool() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());

        let result = executor
            .execute("server1__echo", serde_json::json!({"text": "hi"}))
            .await
            .expect("execute should succeed");
        assert!(result.success);
        assert_eq!(result.text, "hello from tool");
    }

    #[tokio::test]
    async fn execute_on_specific_server_calls_tool() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());

        let result = executor
            .execute_on("server1", "echo", serde_json::json!({}))
            .await
            .expect("execute_on should succeed");
        assert!(result.success);
        assert_eq!(result.text, "hello from tool");
    }

    /// The end-to-end path against a live stub server: `execute` routes by
    /// advertised name, dispatches, and maps the result.
    #[tokio::test]
    async fn execute_routes_to_a_live_server_and_succeeds() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());

        let result = executor
            .execute("server1__echo", serde_json::json!({}))
            .await
            .expect("execute should succeed");
        assert!(result.success);
    }

    #[tokio::test]
    async fn shutdown_all_with_live_client_succeeds_and_clears() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());

        let result = executor.shutdown_all().await;
        assert!(result.is_ok());
        assert!(!executor.has_client("server1"));
    }

    // ─── ExecutionResult ────────────────────────────────────────────────

    #[test]
    fn test_execution_result_clone() {
        let result = ExecutionResult {
            success: true,
            data: serde_json::json!("test"),
            text: "hello".to_string(),
            blobs: Vec::new(),
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
            blobs: Vec::new(),
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("success"));
        assert!(debug.contains("false"));
    }

    // ─── execute_on: call_tool error propagation ────────────────────────
    //
    // Server returns a JSON-RPC error for tools/call, which causes
    // execute_on's `client.call_tool(...).await?` to propagate the error.

    #[tokio::test]
    async fn execute_on_propagates_call_tool_error() {
        let _guard = always_on_tracing_guard();
        let client = spawn_ready_client(
            &McpStub::new()
                .capabilities_json(r#"{"tools": {}}"#)
                .tool("echo", Some("echo"))
                .call_fails("tool execution failed")
                .source(),
        )
        .await;

        let mut executor = ToolExecutor::new();
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());

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

    // ─── map_result over the full content-block set ───────────────────────

    fn text_of(content: Vec<ToolResultContent>) -> String {
        ToolExecutor::map_result(ToolResult {
            content,
            structured_content: None,
            is_error: false,
        })
        .text
    }

    #[test]
    fn map_result_reports_tool_execution_error_as_failure() {
        // The end-to-end consequence of the `isError` rename: a failing tool
        // must reach the model as a failure, not a success.
        let result = ToolExecutor::map_result(ToolResult {
            content: vec![ToolResultContent::Text {
                text: "Invalid departure date".to_string(),
            }],
            structured_content: None,
            is_error: true,
        });
        assert!(!result.success);
        assert_eq!(result.text, "Invalid departure date");
    }

    #[test]
    fn map_result_decodes_binary_blocks_beside_the_text() {
        let _guard = always_on_tracing_guard();
        let result = ToolExecutor::map_result(ToolResult {
            content: vec![
                ToolResultContent::Text {
                    text: "before".to_string(),
                },
                ToolResultContent::Image {
                    data: "YWJj".to_string(),
                    mime_type: "image/png".to_string(),
                },
                ToolResultContent::Audio {
                    data: " YWJj ".to_string(),
                    mime_type: "not a type".to_string(),
                },
                ToolResultContent::Image {
                    data: "!!!".to_string(),
                    mime_type: "image/png".to_string(),
                },
                ToolResultContent::Text {
                    text: "after".to_string(),
                },
            ],
            structured_content: None,
            is_error: false,
        });
        assert_eq!(result.text, "before\nafter");
        assert_eq!(
            result.blobs.len(),
            2,
            "the block that is not base64 is dropped"
        );
        assert_eq!(result.blobs[0].mime_type.as_str(), "image/png");
        assert_eq!(result.blobs[0].bytes, b"abc");
        assert_eq!(result.blobs[0].name, None);
        assert_eq!(
            result.blobs[1].mime_type.as_str(),
            "application/octet-stream",
            "a mime type that does not parse falls back"
        );
    }

    #[test]
    fn map_result_describes_resource_links() {
        let text = text_of(vec![
            ToolResultContent::ResourceLink {
                uri: "file:///x".to_string(),
                name: "x".to_string(),
                description: None,
                mime_type: None,
            },
            ToolResultContent::ResourceLink {
                uri: "https://h/report.pdf".to_string(),
                name: String::new(),
                description: None,
                mime_type: Some("application/pdf".to_string()),
            },
        ]);
        assert_eq!(
            text,
            "[link: x (file:///x)]\n[link: https://h/report.pdf, application/pdf]"
        );
    }

    #[test]
    fn map_result_skips_unknown_blocks_without_losing_the_rest() {
        let _guard = always_on_tracing_guard();
        let text = text_of(vec![
            ToolResultContent::Unknown,
            ToolResultContent::Text {
                text: "still here".to_string(),
            },
        ]);
        assert_eq!(text, "still here");
    }

    #[test]
    fn map_result_falls_back_to_structured_content_when_no_text() {
        // Servers *should* mirror structured output into a text block; not all
        // do, and without this the model would receive an empty string.
        let result = ToolExecutor::map_result(ToolResult {
            content: vec![],
            structured_content: Some(serde_json::json!({"temperature": 22.5})),
            is_error: false,
        });
        assert_eq!(result.text, r#"{"temperature":22.5}"#);
    }

    #[test]
    fn map_result_prefers_text_blocks_over_structured_content() {
        let result = ToolExecutor::map_result(ToolResult {
            content: vec![ToolResultContent::Text {
                text: "human readable".to_string(),
            }],
            structured_content: Some(serde_json::json!({"a": 1})),
            is_error: false,
        });
        assert_eq!(result.text, "human readable");
    }

    #[test]
    fn map_result_embedded_resource_blob_is_a_named_blob() {
        let result = ToolExecutor::map_result(ToolResult {
            content: vec![
                ToolResultContent::Resource {
                    resource: EmbeddedResource {
                        uri: "file:///dir/a.png".to_string(),
                        text: None,
                        blob: Some("YWJj".to_string()),
                        mime_type: Some("image/png".to_string()),
                    },
                },
                ToolResultContent::Resource {
                    resource: EmbeddedResource {
                        uri: "///".to_string(),
                        text: None,
                        blob: Some("YWJj".to_string()),
                        mime_type: None,
                    },
                },
            ],
            structured_content: None,
            is_error: false,
        });
        assert_eq!(result.text, "");
        assert_eq!(result.blobs.len(), 2);
        assert_eq!(result.blobs[0].name.as_deref(), Some("a.png"));
        assert_eq!(result.blobs[0].mime_type.as_str(), "image/png");
        assert_eq!(result.blobs[1].name.as_deref(), Some("///"));
        assert_eq!(
            result.blobs[1].mime_type.as_str(),
            "application/octet-stream"
        );
    }

    // ─── advertised routing with live clients ─────────────────────────────

    /// A client to a stub whose single tool is named `tool_name` and
    /// replies with the name it was called under.
    async fn spawn_named(tool_name: &str) -> MCPClient {
        spawn_ready_client(
            &McpStub::new()
                .tool(tool_name, None)
                .echoing_tool_name()
                .source(),
        )
        .await
    }

    #[tokio::test]
    async fn a_dotted_tool_is_advertised_sanitized_and_still_routes() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_named("github.search").await;
        let advertised = executor.add_client_advertised("gh".to_string(), client, &HashSet::new());
        assert_eq!(advertised[0].name, "gh__github_search");

        // The LLM calls the advertised name; the server is called with its
        // original name ("github.search").
        let result = executor
            .execute("gh__github_search", serde_json::json!({}))
            .await
            .expect("advertised name routes");
        assert!(result.success);
        assert_eq!(result.text, "called github.search");
    }

    /// The name is the same whether or not anything else is registered, so a
    /// blueprint can be written against it before the server is ever reached.
    ///
    /// A suffix for a taken name (`srv__search_2`) would make the name depend
    /// on which servers connected first, so the same `config.toml` could
    /// advertise a different name on each daemon start, and nothing outside
    /// the executor could tell which.
    #[tokio::test]
    async fn an_advertised_name_never_depends_on_what_else_is_registered() {
        let _guard = always_on_tracing_guard();
        let predicted = leviath_core::mcp_names::advertised_name("srv", "search");

        let mut alone = ToolExecutor::new();
        let names = alone.add_client_advertised(
            "srv".to_string(),
            spawn_named("search").await,
            &HashSet::new(),
        );
        assert_eq!(names[0].name, predicted);

        // Now with the name already claimed by something else, and with a
        // built-in reserved. Neither moves it.
        let mut crowded = ToolExecutor::new();
        crowded.aliases.insert(
            "other__search".to_string(),
            ("other".to_string(), "search".to_string()),
        );
        let reserved: HashSet<String> = ["bash".to_string()].into_iter().collect();
        let names = crowded.add_client_advertised(
            "srv".to_string(),
            spawn_named("search").await,
            &reserved,
        );
        assert_eq!(names[0].name, predicted, "no suffix, ever");
    }

    /// A name that is genuinely taken refuses the tool instead of renaming it.
    /// Reachable when two server names collide, or when two long names match
    /// within the provider's 64-character limit.
    #[tokio::test]
    async fn a_tool_whose_name_is_taken_is_refused_not_renamed() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        // Stand in for whatever already owns the name.
        executor.aliases.insert(
            "srv__search".to_string(),
            ("elsewhere".to_string(), "search".to_string()),
        );
        let names = executor.add_client_advertised(
            "srv".to_string(),
            spawn_named("search").await,
            &HashSet::new(),
        );
        assert!(
            names.is_empty(),
            "the tool is not offered at all: {names:?}"
        );
        assert_eq!(
            executor.aliases.get("srv__search"),
            Some(&("elsewhere".to_string(), "search".to_string())),
            "the name still belongs to whoever had it"
        );
        assert!(
            !executor.aliases.contains_key("srv__search_2"),
            "and nothing was invented to hold the refused tool"
        );
    }

    /// A tool whose advertised name is a built-in's is refused too, rather
    /// than renamed around the built-in.
    #[tokio::test]
    async fn a_tool_colliding_with_a_reserved_name_is_refused() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let reserved: HashSet<String> = ["srv__search".to_string()].into_iter().collect();
        let names = executor.add_client_advertised(
            "srv".to_string(),
            spawn_named("search").await,
            &reserved,
        );
        assert!(names.is_empty(), "{names:?}");
    }

    /// Re-registering a server replaces it. Its old aliases have to go with
    /// the old client, or they route this server's names into a client that is
    /// no longer there, and every tool looks like a collision with itself.
    #[tokio::test]
    async fn re_registering_a_server_replaces_its_aliases() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();

        let first = executor.add_client_advertised(
            "srv".to_string(),
            spawn_named("search").await,
            &HashSet::new(),
        );
        assert_eq!(first[0].name, "srv__search");

        // The same server name again, now offering a different tool.
        let second = executor.add_client_advertised(
            "srv".to_string(),
            spawn_named("lookup").await,
            &HashSet::new(),
        );
        assert_eq!(second[0].name, "srv__lookup");
        let left: Vec<&String> = executor.aliases.keys().collect();
        assert!(
            !executor.aliases.contains_key("srv__search"),
            "the replaced server's aliases are gone: {left:?}"
        );
        assert!(
            executor
                .execute("srv__lookup", serde_json::json!({}))
                .await
                .expect("the new tool routes")
                .success
        );
    }

    #[tokio::test]
    async fn two_servers_sharing_a_tool_name_are_disambiguated() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();

        let a = spawn_named("search").await;
        let a_names = executor.add_client_advertised("alpha".to_string(), a, &HashSet::new());
        assert_eq!(a_names[0].name, "alpha__search");

        let b = spawn_named("search").await;
        // Reserve what alpha already advertised.
        let reserved: HashSet<String> = a_names.iter().map(|t| t.name.clone()).collect();
        let b_names = executor.add_client_advertised("beta".to_string(), b, &reserved);
        assert_eq!(b_names[0].name, "beta__search");

        // Both route to their own server with the original name "search".
        assert!(
            executor
                .execute("alpha__search", serde_json::json!({}))
                .await
                .unwrap()
                .success
        );
        assert!(
            executor
                .execute("beta__search", serde_json::json!({}))
                .await
                .unwrap()
                .success
        );
    }

    /// A blueprint granting a whole connector needs to turn a server's name
    /// into the tools it covers. The advertised name carries it, but parsing it
    /// back out is a guess, because a server name may itself contain `_`. The
    /// table answers instead.
    #[tokio::test]
    async fn tool_owners_names_the_server_behind_each_advertised_tool() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();

        let a = spawn_named("search").await;
        let a_names = executor.add_client_advertised("alpha".to_string(), a, &HashSet::new());
        let b = spawn_named("search").await;
        let reserved: HashSet<String> = a_names.iter().map(|t| t.name.clone()).collect();
        let _ = executor.add_client_advertised("beta".to_string(), b, &reserved);

        let owners = executor.tool_owners();
        assert_eq!(
            owners.get("alpha__search").map(String::as_str),
            Some("alpha"),
            "each name says which server it came from, whoever registered first"
        );
        assert_eq!(owners.get("beta__search").map(String::as_str), Some("beta"));
        assert_eq!(owners.len(), 2);
    }

    #[tokio::test]
    async fn add_client_reserving_nothing_registers_identity_aliases() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_named("plain").await;
        let _ = executor.add_client_advertised("s".to_string(), client, &HashSet::new());
        assert!(
            executor
                .execute("s__plain", serde_json::json!({}))
                .await
                .unwrap()
                .success
        );
    }

    /// A server named `name` offering `whoami`, which answers `identity`.
    async fn spawn_identified(identity: &str) -> MCPClient {
        spawn_ready_client(
            &McpStub::new()
                .tool("whoami", None)
                .replying(identity)
                .source(),
        )
        .await
    }

    /// What `tool` answers when routed through `scope`.
    async fn answer_in(executor: &ToolExecutor, scope: &[String], tool: &str) -> String {
        let (client, original) = executor.route_in(scope, tool).expect("routes");
        ToolExecutor::call_routed(&client, &original, serde_json::json!({}))
            .await
            .unwrap()
            .text
    }

    /// Two connections under one server name, kept apart by key: each scope
    /// reaches its own, neither is published, and removing one leaves the
    /// other routable.
    #[tokio::test]
    async fn same_named_servers_under_different_keys_stay_apart() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let none = HashSet::new();
        let a = executor.add_connection(
            "key-a".to_string(),
            "twin".to_string(),
            spawn_identified("A").await,
            &none,
        );
        let b = executor.add_connection(
            "key-b".to_string(),
            "twin".to_string(),
            spawn_identified("B").await,
            &none,
        );
        assert_eq!(
            a[0].name, "twin__whoami",
            "the advertised name is unchanged"
        );
        assert_eq!(b[0].name, "twin__whoami");
        let (scope_a, scope_b) = (vec!["key-a".to_string()], vec!["key-b".to_string()]);
        assert_eq!(answer_in(&executor, &scope_a, "twin__whoami").await, "A");
        assert_eq!(answer_in(&executor, &scope_b, "twin__whoami").await, "B");
        assert!(
            executor.route("twin__whoami").is_err(),
            "an unpublished connection is reached through its key only"
        );
        assert!(executor.tool_owners().is_empty());

        let mut gone = executor.remove_client("key-a").expect("idle, so taken");
        let _ = gone.shutdown().await;
        assert!(!executor.has_client("key-a"));
        assert!(executor.route_in(&scope_a, "twin__whoami").is_err());
        assert_eq!(answer_in(&executor, &scope_b, "twin__whoami").await, "B");
    }

    /// A scope that does not hold the tool falls back to the published table,
    /// which is how a run reaches the global servers.
    #[tokio::test]
    async fn a_scope_falls_back_to_the_published_servers() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let none = HashSet::new();
        let _ = executor.add_published(
            "key-g".to_string(),
            "global".to_string(),
            spawn_identified("G").await,
            &none,
        );
        let _ = executor.add_connection(
            "key-own".to_string(),
            "own".to_string(),
            spawn_identified("O").await,
            &none,
        );
        let scope = vec!["missing".to_string(), "key-own".to_string()];
        assert_eq!(answer_in(&executor, &scope, "global__whoami").await, "G");
        assert_eq!(answer_in(&executor, &scope, "own__whoami").await, "O");
        assert_eq!(
            executor
                .tool_owners()
                .get("global__whoami")
                .map(String::as_str),
            Some("global")
        );
    }

    /// One server offering two tools that land on one advertised name offers
    /// the first and refuses the second, under its key as much as published.
    #[tokio::test]
    async fn a_servers_own_two_tools_on_one_name_offer_the_first() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let client = spawn_ready_client(
            &McpStub::new()
                .tool("find.all", None)
                .tool("find_all", None)
                .echoing_tool_name()
                .source(),
        )
        .await;
        let offered = executor.add_connection(
            "k".to_string(),
            "tracker".to_string(),
            client,
            &HashSet::new(),
        );
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].name, "tracker__find_all");
        let (_, original) = executor
            .route_in(&["k".to_string()], "tracker__find_all")
            .expect("routes");
        assert_eq!(original, "find.all");
    }

    /// A name reserved at publish time is refused there, even when the
    /// connection itself offered it.
    #[tokio::test]
    async fn publishing_refuses_a_name_reserved_since_connecting() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let _ = executor.add_connection(
            "k".to_string(),
            "srv".to_string(),
            spawn_named("search").await,
            &HashSet::new(),
        );
        let reserved: HashSet<String> = ["srv__search".to_string()].into_iter().collect();
        assert_eq!(executor.owner_of("srv__search"), "a built-in tool");
        assert!(executor.publish("k", &reserved).is_empty());
        assert!(executor.route("srv__search").is_err());
    }

    /// Publishing a key nobody registered publishes nothing.
    #[test]
    fn publishing_an_unknown_key_is_a_no_op() {
        let mut executor = ToolExecutor::new();
        assert!(executor.publish("nobody", &HashSet::new()).is_empty());
        assert!(executor.aliases.is_empty());
    }

    /// Publishing a second connection under the same server name replaces the
    /// first one's published names (a global server edited in place) and
    /// leaves the first connection itself registered under its key.
    #[tokio::test]
    async fn publishing_a_server_name_again_takes_over_its_names() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let none = HashSet::new();
        let _ = executor.add_published(
            "old".to_string(),
            "docs".to_string(),
            spawn_identified("old").await,
            &none,
        );
        let published = executor.add_published(
            "new".to_string(),
            "docs".to_string(),
            spawn_identified("new").await,
            &none,
        );
        assert_eq!(published.len(), 1);
        assert_eq!(answer_in(&executor, &[], "docs__whoami").await, "new");
        assert!(executor.has_client("old"));
        assert_eq!(
            answer_in(&executor, &["old".to_string()], "docs__whoami").await,
            "old"
        );
    }

    /// A name a published server holds refuses another published server's
    /// tool, and the warning names the holder by server, not by key.
    #[tokio::test]
    async fn a_published_clash_names_the_holding_server() {
        let _guard = always_on_tracing_guard();
        let mut executor = ToolExecutor::new();
        let none = HashSet::new();
        let _ = executor.add_published(
            "k1".to_string(),
            "a.b".to_string(),
            spawn_identified("first").await,
            &none,
        );
        assert_eq!(executor.owner_of("a_b__whoami"), "a.b's whoami");
        let refused = executor.add_published(
            "k2".to_string(),
            "a_b".to_string(),
            spawn_identified("second").await,
            &none,
        );
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(answer_in(&executor, &[], "a_b__whoami").await, "first");
        // Through its own key, the refused server is still reachable.
        assert_eq!(
            answer_in(&executor, &["k2".to_string()], "a_b__whoami").await,
            "second"
        );
    }
}
