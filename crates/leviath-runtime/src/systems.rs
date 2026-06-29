//! ECS systems for agent execution.
//!
//! Systems implement agent behaviors:
//! - Context management: eviction, compaction, region updates
//! - Inference: calling LLM providers with context
//! - Tool execution: running tools and updating context with results

use crate::components::{
    AgentState, AgentStatus, CancellationToken, ContextWindow, MessageInbox, NeedsCompaction,
    ParentRef, SubAgentChildren, TaskAssignment,
};
use bevy_ecs::prelude::*;
use bevy_ecs::system::ParamSet;

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
pub fn inference_system(mut query: Query<(&mut AgentState, &ContextWindow, &TaskAssignment)>) {
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
pub fn pool_management_system(mut commands: Commands, query: Query<(Entity, &AgentState)>) {
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

/// System that monitors child agent completion and injects results into parent context.
///
/// When a child agent's status becomes Complete or Error, this system:
/// 1. Looks up the parent via the child's ParentRef
/// 2. Injects a completion/error message into the parent's context window
/// 3. Clears the parent's pending_wait if it was waiting for this child
///
/// Uses ParamSet to safely access AgentState from two conflicting queries.
pub fn child_completion_system(
    mut queries: ParamSet<(
        Query<(&AgentState, &ParentRef)>,
        Query<(&mut AgentState, Option<&mut ContextWindow>)>,
    )>,
) {
    // Pass 1: collect completed children info (read-only)
    let mut completions: Vec<(Entity, String, Option<String>)> = Vec::new();

    for (child_state, parent_ref) in queries.p0().iter() {
        match &child_state.status {
            AgentStatus::Complete => {
                completions.push((
                    parent_ref.parent_entity,
                    child_state.agent_id.clone(),
                    None,
                ));
            }
            AgentStatus::Error { message } => {
                completions.push((
                    parent_ref.parent_entity,
                    child_state.agent_id.clone(),
                    Some(message.clone()),
                ));
            }
            _ => {}
        }
    }

    // Pass 2: apply updates to parents (mutable)
    let mut parent_query = queries.p1();
    for (parent_entity, child_id, error_msg) in completions {
        if let Ok((mut parent_state, parent_window)) = parent_query.get_mut(parent_entity) {
            // Skip if already processed
            if !parent_state.spawned_children_ids.contains(&child_id) {
                continue;
            }

            // Clear pending_wait if parent was waiting for this child
            if parent_state.pending_wait.as_deref() == Some(&child_id) {
                parent_state.pending_wait = None;
            }

            // Remove child from tracked list
            parent_state
                .spawned_children_ids
                .retain(|id| id != &child_id);

            tracing::info!(
                parent = %parent_state.agent_id,
                child = %child_id,
                "Child completion injected into parent context"
            );

            // Inject result into parent's context window
            if let Some(mut window) = parent_window {
                let content = if let Some(err) = error_msg {
                    format!("[Child agent '{}' error]: {}", child_id, err)
                } else {
                    format!("[Child agent '{}' completed successfully]", child_id)
                };
                let tokens = content.len() / 4 + 1;
                let _ = window.add_to_region("conversation", content, tokens);
            }
        }
    }
}

/// System that cascades cancellation from parent to all descendants.
///
/// When an agent is Cancelled and has SubAgentChildren, recursively cancel all descendants.
/// Uses ParamSet to avoid query conflicts on AgentState.
pub fn cascade_kill_system(
    mut queries: ParamSet<(
        Query<(&AgentState, &SubAgentChildren)>,
        Query<(&mut AgentState, &CancellationToken)>,
    )>,
) {
    // Pass 1: collect children of cancelled parents
    let mut to_cancel: Vec<Entity> = Vec::new();

    for (state, children) in queries.p0().iter() {
        if matches!(state.status, AgentStatus::Cancelled) {
            for &child in &children.children {
                to_cancel.push(child);
            }
        }
    }

    // Pass 2: cancel each child
    let mut cancel_query = queries.p1();
    for child_entity in to_cancel {
        if let Ok((mut state, token)) = cancel_query.get_mut(child_entity) {
            if !matches!(state.status, AgentStatus::Cancelled) {
                token.cancel();
                state.status = AgentStatus::Cancelled;
                tracing::info!(agent_id = %state.agent_id, "Cascade-cancelled child agent");
            }
        }
    }
}

/// System that gates stage transitions when `requires_children` is set.
///
/// If an agent is Active but has a pending_wait, switch to Waiting.
/// If the agent is Waiting and all children are done, switch back to Active.
pub fn stage_gating_system(mut query: Query<&mut AgentState>) {
    for mut state in query.iter_mut() {
        // If the agent is Active but has pending children, switch to Waiting
        if matches!(state.status, AgentStatus::Active)
            && !state.spawned_children_ids.is_empty()
            && state.pending_wait.is_some()
        {
            state.status = AgentStatus::Waiting;
        }

        // If the agent is Waiting and all children are done, switch back to Active
        if matches!(state.status, AgentStatus::Waiting)
            && state.spawned_children_ids.is_empty()
            && state.pending_wait.is_none()
        {
            state.status = AgentStatus::Active;
            tracing::info!(
                agent_id = %state.agent_id,
                "All children complete, resuming agent"
            );
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
        let _ = child_completion_system;
        let _ = cascade_kill_system;
        let _ = stage_gating_system;
    }
}
