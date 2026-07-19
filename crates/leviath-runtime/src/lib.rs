//! # Leviath Runtime
//!
//! ECS-based agent execution engine using bevy_ecs.
//!
//! The runtime manages agent lifecycle, context window management, task scheduling,
//! and inference execution through a game-loop-inspired architecture where agents
//! are entities and their behaviors are systems.

pub mod components;
pub mod context_setup;
pub mod dynamic_interaction;
pub mod engine;
pub mod graph;
pub mod inference_bridge;
pub mod inference_pool;
pub mod pool;
pub mod provider_creds;
pub mod repetition;
pub mod run_io;
pub mod scheduler;
pub mod spawn;
pub mod systems;
pub mod taint;
pub mod tool_source;
// test_support.rs gates itself with an inner `#![cfg(test)]` attribute, so no
// `#[cfg(test)]` is needed here (adding one would trigger clippy's
// `duplicated_attributes` lint under `-D warnings`).
mod test_support;

pub use components::{
    AgentMessage, AgentState, AgentStatus, CancellationToken, ContextWindow, EvictionResult,
    InferenceConfig, InferenceResult, MessageInbox, NeedsCompaction, ParentRef, SubAgentChildren,
    TaskAssignment, ToolResultRoutingComponent,
};
pub use engine::{
    AgentEngine, EngineHandle, ProviderRegistry, ToolExecutorDyn, ToolResultsFuture,
    run_inference_loop_shared,
};
pub use inference_bridge::{InferenceJob, InferenceOutcome, run_inference_job};
pub use inference_pool::{InferencePermit, InferencePoolConfig, InferencePools};
pub use pool::AgentPool;
pub use provider_creds::{ProviderCreds, build_provider_registry};
pub use run_io::RunIO;
pub use scheduler::TaskScheduler;
pub use spawn::spawn_child_agent;
pub use taint::TaintGate;
