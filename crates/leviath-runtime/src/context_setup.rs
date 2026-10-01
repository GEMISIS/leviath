//! Context-window setup shared by the ECS pipeline's spawner and stage entry.
//!
//! These are pure operations over a [`ContextWindow`], driven by a run's
//! resolved spec: its layout's regions, and the token budget each region was
//! resolved to.

use leviath_core::{EvictionStrategy, Region, RegionKind, truncate_at_boundary};

use crate::ContextWindow;
use crate::spec::graph::{RegionDef, RegionLayoutDef};
use crate::spec::run_spec::RunSpec;

pub(crate) mod parts;
pub(crate) use parts::{PartSink, text_part};

/// The tokens a region gets in one stage: what that stage's plan resolved it
/// to, or its budget against that stage's window when the plan has no entry
/// for it (a region the stage hides).
pub fn stage_region_budget(spec: &RunSpec, stage: usize, def: &RegionDef) -> usize {
    let plan = spec.stages.get(stage);
    match plan.and_then(|p| p.region_budgets.get(&def.name)) {
        Some(n) => *n as usize,
        None => def
            .budget
            .resolve(plan.map_or(0, |p| p.context_window) as usize),
    }
}

/// The window's region for a declared one, holding `budget` tokens.
pub fn region_from_def(def: &RegionDef, budget: usize) -> Region {
    let mut region = Region::new(
        def.name.to_string(),
        crate::pipeline::spec_view::region_kind(def, budget),
        budget,
    );
    region.summarizable = def.summarizable;
    region.admission = def.admission;
    region.volatility = def.volatility;
    region.accepts = def.accepts.iter().map(|m| m.to_string()).collect();
    region.description = def.description.clone();
    region.describe_in_prompt = def.describe_in_prompt;
    region
}

/// Lay `window` out from a layout's regions, each already holding its
/// budget, then the infra `tool_results`/`conversation`/`final_output` regions
/// it does not declare itself. Nothing is written into any of them.
pub(crate) fn lay_out(window: &mut ContextWindow, regions: Vec<Region>) {
    for region in regions {
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
}

/// Marker appended to a seed that was trimmed to fit its region.
const SEED_TRUNCATION_MARKER: &str =
    "\n[...truncated by leviath: seed exceeded this region's budget]";

/// Trim `content` so that its `len/4 + 1` token estimate fits `max_tokens`,
/// leaving room for [`SEED_TRUNCATION_MARKER`]. Returns `content` unchanged when
/// it already fits. Always cuts on a UTF-8 char boundary.
///
/// A seed is trimmed before it is written because `add_entry` refuses an
/// over-budget entry outright rather than truncating it: without this a seed
/// larger than its region (a big README, a long `git ls-files`) would leave
/// the region empty.
pub(crate) fn fit_seed_to_budget(content: &str, max_tokens: usize) -> String {
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

/// Swap a [`ContextWindow`] to one stage's own layout in place, keeping each
/// carried-over region's content by name. Each region gets the budget the
/// stage's plan resolved it to.
pub fn apply_stage_layout(
    window: &mut ContextWindow,
    spec: &RunSpec,
    stage: usize,
    layout: &RegionLayoutDef,
) {
    let regions = layout
        .regions
        .iter()
        .map(|def| region_from_def(def, stage_region_budget(spec, stage, def)))
        .collect();
    swap_layout(window, regions);
}

/// Swap a window to a stage's own layout, given as its regions each already
/// holding its budget. [`apply_stage_layout`] is the same step from a spec.
pub(crate) fn apply_layout(window: &mut ContextWindow, regions: Vec<Region>) {
    swap_layout(window, regions);
}

/// Replace the window's regions with `declared`, carrying every existing
/// region's entries into its namesake, and keeping (hidden) any region the
/// new layout leaves out.
fn swap_layout(window: &mut ContextWindow, declared: Vec<Region>) {
    let mut new_regions = Vec::new();
    let mut kept: std::collections::HashSet<String> = std::collections::HashSet::new();
    for mut new_region in declared {
        if let Some(existing) = window.get_region(&new_region.name) {
            // Carry entries verbatim - kind, metadata, key, timestamp survive
            // the swap. Rebuilding via `add_entry` flattened every carried
            // entry to `EntryKind::Text`, which destroyed the typed tool_use/
            // tool_result pairing of any message-bearing region and left the
            // assembler's orphan sanitizer to strip the whole history.
            for entry in &existing.content {
                let _ = new_region.carry_entry(entry.clone());
            }
            // The region-level taint state carries wholesale too: rebuilding
            // instead of carrying silently resets it.
            new_region.taint = existing.taint.clone();
        }

        kept.insert(new_region.name.clone());
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
    let always_visible = crate::spec::graph::ALWAYS_VISIBLE_REGIONS;
    let mut hidden = std::collections::HashSet::new();
    for existing in &window.regions {
        if kept.contains(&existing.name) {
            continue;
        }
        let mut carried = Region::new(
            existing.name.clone(),
            existing.kind.clone(),
            existing.max_tokens,
        );
        carried.summarizable = existing.summarizable;
        carried.admission = existing.admission;
        carried.volatility = existing.volatility;
        carried.description = existing.description.clone();
        carried.describe_in_prompt = existing.describe_in_prompt;
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
/// [`STAGE_INSTRUCTIONS_REGION`]: crate::spec::graph::STAGE_INSTRUCTIONS_REGION
pub(crate) fn ensure_stage_instructions_region(
    window: &mut ContextWindow,
    prompts: &[Option<String>],
) {
    let declared = crate::spec::graph::STAGE_INSTRUCTIONS_REGION;
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
    use super::*;
    use crate::ContextWindow;
    use crate::spec::graph::{Budget, RunGraph};
    use crate::spec::run_spec::{RunSpec, SeededContent};
    use crate::test_graph::{layout, region};
    use leviath_core::{EvictionStrategy, RegionKind};
    use std::collections::HashMap;

    /// A one-stage graph over `regions`, sharing 100k tokens.
    fn graph_with(regions: Vec<RegionDef>) -> RunGraph {
        crate::test_graph::graph(
            vec![crate::test_graph::stage("main")],
            layout(regions, 100_000),
        )
    }

    /// The spec a spawn of `graph` on a 100k-token model resolves.
    fn spec_of(graph: RunGraph) -> RunSpec {
        let spec = crate::test_graph::spec_with(graph, &[crate::test_graph::plan_inference(0)]);
        let mut spec = (*spec.0).clone();
        for plan in &mut spec.stages {
            plan.context_window = 100_000;
        }
        spec
    }

    /// The window a spawn of `graph` starts with, each of `seeds` written
    /// into the region it names.
    fn seed(graph: RunGraph, seeds: &[(&str, String)]) -> ContextWindow {
        let mut spec = spec_of(graph);
        for (name, text) in seeds {
            spec.seeded.insert(
                crate::test_graph::region_name(name),
                SeededContent {
                    text: text.clone(),
                    parts: Vec::new(),
                },
            );
        }
        crate::insert::seeded_window(&spec, &HashMap::new())
    }

    /// The window a spawn of `graph` starts with, `task` in its `task` region.
    fn seeded_window(graph: RunGraph, task: &str) -> ContextWindow {
        seed(graph, &[("task", task.to_string())])
    }

    /// Swap `window` to `regions` as a stage whose own layout they are.
    fn swap_to(window: &mut ContextWindow, regions: Vec<RegionDef>) {
        let spec = spec_of(graph_with(regions));
        apply_stage_layout(window, &spec, 0, &spec.graph.layout);
    }

    /// The infra region is added when the layout does not already declare it. A
    /// blueprint that names `final_output` itself keeps its own definition,
    /// budget and all, rather than being silently overwritten with the default.
    #[test]
    fn a_layout_that_declares_final_output_keeps_its_own() {
        const DECLARED_TOKENS: u32 = 12_345;
        let bp = graph_with(vec![
            region("task", RegionKind::Pinned, 1_000),
            region(
                crate::output_tool::FINAL_OUTPUT_REGION,
                RegionKind::Pinned,
                DECLARED_TOKENS,
            ),
        ]);

        let window = seeded_window(bp, "t");

        assert_eq!(
            window
                .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
                .expect("the region is there")
                .max_tokens,
            DECLARED_TOKENS as usize,
            "the blueprint's own budget survives"
        );
    }

    /// And a layout that says nothing about it gets the default, so an agent
    /// never has to declare a region it did not ask for.
    #[test]
    fn a_layout_without_final_output_gets_the_default_one() {
        let bp = graph_with(vec![region("task", RegionKind::Pinned, 1_000)]);

        let window = seeded_window(bp, "t");

        assert_eq!(
            window
                .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
                .expect("added for us")
                .max_tokens,
            crate::output_tool::FINAL_OUTPUT_REGION_TOKENS
        );
    }

    #[test]
    fn seeding_fills_multiple_named_regions_and_ignores_unknown() {
        let bp = graph_with(vec![
            region("task", RegionKind::Pinned, 5000),
            region("criteria", RegionKind::Pinned, 5000),
        ]);
        let window = seed(
            bp,
            &[
                ("task", "build a parser".to_string()),
                ("criteria", "focus on safety".to_string()),
                ("ghost", "no such region".to_string()),
            ],
        );

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

    /// The token estimate seeding computes for a fitted seed - the
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
    fn seeding_truncates_a_seed_larger_than_its_region() {
        // Regression: `add_entry` rejects an over-budget entry outright, so a
        // seed must be trimmed first - an untrimmed oversized seed leaves the
        // region completely EMPTY.
        let bp = graph_with(vec![region("facts", RegionKind::Pinned, 50)]);
        let window = seed(bp, &[("facts", "y".repeat(10_000))]);

        let region = window.get_region("facts").unwrap();
        assert!(
            !region.content.is_empty(),
            "an oversized seed must be trimmed, not dropped"
        );
        assert!(region.content[0].content.ends_with(SEED_TRUNCATION_MARKER));
    }

    #[test]
    fn init_prefers_named_task_region_and_keeps_existing_infra_regions() {
        let bp = graph_with(vec![
            region("task", RegionKind::Pinned, 5000),
            region("tool_results", RegionKind::Temporary, 5000),
            region(
                "conversation",
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10_000,
            ),
        ]);

        let window = seeded_window(bp, "do the thing");
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
    fn laying_out_adds_the_infra_regions() {
        // Only a pinned "system" region: tool_results + conversation are
        // auto-added, and a seed for it lands in it.
        let bp = graph_with(vec![region("system", RegionKind::Pinned, 5000)]);

        let window = seed(bp, &[("system", "seed task".to_string())]);
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
    fn a_region_no_seed_names_starts_empty() {
        let bp = graph_with(vec![region("scratch", RegionKind::Temporary, 5000)]);

        let window = seeded_window(bp, "unseeded task");
        // No region is named by the seed; the sole declared region stays
        // empty.
        assert!(window.get_region("scratch").unwrap().content.is_empty());
        // Infra regions still added.
        assert!(window.get_region("tool_results").is_some());
        assert!(window.get_region("conversation").is_some());
    }

    #[test]
    fn apply_layout_preserves_overlapping_content_and_creates_new_regions() {
        let bp = graph_with(vec![region("system", RegionKind::Pinned, 5000)]);
        let mut window = seed(bp, &[("system", "carried content".to_string())]);

        // New layout keeps "system" (content should carry over) and adds a
        // brand-new "scratch" region (no prior content → the None branch).
        let new_layout = vec![
            region("system", RegionKind::Pinned, 5000),
            region("scratch", RegionKind::Temporary, 3000),
        ];

        swap_to(&mut window, new_layout);

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
        // The carry must not rebuild entries via `add_entry`: that stamps
        // every carried entry `EntryKind::Text`, destroying tool_use/
        // tool_result pairing, and silently resets region-level taint.
        let bp = graph_with(vec![region("task", RegionKind::Pinned, 5000)]);
        let mut window = seeded_window(bp, "the task");
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
        let omitting = vec![region("task", RegionKind::Pinned, 5000)];
        swap_to(&mut window, omitting);

        // Swap 2: layout declares conversation (the by-name carry loop).
        let declaring = vec![
            region("task", RegionKind::Pinned, 5000),
            region(
                "conversation",
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10_000,
            ),
        ];
        swap_to(&mut window, declaring);

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
        let bp = graph_with(vec![region("task", RegionKind::Pinned, 5000)]);
        let mut window = seeded_window(bp, "the task");
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "hello from stage 0".to_string(),
                10,
            )
            .unwrap();

        // Transition to a layout that omits conversation entirely.
        let next = vec![region("task", RegionKind::Pinned, 5000)];
        swap_to(&mut window, next);

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

    /// A stage's regions, already holding their budgets, swap a window the
    /// same way the stage's layout does from a spec.
    #[test]
    fn swapping_to_regions_matches_swapping_to_a_stage_layout() {
        let bp = graph_with(vec![
            region("task", RegionKind::Pinned, 5000),
            region("notes", RegionKind::Temporary, 3000),
        ]);
        let mut by_regions = seeded_window(bp, "the task");
        let mut by_spec = by_regions.clone();
        let next = vec![region("task", RegionKind::Pinned, 4000)];
        let regions = next.iter().map(|r| region_from_def(r, 4000)).collect();
        apply_layout(&mut by_regions, regions);
        swap_to(&mut by_spec, next);
        let shape = |w: &ContextWindow| {
            w.regions
                .iter()
                .map(|r| (r.name.clone(), r.max_tokens, r.content.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&by_regions), shape(&by_spec));
        assert_eq!(by_regions.hidden, by_spec.hidden);
    }

    #[test]
    fn a_percentage_budget_is_capped_then_floored() {
        let pct = |percent, min, max| Budget::Percent { percent, min, max };
        assert_eq!(Budget::Tokens(7).resolve(100), 7);
        assert_eq!(pct(0.5, None, None).resolve(1000), 500);
        assert_eq!(pct(0.5, None, Some(100)).resolve(1000), 100);
        assert_eq!(pct(0.5, Some(900), Some(100)).resolve(1000), 900);
    }

    use crate::spec::graph::{CodeRef, Eviction, RegionKind as Kind};

    fn def(kind: crate::spec::graph::RegionKind) -> RegionDef {
        RegionDef {
            name: crate::spec::names::RegionName::new("r").unwrap(),
            kind,
            budget: Budget::Tokens(1000),
            compact_at: None,
            description: Some("d".into()),
            describe_in_prompt: true,
            required: false,
            required_message: None,
            summarizable: false,
            admission: Default::default(),
            volatility: Default::default(),
            seed: None,
            accepts: vec![crate::spec::names::MimePattern::new("image/*").unwrap()],
        }
    }

    /// Every declared kind becomes the window's own, with the declared
    /// settings carried across.
    #[test]
    fn every_region_kind_reaches_the_window() {
        use crate::spec::graph::RegionKind as K;
        use crate::spec::names::RegionName;
        let cases: Vec<(K, RegionKind)> = vec![
            (K::Pinned, RegionKind::Pinned),
            (
                K::SlidingWindow {
                    max_items: 3,
                    eviction: Eviction::PerItem,
                },
                RegionKind::SlidingWindow {
                    max_items: 3,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
            ),
            (
                K::SlidingWindow {
                    max_items: 3,
                    eviction: Eviction::Bulk(2),
                },
                RegionKind::SlidingWindow {
                    max_items: 3,
                    eviction_strategy: EvictionStrategy::Bulk { overflow: 2 },
                },
            ),
            (
                K::SlidingWindow {
                    max_items: 3,
                    eviction: Eviction::Compact(4),
                },
                RegionKind::SlidingWindow {
                    max_items: 3,
                    eviction_strategy: EvictionStrategy::Compact { compact_count: 4 },
                },
            ),
            (K::Temporary, RegionKind::Temporary),
            (
                K::Compacting {
                    threshold_tokens: Some(9),
                },
                RegionKind::Compacting {
                    threshold_tokens: 9,
                },
            ),
            (K::Clearable, RegionKind::Clearable),
            (
                K::CompactHistory {
                    source: Some(RegionName::new("conversation").unwrap()),
                },
                RegionKind::CompactHistory {
                    source_region: "conversation".into(),
                },
            ),
            (
                K::Keyed {
                    max_entries: Some(2),
                },
                RegionKind::HashMap {
                    max_entries: Some(2),
                },
            ),
            (K::Checklist, RegionKind::Checklist),
            (
                K::Custom {
                    code: CodeRef::File("r.rhai".into()),
                    pinned: true,
                },
                RegionKind::Custom {
                    script: "r.rhai".into(),
                    pinned: true,
                },
            ),
            (
                K::Custom {
                    code: CodeRef::Inline("fn render() {}".into()),
                    pinned: false,
                },
                RegionKind::Custom {
                    script: "fn render() {}".into(),
                    pinned: false,
                },
            ),
        ];
        for (kind, want) in cases {
            let region = region_from_def(&def(kind), 1000);
            assert_eq!(region.kind, want);
            assert_eq!(region.max_tokens, 1000);
            assert!(!region.summarizable && region.describe_in_prompt);
            assert_eq!(region.accepts, vec!["image/*".to_string()]);
            assert_eq!(region.description.as_deref(), Some("d"));
        }
    }

    #[test]
    fn a_compacting_region_compacts_at_its_share_under_its_threshold() {
        let mut r = def(Kind::Compacting {
            threshold_tokens: None,
        });
        assert_eq!(r.compaction_threshold(None, 1001), 800);
        assert_eq!(r.compaction_threshold(Some(50), 1000), 50);
        r.compact_at = Some(0.8);
        assert_eq!(r.compaction_threshold(None, 1000), 800);
        assert_eq!(r.compaction_threshold(Some(50), 1000), 50);
        r.compact_at = None;
        r.budget = Budget::Percent {
            percent: 0.5,
            min: None,
            max: None,
        };
        assert_eq!(r.compaction_threshold(None, 1001), 801);
    }

    /// A region only per-stage layouts see, or one every global-layout stage
    /// hides, is sized against the entry stage's window; a stage with no
    /// plan entry for a region sizes it against its own.
    #[test]
    fn a_region_no_stage_resolved_is_sized_against_a_window() {
        let bp = graph_with(vec![region("task", RegionKind::Pinned, 5000)]);
        let mut spec = spec_of(bp);
        spec.stages[0].region_budgets.clear();
        let mut half = spec.graph.layout.regions[0].clone();
        half.budget = Budget::Percent {
            percent: 0.5,
            min: None,
            max: None,
        };
        assert_eq!(stage_region_budget(&spec, 0, &half), 50_000);
        spec.stages[0]
            .region_budgets
            .insert(half.name.clone(), 1234);
        assert_eq!(
            stage_region_budget(&spec, 0, &half),
            1234,
            "the plan's figure wins"
        );
        assert_eq!(
            stage_region_budget(&spec, 9, &half),
            0,
            "no plan, no window"
        );
    }
}
