//! The questions every check asks of a run graph: which tools a stage names,
//! which groups it grants, how big a region can grow, and which regions an
//! input fills. Answered once here so two checks cannot disagree.

use leviath_runtime::spec::graph::{
    Budget, ModelChoice, RegionDef, RegionLayoutDef, RunGraph, StageDef, ToolGroup, ToolSelector,
};
use leviath_runtime::spec::inputs::InputSlot;
use leviath_runtime::spec::names::ModelRef;

/// The tools that change the workspace, which an output stage should not hold.
pub(super) const MODIFYING_TOOLS: &[&str] = &["write_file", "edit_file"];

/// The tool a stage hands back its final output with.
pub(super) const SUBMIT_OUTPUT_TOOL: &str = leviath_core::stage_tools::SUBMIT_OUTPUT_TOOL;

/// The tools a stage names one by one, groups left out.
pub(super) fn named_tools(stage: &StageDef) -> impl Iterator<Item = &str> {
    stage.tools.iter().filter_map(|t| match t {
        ToolSelector::Tool(name) => Some(name.as_str()),
        ToolSelector::Group(_) => None,
    })
}

/// The groups a stage grants, in the order it lists them.
pub(super) fn tool_groups(stage: &StageDef) -> Vec<ToolGroup> {
    stage
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSelector::Group(g) => Some(*g),
            ToolSelector::Tool(_) => None,
        })
        .collect()
}

/// How a group is written in a blueprint: `@builtin`.
pub(super) fn group_token(group: ToolGroup) -> &'static str {
    match group {
        ToolGroup::All => "@all",
        ToolGroup::Builtin => "@builtin",
        ToolGroup::Subagent => "@subagent",
        ToolGroup::Scripts => "@scripts",
        ToolGroup::Mcp => "@mcp",
    }
}

/// Whether `group` reaches a tool from `source`: `@all` reaches everything.
pub(super) fn covers(group: ToolGroup, source: ToolGroup) -> bool {
    group == ToolGroup::All || group == source
}

/// Whether the stage grants every built-in tool, by `@builtin` or `@all`.
pub(super) fn grants_all_builtins(stage: &StageDef) -> bool {
    tool_groups(stage)
        .into_iter()
        .any(|g| covers(g, ToolGroup::Builtin))
}

/// The tokens `budget` resolves to against a `window`: a percentage rounded,
/// then capped by `max`, then floored by `min` (the floor wins when the two
/// cross, since a region starved below a usable size is worse than one a
/// little over its cap).
pub(super) fn resolve_budget(budget: &Budget, window: usize) -> usize {
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

/// Every layout in the graph: its own, then each stage's that has one.
pub(super) fn layouts(graph: &RunGraph) -> impl Iterator<Item = &RegionLayoutDef> {
    std::iter::once(&graph.layout).chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()))
}

/// Whether some declared input fills `region` at spawn.
pub(super) fn bound_by_input(graph: &RunGraph, region: &RegionDef) -> bool {
    graph.inputs.iter().any(|i| {
        i.binds
            .iter()
            .any(|b| matches!(b, InputSlot::Region(r) if r.region == region.name))
    })
}

/// The mime patterns `stage` takes as parts: its own `input_accepts` when it
/// declares one, else the union of `accepts` over the regions it sees. Text
/// is never listed, and a visible region with no `accepts` takes anything,
/// reported as `*/*`.
pub(super) fn stage_inputs(graph: &RunGraph, stage: &StageDef) -> Vec<String> {
    if !stage.input_accepts.is_empty() {
        return stage
            .input_accepts
            .iter()
            .map(ToString::to_string)
            .collect();
    }
    let mut out: Vec<String> = Vec::new();
    for region in &graph.layout_for(stage).regions {
        if stage.hide.contains(&region.name) {
            continue;
        }
        let patterns: Vec<String> = match region.accepts.is_empty() {
            true => vec!["*/*".to_string()],
            false => region.accepts.iter().map(ToString::to_string).collect(),
        };
        for p in patterns {
            if !p.starts_with("text/") && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// A model entry as `(provider, model)`, the provider empty when the entry
/// leaves the route open.
pub(super) fn route(entry: &ModelRef) -> (&str, &str) {
    (
        entry.provider.as_ref().map_or("", |p| p.as_str()),
        entry.model.as_str(),
    )
}

/// Every model entry the graph's stages name, in stage order.
pub(super) fn model_entries(graph: &RunGraph) -> impl Iterator<Item = &ModelRef> {
    graph.stages.iter().flat_map(|s| s.model.models.iter())
}

/// Whether every entry of a model choice pins its provider, so a check that
/// reads compiled per-provider tables can judge the whole list.
pub(super) fn all_pinned(model: &ModelChoice) -> bool {
    !model.models.is_empty() && model.models.iter().all(|m| m.provider.is_some())
}
