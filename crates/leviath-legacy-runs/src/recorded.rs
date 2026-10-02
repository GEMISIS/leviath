//! The graph of an old run whose blueprint cannot be read, from what the run
//! itself recorded.
//!
//! A run that finished never runs again, so it does not need its blueprint to
//! be read back: its stages, the models they ran on, the edges it took and
//! the regions its context held are all in its own files. The graph built
//! here has exactly those, and nothing a run would need to go on (no prompts,
//! no tools), which is why a run converted this way is marked as one that
//! never resumes.

use std::collections::BTreeSet;

use leviath_core::region::{Admission, Volatility};
use leviath_core::run_meta::{ContextSnapshot, RegionSnapshot, RunMeta};
use leviath_runtime::spec::graph::{Budget, EdgeDef, RegionDef, RegionKind, RunGraph, StageDef};
use leviath_runtime::spec::names::{ModelRef, RegionName, StageName};
use serde_json::json;

use crate::context::n32;
use crate::journal::JournalRecord;
use crate::legacy::LegacyRun;
use crate::report::Report;

/// What a stage is called when the run recorded none.
const UNNAMED_STAGE: &str = "stage";

/// The graph the run recorded.
pub(crate) fn graph(old: &LegacyRun, report: &mut Report) -> RunGraph {
    let path = stage_path(old);
    let mut names: Vec<StageName> = Vec::new();
    let ledger = old.stages.iter().map(|r| r.name.as_str());
    for name in ledger.chain(path.iter().map(StageName::as_str)) {
        if let Ok(name) = StageName::new(name)
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    if names.is_empty() {
        names.push(StageName::new(UNNAMED_STAGE).expect("a plain word is a stage name"));
    }
    let launched = old
        .meta()
        .model
        .as_deref()
        .and_then(|m| ModelRef::parse(m).ok());
    let stages = names
        .iter()
        .map(|name| {
            stage(
                name,
                models(old, name).or_else(|| launched.clone().map(|m| vec![m])),
            )
        })
        .collect();
    let mut graph: RunGraph = serde_json::from_value(json!({
        "description": "the graph this run recorded: its blueprint could not be read when it was converted",
        "stages": [],
        "inputs": [{ "name": "task", "type": "text" }],
        "layout": { "total_budget_tokens": n32(old.folded.context.max_tokens), "regions": [] },
    }))
    .expect("a recorded graph's frame reads");
    graph.stages = stages;
    graph.edges = edges(&path);
    graph.layout.regions = regions(old);
    report.fill(
        "graph",
        "what the run recorded",
        "the run's blueprint could not be read; the graph has the stages it entered, the models they ran on, the edges it took and its regions, and the run never resumes",
    );
    graph
}

/// A stage the run recorded running that `graph` does not have, if any.
pub(crate) fn missing_stage(old: &LegacyRun, graph: &RunGraph) -> Option<String> {
    let ledger = old
        .stages
        .iter()
        .filter_map(|r| StageName::new(r.name.as_str()).ok());
    ledger
        .chain(stage_path(old))
        .map(|s| s.to_string())
        .find(|name| graph.stage(name).is_none())
}

/// Every stage the run was recorded in, in order, each change once.
fn stage_path(old: &LegacyRun) -> Vec<StageName> {
    let metas = old.records.iter().filter_map(|r| match r {
        JournalRecord::Header { meta, .. }
        | JournalRecord::Progress { meta, .. }
        | JournalRecord::Checkpoint { meta, .. } => Some(&**meta),
        _ => None,
    });
    let mut path: Vec<StageName> = Vec::new();
    for meta in metas.chain(std::iter::once(old.meta())) {
        let Ok(stage) = StageName::new(meta.current_stage.as_str()) else {
            continue;
        };
        if path.last() != Some(&stage) {
            path.push(stage);
        }
    }
    path
}

/// The models the run's ledger recorded `stage` running on, latest first.
fn models(old: &LegacyRun, stage: &StageName) -> Option<Vec<ModelRef>> {
    let record = old.stages.iter().find(|r| r.name == stage.as_str())?;
    let mut models: Vec<ModelRef> = record
        .models
        .iter()
        .rev()
        .filter_map(crate::plan::ledger_model)
        .collect();
    models.dedup();
    Some(models).filter(|m| !m.is_empty())
}

/// A stage with nothing but its name and the models it ran on.
fn stage(name: &StageName, models: Option<Vec<ModelRef>>) -> StageDef {
    let mut stage: StageDef =
        serde_json::from_value(json!({ "name": name })).expect("a stage with only a name reads");
    stage.model.models = models.unwrap_or_default();
    stage.model.allow_user_default = false;
    stage
}

/// One edge for each move between stages the run made, each once.
fn edges(path: &[StageName]) -> Vec<EdgeDef> {
    let mut seen = BTreeSet::new();
    path.windows(2)
        .filter(|w| seen.insert((w[0].clone(), w[1].clone())))
        .map(|w| {
            serde_json::from_value(json!({ "name": w[1], "from": w[0], "to": w[1] }))
                .expect("an edge between two stage names reads")
        })
        .collect()
}

/// Every region the run's context held, first as it started then as it last
/// stood, each once.
fn regions(old: &LegacyRun) -> Vec<RegionDef> {
    let snapshots: [Option<&ContextSnapshot>; 2] = [old.first_context(), Some(&old.folded.context)];
    let mut out: Vec<RegionDef> = Vec::new();
    for r in snapshots.into_iter().flatten().flat_map(|s| &s.regions) {
        if let Ok(name) = RegionName::new(r.name.as_str())
            && !out.iter().any(|d| d.name == name)
        {
            out.push(region(name, r));
        }
    }
    out
}

/// A region shaped as the snapshot shows it.
fn region(name: RegionName, r: &RegionSnapshot) -> RegionDef {
    let kind = match r.kind.as_str() {
        "temporary" => RegionKind::Temporary,
        "clearable" => RegionKind::Clearable,
        "compacting" => RegionKind::Compacting {
            threshold_tokens: None,
        },
        "compact_history" | "history" => RegionKind::CompactHistory { source: None },
        "sliding_window" | "sliding" => RegionKind::SlidingWindow {
            max_items: n32(r.entries.len()).max(1),
            eviction: Default::default(),
        },
        "keyed" => RegionKind::Keyed { max_entries: None },
        "checklist" => RegionKind::Checklist,
        _ => RegionKind::Pinned,
    };
    RegionDef {
        name,
        kind,
        budget: Budget::Tokens(n32(r.max_tokens)),
        compact_at: None,
        description: None,
        describe_in_prompt: false,
        required: false,
        required_message: None,
        summarizable: true,
        admission: Admission::default(),
        volatility: Volatility::default(),
        seed: None,
        accepts: Vec::new(),
    }
}

/// The note a run that was not finished when it was converted gets: what it
/// was doing, and why it cannot carry on.
pub(crate) fn stopped(meta: &RunMeta, why: &str) -> String {
    let doing = serde_json::to_value(&meta.status)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_default();
    format!("the run was {doing} when it was converted, and cannot resume: {why}")
}
