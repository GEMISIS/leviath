//! # Leviath Runtime
//!
//! ECS-based agent execution engine using bevy_ecs.
//!
//! The runtime manages agent lifecycle, context window management, task scheduling,
//! and inference execution through a game-loop-inspired architecture where agents
//! are entities and their behaviors are systems.

pub(crate) mod cancel;
pub(crate) mod compaction_bridge;
pub mod components;
pub mod content_interner;
pub mod context_setup;
pub(crate) mod context_tools;
pub(crate) mod context_transform;
pub mod control_socket;
pub mod custom_region;
pub mod dynamic_interaction;
pub mod fanout;
pub(crate) mod gate_prompt;
pub mod host;
pub(crate) mod inference_bridge;
pub mod inference_pool;
pub mod interaction_hub;
pub mod interaction_points;
pub mod persistence;
pub(crate) mod persistence_bridge;
pub mod pipeline;
pub mod provider_creds;
pub(crate) mod providers;
pub(crate) mod repetition;
pub mod restore;
pub mod script_provider;
pub mod taint;
pub mod telemetry;
pub(crate) mod tick_scope;
pub mod title;
pub(crate) mod title_bridge;
pub mod tool_bridge;
pub mod world;
// test_support.rs gates itself with an inner `#![cfg(test)]` attribute, so no
// `#[cfg(test)]` is needed here (adding one would trigger clippy's
// `duplicated_attributes` lint under `-D warnings`).
mod test_support;

pub use components::{AgentState, AgentStatus, ContextWindow, ParentRef, SubAgentChildren};
pub use content_interner::ContentInternerRes;
pub use fanout::{FanOutSpawner, FanOutSpawnerRes};
pub use inference_bridge::RetryPolicy;
pub use inference_pool::{InferencePoolConfig, InferencePools};
pub use provider_creds::{ProviderCreds, build_provider_registry};
pub use providers::ProviderRegistry;
pub use taint::TaintGate;
pub use tool_bridge::BoxedToolExec;
