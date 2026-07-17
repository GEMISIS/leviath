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
    if let Some(mut state) = eng.world_mut().get_mut::<AgentState>(child_entity) {
        state.current_stage = entry_stage.to_string();
    }

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
    if let Some(mut state) = eng.world_mut().get_mut::<AgentState>(child_entity) {
        state.status = AgentStatus::Active;
        state.accepts_messages = true;
    }

    (child_agent_id, child_entity)
}
