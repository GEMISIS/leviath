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

pub use engine::{AgentEngine, ProviderRegistry, ToolResultRoutingConfig};
pub use components::{
    AgentMessage, AgentState, AgentStatus, CancellationToken, ContextWindow, InferenceResult,
    MessageInbox, TaskAssignment,
};
pub use pool::AgentPool;
pub use scheduler::TaskScheduler;
