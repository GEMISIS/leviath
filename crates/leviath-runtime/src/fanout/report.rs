//! The consolidated results report a fan-out hands its merge stage, and the
//! status helpers the fan-out systems share. Moved out of `fanout.rs` whole;
//! nothing here changed but the file it lives in.

use super::*;

/// Smallest per-worker share worth writing, in bytes.
///
/// Below this a section says nothing useful, and the honest move is to tell the
/// merge stage that the results are too many to carry rather than hand it a
/// hundred fragments. That is what `max_items` on the fan-out config is for.
pub(super) const MIN_REPORT_BYTES_PER_WORKER: usize = 200;

/// Per-worker share when the results region's budget cannot be read.
pub(super) const DEFAULT_REPORT_BYTES_PER_WORKER: usize = 4_000;

/// Marker appended to a worker's section that was cut to fit the report.
pub(super) const REPORT_TRUNCATION_MARKER: &str =
    "\n[...truncated; read this worker's own run for the full answer]";

/// How many bytes each worker's section may use, given the region's token
/// budget and how many workers there are.
///
/// An equal share, so every worker appears. The first cut at this capped each
/// worker at a fixed size and then trimmed the finished report to fit, which
/// meant the early workers got their full allowance and the late ones were cut
/// off entirely - a hundred-way fan-out where only the first twenty were
/// readable, with nothing saying so.
pub(super) fn bytes_per_worker(region_budget_tokens: Option<usize>, workers: usize) -> usize {
    let Some(tokens) = region_budget_tokens.filter(|t| *t > 0) else {
        return DEFAULT_REPORT_BYTES_PER_WORKER;
    };
    // The workspace's bytes-over-four estimate, minus a margin for the header
    // and the per-worker `## worker <id>` lines.
    let usable = tokens.saturating_mul(4).saturating_mul(9) / 10;
    (usable / workers.max(1)).max(MIN_REPORT_BYTES_PER_WORKER)
}

/// One worker's contribution, trimmed to `budget` bytes.
pub(super) fn fit_worker_section(content: &str, budget: usize) -> String {
    if content.len() <= budget {
        return content.to_string();
    }
    let room = budget.saturating_sub(REPORT_TRUNCATION_MARKER.len());
    format!(
        "{}{REPORT_TRUNCATION_MARKER}",
        leviath_core::truncate_at_boundary(content, room)
    )
}

/// Build the consolidated `[fan_out results: …]` report from worker outcomes.
///
/// `region_budget_tokens` is the results region's budget, which the workers'
/// sections divide equally between them.
pub(super) fn build_report(
    summaries: &[(String, String)],
    failures: &[(String, String)],
    region_budget_tokens: Option<usize>,
) -> String {
    let sections = summaries.len().max(1);
    let budget = bytes_per_worker(region_budget_tokens, sections);
    let mut report = format!(
        "[fan_out results: {} succeeded, {} failed]\n",
        summaries.len(),
        failures.len()
    );
    // Say the share out loud when it is tight, so the merge stage knows it is
    // reading extracts and can go to a worker's own run for the rest.
    if summaries.iter().any(|(_, c)| c.len() > budget) {
        report.push_str(&format!(
            "[each worker's answer is shown up to {budget} characters; \
             read a worker's own run for the whole thing]\n"
        ));
    }
    for (id, content) in summaries {
        report.push_str(&format!(
            "\n## worker {id}\n{}\n",
            fit_worker_section(content, budget)
        ));
    }
    for (id, err) in failures {
        report.push_str(&format!("\n## worker {id} FAILED\n{err}\n"));
    }
    report
}

/// Add `text` to the parent's results region, trimming it to fit.
///
/// `add_entry` rejects an over-budget entry outright, so a report too big for
/// the region would leave the merge stage with nothing at all. Trimming first
/// means the merge always receives *something*, and a report that had to be cut
/// says so where the model will read it.
pub(super) fn inject_results(world: &mut World, parent: Entity, region: &str, text: &str) {
    let Some(mut window) = world.get_mut::<ContextWindow>(parent) else {
        return;
    };
    // A named region the layout does not declare would silently swallow the
    // whole report, so fall back to the one every agent has. `lev validate`
    // catches the typo before a run gets here.
    let region = match window.get_region(region).is_some() {
        true => region,
        false => {
            tracing::warn!(
                region = %region,
                "fan-out results region is not in this agent's layout; using conversation"
            );
            "conversation"
        }
    };
    let budget = window
        .get_region(region)
        .map(|r| r.max_tokens.saturating_sub(r.current_tokens))
        .unwrap_or(0);
    let allowed = budget.saturating_mul(4);
    let fitted = match text.len() <= allowed {
        true => text.to_string(),
        false => {
            let room = allowed.saturating_sub(REPORT_TRUNCATION_MARKER.len());
            format!(
                "{}{REPORT_TRUNCATION_MARKER}",
                leviath_core::truncate_at_boundary(text, room)
            )
        }
    };
    let tokens = leviath_core::estimate_tokens(&fitted);
    let _ = window.add_typed_entry(region, leviath_core::EntryKind::UserMessage, fitted, tokens);
}

/// An agent's status, if it still exists.
pub(super) fn agent_status(world: &World, entity: Entity) -> Option<AgentStatus> {
    world.get::<AgentState>(entity).map(|s| s.status.clone())
}

/// Set an agent's status (no-op if it despawned).
pub(super) fn set_status(world: &mut World, entity: Entity, status: AgentStatus) {
    if let Some(mut state) = world.get_mut::<AgentState>(entity) {
        state.status = status;
    }
}
