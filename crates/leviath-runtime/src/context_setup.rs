//! Engine context-window setup helpers shared by the relocated stage engine.
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

/// Swap context layout to a stage-specific layout (preserving existing content where possible).
pub fn swap_context_layout(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    layout: &ContextLayout,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
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
}
