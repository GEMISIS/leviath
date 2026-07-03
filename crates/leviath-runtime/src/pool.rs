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

    /// Minimal `tracing::Subscriber` that reports every level as enabled.
    ///
    /// The multi-line `tracing::debug!`/`info!`/`warn!` calls in this file
    /// (structured fields spread across several lines) internally check
    /// "is this level enabled" *before* evaluating their field expressions.
    /// With no subscriber registered (the default in unit tests), that check
    /// is always false, so the field-expression lines are never executed --
    /// llvm-cov reports them as 0-hit even when the surrounding branch runs.
    /// Single-line tracing calls elsewhere in this file don't show this gap
    /// because the whole call collapses onto one already-covered line.
    /// Running a test under this no-op subscriber makes the level check
    /// pass so the field expressions actually execute, without pulling in
    /// `tracing-subscriber` as a new dependency.
    struct AlwaysOnSubscriber;

    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
        tracing::subscriber::with_default(AlwaysOnSubscriber, f)
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        // The code in this file only ever uses `tracing::info!` event
        // macros, never `tracing::span!` -- so
        // `Subscriber::{new_span,record,record_follows_from,enter,exit}`
        // are never invoked by `with_tracing`'s callers. Exercise them here
        // via a real span (entered twice, to also hit `record_follows_from`
        // through a causal link) so this test doesn't hand-roll low-level
        // `tracing-core` metadata construction.
        with_tracing(|| {
            let span_a = tracing::info_span!("a", value = tracing::field::Empty);
            span_a.record("value", 1);
            let span_b = tracing::info_span!("b");
            span_b.follows_from(&span_a);
            let _enter_a = span_a.enter();
            let _enter_b = span_b.enter();
        });
    }

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
        with_tracing(|| {
            let blueprint = create_test_blueprint();
            let mut pool = AgentPool::new(blueprint);
            let mut world = World::new();

            let agent_id = pool.spawn_agent(&mut world);
            assert_eq!(pool.agent_count(), 1);
            assert!(agent_id.starts_with("test-agent-"));
        });
    }

    #[test]
    fn test_get_agent_returns_entity_for_known_id() {
        with_tracing(|| {
            let blueprint = create_test_blueprint();
            let mut pool = AgentPool::new(blueprint);
            let mut world = World::new();

            let agent_id = pool.spawn_agent(&mut world);
            assert!(pool.get_agent(&agent_id).is_some());
        });
    }

    #[test]
    fn test_get_agent_returns_none_for_unknown_id() {
        let blueprint = create_test_blueprint();
        let pool = AgentPool::new(blueprint);
        assert!(pool.get_agent("nonexistent-agent").is_none());
    }

    #[test]
    fn test_remove_agent_decrements_count() {
        with_tracing(|| {
            let blueprint = create_test_blueprint();
            let mut pool = AgentPool::new(blueprint);
            let mut world = World::new();

            let agent_id = pool.spawn_agent(&mut world);
            assert_eq!(pool.agent_count(), 1);

            pool.remove_agent(&agent_id);
            assert_eq!(pool.agent_count(), 0);
            assert!(pool.get_agent(&agent_id).is_none());
        });
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

    #[test]
    fn test_spawn_agent_with_no_stages_uses_default_stage_name() {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout = ContextLayout::new(regions, 10000);
        // Blueprint with empty stages vec — triggers the `unwrap_or_else(|| "default")` path
        let blueprint = Blueprint::new(
            "no-stages-agent".to_string(),
            "No Stages".to_string(),
            vec![],
            layout,
        );
        let mut pool = AgentPool::new(blueprint);
        let mut world = World::new();

        with_tracing(|| {
            let agent_id = pool.spawn_agent(&mut world);
            let entity = pool.get_agent(&agent_id).unwrap();
            let state = world.get::<AgentState>(entity).unwrap();
            assert_eq!(state.current_stage, "default");
        });
    }
}
