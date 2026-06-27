//! ECS components for agent state and execution.

use bevy_ecs::prelude::*;
use leviath_core::Region;
use serde::{Deserialize, Serialize};

/// Agent execution state component.
///
/// Tracks the current state of an agent's execution, including which stage
/// it's in and iteration counts.
#[derive(Component, Debug, Clone)]
pub struct AgentState {
    /// Unique identifier for this agent instance
    pub agent_id: String,

    /// Current execution stage
    pub current_stage: String,

    /// Number of iterations in current stage
    pub iteration: usize,

    /// Agent status
    pub status: AgentStatus,
}

/// Status of an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentStatus {
    /// Agent is idle, ready for tasks
    Idle,

    /// Agent is actively working on a task
    Active,

    /// Agent is waiting for input or external event
    Waiting,

    /// Agent has completed its task
    Complete,

    /// Agent encountered an error
    Error { message: String },
}

/// Context window component storing the agent's memory regions.
#[derive(Component, Debug, Clone)]
pub struct ContextWindow {
    /// All regions in this context window
    pub regions: Vec<Region>,

    /// Current total token usage
    pub current_tokens: usize,

    /// Maximum token budget
    pub max_tokens: usize,
}

impl ContextWindow {
    /// Create a new context window with the specified budget.
    pub fn new(max_tokens: usize) -> Self {
        Self {
            regions: Vec::new(),
            current_tokens: 0,
            max_tokens,
        }
    }

    /// Get a region by name.
    pub fn get_region(&self, name: &str) -> Option<&Region> {
        self.regions.iter().find(|r| r.name == name)
    }

    /// Get a mutable reference to a region by name.
    pub fn get_region_mut(&mut self, name: &str) -> Option<&mut Region> {
        self.regions.iter_mut().find(|r| r.name == name)
    }

    /// Calculate current token usage across all regions.
    pub fn calculate_tokens(&self) -> usize {
        self.regions.iter().map(|r| r.current_tokens).sum()
    }

    /// Check if the context window needs eviction.
    pub fn needs_eviction(&self, threshold: f32) -> bool {
        let usage_ratio = self.current_tokens as f32 / self.max_tokens as f32;
        usage_ratio >= threshold
    }
}

/// Task assignment component.
///
/// Represents a task that has been assigned to an agent for execution.
#[derive(Component, Debug, Clone)]
pub struct TaskAssignment {
    /// Unique task identifier
    pub task_id: String,

    /// Task description or prompt
    pub prompt: String,

    /// Task priority (higher = more important)
    pub priority: i32,

    /// Timestamp when task was assigned
    pub assigned_at: i64,
}

/// Inference result component.
///
/// Stores the result of an LLM inference call, including the response
/// and any tool calls that need to be executed.
#[derive(Component, Debug, Clone)]
pub struct InferenceResult {
    /// The model's response text
    pub response: String,

    /// Tool calls requested by the model
    pub tool_calls: Vec<ToolCall>,

    /// Tokens used in this inference
    pub tokens_used: usize,

    /// Timestamp of this inference
    pub timestamp: i64,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool identifier
    pub tool_id: String,

    /// Tool name
    pub name: String,

    /// Tool arguments
    pub arguments: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_window_creation() {
        let window = ContextWindow::new(10000);
        assert_eq!(window.max_tokens, 10000);
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn test_needs_eviction() {
        let mut window = ContextWindow::new(10000);
        window.current_tokens = 9500;
        assert!(window.needs_eviction(0.9));
        
        window.current_tokens = 5000;
        assert!(!window.needs_eviction(0.9));
    }
}
