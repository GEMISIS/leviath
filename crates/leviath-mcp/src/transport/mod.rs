//! Transport abstraction for MCP connections.
//!
//! MCP defines more than one way to carry the same JSON-RPC conversation:
//! stdio to a child process, and HTTP to a (usually remote) server. Everything
//! above this module — [`crate::MCPClient`], discovery, execution — is written
//! against the crate-internal `Transport` trait and never learns which one it
//! is talking over.

use std::time::Duration;

use async_trait::async_trait;

pub(crate) mod jsonrpc;
pub mod stdio;

pub(crate) use jsonrpc::{JsonRpcRequest, JsonRpcResponse};

/// How long to wait for a server to answer a request before giving up.
///
/// Generous, because a legitimate tool call can be genuinely slow (a build, a
/// network fetch). The point is only that "slow" can never become "forever" —
/// an unbounded read is how a silent server used to wedge the caller
/// permanently.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for the initial `initialize` handshake.
///
/// Much tighter than [`DEFAULT_REQUEST_TIMEOUT`]: a server that hasn't
/// completed its handshake in this long is misconfigured, not busy, and
/// blocking an agent's startup on it helps nobody.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A bidirectional MCP message channel.
///
/// Implementations own the framing and the connection lifecycle, and are
/// responsible for handling frames the *server* initiates (pings and the like)
/// without disturbing the request/response pairing the caller sees.
#[async_trait]
pub(crate) trait Transport: Send {
    /// Send a request and wait for its response, bounded by `timeout`.
    async fn send_request(
        &mut self,
        req: &JsonRpcRequest,
        timeout: Duration,
    ) -> anyhow::Result<JsonRpcResponse>;

    /// Send a notification. There is no reply to wait for.
    async fn send_notification(&mut self, req: &JsonRpcRequest) -> anyhow::Result<()>;

    /// Release the connection. Implementations swallow errors from an
    /// already-dead peer so cleanup can never fail.
    async fn close(&mut self) -> anyhow::Result<()>;
}
