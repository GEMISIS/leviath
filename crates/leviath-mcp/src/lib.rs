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
pub mod transport;

#[cfg(test)]
mod test_support;

pub use auth::{AuthStore, BrowserOpener, OAuthClient, ServerAuth, StoredTokenRefresher};
pub use client::{EmbeddedResource, MCPClient, ToolResult};
pub use discovery::{MCPServerConfig, MCPTransport, ResolvedTransport, ToolDiscovery};
pub use execution::ToolExecutor;
