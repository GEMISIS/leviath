//! ECS systems for agent execution.
//!
//! Systems implement agent behaviors:
//! - Context management: eviction, compaction, region updates
//! - Inference: calling LLM providers with context
//! - Tool execution: running tools and updating context with results

use bevy_ecs::prelude::*;
use crate::components::{AgentState, AgentStatus, ContextWindow, TaskAssignment};

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
                max_tokens = window.max_tokens,
                "Context window needs eviction"
            );

            let target_free = window.max_tokens / 10; // Free up 10%
            match window.try_evict(target_free) {
                Ok(freed) => {
                    tracing::info!(
                        agent_id = %_state.agent_id,
                        tokens_freed = freed,
                        "Eviction cascade freed tokens"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %_state.agent_id,
                        error = %e,
                        "Eviction cascade incomplete"
                    );
                }
            }
        }
    }
}

/// System that executes inference for agents with tasks.
///
/// Constructs prompts from context windows and calls LLM providers.
/// Note: Actual provider calls happen asynchronously via the engine's
/// `run_inference` method. This system prepares agents for inference.
pub fn inference_system(
    mut query: Query<(&mut AgentState, &ContextWindow, &TaskAssignment)>,
) {
    for (mut state, window, task) in query.iter_mut() {
        if !matches!(state.status, AgentStatus::Active) {
            continue;
        }

        tracing::debug!(
            agent_id = %state.agent_id,
            stage = %state.current_stage,
            iteration = state.iteration,
            task_id = %task.task_id,
            tokens = window.current_tokens,
            "Agent ready for inference"
        );

        state.iteration += 1;
    }
}

/// System that handles eviction when context windows fill up.
///
/// Implements the eviction cascade:
/// 1. Clearable regions → clear entirely
/// 2. Temporary regions → evict oldest
/// 3. Compacting regions → summarize (needs LLM)
/// 4. SlidingWindow regions → never reduced
/// 5. Pinned regions → never touched
pub fn eviction_system(
    mut query: Query<(&AgentState, &mut ContextWindow)>,
) {
    for (state, mut window) in query.iter_mut() {
        if !window.needs_eviction(0.95) {
            continue;
        }

        tracing::debug!(
            agent_id = %state.agent_id,
            tokens = window.current_tokens,
            max_tokens = window.max_tokens,
            "Running eviction system"
        );

        let target_free = window.max_tokens / 5; // Free up 20%
        match window.try_evict(target_free) {
            Ok(freed) => {
                tracing::info!(
                    agent_id = %state.agent_id,
                    tokens_freed = freed,
                    "Eviction freed tokens"
                );
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %state.agent_id,
                    error = %e,
                    "Eviction failed — may need compaction"
                );
            }
        }
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
