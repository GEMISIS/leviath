//! Engine context-window setup helpers shared by the stage engine.
//!
//! These are pure operations over an [`AgentEngine`]'s [`ContextWindow`] driven
//! by a blueprint/layout, so they live in the runtime (the CLI re-exports them
//! from `commands::run::helpers` for existing call sites).

use leviath_core::{Blueprint, ContextLayout, EvictionStrategy, Region, RegionKind};

use crate::{AgentEngine, ContextWindow};

/// Initialize context window regions on an entity from the blueprint.
pub fn initialize_context_window(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    blueprint: &Blueprint,
    task: &str,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        for region_def in &blueprint.context_layout.regions {
            let region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );
            window.add_region(region);
        }

        if window.get_region("tool_results").is_none() {
            let tool_region = Region::new("tool_results".to_string(), RegionKind::Temporary, 5000);
            window.add_region(tool_region);
        }

        if window.get_region("conversation").is_none() {
            let conv_region = Region::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10000,
            );
            window.add_region(conv_region);
        }

        // Seed the task text into a pinned region.
        // Prefer a region explicitly named "task"; fall back to the first pinned region.
        let task_region_name = blueprint
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "task" && matches!(r.kind, RegionKind::Pinned))
            .or_else(|| {
                blueprint
                    .context_layout
                    .regions
                    .iter()
                    .find(|r| matches!(r.kind, RegionKind::Pinned))
            })
            .map(|r| r.name.clone());

        if let Some(region_name) = task_region_name {
            let task_tokens = task.len() / 4 + 1;
            let _ = window.add_to_region(&region_name, task.to_string(), task_tokens);
        }
    }
}

/// Swap a [`ContextWindow`] to a stage-specific layout in place, preserving each
/// carried-over region's existing content by name. Pure over the window (no
/// engine/entity), so both the imperative engine and the ECS pipeline's
/// stage-entry can share it.
pub fn apply_layout(window: &mut ContextWindow, layout: &ContextLayout) {
    let mut new_regions = Vec::new();
    for region_def in &layout.regions {
        let mut new_region = Region::new(
            region_def.name.clone(),
            region_def.kind.clone(),
            region_def.max_tokens,
        );

        if let Some(existing) = window.get_region(&region_def.name) {
            for entry in &existing.content {
                let _ = new_region.add_entry(entry.content.clone(), entry.tokens);
            }
        }

        new_regions.push(new_region);
    }

    window.regions = new_regions;
    window.current_tokens = window.calculate_tokens();
}

/// Swap context layout to a stage-specific layout (preserving existing content where possible).
pub fn swap_context_layout(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    layout: &ContextLayout,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        apply_layout(&mut window, layout);
    }
}

#[cfg(test)]
mod tests {
    use super::{initialize_context_window, swap_context_layout};
    use crate::{AgentEngine, AgentPool, ContextWindow, ProviderRegistry};
    use bevy_ecs::prelude::Entity;
    use leviath_core::{
        Blueprint, ContextLayout, EvictionStrategy, RegionKind, Stage, blueprint::ModelConfig,
        layout::RegionDefinition,
    };

    fn blueprint_with(regions: Vec<RegionDefinition>) -> Blueprint {
        let layout = ContextLayout::new(regions, 100_000);
        let stages = vec![Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()),
        )];
        Blueprint::new("bp".to_string(), "desc".to_string(), stages, layout)
    }

    fn engine_and_entity() -> (AgentEngine, Entity) {
        let mut engine = AgentEngine::with_providers(ProviderRegistry::new());
        let mut pool = AgentPool::new(blueprint_with(vec![]));
        let id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&id).unwrap();
        (engine, entity)
    }

    #[test]
    fn initialize_prefers_named_task_region_and_keeps_existing_infra_regions() {
        let (mut engine, entity) = engine_and_entity();
        let bp = blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("tool_results".to_string(), RegionKind::Temporary, 5000),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10_000,
            ),
        ]);

        initialize_context_window(&mut engine, entity, &bp, "do the thing");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        // Task seeded into the explicitly-named "task" pinned region.
        assert!(
            window
                .get_region("task")
                .unwrap()
                .content
                .iter()
                .any(|e| e.content.contains("do the thing"))
        );
        // Blueprint-declared tool_results / conversation are not duplicated.
        assert_eq!(
            window
                .regions
                .iter()
                .filter(|r| r.name == "tool_results")
                .count(),
            1
        );
        assert_eq!(
            window
                .regions
                .iter()
                .filter(|r| r.name == "conversation")
                .count(),
            1
        );
    }

    #[test]
    fn initialize_adds_infra_regions_and_falls_back_to_first_pinned() {
        let (mut engine, entity) = engine_and_entity();
        // Only a pinned "system" region (not named "task"): task falls back to
        // it, and tool_results + conversation are auto-added.
        let bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);

        initialize_context_window(&mut engine, entity, &bp, "seed task");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        assert!(window.get_region("tool_results").is_some());
        assert!(window.get_region("conversation").is_some());
        assert!(
            window
                .get_region("system")
                .unwrap()
                .content
                .iter()
                .any(|e| e.content.contains("seed task"))
        );
    }

    #[test]
    fn initialize_without_pinned_region_does_not_seed_task() {
        let (mut engine, entity) = engine_and_entity();
        let bp = blueprint_with(vec![RegionDefinition::new(
            "scratch".to_string(),
            RegionKind::Temporary,
            5000,
        )]);

        initialize_context_window(&mut engine, entity, &bp, "unseeded task");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        // No pinned region → task text is seeded nowhere; the sole declared
        // region stays empty.
        assert!(window.get_region("scratch").unwrap().content.is_empty());
        // Infra regions still added.
        assert!(window.get_region("tool_results").is_some());
        assert!(window.get_region("conversation").is_some());
    }

    #[test]
    fn initialize_task_named_region_that_is_not_pinned_falls_back_to_first_pinned() {
        let (mut engine, entity) = engine_and_entity();
        // A region literally named "task" but NOT pinned must be rejected by
        // the `name == "task" && matches!(kind, Pinned)` guard, falling back to
        // the first pinned region ("system").
        let bp = blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Temporary, 5000),
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 5000),
        ]);

        initialize_context_window(&mut engine, entity, &bp, "fallback seed");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        // The non-pinned "task" region is left empty...
        assert!(window.get_region("task").unwrap().content.is_empty());
        // ...and the seed lands in the first pinned region instead.
        assert!(
            window
                .get_region("system")
                .unwrap()
                .content
                .iter()
                .any(|e| e.content.contains("fallback seed"))
        );
    }

    #[test]
    fn swap_layout_preserves_overlapping_content_and_creates_new_regions() {
        let (mut engine, entity) = engine_and_entity();
        let bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        initialize_context_window(&mut engine, entity, &bp, "carried content");

        // New layout keeps "system" (content should carry over) and adds a
        // brand-new "scratch" region (no prior content → the None branch).
        let new_layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 5000),
                RegionDefinition::new("scratch".to_string(), RegionKind::Temporary, 3000),
            ],
            8000,
        );

        swap_context_layout(&mut engine, entity, &new_layout);

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        assert_eq!(window.regions.len(), 2);
        assert!(
            window
                .get_region("system")
                .unwrap()
                .content
                .iter()
                .any(|e| e.content.contains("carried content"))
        );
        assert!(window.get_region("scratch").unwrap().content.is_empty());
        // Token total recomputed from the surviving content.
        assert_eq!(window.current_tokens, window.calculate_tokens());
        assert!(window.current_tokens > 0);
    }

    #[test]
    fn both_helpers_no_op_on_entity_without_context_window() {
        // An entity that has no ContextWindow component makes the guarding
        // `if let Some(mut window)` fall through — both helpers are safe no-ops.
        let mut engine = AgentEngine::with_providers(ProviderRegistry::new());
        let bare = engine.world_mut().spawn_empty().id();

        let bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        initialize_context_window(&mut engine, bare, &bp, "ignored");

        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "system".to_string(),
                RegionKind::Pinned,
                5000,
            )],
            5000,
        );
        swap_context_layout(&mut engine, bare, &layout);

        // Still no ContextWindow was created.
        assert!(engine.world().get::<ContextWindow>(bare).is_none());
    }
}
