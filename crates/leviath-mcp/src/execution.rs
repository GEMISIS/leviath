//! Tool execution via MCP.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::client::{MCPClient, ToolResult, ToolResultContent};
use crate::discovery::ToolMetadata;

/// Provider tool-name limit: the name advertised to the LLM must match
/// `^[A-Za-z0-9_-]{1,64}$` (the Anthropic/OpenAI rule). MCP names are laxer
/// (they allow dots), so any MCP name that violates this would make the
/// provider reject the *entire* request.
const MAX_TOOL_NAME_LEN: usize = 64;

/// Sanitize an MCP tool name into the provider-accepted character set.
///
/// Every character outside `[A-Za-z0-9_-]` (notably `.`, which MCP allows and
/// real servers use) becomes `_`, and the result is truncated to 64 bytes. An
/// empty result (a name of only illegal characters) falls back to `tool`.
pub fn sanitize_tool_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(MAX_TOOL_NAME_LEN);
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

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
    /// Each advertised name is [`sanitize_tool_name`]d and made unique against
    /// `reserved` (e.g. the built-in tool names) and every previously-registered
    /// tool, so the set of names handed to the provider is always valid and
    /// collision-free. The returned metadata carries the advertised names; the
    /// alias back to `(server, original)` is recorded for routing.
    pub fn add_client_advertised(
        &mut self,
        server_name: String,
        client: MCPClient,
        reserved: &HashSet<String>,
    ) -> Vec<ToolMetadata> {
        let mut advertised = Vec::new();
        for tool in client.cached_tools() {
            let name = self.unique_advertised_name(&tool.name, &server_name, reserved);
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

    /// Take a server's client back out of the executor, dropping the aliases
    /// that routed to it. The caller owns the returned client and is expected
    /// to shut it down - removal alone does not kill a stdio server's child
    /// process. `None` if no such server is registered.
    ///
    /// The pool's idle-disconnect uses this: a per-agent server whose last
    /// leasing run ended has no caller left, and before this the connection
    /// (and its child process) lived until the daemon exited.
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

    /// Compute a unique, provider-safe advertised name for `original`.
    ///
    /// **Always** `<server>__<tool>`, sanitized. The server is part of the name
    /// whether or not anything would have collided, because the alternative -
    /// bare name, prefixed only on a clash - made the name a function of
    /// registration order. Two servers both advertising `search` gave the bare
    /// name to whichever appeared first in `config.toml`, so a blueprint saying
    /// `available_tools = ["search"]` meant a different server's tool depending
    /// on the order of a file it does not control, and reordering that file
    /// silently re-pointed the grant.
    ///
    /// Qualifying every name removes the question. `alpha__search` and
    /// `beta__search` say which server they came from, and neither depends on
    /// who registered first.
    ///
    /// `__` rather than the `.` this reads more naturally as: the advertised
    /// name has to match `^[A-Za-z0-9_-]{1,64}$` or the provider rejects the
    /// whole request, and [`sanitize_tool_name`] would rewrite a dot to `_`
    /// anyway - so a manifest written with one would match nothing.
    ///
    /// A numeric suffix still resolves the residual case: two servers whose
    /// *names* sanitize to the same thing, or a qualified name that collides
    /// with a reserved built-in.
    fn unique_advertised_name(
        &self,
        original: &str,
        server: &str,
        reserved: &HashSet<String>,
    ) -> String {
        let free = |name: &str| !reserved.contains(name) && !self.aliases.contains_key(name);

        let qualified = sanitize_tool_name(&format!("{server}__{original}"));
        if free(&qualified) {
            return qualified;
        }
        let mut n = 2;
        loop {
            let candidate = sanitize_tool_name(&format!("{qualified}_{n}"));
            if free(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Which server advertises each tool: advertised name -> server name.
    ///
    /// The routing table read the other way round: a blueprint that grants a
    /// whole server needs to turn that server's name into the set of tools it
    /// covers.
    ///
    /// Advertised names are server-qualified, so most of them carry the answer
    /// in the string - but not reliably enough to parse it back out. A server
    /// named `my.server` sanitizes to `my_server`, and a collision appends
    /// `_2`, so splitting on `__` is a guess where this is a fact.
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

    /// Get the number of connected servers.
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    /// Map a ToolResult into an ExecutionResult.
    ///
    /// Only the model-readable blocks contribute to `text`; binary payloads
    /// (image/audio) and bare resource links do not, and an unmodelled block is
    /// skipped with a warning rather than failing the call.
    fn map_result(tool_result: ToolResult) -> ExecutionResult {
        let mut parts: Vec<&str> = Vec::new();
        for content in &tool_result.content {
            match content {
                ToolResultContent::Text { text } => parts.push(text.as_str()),
                ToolResultContent::Resource { resource } => {
                    if let Some(text) = resource.text.as_deref() {
                        parts.push(text);
                    }
                }
                ToolResultContent::Image { .. }
                | ToolResultContent::Audio { .. }
                | ToolResultContent::ResourceLink { .. } => {}
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

    // ─── execute_filtered: allowed tool ────────────────────────────────

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
    /// slow one, not the sum. Before the per-server lock the executor's own
    /// lock serialised every call behind whichever was in flight.
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
        in_flight
            .await
            .expect("task")
            .expect("the call still completes");
        let mut taken = executor.remove_client("slow").expect("idle now, so taken");
        taken.shutdown().await.expect("best-effort shutdown");
    }

    // ─── add_client / execute / execute_on with a live client ───────────
    //
    // Same Python-backed JSON-RPC stub approach used in client.rs/discovery.rs
    // tests. Note: MCPClient::shutdown() always returns Ok(()) by design (it
    // swallows failures so a dead server can't block cleanup) - so
    // shutdown_all()'s error-collection branch is intentionally left
    // uncovered here; there's no way to make client.shutdown() fail without
    // changing that documented "always succeeds" behavior.

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

    /// The end-to-end path against a live stub server. This used to go through
    /// `execute_filtered`, which is gone; the call it actually exercised -
    /// route by advertised name, dispatch, map the result - is `execute`, so
    /// the coverage moves rather than disappearing with its wrapper.
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
    fn map_result_skips_binary_blocks() {
        let text = text_of(vec![
            ToolResultContent::Text {
                text: "before".to_string(),
            },
            ToolResultContent::Image {
                data: "YWJj".to_string(),
                mime_type: "image/png".to_string(),
            },
            ToolResultContent::Audio {
                data: "YWJj".to_string(),
                mime_type: "audio/wav".to_string(),
            },
            ToolResultContent::Text {
                text: "after".to_string(),
            },
        ]);
        assert_eq!(text, "before\nafter");
    }

    #[test]
    fn map_result_skips_resource_links() {
        let text = text_of(vec![ToolResultContent::ResourceLink {
            uri: "file:///x".to_string(),
            name: "x".to_string(),
            description: None,
            mime_type: None,
        }]);
        assert_eq!(text, "");
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
    fn map_result_embedded_resource_blob_contributes_no_text() {
        let text = text_of(vec![ToolResultContent::Resource {
            resource: EmbeddedResource {
                uri: "file:///a.png".to_string(),
                text: None,
                blob: Some("YWJj".to_string()),
                mime_type: Some("image/png".to_string()),
            },
        }]);
        assert_eq!(text, "");
    }

    // ─── tool-name sanitization ───────────────────────────────────────────

    #[test]
    fn sanitize_passes_a_clean_name_through() {
        assert_eq!(sanitize_tool_name("get_weather-2"), "get_weather-2");
    }

    #[test]
    fn sanitize_replaces_dots_and_other_illegal_chars() {
        // Dots are legal in MCP but rejected by the provider name rule.
        assert_eq!(sanitize_tool_name("admin.tools.list"), "admin_tools_list");
        assert_eq!(sanitize_tool_name("weird name!/#"), "weird_name___");
    }

    #[test]
    fn sanitize_truncates_to_the_limit() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_tool_name(&long).len(), MAX_TOOL_NAME_LEN);
    }

    #[test]
    fn sanitize_of_illegal_chars_becomes_underscores_and_empty_falls_back() {
        // Illegal chars each become `_` (still a valid name); only a fully
        // empty result falls back to a placeholder.
        assert_eq!(sanitize_tool_name("...."), "____");
        assert_eq!(sanitize_tool_name(""), "tool");
    }

    // ─── unique_advertised_name ───────────────────────────────────────────

    /// Every advertised name carries its server, whether or not anything would
    /// have collided. The old scheme handed out the bare name first, which made
    /// it depend on which server registered earliest.
    #[test]
    fn unique_name_always_qualifies_with_the_server() {
        let exec = ToolExecutor::new();
        let reserved = HashSet::new();
        assert_eq!(
            exec.unique_advertised_name("github.search", "gh", &reserved),
            "gh__github_search",
            "qualified, and the dot sanitized to satisfy the provider rule"
        );
        assert_eq!(
            exec.unique_advertised_name("search", "alpha", &reserved),
            "alpha__search",
            "qualified even with nothing to collide with"
        );
    }

    #[test]
    fn unique_name_prefixes_on_a_reserved_collision() {
        // The base clashes with a built-in tool name → prefix with the server.
        let exec = ToolExecutor::new();
        let reserved: HashSet<String> = ["bash".to_string()].into_iter().collect();
        assert_eq!(
            exec.unique_advertised_name("bash", "srv", &reserved),
            "srv__bash"
        );
    }

    #[test]
    fn unique_name_prefixes_on_an_existing_alias_collision() {
        let mut exec = ToolExecutor::new();
        exec.aliases.insert(
            "search".to_string(),
            ("a".to_string(), "search".to_string()),
        );
        assert_eq!(
            exec.unique_advertised_name("search", "b", &HashSet::new()),
            "b__search"
        );
    }

    #[test]
    fn unique_name_appends_a_number_when_the_prefix_also_collides() {
        let mut exec = ToolExecutor::new();
        // Both the base and the server-prefixed form are already taken.
        exec.aliases.insert(
            "search".to_string(),
            ("a".to_string(), "search".to_string()),
        );
        exec.aliases
            .insert("b__search".to_string(), ("x".to_string(), "y".to_string()));
        assert_eq!(
            exec.unique_advertised_name("search", "b", &HashSet::new()),
            "b__search_2"
        );

        // And when _2 is taken too, it moves on to _3 (covers the loop step).
        exec.aliases.insert(
            "b__search_2".to_string(),
            ("x".to_string(), "y".to_string()),
        );
        assert_eq!(
            exec.unique_advertised_name("search", "b", &HashSet::new()),
            "b__search_3"
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
    /// into the tools it covers. The advertised name usually carries it, but
    /// parsing it back out is a guess - a server named `my.server` sanitizes to
    /// `my_server`, and a collision appends `_2` - so the table answers instead.
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
