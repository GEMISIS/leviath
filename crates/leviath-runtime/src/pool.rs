//! Agent pool management.
//!
//! The pool manages a collection of agent instances, recycling completed agents
//! and spawning new ones as needed. Agents are auto-numbered and tracked.

use crate::components::{AgentState, AgentStatus, CancellationToken, ContextWindow, MessageInbox};
use bevy_ecs::prelude::*;
use leviath_core::Blueprint;
use std::collections::HashMap;

/// Manager for a pool of agents.
///
/// Handles spawning, recycling, and tracking agent instances.
pub struct AgentPool {
    /// Blueprint for agents in this pool
    blueprint: Blueprint,

    /// Counter for auto-numbering agents
    next_id: usize,

    /// Active agent entities
    active_agents: HashMap<String, Entity>,
}

impl AgentPool {
    /// Create a new agent pool for the given blueprint.
    pub fn new(blueprint: Blueprint) -> Self {
        Self {
            blueprint,
            next_id: 1,
            active_agents: HashMap::new(),
        }
    }

    /// Spawn a new agent in the pool.
    ///
    /// Creates an entity with all necessary components based on the blueprint.
    pub fn spawn_agent(&mut self, world: &mut World) -> String {
        let agent_id = format!("{}-{}", self.blueprint.name, self.next_id);
        self.next_id += 1;

        // Create context window from blueprint layout
        let context_window = ContextWindow::new(self.blueprint.context_layout.total_budget_tokens);

        // Create agent state
        let agent_state = AgentState {
            agent_id: agent_id.clone(),
            current_stage: self
                .blueprint
                .stages
                .first()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "default".to_string()),
            iteration: 0,
            status: AgentStatus::Idle,
            spawned_children_ids: Vec::new(),
            pending_wait: None,
            accepts_messages: true,
        };

        // Spawn entity with components
        let cancellation_token = CancellationToken::new();
        let message_inbox = MessageInbox::new();
        let entity = world
            .spawn((
                agent_state,
                context_window,
                cancellation_token,
                message_inbox,
            ))
            .id();

        self.active_agents.insert(agent_id.clone(), entity);
        tracing::info!(agent_id = %agent_id, "Spawned new agent");

        agent_id
    }

    /// Get the entity for an agent by ID.
    pub fn get_agent(&self, agent_id: &str) -> Option<Entity> {
        self.active_agents.get(agent_id).copied()
    }

    /// Remove an agent from the pool.
    pub fn remove_agent(&mut self, agent_id: &str) {
        self.active_agents.remove(agent_id);
    }

    /// Get the number of active agents.
    pub fn agent_count(&self) -> usize {
        self.active_agents.len()
    }

    /// Get the blueprint for this pool.
    pub fn blueprint(&self) -> &Blueprint {
        &self.blueprint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::{
        layout::RegionDefinition, region::RegionKind, Blueprint, ContextLayout, Stage,
    };

    fn create_test_blueprint() -> Blueprint {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout = ContextLayout::new(regions, 10000);
        let stages = vec![Stage::new(
            "test".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()),
        )];
        Blueprint::new("test-agent".to_string(), "Test".to_string(), stages, layout)
    }

    #[test]
    fn test_pool_creation() {
        let blueprint = create_test_blueprint();
        let pool = AgentPool::new(blueprint);
        assert_eq!(pool.agent_count(), 0);
    }

    #[test]
    fn test_spawn_agent() {
        let blueprint = create_test_blueprint();
        let mut pool = AgentPool::new(blueprint);
        let mut world = World::new();

        let agent_id = pool.spawn_agent(&mut world);
        assert_eq!(pool.agent_count(), 1);
        assert!(agent_id.starts_with("test-agent-"));
    }

    #[test]
    fn test_get_agent_returns_entity_for_known_id() {
        let blueprint = create_test_blueprint();
        let mut pool = AgentPool::new(blueprint);
        let mut world = World::new();

        let agent_id = pool.spawn_agent(&mut world);
        assert!(pool.get_agent(&agent_id).is_some());
    }

    #[test]
    fn test_get_agent_returns_none_for_unknown_id() {
        let blueprint = create_test_blueprint();
        let pool = AgentPool::new(blueprint);
        assert!(pool.get_agent("nonexistent-agent").is_none());
    }

    #[test]
    fn test_remove_agent_decrements_count() {
        let blueprint = create_test_blueprint();
        let mut pool = AgentPool::new(blueprint);
        let mut world = World::new();

        let agent_id = pool.spawn_agent(&mut world);
        assert_eq!(pool.agent_count(), 1);

        pool.remove_agent(&agent_id);
        assert_eq!(pool.agent_count(), 0);
        assert!(pool.get_agent(&agent_id).is_none());
    }

    #[test]
    fn test_remove_agent_unknown_id_is_noop() {
        let blueprint = create_test_blueprint();
        let mut pool = AgentPool::new(blueprint);
        // Removing an agent that was never spawned must not panic.
        pool.remove_agent("nonexistent-agent");
        assert_eq!(pool.agent_count(), 0);
    }

    #[test]
    fn test_blueprint_accessor_returns_original_blueprint() {
        let blueprint = create_test_blueprint();
        let expected_name = blueprint.name.clone();
        let pool = AgentPool::new(blueprint);
        assert_eq!(pool.blueprint().name, expected_name);
    }
}
