//! # Leviath Runtime
//!
//! ECS-based agent execution engine using bevy_ecs.
//!
//! The runtime manages agent lifecycle, context window management, task scheduling,
//! and inference execution through a game-loop-inspired architecture where agents
//! are entities and their behaviors are systems.

pub mod compaction_bridge;
pub mod components;
pub mod context_setup;
pub mod context_tools;
pub mod context_transform;
pub mod control_socket;
pub mod dynamic_interaction;
pub mod fanout;
pub mod gate_prompt;
pub mod host;
pub mod inference_bridge;
pub mod inference_pool;
pub mod interaction_hub;
pub mod interaction_points;
pub mod persistence;
pub mod persistence_bridge;
pub mod pipeline;
pub mod provider_creds;
pub mod providers;
pub mod repetition;
pub mod restore;
pub mod taint;
pub mod tool_bridge;
pub mod world;
// test_support.rs gates itself with an inner `#![cfg(test)]` attribute, so no
// `#[cfg(test)]` is needed here (adding one would trigger clippy's
// `duplicated_attributes` lint under `-D warnings`).
mod test_support;

pub use components::{
    AgentMessage, AgentState, AgentStatus, CancellationToken, ContextWindow, EvictionResult,
    InferenceConfig, InferenceResult, MessageInbox, NeedsCompaction, ParentRef, SubAgentChildren,
    TaskAssignment, ToolResultRoutingComponent,
};
pub use fanout::{FanOutSpawner, FanOutSpawnerRes, WorkItem, parse_work_items};
pub use inference_bridge::{InferenceJob, InferenceOutcome, run_inference_job};
pub use inference_pool::{InferencePermit, InferencePoolConfig, InferencePools};
pub use provider_creds::{ProviderCreds, build_provider_registry};
pub use providers::ProviderRegistry;
pub use taint::TaintGate;
pub use tool_bridge::{BoxedToolExec, ToolExecFuture, ToolJob, ToolOutcome, tool_worker};
