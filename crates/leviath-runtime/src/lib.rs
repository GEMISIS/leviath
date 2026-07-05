//! # Leviath Runtime
//!
//! ECS-based agent execution engine using bevy_ecs.
//!
//! The runtime manages agent lifecycle, context window management, task scheduling,
//! and inference execution through a game-loop-inspired architecture where agents
//! are entities and their behaviors are systems.

pub mod components;
pub mod engine;
pub mod pool;
pub mod scheduler;
pub mod systems;
pub mod taint;
// test_support.rs gates itself with an inner `#![cfg(test)]` attribute, so no
// `#[cfg(test)]` is needed here (adding one would trigger clippy's
// `duplicated_attributes` lint under `-D warnings`).
mod test_support;

pub use components::{
    AgentMessage, AgentState, AgentStatus, CancellationToken, ContextWindow, EvictionResult,
    InferenceResult, MessageInbox, NeedsCompaction, ParentRef, SubAgentChildren, TaskAssignment,
};
pub use engine::{AgentEngine, ProviderRegistry, ToolResultRoutingConfig};
pub use pool::AgentPool;
pub use scheduler::TaskScheduler;
pub use taint::TaintGate;
