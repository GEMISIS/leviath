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

/// Tool execution service that routes tool calls to the correct MCP server.
pub struct ToolExecutor {
    /// Active MCP clients, keyed by server name
    clients: HashMap<String, SharedClient>,
    /// Advertised tool name → (server name, original tool name).
    ///
    /// The name advertised to the LLM is sanitized to the provider's character
    /// rule and made unique across servers; this maps it back to the server and
    /// the original name the server itself expects on a `tools/call`.
    aliases: HashMap<String, (String, String)>,
}

impl ToolExecutor {
    /// Create a new tool executor.
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    /// Register a client and return its tools under *advertised* names.
    ///
    /// Every name is [`leviath_core::mcp_names::advertised_name`] and nothing
    /// else, so a blueprint can name a tool before the server has ever been
    /// reached. The alias back to `(server, original)` is recorded for routing.
    ///
    /// A name that is already taken is **refused, not renamed**: that tool is
    /// left out of the returned set and a warning says which two names ran into
    /// each other. Re-registering the same server replaces it, so its own
    /// aliases are dropped first and only a genuine clash is left to refuse.
    pub fn add_client_advertised(
        &mut self,
        server_name: String,
        client: MCPClient,
        reserved: &HashSet<String>,
    ) -> Vec<ToolMetadata> {
        // Re-registering a server (a reload, or a second blueprint declaring
        // the same name) replaces its client below. Its old aliases would
        // otherwise survive and route this server's names to the client that
        // just went away.
        self.aliases.retain(|_, (server, _)| *server != server_name);

        let mut advertised = Vec::new();
        for tool in client.cached_tools() {
            let name = leviath_core::mcp_names::advertised_name(&server_name, &tool.name);
            if reserved.contains(&name) || self.aliases.contains_key(&name) {
                self.refuse_tool(&server_name, &tool.name, &name);
                continue;
            }
            self.aliases
                .insert(name.clone(), (server_name.clone(), tool.name.clone()));
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
            advertised.push(ToolMetadata {
                name,
                description: tool.description.clone(),
                schema: tool.schema.clone(),
            });
        }
        self.clients
            .insert(server_name, Arc::new(tokio::sync::Mutex::new(client)));
        advertised
    }

    /// Say out loud that a tool is not being offered, and why.
    ///
    /// The alternative was a `_2` suffix, which renamed the tool to something
    /// no blueprint, `[mcp_overrides]` key or grant could have predicted, and
    /// which changed with the order servers connected in. Refusing the one
    /// tool keeps every other name exactly `<server>__<tool>`.
    ///
    /// Two ways to get here, and the message has to serve both. Two servers
    /// whose names collide is the operator's to fix, by renaming one in
    /// `config.toml`. A single server offering two tools that collide only
    /// after the 64-character limit is not, so the message says which names
    /// ran into each other rather than implying a fix.
    fn refuse_tool(&self, server: &str, original: &str, advertised: &str) {
        let owner = match self.aliases.get(advertised) {
            Some((other_server, other_tool)) => {
                format!("{other_server}'s {other_tool}")
            }
            None => "a built-in tool".to_string(),
        };
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

    /// Take a server's client back out of the executor, dropping the aliases
    /// that routed to it. The caller owns the returned client and is expected
    /// to shut it down - removal alone does not kill a stdio server's child
    /// process. `None` if no such server is registered.
    ///
    /// The pool's idle-disconnect uses this: a per-agent server whose last
    /// leasing run ended has no caller left, and its connection and child
    /// process would otherwise live until the daemon exits.
    pub fn remove_client(&mut self, server_name: &str) -> Option<MCPClient> {
        let shared = self.clients.remove(server_name)?;
        // A call still in flight holds a clone of the `Arc`. Removing the
        // server out from under it would end a `tools/call` mid-flight, so the
        // client goes back and the caller is told there is nothing to take;
        // the pool's idle-disconnect only removes a server no run is leasing,
        // so in practice this arm is a race that was lost by a hair.
        let client = match Arc::try_unwrap(shared) {
            Ok(mutex) => mutex.into_inner(),
            Err(shared) => {
                tracing::debug!(server = %server_name, "a call is in flight; keeping the server");
                self.clients.insert(server_name.to_string(), shared);
                return None;
            }
        };
        self.aliases.retain(|_, (server, _)| server != server_name);
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
    pub fn tool_owners(&self) -> HashMap<String, String> {
        self.aliases
            .iter()
            .map(|(advertised, (server, _))| (advertised.clone(), server.clone()))
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
        // The advertised → (server, original) alias is the authoritative route.
        // Aliases and clients are inserted and removed together, so an alias
        // whose server is missing cannot happen; folding the two lookups into
        // one answer keeps that from being a branch of its own.
        self.aliases
            .get(tool_name)
            .and_then(|(server, original)| {
                self.clients
                    .get(server)
                    .map(|client| (client.clone(), original.clone()))
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No MCP server found with tool '{}'. Available tools: {:?}",
                    tool_name,
                    self.aliases.keys().collect::<Vec<_>>()
                )
            })
    }

    /// Execute a tool on a specific server.
    pub async fn execute_on(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<ExecutionResult> {
        tracing::info!(server = %server_name, tool = %tool_name, "Executing tool on server");
        let client = self.shared_client(server_name)?;
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

    fn shared_client(&self, server_name: &str) -> anyhow::Result<SharedClient> {
        self.clients
            .get(server_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not found", server_name))
    }

    /// Shutdown all connected MCP clients.
    ///
    /// `MCPClient::shutdown` always returns `Ok` by design (it swallows
    /// subprocess errors so a dead server cannot block cleanup), so errors
    /// are discarded here too.
    pub async fn shutdown_all(&mut self) -> anyhow::Result<()> {
        tracing::info!("Shutting down all MCP clients");
        for client in self.clients.values() {
            let _ = client.lock().await.shutdown().await;
        }
        self.clients.clear();
        Ok(())
    }

    /// Whether a server is registered under `server_name`, busy or not.
    ///
    /// The distinction [`remove_client`](Self::remove_client) cannot make on
    /// its own: `None` from it means either "no such server" or "a call is in
    /// flight, so it stayed". A caller that wants to try again later needs to
    /// know which.
    pub fn has_client(&self, server_name: &str) -> bool {
        self.clients.contains_key(server_name)
    }

    /// Get the number of connected servers.
    pub fn server_count(&self) -> usize {
        self.clients.len()
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
        assert_eq!(executor.server_count(), 0);
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
        assert_eq!(executor.server_count(), 1);
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
    async fn add_client_and_server_count_reflects_it() {
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());
        assert_eq!(executor.server_count(), 1);
    }

    /// Removing a server takes back its client and drops the aliases that
    /// routed to it, so a later `execute` of its tools misses cleanly - and an
    /// unknown name removes nothing.
    #[tokio::test]
    async fn remove_client_takes_the_server_and_its_aliases() {
        let mut executor = ToolExecutor::new();
        let client = spawn_echo_client().await;
        let _ = executor.add_client_advertised("server1".to_string(), client, &HashSet::new());
        assert_eq!(executor.server_count(), 1);

        assert!(executor.remove_client("nope").is_none());
        let mut taken = executor.remove_client("server1").expect("was registered");
        assert_eq!(executor.server_count(), 0);
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
        assert_eq!(executor.server_count(), 0);
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
    /// This is what the `_2` suffix used to take away. A tool's name depended
    /// on which servers had connected first, so the same `config.toml` could
    /// advertise `srv__search` on one daemon start and `srv__search_2` on the
    /// next, and nothing outside the executor could tell which.
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
}
