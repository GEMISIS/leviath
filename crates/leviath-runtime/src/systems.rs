//! ECS systems for agent execution.
//!
//! Systems implement agent behaviors:
//! - Context management: eviction, compaction, region updates
//! - Inference: calling LLM providers with context
//! - Tool execution: running tools and updating context with results

use bevy_ecs::prelude::*;
use crate::components::{AgentState, ContextWindow, TaskAssignment, InferenceResult};

/// System that manages context window state.
///
/// Monitors token usage and triggers eviction when needed.
pub fn context_management_system(
    mut query: Query<(&AgentState, &mut ContextWindow)>,
) {
    for (_state, mut window) in query.iter_mut() {
        // Update current token count
        window.current_tokens = window.calculate_tokens();

        // Check if eviction is needed
        if window.needs_eviction(0.9) {
            tracing::debug!(
                agent_id = %_state.agent_id,
                tokens = window.current_tokens,
                "Context window needs eviction"
            );
            // TODO: Trigger eviction cascade
        }
    }
}

/// System that executes inference for agents with tasks.
///
/// Constructs prompts from context windows and calls LLM providers.
pub fn inference_system(
    mut query: Query<(&mut AgentState, &ContextWindow, &TaskAssignment)>,
) {
    for (mut _state, _window, _task) in query.iter_mut() {
        // TODO: Construct prompt from context window
        // TODO: Call provider inference
        // TODO: Add InferenceResult component
        tracing::debug!(agent_id = %_state.agent_id, "Running inference");
    }
}

/// System that handles eviction when context windows fill up.
///
/// Implements the eviction cascade:
/// 1. Temporary regions → evict oldest
/// 2. Compacting regions → summarize
/// 3. SlidingWindow regions → reduce size
/// 4. Pinned regions → never touched
pub fn eviction_system(
    mut query: Query<(&AgentState, &mut ContextWindow)>,
) {
    for (_state, mut _window) in query.iter_mut() {
        // TODO: Implement eviction cascade
        tracing::trace!(agent_id = %_state.agent_id, "Checking for eviction");
    }
}

/// System that manages the agent pool.
///
/// Recycles completed agents, spawns new agents as needed.
pub fn pool_management_system(
    mut commands: Commands,
    query: Query<(Entity, &AgentState)>,
) {
    for (entity, state) in query.iter() {
        match &state.status {
            crate::components::AgentStatus::Complete => {
                tracing::info!(agent_id = %state.agent_id, "Agent completed, recycling");
                // TODO: Recycle or despawn agent
                commands.entity(entity).despawn();
            }
            crate::components::AgentStatus::Error { message } => {
                tracing::error!(agent_id = %state.agent_id, error = %message, "Agent error");
                commands.entity(entity).despawn();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_systems_compile() {
        // Just verify systems compile and have correct signatures
        let _ = context_management_system;
        let _ = inference_system;
        let _ = eviction_system;
        let _ = pool_management_system;
    }
}
