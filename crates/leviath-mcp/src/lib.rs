//! # Leviath MCP
//!
//! Model Context Protocol (MCP) integration for tool discovery and execution.
//!
//! MCP enables agents to discover and use tools from external providers,
//! standardizing tool interfaces across different implementations.
//!
//! Tool servers are reached over JSON-RPC 2.0, carried by one of the transports
//! in [`transport`] — stdio to a spawned child process, or HTTP to a remote
//! server. Everything above the transport layer is identical either way.

pub mod client;
pub mod discovery;
pub mod execution;
pub mod transport;

#[cfg(test)]
mod test_support;

pub use client::{
    EmbeddedResource, MCPClient, PREFERRED_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS,
    ServerCapabilities, ToolResult, ToolResultContent, ToolsCapability,
};
pub use discovery::{MCPServerConfig, ToolDiscovery, ToolMetadata};
pub use execution::{ExecutionResult, ToolExecutor};
pub use transport::stdio::filter_env;
pub use transport::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT};
