//! # Leviath MCP
//!
//! Model Context Protocol (MCP) integration for tool discovery and execution.
//!
//! MCP enables agents to discover and use tools from external providers,
//! standardizing tool interfaces across different implementations.
//!
//! Tool servers are reached over JSON-RPC 2.0, carried by one of the transports
//! in [`transport`] - stdio to a spawned child process, or HTTP to a remote
//! server. Everything above the transport layer is identical either way.

pub mod auth;
pub mod client;
pub mod discovery;
pub mod execution;
pub mod server;
pub mod transport;

#[cfg(test)]
mod test_support;

pub use auth::{
    AuthStore, BrowserOpener, CALLBACK_PATH, LoginOutcome, OAuthClient, Pkce, ServerAuth,
    StoredTokenRefresher, wait_for_callback,
};
pub use client::{EmbeddedResource, MCPClient, ToolResult};
pub use client::{PREFERRED_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS};
pub use discovery::{MCPServerConfig, MCPTransport, ResolvedTransport, ToolDiscovery};
pub use execution::ToolExecutor;
/// The handshake deadline a caller with a person waiting should use. Exported
/// so a caller that overrides it (a test) still names the production value in
/// the one place it does not.
pub use transport::DEFAULT_CONNECT_TIMEOUT;
