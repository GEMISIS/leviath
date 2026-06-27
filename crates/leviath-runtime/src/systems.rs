//! ECS systems for agent execution.
//!
//! Systems implement agent behaviors:
//! - Context management: eviction, compaction, region updates
//! - Inference: calling LLM providers with context
//! - Tool execution: running tools and updating context with results

use bevy_ecs::prelude::*;
use crate::components::{AgentState, AgentStatus, ContextWindow, MessageInbox, NeedsCompaction, TaskAssignment};

/// System that manages context window state.
///
/// Monitors token usage and triggers eviction when needed.
/// If eviction identifies regions needing LLM compaction, adds a
/// `NeedsCompaction` component so the async engine can handle it.
pub fn context_management_system(
    mut commands: Commands,
    mut query: Query<(Entity, &AgentState, &mut ContextWindow)>,
) {
    for (entity, state, mut window) in query.iter_mut() {
        // Update current token count
        window.current_tokens = window.calculate_tokens();

        // Check if eviction is needed
        if window.needs_eviction(0.9) {
            tracing::debug!(
                agent_id = %state.agent_id,
                tokens = window.current_tokens,
                max_tokens = window.max_tokens,
                "Context window needs eviction"
            );

            let target_free = window.max_tokens / 10; // Free up 10%
            match window.try_evict(target_free) {
                Ok(result) => {
                    if result.tokens_freed > 0 {
                        tracing::info!(
                            agent_id = %state.agent_id,
                            tokens_freed = result.tokens_freed,
                            "Eviction cascade freed tokens"
                        );
                    }
                    if !result.needs_compaction.is_empty() {
                        tracing::info!(
                            agent_id = %state.agent_id,
                            regions = ?result.needs_compaction,
                            "Regions need LLM compaction"
                        );
                        commands.entity(entity).insert(NeedsCompaction {
                            regions: result.needs_compaction,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        agent_id = %state.agent_id,
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
/// 3. Compacting regions → identified for async LLM compaction
/// 4. SlidingWindow regions → never reduced
/// 5. Pinned regions → never touched
///
/// When compacting regions are identified, adds a `NeedsCompaction` component
/// so the async engine or inference loop can perform compaction.
pub fn eviction_system(
    mut commands: Commands,
    mut query: Query<(Entity, &AgentState, &mut ContextWindow)>,
) {
    for (entity, state, mut window) in query.iter_mut() {
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
            Ok(result) => {
                if result.tokens_freed > 0 {
                    tracing::info!(
                        agent_id = %state.agent_id,
                        tokens_freed = result.tokens_freed,
                        "Eviction freed tokens"
                    );
                }
                if !result.needs_compaction.is_empty() {
                    tracing::info!(
                        agent_id = %state.agent_id,
                        regions = ?result.needs_compaction,
                        "Regions need LLM compaction (eviction system)"
                    );
                    commands.entity(entity).insert(NeedsCompaction {
                        regions: result.needs_compaction,
                    });
                }
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %state.agent_id,
                    error = %e,
                    "Eviction failed"
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
            crate::components::AgentStatus::Cancelled => {
                tracing::info!(agent_id = %state.agent_id, "Agent cancelled, recycling");
                commands.entity(entity).despawn();
            }
            _ => {}
        }
    }
}

/// System that delivers messages from agent inboxes to context windows.
///
/// Checks each agent's MessageInbox, adds messages to their context windows,
/// and clears the inbox.
pub fn message_delivery_system(
    mut query: Query<(&AgentState, &mut MessageInbox, &mut ContextWindow)>,
) {
    for (state, mut inbox, mut window) in query.iter_mut() {
        let messages = inbox.drain_all();
        for msg in messages {
            let region_name = msg.target_region.as_deref().unwrap_or("conversation");
            let tokens = msg.content.len() / 4 + 1;
            let formatted = format!("[Message]: {}", msg.content);
            if let Err(e) = window.add_to_region(region_name, formatted, tokens) {
                tracing::warn!(
                    agent_id = %state.agent_id,
                    region = region_name,
                    error = %e,
                    "Failed to deliver message to context window"
                );
            }
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
