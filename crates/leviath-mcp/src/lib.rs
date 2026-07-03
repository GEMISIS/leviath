//! # Leviath MCP
//!
//! Model Context Protocol (MCP) integration for tool discovery and execution.
//!
//! MCP enables agents to discover and use tools from external providers,
//! standardizing tool interfaces across different implementations.
//! Uses JSON-RPC 2.0 over stdin/stdout to communicate with tool servers.

pub mod client;
pub mod discovery;
pub mod execution;

#[cfg(test)]
mod test_support;

pub use client::{MCPClient, ServerCapabilities, ToolResult, ToolResultContent, ToolsCapability};
pub use discovery::{MCPServerConfig, ToolDiscovery, ToolMetadata};
pub use execution::{ExecutionResult, ToolExecutor};
