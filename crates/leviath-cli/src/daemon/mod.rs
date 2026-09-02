//! The shared-world daemon: hosts one ECS world for every agent and serves the
//! control socket. This module holds the CLI-side pieces that plug into the
//! runtime's daemon library ([`leviath_runtime::host`],
//! [`leviath_runtime::control_socket`]): the tool service that bridges tool calls
//! to the built-in / MCP executors and the interaction hub.

pub mod client;
pub(crate) mod config_reload;
pub(crate) mod fanout_spawner;
pub(crate) mod gate_rules;
pub mod lifecycle;
pub mod live_limits;
pub mod mcp_pool;
pub mod mcp_reload;
pub(crate) mod policy_reload;
pub mod provider_reload;
pub mod readiness;
pub(crate) mod recovery;
pub(crate) mod sandbox_manager;
pub(crate) mod script_host;
pub(crate) mod seed_command;
pub(crate) mod seed_tool;
pub mod setup;
pub(crate) mod spawn;
pub(crate) mod subagent;
pub(crate) mod telemetry_reload;
pub(crate) mod tool_service;
pub(crate) mod wait;
