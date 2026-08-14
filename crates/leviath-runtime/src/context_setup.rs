//! Context-window setup helpers shared by the ECS pipeline's spawner and
//! stage-entry.
//!
//! These are pure operations over a [`ContextWindow`] driven by a
//! blueprint/layout.

use std::collections::HashMap;

use leviath_core::{
    Blueprint, ContextLayout, EvictionStrategy, Region, RegionKind, truncate_at_boundary,
};

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
        let mut region = Region::new(
            region_def.name.clone(),
            region_def.kind.clone(),
            region_def.max_tokens,
        );
        region.summarizable = region_def.summarizable;
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

    // Where `submit_output` mirrors the run's answer. Pinned, so the answer
    // stays visible to later stages (one can revise it) and is never evicted to
    // make room for the work that produced it. Its budget is the output cap
    // expressed in tokens, so a submission at the size limit still fits.
    if window
        .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
        .is_none()
    {
        window.add_region(Region::new(
            crate::output_tool::FINAL_OUTPUT_REGION.to_string(),
            RegionKind::Pinned,
            crate::output_tool::FINAL_OUTPUT_REGION_TOKENS,
        ));
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
            // Trim to the region's (already-resolved) budget first: `add_entry`
            // REJECTS an over-budget entry outright rather than truncating it, so
            // without this a seed larger than its region - a big README, a long
            // `git ls-files` - would silently leave the region completely empty.
            let budget = window
                .get_region(&region_name)
                .map(|r| r.max_tokens)
                .unwrap_or(0);
            let fitted = fit_seed_to_budget(content, budget);
            let tokens = leviath_core::estimate_tokens(&fitted);
            let _ = window.add_to_region(&region_name, fitted, tokens);
        }
    }
}

/// Marker appended to a seed that was trimmed to fit its region.
const SEED_TRUNCATION_MARKER: &str =
    "\n[...truncated by leviath: seed exceeded this region's budget]";

/// Trim `content` so that its `len/4 + 1` token estimate fits `max_tokens`,
/// leaving room for [`SEED_TRUNCATION_MARKER`]. Returns `content` unchanged when
/// it already fits. Always cuts on a UTF-8 char boundary.
fn fit_seed_to_budget(content: &str, max_tokens: usize) -> String {
    // The token estimate used throughout: `len / 4 + 1`. Fitting means
    // `len / 4 + 1 <= max_tokens`, i.e. `len <= (max_tokens - 1) * 4`.
    let allowed = max_tokens.saturating_sub(1).saturating_mul(4);
    if content.len() <= allowed {
        return content.to_string();
    }
    // Reserve room for the marker; if even that doesn't fit, the region is too
    // small to say anything useful, so emit nothing rather than a lone marker.
    let Some(room) = allowed.checked_sub(SEED_TRUNCATION_MARKER.len()) else {
        return String::new();
    };
    format!(
        "{}{SEED_TRUNCATION_MARKER}",
        truncate_at_boundary(content, room)
    )
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

/// Initialize a [`ContextWindow`] seeding only the task text - the thin
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
    let mut kept: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for region_def in &layout.regions {
        let mut new_region = Region::new(
            region_def.name.clone(),
            region_def.kind.clone(),
            region_def.max_tokens,
        );
        new_region.summarizable = region_def.summarizable;

        if let Some(existing) = window.get_region(&region_def.name) {
            // Carry entries verbatim - kind, metadata, key, timestamp survive
            // the swap. Rebuilding via `add_entry` flattened every carried
            // entry to `EntryKind::Text`, which destroyed the typed tool_use/
            // tool_result pairing of any message-bearing region and left the
            // assembler's orphan sanitizer to strip the whole history.
            for entry in &existing.content {
                let _ = new_region.carry_entry(entry.clone());
            }
            // The region-level taint state carries wholesale too; the rebuild
            // used to silently reset it.
            new_region.taint = existing.taint.clone();
        }

        kept.insert(region_def.name.as_str());
        new_regions.push(new_region);
    }

    // Everything the stage layout did not declare is carried anyway, and
    // hidden instead of deleted.
    //
    // Dropping them made `[stages.X.context.regions]` unusable for the thing it
    // looks designed for: narrowing what one stage attends to, in a pipeline
    // whose later stages still need the data. Re-declaring a region downstream
    // brought it back empty, so an author had to choose between carrying a
    // 6,700-token data preview through every call of every stage and destroying
    // it. Omission now means "not assembled for this stage" and nothing else.
    //
    // `conversation`, `tool_results` and `final_output` are carried *visible*
    // regardless: the first two hold the typed tool_use/tool_result turns, and
    // hiding them would strand a message history the next stage's own turns
    // have to attach to. An answer submitted early has to survive to the end
    // for the same reason.
    // `stage_instructions` joins them for a different reason: it holds the
    // prompt of the stage being entered, which is written straight after this
    // runs. Hiding it because a stage's own `[context.regions]` did not list it
    // would silently drop that stage's instructions - the region is the
    // runtime's to fill, not something an author has to remember to re-declare
    // in every stage.
    let always_visible = [
        "conversation",
        "tool_results",
        crate::output_tool::FINAL_OUTPUT_REGION,
        leviath_core::layout::STAGE_INSTRUCTIONS_REGION,
    ];
    let mut hidden = std::collections::HashSet::new();
    for existing in &window.regions {
        if kept.contains(existing.name.as_str()) {
            continue;
        }
        let mut carried = Region::new(
            existing.name.clone(),
            existing.kind.clone(),
            existing.max_tokens,
        );
        carried.summarizable = existing.summarizable;
        // Verbatim, exactly as above: these are the regions whose typed turns
        // a rebuild would flatten.
        for entry in &existing.content {
            let _ = carried.carry_entry(entry.clone());
        }
        carried.taint = existing.taint.clone();
        if !always_visible.contains(&existing.name.as_str()) {
            hidden.insert(existing.name.clone());
        }
        new_regions.push(carried);
    }
    // Describes the stage being entered, so it replaces rather than accumulates.
    window.hidden = hidden;

    window.regions = new_regions;
    window.current_tokens = window.calculate_tokens();
}

/// Give the stage prompts a region of their own when the blueprint did not.
///
/// [`STAGE_INSTRUCTIONS_REGION`] is, in this file's own words further up, "the
/// runtime's to fill, not something an author has to remember to re-declare".
/// It was only ever *used* when an author declared it, though - and when they
/// did not, the prompt went into whatever pinned region happened to be first.
/// That is usually `task`, whose budget is sized for a sentence from the caller
/// and not for a stage's instructions.
///
/// Under window pressure that is a spawn failure rather than a squeeze:
///
/// ```text
/// stage system prompt does not fit region 'task' (2887 > 2560)
/// ```
///
/// The workaround is to floor every `task` declaration with a `min_tokens` sized
/// for the largest *stage prompt* - which couples an unrelated region's floor to
/// prompt lengths, and only shows up at spawn on a small window, so it reads as
/// the caller's fault rather than as routing.
///
/// Sized to the largest prompt the blueprint actually carries, because that is
/// the one that has to fit and anything beyond it is budget taken from the work.
/// A blueprint whose stages have no prompts gets no region: there would be
/// nothing to put in it.
///
/// Capped at a quarter of the window, which is what keeps
/// this from turning a real failure into a silent one. A prompt larger than the
/// whole window cannot be made to fit by giving it a bigger region, and a spawn
/// that says so is right to. What changes is only which region the message
/// names: `stage_instructions`, which is where the prompt was going, rather than
/// `task`, which is the caller's.
///
/// [`STAGE_INSTRUCTIONS_REGION`]: leviath_core::layout::STAGE_INSTRUCTIONS_REGION
pub fn ensure_stage_instructions_region(window: &mut ContextWindow, prompts: &[Option<String>]) {
    let declared = leviath_core::layout::STAGE_INSTRUCTIONS_REGION;
    if window.get_region(declared).is_some() {
        return;
    }
    // The wrapper travels with the prompt, so it is measured with it.
    let widest = prompts
        .iter()
        .flatten()
        .map(|p| leviath_core::estimate_tokens(&format!("[Stage instructions: {p}]")))
        .max();
    let Some(widest) = widest.filter(|t| *t > 0) else {
        return;
    };
    let ceiling = window.max_tokens / INSTRUCTIONS_SHARE_OF_WINDOW;
    window.add_region(Region::new(
        declared.to_string(),
        RegionKind::Pinned,
        widest.min(ceiling),
    ));
}

/// The largest share of the window an auto-created instructions region may take,
/// as a divisor: a quarter.
///
/// Only ever a ceiling - the region is sized to the prompt it has to hold, and
/// this is what it may not exceed. A quarter is generous for instructions and
/// still leaves the window mostly for the work; a prompt that will not fit in it
/// is one no region size was going to rescue.
const INSTRUCTIONS_SHARE_OF_WINDOW: usize = 4;

#[cfg(test)]
mod tests {
    use super::{
        SEED_TRUNCATION_MARKER, apply_layout, fit_seed_to_budget, init_window, init_window_seeded,
    };
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

    /// The infra region is added when the layout does not already declare it. A
    /// blueprint that names `final_output` itself keeps its own definition,
    /// budget and all, rather than being silently overwritten with the default.
    #[test]
    fn a_layout_that_declares_final_output_keeps_its_own() {
        const DECLARED_TOKENS: usize = 12_345;
        let bp = blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Pinned, 1_000),
            RegionDefinition::new(
                crate::output_tool::FINAL_OUTPUT_REGION.to_string(),
                RegionKind::Pinned,
                DECLARED_TOKENS,
            ),
        ]);

        let window = seeded_window(&bp, "t");

        assert_eq!(
            window
                .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
                .expect("the region is there")
                .max_tokens,
            DECLARED_TOKENS,
            "the blueprint's own budget survives"
        );
    }

    /// And a layout that says nothing about it gets the default, so an agent
    /// never has to declare a region it did not ask for.
    #[test]
    fn a_layout_without_final_output_gets_the_default_one() {
        let bp = blueprint_with(vec![RegionDefinition::new(
            "task".to_string(),
            RegionKind::Pinned,
            1_000,
        )]);

        let window = seeded_window(&bp, "t");

        assert_eq!(
            window
                .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
                .expect("added for us")
                .max_tokens,
            crate::output_tool::FINAL_OUTPUT_REGION_TOKENS
        );
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
    fn fit_seed_to_budget_leaves_a_fitting_seed_untouched() {
        assert_eq!(fit_seed_to_budget("hello", 100), "hello");
        // Exactly at the limit: len == (max_tokens - 1) * 4.
        let exact = "x".repeat(36);
        assert_eq!(fit_seed_to_budget(&exact, 10), exact);
    }

    /// The token estimate `init_window_seeded` computes for a fitted seed - the
    /// number that has to land inside the region's budget.
    fn estimated_tokens(fitted: &str) -> usize {
        leviath_core::estimate_tokens(fitted)
    }

    #[test]
    fn fit_seed_to_budget_truncates_and_marks_an_oversized_seed() {
        let big = "x".repeat(10_000);
        let fitted = fit_seed_to_budget(&big, 100);
        assert!(fitted.ends_with(SEED_TRUNCATION_MARKER));
        // The estimate the caller will compute must actually fit the budget.
        let estimate = estimated_tokens(&fitted);
        assert!(estimate <= 100, "estimate was {estimate}");
    }

    #[test]
    fn fit_seed_to_budget_cuts_on_a_char_boundary() {
        // Place a 2-byte char so it straddles the cut exactly: slicing there
        // would panic, so the walk-back has to move off it.
        const MAX_TOKENS: usize = 60;
        let room = (MAX_TOKENS - 1) * 4 - SEED_TRUNCATION_MARKER.len();
        let mut s = "a".repeat(room - 1);
        s.push('é'); // occupies bytes room-1 and room - the cut lands inside it
        s.push_str(&"b".repeat(500));
        assert!(!s.is_char_boundary(room), "test must straddle the cut");

        let fitted = fit_seed_to_budget(&s, MAX_TOKENS);
        assert!(fitted.ends_with(SEED_TRUNCATION_MARKER));
        assert!(estimated_tokens(&fitted) <= MAX_TOKENS);
        // The straddling char was dropped whole rather than split.
        assert_eq!(
            fitted,
            format!("{}{SEED_TRUNCATION_MARKER}", "a".repeat(room - 1))
        );
    }

    #[test]
    fn fit_seed_to_budget_yields_nothing_when_even_the_marker_cannot_fit() {
        // A region too small to hold the marker gets nothing rather than a bare
        // "[...truncated]" with no content.
        assert_eq!(fit_seed_to_budget("some content here", 2), "");
        // Degenerate budgets are handled by the saturating arithmetic.
        assert_eq!(fit_seed_to_budget("x", 0), "");
    }

    #[test]
    fn init_window_seeded_truncates_a_seed_larger_than_its_region() {
        // Regression: `add_entry` rejects an over-budget entry outright, so a
        // seed must be trimmed first - an untrimmed oversized seed leaves the
        // region completely EMPTY.
        let bp = blueprint_with(vec![RegionDefinition::new(
            "facts".to_string(),
            RegionKind::Pinned,
            50,
        )]);
        let seeds = HashMap::from([("facts".to_string(), "y".repeat(10_000))]);
        let mut window = ContextWindow::new(100_000);
        init_window_seeded(&mut window, &bp, &seeds);

        let region = window.get_region("facts").unwrap();
        assert!(
            !region.content.is_empty(),
            "an oversized seed must be trimmed, not dropped"
        );
        assert!(region.content[0].content.ends_with(SEED_TRUNCATION_MARKER));
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

        // system + scratch from the new layout, PLUS the auto-added infra regions
        // carried across the transition even though the new layout doesn't declare
        // them: conversation and tool_results so the message history survives, and
        // final_output so an answer submitted before the transition is still there
        // after it.
        assert_eq!(window.regions.len(), 5);
        assert!(window.get_region("conversation").is_some());
        assert!(window.get_region("tool_results").is_some());
        assert!(
            window
                .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
                .is_some(),
            "a submitted answer must survive a stage transition"
        );
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
    fn apply_layout_preserves_entry_kinds_and_taint_across_swap() {
        // Regression: the carry used to rebuild entries via `add_entry`, which
        // stamped every carried entry `EntryKind::Text` (destroying tool_use/
        // tool_result pairing) and silently reset region-level taint.
        let bp = blueprint_with(vec![RegionDefinition::new(
            "task".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let mut window = seeded_window(&bp, "the task");
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "call_9".to_string(),
                        name: "shell".to_string(),
                        arguments: serde_json::json!({"command": "ls"}),
                        thought_signature: None,
                    }],
                },
                "running ls".to_string(),
                10,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "call_9".to_string(),
                    tool_name: "shell".to_string(),
                    is_error: false,
                },
                "file_a\nfile_b".to_string(),
                10,
            )
            .unwrap();
        window
            .get_region_mut("conversation")
            .unwrap()
            .enable_taint_tracking();

        // Swap 1: layout omits conversation (the infra-carry loop).
        let omitting = ContextLayout::new(
            vec![RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                5000,
            )],
            8000,
        );
        apply_layout(&mut window, &omitting);

        // Swap 2: layout declares conversation (the by-name carry loop).
        let declaring = ContextLayout::new(
            vec![
                RegionDefinition::new("task".to_string(), RegionKind::Pinned, 5000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 10,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    10_000,
                ),
            ],
            20_000,
        );
        apply_layout(&mut window, &declaring);

        let conv = window.get_region("conversation").unwrap();
        assert!(
            conv.content.iter().any(|e| matches!(
                &e.kind,
                leviath_core::EntryKind::AssistantTurn { tool_calls }
                    if tool_calls.iter().any(|c| c.id == "call_9")
            )),
            "assistant turn must keep its typed tool_calls through both carry paths"
        );
        assert!(
            conv.content.iter().any(|e| matches!(
                &e.kind,
                leviath_core::EntryKind::ToolResult { tool_call_id, .. }
                    if tool_call_id == "call_9"
            )),
            "tool result must keep its typed pairing through both carry paths"
        );
        assert!(
            conv.taint.is_some(),
            "region-level taint state must carry across layout swaps"
        );
    }

    #[test]
    fn apply_layout_carries_conversation_when_new_layout_omits_it() {
        // A blueprint whose stage layout has NO conversation region. The auto-added
        // conversation (with typed history) must survive the transition, else the
        // next stage assembles with no messages.
        let bp = blueprint_with(vec![RegionDefinition::new(
            "task".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let mut window = seeded_window(&bp, "the task");
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "hello from stage 0".to_string(),
                10,
            )
            .unwrap();

        // Transition to a layout that omits conversation entirely.
        let next = ContextLayout::new(
            vec![RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                5000,
            )],
            8000,
        );
        apply_layout(&mut window, &next);

        let conv = window
            .get_region("conversation")
            .expect("conversation carried across transition");
        assert!(
            conv.content
                .iter()
                .any(|e| e.content.contains("hello from stage 0")),
            "carried conversation must retain its history"
        );
    }
}
