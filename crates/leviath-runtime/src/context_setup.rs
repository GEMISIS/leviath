//! Context-window setup helpers shared by the ECS pipeline's spawner and
//! stage-entry.
//!
//! These are pure operations over a [`ContextWindow`] driven by a
//! blueprint/layout.

use std::collections::HashMap;

use leviath_core::{Blueprint, ContextLayout, EvictionStrategy, Region, RegionKind};

use crate::ContextWindow;

/// Initialize a [`ContextWindow`] from a blueprint and seed its regions from a
/// name→content map. Adds each layout region plus the infra
/// `tool_results`/`conversation` regions, then fills each seed whose key matches
/// a declared region. The `task` key gets the legacy fallback: if there is no
/// region literally named `task`, it seeds the first pinned region instead.
/// Pure over the window (no engine/entity), so both the imperative engine and
/// the ECS pipeline's spawner can share it.
pub fn init_window_seeded(
    window: &mut ContextWindow,
    blueprint: &Blueprint,
    seeds: &HashMap<String, String>,
) {
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

    for (name, content) in seeds {
        // The task key keeps its legacy fallback: prefer a region named "task",
        // else the first pinned region. Every other key targets its region by
        // exact name (unknown names are already rejected upstream, so ignore
        // them here to keep this pure/infallible).
        let target = if name == "task" {
            task_region_name(blueprint)
        } else {
            blueprint
                .context_layout
                .regions
                .iter()
                .find(|r| &r.name == name)
                .map(|r| r.name.clone())
        };
        if let Some(region_name) = target {
            let tokens = content.len() / 4 + 1;
            let _ = window.add_to_region(&region_name, content.clone(), tokens);
        }
    }
}

/// Resolve which region the `task` text seeds into: prefer a pinned region named
/// `task`, else the first pinned region.
fn task_region_name(blueprint: &Blueprint) -> Option<String> {
    blueprint
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
        .map(|r| r.name.clone())
}

/// Initialize a [`ContextWindow`] seeding only the task text — the thin
/// back-compat wrapper over [`init_window_seeded`] used by callers that carry a
/// single task string (the imperative engine and existing tests).
pub fn init_window(window: &mut ContextWindow, blueprint: &Blueprint, task: &str) {
    let seeds = HashMap::from([("task".to_string(), task.to_string())]);
    init_window_seeded(window, blueprint, &seeds);
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

#[cfg(test)]
mod tests {
    use super::{apply_layout, init_window, init_window_seeded};
    use crate::ContextWindow;
    use leviath_core::{
        Blueprint, ContextLayout, EvictionStrategy, RegionKind, Stage, blueprint::ModelConfig,
        layout::RegionDefinition,
    };
    use std::collections::HashMap;

    fn blueprint_with(regions: Vec<RegionDefinition>) -> Blueprint {
        let layout = ContextLayout::new(regions, 100_000);
        let stages = vec![Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()),
        )];
        Blueprint::new("bp".to_string(), "desc".to_string(), stages, layout)
    }

    fn seeded_window(bp: &Blueprint, task: &str) -> ContextWindow {
        let mut window = ContextWindow::new(100_000);
        init_window(&mut window, bp, task);
        window
    }

    #[test]
    fn init_window_seeded_fills_multiple_named_regions_and_ignores_unknown() {
        let bp = blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("criteria".to_string(), RegionKind::Pinned, 5000),
        ]);
        let seeds = HashMap::from([
            ("task".to_string(), "build a parser".to_string()),
            ("criteria".to_string(), "focus on safety".to_string()),
            ("ghost".to_string(), "no such region".to_string()),
        ]);
        let mut window = ContextWindow::new(100_000);
        init_window_seeded(&mut window, &bp, &seeds);

        assert!(
            window
                .get_region("task")
                .unwrap()
                .content
                .iter()
                .any(|e| e.content.contains("build a parser"))
        );
        assert!(
            window
                .get_region("criteria")
                .unwrap()
                .content
                .iter()
                .any(|e| e.content.contains("focus on safety"))
        );
        // An unknown seed key targets no region and is silently dropped.
        assert!(window.get_region("ghost").is_none());
    }

    #[test]
    fn init_window_seeded_task_key_falls_back_to_first_pinned() {
        // No region literally named "task": the "task" seed key still lands in
        // the first pinned region (legacy fallback), while a named key does not.
        let bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let seeds = HashMap::from([("task".to_string(), "fallback text".to_string())]);
        let mut window = ContextWindow::new(100_000);
        init_window_seeded(&mut window, &bp, &seeds);
        assert!(
            window
                .get_region("system")
                .unwrap()
                .content
                .iter()
                .any(|e| e.content.contains("fallback text"))
        );
    }

    #[test]
    fn init_prefers_named_task_region_and_keeps_existing_infra_regions() {
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

        let window = seeded_window(&bp, "do the thing");
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
    fn init_adds_infra_regions_and_falls_back_to_first_pinned() {
        // Only a pinned "system" region (not named "task"): task falls back to
        // it, and tool_results + conversation are auto-added.
        let bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);

        let window = seeded_window(&bp, "seed task");
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
    fn init_without_pinned_region_does_not_seed_task() {
        let bp = blueprint_with(vec![RegionDefinition::new(
            "scratch".to_string(),
            RegionKind::Temporary,
            5000,
        )]);

        let window = seeded_window(&bp, "unseeded task");
        // No pinned region → task text is seeded nowhere; the sole declared
        // region stays empty.
        assert!(window.get_region("scratch").unwrap().content.is_empty());
        // Infra regions still added.
        assert!(window.get_region("tool_results").is_some());
        assert!(window.get_region("conversation").is_some());
    }

    #[test]
    fn init_task_named_region_that_is_not_pinned_falls_back_to_first_pinned() {
        // A region literally named "task" but NOT pinned must be rejected by
        // the `name == "task" && matches!(kind, Pinned)` guard, falling back to
        // the first pinned region ("system").
        let bp = blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Temporary, 5000),
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 5000),
        ]);

        let window = seeded_window(&bp, "fallback seed");
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
    fn apply_layout_preserves_overlapping_content_and_creates_new_regions() {
        let bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let mut window = seeded_window(&bp, "carried content");

        // New layout keeps "system" (content should carry over) and adds a
        // brand-new "scratch" region (no prior content → the None branch).
        let new_layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 5000),
                RegionDefinition::new("scratch".to_string(), RegionKind::Temporary, 3000),
            ],
            8000,
        );

        apply_layout(&mut window, &new_layout);

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
}
