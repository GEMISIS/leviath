//! The shared-world daemon: hosts one ECS world for every agent and serves the
//! control socket. This module holds the CLI-side pieces that plug into the
//! runtime's daemon library ([`leviath_runtime::host`],
//! [`leviath_runtime::control_socket`]): the tool service that bridges tool calls
//! to the built-in / MCP executors and the interaction hub.

pub mod client;
pub mod config_reload;
pub mod fanout_spawner;
pub mod gate_rules;
pub mod mcp_pool;
pub mod readiness;
pub mod recovery;
pub mod sandbox_manager;
pub mod script_host;
pub mod seed_command;
pub mod setup;
pub mod spawn;
pub mod subagent;
pub mod tool_service;
