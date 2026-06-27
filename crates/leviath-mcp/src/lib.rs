//! # Leviath MCP
//!
//! Model Context Protocol (MCP) integration for tool discovery and execution.
//!
//! MCP enables agents to discover and use tools from external providers,
//! standardizing tool interfaces across different implementations.

pub mod client;
pub mod discovery;
pub mod execution;

pub use client::MCPClient;
pub use discovery::ToolDiscovery;
pub use execution::ToolExecutor;
