//! Context-window setup shared by the ECS pipeline's spawner and stage entry.
//!
//! These are pure operations over a [`ContextWindow`], driven by a run's
//! resolved spec: its layout's regions, and the token budget each region was
//! resolved to.

use std::collections::HashMap;

use leviath_core::{EvictionStrategy, Region, RegionKind, truncate_at_boundary};

use crate::ContextWindow;
use crate::spec::graph::{
    Budget, CodeRef, Eviction, RegionDef, RegionKind as Kind, RegionLayoutDef,
};
use crate::spec::run_spec::RunSpec;
use crate::spec::{Blueprint, ContextLayout};

pub(crate) mod parts;
pub(crate) use parts::{PartSink, ingest_parts_into, text_part};

/// A region's budget as tokens, against a model window of `window` tokens.
///
/// A percentage rounds `window * percent`, then applies the cap, then the
/// floor, so a floor above the cap wins.
pub fn budget_tokens(budget: &Budget, window: usize) -> usize {
    match budget {
        Budget::Tokens(n) => *n as usize,
        Budget::Percent { percent, min, max } => {
            let mut v = (window as f64 * percent).round() as usize;
            if let Some(max) = max {
                v = v.min(*max as usize);
            }
            if let Some(min) = min {
                v = v.max(*min as usize);
            }
            v
        }
    }
}

/// The tokens a region of the graph's own layout gets.
///
/// The smallest budget any stage using that layout resolved it to, which is
/// the budget sized against the smallest window that sees the region. A
/// region no such stage sees is sized against the entry stage's window.
pub(crate) fn layout_region_budget(spec: &RunSpec, def: &RegionDef) -> usize {
    spec.graph
        .stages
        .iter()
        .zip(&spec.stages)
        .filter(|(stage, _)| stage.layout.is_none())
        .filter_map(|(_, plan)| plan.region_budgets.get(&def.name))
        .min()
        .map(|n| *n as usize)
        .unwrap_or_else(|| {
            let window = spec.stages.first().map_or(0, |p| p.context_window);
            budget_tokens(&def.budget, window as usize)
        })
}

/// The tokens a region gets in one stage: what that stage's plan resolved it
/// to, or its budget against that stage's window when the plan has no entry
/// for it (a region the stage hides).
pub fn stage_region_budget(spec: &RunSpec, stage: usize, def: &RegionDef) -> usize {
    let plan = spec.stages.get(stage);
    match plan.and_then(|p| p.region_budgets.get(&def.name)) {
        Some(n) => *n as usize,
        None => budget_tokens(&def.budget, plan.map_or(0, |p| p.context_window) as usize),
    }
}

/// The window's region for a declared one, holding `budget` tokens.
pub fn region_from_def(def: &RegionDef, budget: usize) -> Region {
    let mut region = Region::new(def.name.to_string(), core_kind(def, budget), budget);
    region.summarizable = def.summarizable;
    region.admission = def.admission;
    region.volatility = def.volatility;
    region.accepts = def.accepts.iter().map(|m| m.to_string()).collect();
    region.description = def.description.clone();
    region.describe_in_prompt = def.describe_in_prompt;
    region
}

/// How the window keeps a declared region's entries.
fn core_kind(def: &RegionDef, budget: usize) -> RegionKind {
    match &def.kind {
        Kind::Pinned => RegionKind::Pinned,
        Kind::SlidingWindow {
            max_items,
            eviction,
        } => RegionKind::SlidingWindow {
            max_items: *max_items as usize,
            eviction_strategy: match eviction {
                Eviction::PerItem => EvictionStrategy::PerItem,
                Eviction::Bulk(n) => EvictionStrategy::Bulk {
                    overflow: *n as usize,
                },
                Eviction::Compact(n) => EvictionStrategy::Compact {
                    compact_count: *n as usize,
                },
            },
        },
        Kind::Temporary => RegionKind::Temporary,
        Kind::Compacting { threshold_tokens } => RegionKind::Compacting {
            threshold_tokens: compaction_threshold(*threshold_tokens, def.compact_at, budget),
        },
        Kind::Clearable => RegionKind::Clearable,
        Kind::CompactHistory { source } => RegionKind::CompactHistory {
            source_region: source.to_string(),
        },
        Kind::Keyed { max_entries } => RegionKind::HashMap {
            max_entries: max_entries.map(|n| n as usize),
        },
        Kind::Checklist => RegionKind::Checklist,
        Kind::Custom { code, pinned } => RegionKind::Custom {
            script: match code {
                CodeRef::File(path) => path.clone(),
                CodeRef::Inline(source) => source.clone(),
            },
            pinned: *pinned,
        },
    }
}

/// Where a compacting region compacts: its `compact_at` share of the budget,
/// under any fixed threshold; the fixed threshold alone; or never, when it
/// names neither.
fn compaction_threshold(threshold: Option<u32>, compact_at: Option<f64>, budget: usize) -> usize {
    let fixed = threshold.map_or(usize::MAX, |t| t as usize);
    match compact_at {
        Some(fraction) => ((budget as f64 * fraction).round() as usize).min(fixed),
        None => fixed,
    }
}

/// The region the `task` text seeds: a pinned region named `task`, else the
/// first pinned region.
pub(crate) fn task_region(layout: &RegionLayoutDef) -> Option<String> {
    layout
        .regions
        .iter()
        .find(|r| r.name.as_str() == "task" && matches!(r.kind, Kind::Pinned))
        .or_else(|| {
            layout
                .regions
                .iter()
                .find(|r| matches!(r.kind, Kind::Pinned))
        })
        .map(|r| r.name.to_string())
}

/// Initialize a [`ContextWindow`] from a run's spec and seed its regions from
/// a name→content map. Adds each region of the graph's layout at the budget
/// it was resolved to, plus the infra `tool_results`/`conversation`/
/// `final_output` regions, then fills each seed whose key matches a declared
/// region. The `task` key falls back to the first pinned region when no pinned
/// region is named `task`.
pub(crate) fn init_window_from_spec(
    window: &mut ContextWindow,
    spec: &RunSpec,
    seeds: &HashMap<String, String>,
) {
    let layout = &spec.graph.layout;
    let regions = layout
        .regions
        .iter()
        .map(|def| region_from_def(def, layout_region_budget(spec, def)))
        .collect();
    fill_window(window, regions, task_region(layout), seeds);
}

/// Initialize a window from a parsed blueprint's (already resolved) layout:
/// [`init_window_from_spec`] for a caller holding a blueprint.
pub(crate) fn init_window_seeded(
    window: &mut ContextWindow,
    blueprint: &Blueprint,
    seeds: &HashMap<String, String>,
) {
    let regions: Vec<Region> = blueprint
        .context_layout
        .regions
        .iter()
        .map(region_from_definition)
        .collect();
    let task = task_region_of(&regions);
    fill_window(window, regions, task, seeds);
}

/// The window's region for a parsed blueprint's (already resolved) one.
fn region_from_definition(def: &crate::spec::layout::RegionDefinition) -> Region {
    let mut region = Region::new(def.name.clone(), def.kind.clone(), def.max_tokens);
    region.summarizable = def.summarizable;
    region.admission = def.admission;
    region.volatility = def.volatility;
    region.accepts = def.accepts.clone();
    region.description = def.description.clone();
    region.describe_in_prompt = def.describe_in_prompt;
    region
}

/// The task region among regions already built: a pinned `task`, else the
/// first pinned one.
fn task_region_of(regions: &[Region]) -> Option<String> {
    regions
        .iter()
        .find(|r| r.name == "task" && matches!(r.kind, RegionKind::Pinned))
        .or_else(|| {
            regions
                .iter()
                .find(|r| matches!(r.kind, RegionKind::Pinned))
        })
        .map(|r| r.name.clone())
}

/// Add `regions` and the infra regions to `window`, then write each seed into
/// its region. `task` is where the `task` key goes.
fn fill_window(
    window: &mut ContextWindow,
    regions: Vec<Region>,
    task: Option<String>,
    seeds: &HashMap<String, String>,
) {
    let declared: Vec<String> = regions.iter().map(|r| r.name.clone()).collect();
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

    for (name, content) in seeds {
        // The task key has a fallback of its own: prefer a region named "task",
        // else the first pinned region. Every other key targets its region by
        // exact name (unknown names are already rejected upstream, so ignore
        // them here to keep this pure/infallible).
        let target = match name == "task" {
            true => task.clone(),
            false => declared.iter().find(|r| *r == name).cloned(),
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
            let _ = window.add_to_region_caused(
                leviath_core::ContextCause::Seed,
                &region_name,
                fitted,
                tokens,
            );
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

/// Initialize a window from a blueprint, seeding only the task text: the
/// one-seed convenience over `init_window_seeded`, for callers that carry a
/// single task string.
pub fn init_window(window: &mut ContextWindow, blueprint: &Blueprint, task: &str) {
    let seeds = HashMap::from([("task".to_string(), task.to_string())]);
    init_window_seeded(window, blueprint, &seeds);
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

/// Swap a window to a parsed blueprint's (already resolved) stage layout.
/// Stage entry still reads the blueprint; [`apply_stage_layout`] is the same
/// step from a spec.
pub(crate) fn apply_layout(window: &mut ContextWindow, layout: &ContextLayout) {
    let regions = layout.regions.iter().map(region_from_definition).collect();
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
    let always_visible = crate::spec::blueprint::ALWAYS_VISIBLE_REGIONS;
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
/// [`STAGE_INSTRUCTIONS_REGION`]: crate::spec::layout::STAGE_INSTRUCTIONS_REGION
pub(crate) fn ensure_stage_instructions_region(
    window: &mut ContextWindow,
    prompts: &[Option<String>],
) {
    let declared = crate::spec::layout::STAGE_INSTRUCTIONS_REGION;
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
    use crate::spec::run_spec::RunSpec;
    use crate::spec::{
        Blueprint, ContextLayout, Stage, blueprint::ModelConfig, layout::RegionDefinition,
    };
    use leviath_core::{EvictionStrategy, RegionKind};

    fn blueprint_with(regions: Vec<RegionDefinition>) -> Blueprint {
        let layout = ContextLayout::new(regions, 100_000);
        let stages = vec![Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()),
        )];
        Blueprint::new("bp".to_string(), "desc".to_string(), stages, layout)
    }

    /// The spec a spawn of `bp` on a 100k-token model resolves.
    fn spec_of(bp: &Blueprint) -> RunSpec {
        crate::spec_bridge::run_spec_from_blueprint(
            bp,
            "t",
            &[crate::pipeline::StageInference {
                provider_name: "p".into(),
                model: "m".into(),
                tools: vec![],
                tool_filter: None,
                fallbacks: vec![],
                output: None,
            }],
            &[100_000],
        )
        .unwrap()
    }

    fn seed(window: &mut ContextWindow, bp: &Blueprint, seeds: &HashMap<String, String>) {
        init_window_from_spec(window, &spec_of(bp), seeds);
    }

    fn seeded_window(bp: &Blueprint, task: &str) -> ContextWindow {
        let mut window = ContextWindow::new(100_000);
        seed(
            &mut window,
            bp,
            &HashMap::from([("task".to_string(), task.to_string())]),
        );
        window
    }

    /// Swap `window` to `layout` as a stage whose own layout it is.
    fn swap_to(window: &mut ContextWindow, layout: &ContextLayout) {
        let spec = spec_of(&blueprint_with(layout.regions.clone()));
        apply_stage_layout(window, &spec, 0, &spec.graph.layout);
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
        seed(&mut window, &bp, &seeds);

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
        seed(&mut window, &bp, &seeds);

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
        // the first pinned region (the task fallback), while a named key does not.
        let bp = blueprint_with(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let seeds = HashMap::from([("task".to_string(), "fallback text".to_string())]);
        let mut window = ContextWindow::new(100_000);
        seed(&mut window, &bp, &seeds);
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

        swap_to(&mut window, &new_layout);

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
        swap_to(&mut window, &omitting);

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
        swap_to(&mut window, &declaring);

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
        swap_to(&mut window, &next);

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

    /// The blueprint forms still build the same window as the spec forms,
    /// while spawning and stage entry hand them a blueprint.
    #[test]
    fn the_blueprint_forms_build_the_same_window() {
        let bp = blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("notes".to_string(), RegionKind::Temporary, 3000),
        ]);
        let mut old = ContextWindow::new(100_000);
        init_window(&mut old, &bp, "the task");
        let new = seeded_window(&bp, "the task");
        let shape = |w: &ContextWindow| {
            w.regions
                .iter()
                .map(|r| (r.name.clone(), r.max_tokens, r.content.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&old), shape(&new));
        let next = ContextLayout::new(
            vec![RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                4000,
            )],
            8000,
        );
        let mut by_spec = new.clone();
        apply_layout(&mut old, &next);
        swap_to(&mut by_spec, &next);
        assert_eq!(shape(&old), shape(&by_spec));
        assert_eq!(old.hidden, by_spec.hidden);
        // No pinned `task`: the task lands in the first pinned region.
        let bp = blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Temporary, 3000),
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 5000),
        ]);
        let mut old = ContextWindow::new(100_000);
        init_window(&mut old, &bp, "fallback");
        assert_eq!(old.get_region("system").unwrap().content.len(), 1);
    }

    #[test]
    fn a_percentage_budget_is_capped_then_floored() {
        let pct = |percent, min, max| Budget::Percent { percent, min, max };
        assert_eq!(budget_tokens(&Budget::Tokens(7), 100), 7);
        assert_eq!(budget_tokens(&pct(0.5, None, None), 1000), 500);
        assert_eq!(budget_tokens(&pct(0.5, None, Some(100)), 1000), 100);
        assert_eq!(budget_tokens(&pct(0.5, Some(900), Some(100)), 1000), 900);
    }

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
                    source: RegionName::new("conversation").unwrap(),
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
        assert_eq!(compaction_threshold(None, None, 1000), usize::MAX);
        assert_eq!(compaction_threshold(Some(50), None, 1000), 50);
        assert_eq!(compaction_threshold(None, Some(0.8), 1000), 800);
        assert_eq!(compaction_threshold(Some(50), Some(0.8), 1000), 50);
    }

    /// A region only per-stage layouts see, or one every global-layout stage
    /// hides, is sized against the entry stage's window; a stage with no
    /// plan entry for a region sizes it against its own.
    #[test]
    fn a_region_no_stage_resolved_is_sized_against_a_window() {
        let bp = blueprint_with(vec![RegionDefinition::new(
            "task".to_string(),
            RegionKind::Pinned,
            5000,
        )]);
        let mut spec = spec_of(&bp);
        spec.stages[0].region_budgets.clear();
        let mut half = spec.graph.layout.regions[0].clone();
        half.budget = Budget::Percent {
            percent: 0.5,
            min: None,
            max: None,
        };
        assert_eq!(layout_region_budget(&spec, &half), 50_000);
        assert_eq!(stage_region_budget(&spec, 0, &half), 50_000);
        assert_eq!(
            stage_region_budget(&spec, 9, &half),
            0,
            "no plan, no window"
        );
        spec.stages.clear();
        assert_eq!(layout_region_budget(&spec, &half), 0);
    }

    #[test]
    fn the_task_region_is_a_pinned_task_or_the_first_pinned_one() {
        let spec = spec_of(&blueprint_with(vec![
            RegionDefinition::new("task".to_string(), RegionKind::Temporary, 5000),
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 5000),
        ]));
        assert_eq!(task_region(&spec.graph.layout).as_deref(), Some("system"));
        let spec = spec_of(&blueprint_with(vec![RegionDefinition::new(
            "scratch".to_string(),
            RegionKind::Temporary,
            5000,
        )]));
        assert_eq!(task_region(&spec.graph.layout), None);
    }
}
