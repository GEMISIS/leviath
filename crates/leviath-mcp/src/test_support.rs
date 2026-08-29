//! Crate-local names for the shared helpers in `leviath-testkit`, plus the
//! one Python stub server the client, discovery and execution tests all talk
//! to.

use std::collections::HashMap;

pub(crate) use leviath_testkit::mcp_stub::McpStub;
pub(crate) use leviath_testkit::tracing_guard as always_on_tracing_guard;

use crate::client::MCPClient;

/// The stub most tests want: a `tools` capability with `listChanged`, one
/// tool named `echo` described as `echo tool`, and a `tools/call` that
/// answers `hello from tool`.
pub(crate) fn echo_tool_stub() -> McpStub {
    McpStub::new()
        .list_changed(true)
        .tool("echo", Some("echo tool"))
}

/// Spawn `source` under `python3`, complete the handshake and list its
/// tools, so the returned client is ready to have tools called on it.
pub(crate) async fn spawn_ready_client(source: &str) -> MCPClient {
    let mut client = MCPClient::spawn("python3", &["-c", source], &HashMap::new())
        .await
        .expect("failed to spawn stub server");
    client.connect().await.expect("connect should succeed");
    client
        .list_tools()
        .await
        .expect("list_tools should succeed");
    client
}
