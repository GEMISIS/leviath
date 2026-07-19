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
#[allow(clippy::type_complexity)]
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
                completions.push((parent_ref.parent_entity, child_state.agent_id.clone(), None));
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
#[allow(clippy::type_complexity)]
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
        if let Ok((mut state, token)) = cancel_query.get_mut(child_entity)
            && !matches!(state.status, AgentStatus::Cancelled)
        {
            token.cancel();
            state.status = AgentStatus::Cancelled;
            tracing::info!(agent_id = %state.agent_id, "Cascade-cancelled child agent");
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
    use crate::AgentMessage;
    use crate::test_support::with_tracing;
    use leviath_core::{EvictionStrategy, Region, RegionKind};

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

    // ── Helper to create an AgentState ─────────────────────────────────────

    fn make_agent_state(id: &str, status: AgentStatus) -> AgentState {
        AgentState {
            agent_id: id.to_string(),
            current_stage: "main".to_string(),
            iteration: 0,
            status,
            spawned_children_ids: Vec::new(),
            pending_wait: None,
            accepts_messages: true,
        }
    }

    // ── pool_management_system ─────────────────────────────────────────────

    #[test]
    fn pool_management_despawns_completed_agents() {
        let mut world = World::new();
        let entity = world
            .spawn(make_agent_state("agent-1", AgentStatus::Complete))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(pool_management_system);
        with_tracing(|| schedule.run(&mut world));

        assert!(world.get_entity(entity).is_err());
    }

    #[test]
    fn pool_management_despawns_error_agents() {
        let mut world = World::new();
        let entity = world
            .spawn(make_agent_state(
                "agent-err",
                AgentStatus::Error {
                    message: "boom".to_string(),
                },
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(pool_management_system);
        with_tracing(|| schedule.run(&mut world));

        assert!(world.get_entity(entity).is_err());
    }

    #[test]
    fn pool_management_despawns_cancelled_agents() {
        let mut world = World::new();
        let entity = world
            .spawn(make_agent_state("agent-cancel", AgentStatus::Cancelled))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(pool_management_system);
        with_tracing(|| schedule.run(&mut world));

        assert!(world.get_entity(entity).is_err());
    }

    #[test]
    fn pool_management_keeps_active_agents() {
        let mut world = World::new();
        let entity = world
            .spawn(make_agent_state("agent-active", AgentStatus::Active))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(pool_management_system);
        schedule.run(&mut world);

        assert!(world.get_entity(entity).is_ok());
    }

    #[test]
    fn pool_management_keeps_waiting_agents() {
        let mut world = World::new();
        let entity = world
            .spawn(make_agent_state("agent-wait", AgentStatus::Waiting))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(pool_management_system);
        schedule.run(&mut world);

        assert!(world.get_entity(entity).is_ok());
    }

    #[test]
    fn pool_management_keeps_idle_agents() {
        let mut world = World::new();
        let entity = world
            .spawn(make_agent_state("agent-idle", AgentStatus::Idle))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(pool_management_system);
        schedule.run(&mut world);

        assert!(world.get_entity(entity).is_ok());
    }

    // ── inference_system ───────────────────────────────────────────────────

    #[test]
    fn inference_system_increments_iteration_for_active_agent() {
        let mut world = World::new();
        let entity = world
            .spawn((
                make_agent_state("agent-1", AgentStatus::Active),
                ContextWindow::new(10000),
                TaskAssignment {
                    task_id: "task-1".to_string(),
                    prompt: "Do something".to_string(),
                    priority: 1,
                    assigned_at: 0,
                },
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(inference_system);
        with_tracing(|| schedule.run(&mut world));

        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.iteration, 1);
    }

    #[test]
    fn inference_system_skips_idle_agents() {
        let mut world = World::new();
        let entity = world
            .spawn((
                make_agent_state("agent-idle", AgentStatus::Idle),
                ContextWindow::new(10000),
                TaskAssignment {
                    task_id: "task-1".to_string(),
                    prompt: "Do something".to_string(),
                    priority: 1,
                    assigned_at: 0,
                },
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(inference_system);
        schedule.run(&mut world);

        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.iteration, 0);
    }

    #[test]
    fn inference_system_skips_waiting_agents() {
        let mut world = World::new();
        let entity = world
            .spawn((
                make_agent_state("agent-wait", AgentStatus::Waiting),
                ContextWindow::new(10000),
                TaskAssignment {
                    task_id: "task-1".to_string(),
                    prompt: "Do something".to_string(),
                    priority: 1,
                    assigned_at: 0,
                },
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(inference_system);
        schedule.run(&mut world);

        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.iteration, 0);
    }

    // ── context_management_system ──────────────────────────────────────────

    #[test]
    fn context_management_updates_current_tokens() {
        let mut world = World::new();
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("scratch".to_string(), RegionKind::Clearable, 5000);
        region.add_entry("data".to_string(), 500).unwrap();
        window.add_region(region);

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(context_management_system);
        schedule.run(&mut world);

        let window = world.get::<ContextWindow>(entity).unwrap();
        assert_eq!(window.current_tokens, 500);
    }

    #[test]
    fn context_management_triggers_eviction_when_over_threshold() {
        let mut world = World::new();
        let mut window = ContextWindow::new(1000);
        let mut region = Region::new("scratch".to_string(), RegionKind::Clearable, 1000);
        region.add_entry("data".to_string(), 950).unwrap();
        window.add_region(region);
        // Set current_tokens to over 90%
        window.current_tokens = 950;

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(context_management_system);
        with_tracing(|| schedule.run(&mut world));

        let window = world.get::<ContextWindow>(entity).unwrap();
        // After eviction, clearable region should be cleared
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn context_management_adds_needs_compaction_component() {
        let mut world = World::new();
        let mut window = ContextWindow::new(1000);
        // Only compacting region, no clearable/temporary
        let mut region = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 500,
            },
            1000,
        );
        region.add_entry("data".to_string(), 920).unwrap();
        window.add_region(region);
        window.current_tokens = 920;

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(context_management_system);
        with_tracing(|| schedule.run(&mut world));

        // Should have NeedsCompaction component
        let compaction = world.get::<NeedsCompaction>(entity);
        assert!(compaction.is_some());
        assert!(compaction.unwrap().regions.contains(&"impl".to_string()));
    }

    // ── eviction_system ────────────────────────────────────────────────────

    #[test]
    fn eviction_system_triggers_at_95_percent() {
        let mut world = World::new();
        let mut window = ContextWindow::new(1000);
        let mut region = Region::new("temp".to_string(), RegionKind::Temporary, 1000);
        region.add_entry("old data".to_string(), 960).unwrap();
        window.add_region(region);
        window.current_tokens = 960;

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(eviction_system);
        with_tracing(|| schedule.run(&mut world));

        let window = world.get::<ContextWindow>(entity).unwrap();
        // After eviction, temporary entry should be removed
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn eviction_system_no_trigger_below_threshold() {
        let mut world = World::new();
        let mut window = ContextWindow::new(1000);
        let mut region = Region::new("temp".to_string(), RegionKind::Temporary, 1000);
        region.add_entry("data".to_string(), 500).unwrap();
        window.add_region(region);
        window.current_tokens = 500;

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(eviction_system);
        schedule.run(&mut world);

        let window = world.get::<ContextWindow>(entity).unwrap();
        assert_eq!(window.current_tokens, 500);
    }

    #[test]
    fn eviction_system_adds_needs_compaction_component() {
        let mut world = World::new();
        let mut window = ContextWindow::new(1000);
        // Only a Compacting region — nothing clearable/temporary to evict,
        // so try_evict should surface it as needing LLM compaction.
        let mut region = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 500,
            },
            1000,
        );
        region.add_entry("data".to_string(), 960).unwrap();
        window.add_region(region);
        window.current_tokens = 960;

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(eviction_system);
        with_tracing(|| schedule.run(&mut world));

        let compaction = world.get::<NeedsCompaction>(entity);
        assert!(compaction.is_some());
        assert!(compaction.unwrap().regions.contains(&"impl".to_string()));
    }

    // ── context_management_system / eviction_system: try_evict error path ──
    // A Pinned region whose tokens alone exceed max_tokens makes try_evict()
    // return Err(PinnedRegionsOverBudget) — both systems must log and
    // continue rather than propagate/panic.

    #[test]
    fn context_management_system_handles_eviction_error_without_panicking() {
        let mut world = World::new();
        let mut window = ContextWindow::new(1000);
        let mut region = Region::new("architecture".to_string(), RegionKind::Pinned, 2000);
        region
            .add_entry("huge pinned doc".to_string(), 1500)
            .unwrap();
        window.add_region(region);
        window.current_tokens = 1500;

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(context_management_system);
        with_tracing(|| schedule.run(&mut world));

        // Pinned regions are never touched, even on error — nothing evicted.
        let window = world.get::<ContextWindow>(entity).unwrap();
        assert_eq!(window.current_tokens, 1500);
        assert!(world.get::<NeedsCompaction>(entity).is_none());
    }

    #[test]
    fn eviction_system_handles_eviction_error_without_panicking() {
        let mut world = World::new();
        let mut window = ContextWindow::new(1000);
        let mut region = Region::new("architecture".to_string(), RegionKind::Pinned, 2000);
        region
            .add_entry("huge pinned doc".to_string(), 1500)
            .unwrap();
        window.add_region(region);
        window.current_tokens = 1500;

        let entity = world
            .spawn((make_agent_state("agent-1", AgentStatus::Active), window))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(eviction_system);
        with_tracing(|| schedule.run(&mut world));

        let window = world.get::<ContextWindow>(entity).unwrap();
        assert_eq!(window.current_tokens, 1500);
        assert!(world.get::<NeedsCompaction>(entity).is_none());
    }

    // ── stage_gating_system ────────────────────────────────────────────────

    #[test]
    fn stage_gating_active_with_pending_wait_switches_to_waiting() {
        let mut world = World::new();
        let mut state = make_agent_state("agent-1", AgentStatus::Active);
        state.spawned_children_ids = vec!["child-1".to_string()];
        state.pending_wait = Some("child-1".to_string());

        let entity = world.spawn(state).id();

        let mut schedule = Schedule::default();
        schedule.add_systems(stage_gating_system);
        schedule.run(&mut world);

        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.status, AgentStatus::Waiting);
    }

    #[test]
    fn stage_gating_waiting_with_no_children_switches_to_active() {
        let mut world = World::new();
        let mut state = make_agent_state("agent-1", AgentStatus::Waiting);
        state.spawned_children_ids = Vec::new();
        state.pending_wait = None;

        let entity = world.spawn(state).id();

        let mut schedule = Schedule::default();
        schedule.add_systems(stage_gating_system);
        with_tracing(|| schedule.run(&mut world));

        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.status, AgentStatus::Active);
    }

    #[test]
    fn stage_gating_waiting_with_children_stays_waiting() {
        let mut world = World::new();
        let mut state = make_agent_state("agent-1", AgentStatus::Waiting);
        state.spawned_children_ids = vec!["child-1".to_string()];
        state.pending_wait = Some("child-1".to_string());

        let entity = world.spawn(state).id();

        let mut schedule = Schedule::default();
        schedule.add_systems(stage_gating_system);
        schedule.run(&mut world);

        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.status, AgentStatus::Waiting);
    }

    #[test]
    fn stage_gating_active_without_pending_stays_active() {
        let mut world = World::new();
        let state = make_agent_state("agent-1", AgentStatus::Active);

        let entity = world.spawn(state).id();

        let mut schedule = Schedule::default();
        schedule.add_systems(stage_gating_system);
        schedule.run(&mut world);

        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.status, AgentStatus::Active);
    }

    // ── cascade_kill_system ────────────────────────────────────────────────

    #[test]
    fn cascade_kill_cancels_children_of_cancelled_parent() {
        let mut world = World::new();

        let child_entity = world
            .spawn((
                make_agent_state("child-1", AgentStatus::Active),
                CancellationToken::new(),
            ))
            .id();

        world.spawn((
            make_agent_state("parent", AgentStatus::Cancelled),
            SubAgentChildren {
                children: vec![child_entity],
                max_child_depth: 3,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(cascade_kill_system);
        with_tracing(|| schedule.run(&mut world));

        let child_state = world.get::<AgentState>(child_entity).unwrap();
        assert_eq!(child_state.status, AgentStatus::Cancelled);

        let token = world.get::<CancellationToken>(child_entity).unwrap();
        assert!(token.is_cancelled());
    }

    #[test]
    fn cascade_kill_does_not_cancel_children_of_active_parent() {
        let mut world = World::new();

        let child_entity = world
            .spawn((
                make_agent_state("child-1", AgentStatus::Active),
                CancellationToken::new(),
            ))
            .id();

        world.spawn((
            make_agent_state("parent", AgentStatus::Active),
            SubAgentChildren {
                children: vec![child_entity],
                max_child_depth: 3,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(cascade_kill_system);
        schedule.run(&mut world);

        let child_state = world.get::<AgentState>(child_entity).unwrap();
        assert_eq!(child_state.status, AgentStatus::Active);
    }

    #[test]
    fn cascade_kill_skips_already_cancelled_children() {
        let mut world = World::new();

        let child_entity = world
            .spawn((
                make_agent_state("child-1", AgentStatus::Cancelled),
                CancellationToken::new(),
            ))
            .id();

        world.spawn((
            make_agent_state("parent", AgentStatus::Cancelled),
            SubAgentChildren {
                children: vec![child_entity],
                max_child_depth: 3,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(cascade_kill_system);
        schedule.run(&mut world);

        // Should still be cancelled but not error
        let child_state = world.get::<AgentState>(child_entity).unwrap();
        assert_eq!(child_state.status, AgentStatus::Cancelled);
    }

    // ── child_completion_system ────────────────────────────────────────────

    #[test]
    fn child_completion_injects_success_into_parent_context() {
        let mut world = World::new();

        let mut parent_state = make_agent_state("parent", AgentStatus::Active);
        parent_state.spawned_children_ids = vec!["child-1".to_string()];
        parent_state.pending_wait = Some("child-1".to_string());

        let mut parent_window = ContextWindow::new(10000);
        let conv = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        );
        parent_window.add_region(conv);

        let parent_entity = world.spawn((parent_state, parent_window)).id();

        world.spawn((
            make_agent_state("child-1", AgentStatus::Complete),
            ParentRef {
                parent_entity,
                parent_agent_id: "parent".to_string(),
                depth: 1,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(child_completion_system);
        with_tracing(|| schedule.run(&mut world));

        let parent_state = world.get::<AgentState>(parent_entity).unwrap();
        assert!(parent_state.pending_wait.is_none());
        assert!(parent_state.spawned_children_ids.is_empty());

        let parent_window = world.get::<ContextWindow>(parent_entity).unwrap();
        let conv = parent_window.get_region("conversation").unwrap();
        let has_completion_msg = conv
            .content
            .iter()
            .any(|e| e.content.contains("completed successfully"));
        assert!(has_completion_msg);
    }

    #[test]
    fn child_completion_injects_error_into_parent_context() {
        let mut world = World::new();

        let mut parent_state = make_agent_state("parent", AgentStatus::Active);
        parent_state.spawned_children_ids = vec!["child-err".to_string()];

        let mut parent_window = ContextWindow::new(10000);
        let conv = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        );
        parent_window.add_region(conv);

        let parent_entity = world.spawn((parent_state, parent_window)).id();

        let mut child_state = make_agent_state(
            "child-err",
            AgentStatus::Error {
                message: "something went wrong".to_string(),
            },
        );
        child_state.agent_id = "child-err".to_string();

        world.spawn((
            child_state,
            ParentRef {
                parent_entity,
                parent_agent_id: "parent".to_string(),
                depth: 1,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(child_completion_system);
        schedule.run(&mut world);

        let parent_window = world.get::<ContextWindow>(parent_entity).unwrap();
        let conv = parent_window.get_region("conversation").unwrap();
        assert!(
            conv.content
                .iter()
                .any(|e| e.content.contains("error") && e.content.contains("something went wrong"))
        );
    }

    #[test]
    fn child_completion_skips_untracked_children() {
        let mut world = World::new();

        let parent_state = make_agent_state("parent", AgentStatus::Active);
        // Note: spawned_children_ids is empty — child is not tracked

        let parent_entity = world.spawn(parent_state).id();

        world.spawn((
            make_agent_state("child-unknown", AgentStatus::Complete),
            ParentRef {
                parent_entity,
                parent_agent_id: "parent".to_string(),
                depth: 1,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(child_completion_system);
        schedule.run(&mut world);

        // Should not panic or error
        let parent_state = world.get::<AgentState>(parent_entity).unwrap();
        assert!(parent_state.spawned_children_ids.is_empty());
    }

    #[test]
    fn child_completion_ignores_non_terminal_child_status() {
        let mut world = World::new();

        let mut parent_state = make_agent_state("parent", AgentStatus::Active);
        parent_state.spawned_children_ids = vec!["child-active".to_string()];
        let parent_entity = world.spawn(parent_state).id();

        // Child is still Active — not Complete or Error — so it must be
        // ignored entirely (the `_ => {}` catch-all arm).
        world.spawn((
            make_agent_state("child-active", AgentStatus::Active),
            ParentRef {
                parent_entity,
                parent_agent_id: "parent".to_string(),
                depth: 1,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(child_completion_system);
        schedule.run(&mut world);

        let parent_state = world.get::<AgentState>(parent_entity).unwrap();
        assert_eq!(parent_state.spawned_children_ids, vec!["child-active"]);
    }

    // ── message_delivery_system ────────────────────────────────────────────

    #[test]
    fn message_delivery_adds_messages_to_context() {
        let mut world = World::new();

        let mut window = ContextWindow::new(10000);
        let conv = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        );
        window.add_region(conv);

        let mut inbox = MessageInbox::new();
        inbox.push(AgentMessage {
            agent_id: "agent-1".to_string(),
            content: "Hello from user".to_string(),
            target_region: Some("conversation".to_string()),
            priority: 0,
        });

        let entity = world
            .spawn((
                make_agent_state("agent-1", AgentStatus::Active),
                inbox,
                window,
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(message_delivery_system);
        schedule.run(&mut world);

        let inbox = world.get::<MessageInbox>(entity).unwrap();
        assert!(inbox.messages.is_empty());

        let window = world.get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(
            conv.content
                .iter()
                .any(|e| e.content.contains("Hello from user"))
        );
    }

    #[test]
    fn message_delivery_logs_and_continues_when_target_region_missing() {
        let mut world = World::new();

        // No regions added at all — "conversation" (the default target) does
        // not exist, so add_to_region() must return Err(RegionNotFound).
        let window = ContextWindow::new(10000);

        let mut inbox = MessageInbox::new();
        inbox.push(AgentMessage {
            agent_id: "agent-1".to_string(),
            content: "Hello".to_string(),
            target_region: Some("nonexistent".to_string()),
            priority: 0,
        });

        let entity = world
            .spawn((
                make_agent_state("agent-1", AgentStatus::Active),
                inbox,
                window,
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(message_delivery_system);
        // Must not panic even though the target region doesn't exist.
        with_tracing(|| schedule.run(&mut world));

        let inbox = world.get::<MessageInbox>(entity).unwrap();
        assert!(inbox.messages.is_empty());
    }

    #[test]
    fn message_delivery_defaults_to_conversation_region() {
        let mut world = World::new();

        let mut window = ContextWindow::new(10000);
        let conv = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        );
        window.add_region(conv);

        let mut inbox = MessageInbox::new();
        inbox.push(AgentMessage {
            agent_id: "agent-1".to_string(),
            content: "Test message".to_string(),
            target_region: None, // Should default to "conversation"
            priority: 0,
        });

        let entity = world
            .spawn((
                make_agent_state("agent-1", AgentStatus::Active),
                inbox,
                window,
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(message_delivery_system);
        schedule.run(&mut world);

        let window = world.get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(
            conv.content
                .iter()
                .any(|e| e.content.contains("Test message"))
        );
    }

    // ─── child_completion with despawned parent entity ────────────────────

    #[test]
    fn child_completion_skips_when_parent_entity_no_longer_exists() {
        let mut world = World::new();

        // Create a phantom parent entity ID and immediately despawn it
        let phantom_parent = world.spawn_empty().id();
        world.despawn(phantom_parent);

        // Spawn a child that references the despawned parent
        world.spawn((
            make_agent_state("child-orphan", AgentStatus::Complete),
            ParentRef {
                parent_entity: phantom_parent,
                parent_agent_id: "ghost-parent".to_string(),
                depth: 1,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(child_completion_system);
        // Must not panic when parent entity is gone
        schedule.run(&mut world);
    }

    // ─── child_completion with parent having no ContextWindow ─────────────

    #[test]
    fn child_completion_skips_context_injection_when_parent_has_no_window() {
        let mut world = World::new();

        let mut parent_state = make_agent_state("parent", AgentStatus::Active);
        parent_state.spawned_children_ids = vec!["child-1".to_string()];
        parent_state.pending_wait = Some("child-1".to_string());

        // Spawn parent WITHOUT a ContextWindow
        let parent_entity = world.spawn(parent_state).id();

        // Spawn child with Complete status and a ParentRef pointing at the parent
        world.spawn((
            make_agent_state("child-1", AgentStatus::Complete),
            ParentRef {
                parent_entity,
                parent_agent_id: "parent".to_string(),
                depth: 1,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(child_completion_system);
        with_tracing(|| schedule.run(&mut world));

        // Parent pending_wait should be cleared (state updates still happen)
        let state = world.get::<AgentState>(parent_entity).unwrap();
        assert!(state.pending_wait.is_none());
    }

    // ─── cascade_kill with non-existent child entity ──────────────────────

    #[test]
    fn cascade_kill_skips_nonexistent_child_entity() {
        let mut world = World::new();

        // Spawn a parent that references a child entity that was never spawned
        let phantom_child = world.spawn_empty().id();
        world.despawn(phantom_child); // now it doesn't exist

        world.spawn((
            make_agent_state("parent", AgentStatus::Cancelled),
            SubAgentChildren {
                children: vec![phantom_child],
                max_child_depth: 3,
            },
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(cascade_kill_system);
        // Must not panic when the child entity is missing
        schedule.run(&mut world);
    }
}
