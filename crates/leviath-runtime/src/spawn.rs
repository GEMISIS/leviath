//! Spawning first-class child (sub-agent / fan-out worker) entities.

use bevy_ecs::prelude::Entity;
use leviath_core::{Blueprint, Region};

use crate::{
    AgentPool, AgentState, AgentStatus, ContextWindow, EngineHandle, ParentRef, SubAgentChildren,
};

/// Spawn a child agent as a first-class ECS agent parented to `parent_entity`.
///
/// Creates the entity from `pool`, initializes its context regions from
/// `blueprint`, sets its entry stage, links parent↔child (so the dashboard tree
/// shows it), seeds an optional context blob into the first pinned region, and
/// marks it `Active` + message-accepting. Returns `(child_agent_id, entity)`.
///
/// Shared by the `spawn_agent` tool path and fan-out worker spawning
/// (`run_fan_out_stage`).
#[allow(clippy::too_many_arguments)]
pub async fn spawn_child_agent(
    engine: &EngineHandle,
    pool: &mut AgentPool,
    parent_entity: Entity,
    parent_agent_id: &str,
    blueprint: &Blueprint,
    entry_stage: &str,
    depth: usize,
    max_depth: usize,
    seed_context: Option<&str>,
) -> (String, Entity) {
    let child_agent_id = {
        let mut eng = engine.write().await;
        pool.spawn_agent(eng.world_mut())
    };
    // `spawn_agent` just inserted this entry — it is always present.
    let child_entity = pool
        .get_agent(&child_agent_id)
        .expect("just-spawned child must be in pool");

    let mut eng = engine.write().await;

    // Populate the child's context regions from its blueprint (spawn_agent only
    // allocates an empty ContextWindow).
    {
        let mut window = eng
            .world_mut()
            .get_mut::<ContextWindow>(child_entity)
            .expect("spawn_agent always creates ContextWindow");
        for region_def in &blueprint.context_layout.regions {
            window.add_region(Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            ));
        }
        if window.get_region("conversation").is_none() {
            window.add_region(Region::new(
                "conversation".to_string(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                10000,
            ));
        }
    }

    // Enter the worker at the requested stage.
    eng.world_mut()
        .get_mut::<AgentState>(child_entity)
        .expect("spawn_agent always creates AgentState")
        .current_stage = entry_stage.to_string();

    // Link parent ↔ child so the dashboard tree shows the sub-agent.
    eng.world_mut().entity_mut(child_entity).insert(ParentRef {
        parent_entity,
        parent_agent_id: parent_agent_id.to_string(),
        depth,
    });
    if eng.world().get::<SubAgentChildren>(parent_entity).is_some() {
        eng.world_mut()
            .get_mut::<SubAgentChildren>(parent_entity)
            .expect("SubAgentChildren confirmed present")
            .children
            .push(child_entity);
    } else {
        eng.world_mut()
            .entity_mut(parent_entity)
            .insert(SubAgentChildren {
                children: vec![child_entity],
                max_child_depth: max_depth,
            });
    }
    if let Some(mut state) = eng.world_mut().get_mut::<AgentState>(parent_entity) {
        state.spawned_children_ids.push(child_agent_id.clone());
    }

    // Seed the work item / task into the first pinned region.
    if let Some(seed) = seed_context {
        let mut window = eng
            .world_mut()
            .get_mut::<ContextWindow>(child_entity)
            .expect("spawn_agent always creates ContextWindow");
        let tokens = seed.len() / 4 + 1;
        if let Some(pinned_name) = window
            .regions
            .iter()
            .find(|r| r.kind == leviath_core::RegionKind::Pinned)
            .map(|r| r.name.clone())
        {
            let _ = window.add_to_region(&pinned_name, seed.to_string(), tokens);
        }
    }

    // Activate and allow mid-run messages (interruptible like any agent).
    let mut state = eng
        .world_mut()
        .get_mut::<AgentState>(child_entity)
        .expect("spawn_agent always creates AgentState");
    state.status = AgentStatus::Active;
    state.accepts_messages = true;

    (child_agent_id, child_entity)
}

#[cfg(test)]
mod tests {
    use super::spawn_child_agent;
    use crate::{
        AgentEngine, AgentPool, AgentState, AgentStatus, ContextWindow, EngineHandle, ParentRef,
        ProviderRegistry, SubAgentChildren,
    };
    use leviath_core::{
        Blueprint, ContextLayout, RegionKind, Stage, blueprint::ModelConfig,
        layout::RegionDefinition,
    };
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn blueprint_with(regions: Vec<RegionDefinition>) -> Blueprint {
        let layout = ContextLayout::new(regions, 100_000);
        let stages = vec![Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()),
        )];
        Blueprint::new("bp".to_string(), "desc".to_string(), stages, layout)
    }

    /// Build an engine handle with a registered root/parent agent.
    fn engine_with_root() -> (EngineHandle, bevy_ecs::prelude::Entity, String) {
        let mut engine = AgentEngine::with_providers(ProviderRegistry::new());
        let mut root_pool = AgentPool::new(blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]));
        let root_id = root_pool.spawn_agent(engine.world_mut());
        let root_entity = root_pool.get_agent(&root_id).unwrap();
        let handle: EngineHandle = Arc::new(RwLock::new(engine));
        (handle, root_entity, root_id)
    }

    #[tokio::test]
    async fn spawn_child_links_parent_seeds_pinned_and_adds_default_conversation() {
        let (engine, root_entity, root_id) = engine_with_root();

        // Blueprint has a pinned "system" region but NO conversation region, so
        // the default conversation region is auto-added.
        let worker_bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let mut worker_pool = AgentPool::new(worker_bp.clone());

        let (child_id, child_entity) = spawn_child_agent(
            &engine,
            &mut worker_pool,
            root_entity,
            &root_id,
            &worker_bp,
            "main",
            1,
            3,
            Some("work item context"),
        )
        .await;

        let eng = engine.read().await;
        let parent_ref = eng.world().get::<ParentRef>(child_entity).unwrap();
        assert_eq!(parent_ref.parent_agent_id, root_id);
        assert_eq!(parent_ref.depth, 1);

        let child_state = eng.world().get::<AgentState>(child_entity).unwrap();
        assert_eq!(child_state.current_stage, "main");
        assert_eq!(child_state.status, AgentStatus::Active);
        assert!(child_state.accepts_messages);

        // Parent tracks the child (SubAgentChildren was absent → inserted).
        let children = eng.world().get::<SubAgentChildren>(root_entity).unwrap();
        assert!(children.children.contains(&child_entity));
        assert_eq!(children.max_child_depth, 3);
        assert!(
            eng.world()
                .get::<AgentState>(root_entity)
                .unwrap()
                .spawned_children_ids
                .contains(&child_id)
        );

        // Seed landed in the pinned region.
        let window = eng.world().get::<ContextWindow>(child_entity).unwrap();
        let sys = window.get_region("system").unwrap();
        assert!(
            sys.content
                .iter()
                .any(|e| e.content.contains("work item context"))
        );
        // Default conversation region was auto-added.
        assert!(window.get_region("conversation").is_some());
    }

    #[tokio::test]
    async fn spawn_second_child_appends_to_existing_children_without_seed() {
        let (engine, root_entity, root_id) = engine_with_root();

        // Worker blueprint already declares a conversation region, so the
        // default-conversation branch is skipped.
        let worker_bp = blueprint_with(vec![
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                10_000,
            ),
        ]);
        let mut worker_pool = AgentPool::new(worker_bp.clone());

        // First child: SubAgentChildren absent → insert.
        let (_c1, e1) = spawn_child_agent(
            &engine,
            &mut worker_pool,
            root_entity,
            &root_id,
            &worker_bp,
            "main",
            1,
            3,
            None,
        )
        .await;
        // Second child on the same parent: SubAgentChildren present → push.
        let (_c2, e2) = spawn_child_agent(
            &engine,
            &mut worker_pool,
            root_entity,
            &root_id,
            &worker_bp,
            "main",
            1,
            3,
            None,
        )
        .await;

        let eng = engine.read().await;
        let children = eng.world().get::<SubAgentChildren>(root_entity).unwrap();
        assert!(children.children.contains(&e1));
        assert!(children.children.contains(&e2));
        assert_eq!(children.children.len(), 2);

        // With no seed, the pinned region stays empty.
        let window = eng.world().get::<ContextWindow>(e1).unwrap();
        assert!(window.get_region("system").unwrap().content.is_empty());
    }

    #[tokio::test]
    async fn spawn_child_with_seed_but_no_pinned_region_skips_seeding() {
        let (engine, root_entity, root_id) = engine_with_root();

        // No pinned region → the seed has nowhere to go and is skipped.
        let worker_bp = blueprint_with(vec![RegionDefinition::new(
            "scratch".to_string(),
            RegionKind::Temporary,
            5000,
        )]);
        let mut worker_pool = AgentPool::new(worker_bp.clone());

        let (_child_id, child_entity) = spawn_child_agent(
            &engine,
            &mut worker_pool,
            root_entity,
            &root_id,
            &worker_bp,
            "main",
            1,
            3,
            Some("orphaned seed"),
        )
        .await;

        let eng = engine.read().await;
        let window = eng.world().get::<ContextWindow>(child_entity).unwrap();
        // No pinned region exists, so the seed is dropped: the only content
        // region stays empty.
        let scratch = window.get_region("scratch").unwrap();
        assert!(scratch.content.is_empty());
    }

    #[tokio::test]
    async fn spawn_child_onto_parent_without_agent_state_skips_child_tracking() {
        // A bare parent entity (no AgentState component) makes the
        // `if let Some(mut state) = get_mut::<AgentState>(parent_entity)` guard
        // fall through — the child is still parented and SubAgentChildren is
        // still created, but the parent records no spawned-child id.
        let engine: EngineHandle = Arc::new(RwLock::new(AgentEngine::with_providers(
            ProviderRegistry::new(),
        )));
        let bare_parent = {
            let mut eng = engine.write().await;
            eng.world_mut().spawn_empty().id()
        };

        let worker_bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let mut worker_pool = AgentPool::new(worker_bp.clone());

        let (_child_id, child_entity) = spawn_child_agent(
            &engine,
            &mut worker_pool,
            bare_parent,
            "bare-parent",
            &worker_bp,
            "main",
            2,
            5,
            None,
        )
        .await;

        let eng = engine.read().await;
        // Child is parented and the parent's SubAgentChildren was created.
        assert_eq!(eng.world().get::<ParentRef>(child_entity).unwrap().depth, 2);
        let children = eng.world().get::<SubAgentChildren>(bare_parent).unwrap();
        assert!(children.children.contains(&child_entity));
        // The parent has no AgentState, so nothing tracks the child id there.
        assert!(eng.world().get::<AgentState>(bare_parent).is_none());
    }
}
