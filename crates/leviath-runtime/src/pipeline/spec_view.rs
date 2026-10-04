//! What the pipeline reads off a run's resolved spec.
//!
//! The systems read the graph (stages, edges, layout) and each stage's plan
//! straight from [`RunSpecC`](crate::insert::RunSpecC). The runtime values a
//! stage is entered with, its [`StageSetup`] and its [`StageInference`], are
//! worked out here from those two, so they are always what the spec says and
//! never a second copy that could drift from it.

use std::collections::{BTreeMap, HashSet};

use leviath_providers::Tool;

use super::{StageInference, StageSetup};
use crate::components::InferenceConfig;
use crate::spec::graph::{
    Budget, CodeRef, EdgeDef, Eviction, NudgeDef, OutputDef, RegionDef, RegionKind,
    RegionLayoutDef, ResolvedNudge, RunGraph, StageDef, StageMode, ToolGroup, ToolSelector,
};
use crate::spec::run_spec::{RunSpec, StagePlan, ToolDef};

/// The context window a stage is budgeted against when its plan is missing.
const FALLBACK_WINDOW: u32 = super::DEFAULT_CONTEXT_WINDOW_TOKENS as u32;

/// The text a piece of code is filed under in a run's compiled-code tables:
/// the path of a file, or the source of inline code.
pub(crate) fn code_key(code: &CodeRef) -> &str {
    match code {
        CodeRef::File(path) => path,
        CodeRef::Inline(source) => source,
    }
}

/// A stage's position in the graph.
pub(crate) fn stage_index(graph: &RunGraph, name: &str) -> Option<usize> {
    graph.stages.iter().position(|s| s.name.as_str() == name)
}

/// Where a run starts: its entry stage, or the first.
pub(crate) fn entry_index(graph: &RunGraph) -> usize {
    graph
        .entry
        .as_ref()
        .and_then(|e| stage_index(graph, e.as_str()))
        .unwrap_or(0)
}

/// Whether a stage's tool list grants `group`, directly or through `all`.
pub(crate) fn grants_group(stage: &StageDef, group: ToolGroup) -> bool {
    stage
        .tools
        .iter()
        .any(|t| matches!(t, ToolSelector::Group(g) if *g == ToolGroup::All || *g == group))
}

/// Whether a stage is granted every built-in tool.
pub(crate) fn grants_all_builtins(stage: &StageDef) -> bool {
    grants_group(stage, ToolGroup::Builtin)
}

/// The tools a stage names one by one.
pub(crate) fn named_tools(stage: &StageDef) -> impl Iterator<Item = &str> {
    stage.tools.iter().filter_map(|t| match t {
        ToolSelector::Tool(name) => Some(name.as_str()),
        ToolSelector::Group(_) => None,
    })
}

/// The regions a stage shows its model: its layout's, the ones the runtime
/// always shows, less the ones it hides.
pub(crate) fn visible_regions<'a>(graph: &'a RunGraph, stage: &'a StageDef) -> HashSet<&'a str> {
    let mut names: HashSet<&str> = graph
        .layout_for(stage)
        .regions
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    names.extend(crate::spec::graph::ALWAYS_VISIBLE_REGIONS);
    for hidden in &stage.hide {
        names.remove(hidden.as_str());
    }
    names
}

/// Whether an input of the run is bound to `region`, so the caller fills it
/// at spawn rather than the model during the run.
pub(crate) fn input_fills(graph: &RunGraph, region: &str) -> bool {
    graph.inputs.iter().any(|input| {
        input.binds.iter().any(|slot| {
            matches!(slot, crate::spec::inputs::InputSlot::Region(b) if b.region.as_str() == region)
        })
    })
}

/// The edges leaving a stage, in the graph's order.
pub(crate) fn edges_from<'a>(graph: &'a RunGraph, stage: &'a StageDef) -> Vec<&'a EdgeDef> {
    graph.edges_from(stage.name.as_str()).collect()
}

/// A stage's plan, by position.
fn plan(spec: &RunSpec, idx: usize) -> Option<&StagePlan> {
    let name = spec.graph.stages.get(idx)?.name.as_str();
    spec.stage(name)
}

/// The window a stage's percentage budgets are sized against.
fn window_of(spec: &RunSpec, idx: usize) -> u32 {
    plan(spec, idx).map_or(FALLBACK_WINDOW, |p| p.context_window)
}

/// One region's budget in one stage: the plan's figure, or the region's own
/// budget sized against the stage's window when the plan has none.
fn budget_in(spec: &RunSpec, idx: usize, region: &RegionDef) -> usize {
    let planned = plan(spec, idx).and_then(|p| p.region_budgets.get(region.name.as_str()));
    match planned {
        Some(tokens) => *tokens as usize,
        None => region.budget.resolve(window_of(spec, idx) as usize),
    }
}

/// The source a history region names in the window's vocabulary. A history
/// with no source names the empty text, which no region is called, so
/// compaction never writes into it.
pub(crate) fn history_source(source: Option<&crate::spec::names::RegionName>) -> String {
    source.map(ToString::to_string).unwrap_or_default()
}

/// A region's kind in the vocabulary the window keeps, for a region holding
/// `budget` tokens.
pub(crate) fn region_kind(region: &RegionDef, budget: usize) -> leviath_core::RegionKind {
    use leviath_core::EvictionStrategy;
    use leviath_core::RegionKind as K;
    match &region.kind {
        RegionKind::Pinned => K::Pinned,
        RegionKind::SlidingWindow {
            max_items,
            eviction,
        } => K::SlidingWindow {
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
        RegionKind::Temporary => K::Temporary,
        RegionKind::Compacting { threshold_tokens } => K::Compacting {
            threshold_tokens: region.compaction_threshold(*threshold_tokens, budget),
        },
        RegionKind::Clearable => K::Clearable,
        RegionKind::CompactHistory { source } => K::CompactHistory {
            source_region: history_source(source.as_ref()),
        },
        RegionKind::Keyed { max_entries } => K::HashMap {
            max_entries: max_entries.map(|m| m as usize),
        },
        RegionKind::Checklist => K::Checklist,
        RegionKind::Custom { code, pinned } => K::Custom {
            script: code_key(code).to_string(),
            pinned: *pinned,
        },
    }
}

/// A layout's regions as a window holds them, each region's budget chosen by
/// `budget`. Compaction thresholds that are a share of a region's budget are
/// worked out from the chosen budget.
fn regions_with(
    def: &RegionLayoutDef,
    budget: impl Fn(&RegionDef) -> usize,
) -> Vec<leviath_core::Region> {
    def.regions
        .iter()
        .map(|r| crate::context_setup::region_from_def(r, budget(r)))
        .collect()
}

/// A layout laid out for a window: its regions, each holding its budget in
/// tokens, and the window's whole budget.
pub(crate) struct LaidOut {
    /// The regions, in the layout's order.
    pub regions: Vec<leviath_core::Region>,
    /// The tokens the whole window may hold.
    pub total: usize,
}

/// Whether any region of a layout is budgeted as a share of the window.
fn has_percent(def: &RegionLayoutDef) -> bool {
    def.regions
        .iter()
        .any(|r| matches!(r.budget, Budget::Percent { .. }))
}

/// The graph's own layout, as the run's window starts with it.
///
/// A region's budget is the smallest any stage that uses this layout and can
/// see the region gives it, so a region budgeted for a wide-window stage is
/// never counted against a narrow one that shares it. A region no such stage
/// sees takes the first stage's budget, which nothing reads.
pub(crate) fn graph_layout(spec: &RunSpec) -> LaidOut {
    let graph = &spec.graph;
    let seeing = |region: &str| -> Vec<usize> {
        graph
            .stages
            .iter()
            .enumerate()
            .filter(|(_, s)| s.layout.is_none() && visible_regions(graph, s).contains(region))
            .map(|(i, _)| i)
            .collect()
    };
    let window_for = |region: &str| -> usize {
        seeing(region)
            .into_iter()
            .map(|i| window_of(spec, i) as usize)
            .min()
            .unwrap_or(window_of(spec, 0) as usize)
    };
    let total = match has_percent(&graph.layout) {
        true => graph
            .layout
            .regions
            .iter()
            .map(|r| window_for(r.name.as_str()))
            .max()
            .unwrap_or(graph.layout.total_budget_tokens as usize),
        false => graph.layout.total_budget_tokens as usize,
    };
    let regions = regions_with(&graph.layout, |r| {
        seeing(r.name.as_str())
            .into_iter()
            .map(|i| budget_in(spec, i, r))
            .min()
            .unwrap_or_else(|| budget_in(spec, 0, r))
    });
    LaidOut { regions, total }
}

/// A stage's own layout's regions, sized for that stage, when it declares one.
pub(crate) fn stage_layout(spec: &RunSpec, idx: usize) -> Option<Vec<leviath_core::Region>> {
    let def = spec.graph.stages.get(idx)?.layout.as_ref()?;
    Some(regions_with(def, |r| budget_in(spec, idx, r)))
}

/// The nudge a stage sends when its model answers with text alone: each
/// setting from the stage, else the graph, else `global`.
///
/// With nothing set anywhere, a stage whose output is reviewed is not nudged.
/// A stage with interaction points presents what it writes for the user to
/// approve, revise or edit - the text is the work product, not a model
/// stalling before it starts - and the nudge says "use your tools to complete
/// the task" to a stage built to produce a document, which usually has no tool
/// that could. An explicit `enabled` at any level speaks for itself.
pub(crate) fn stage_nudge(
    graph: &RunGraph,
    stage: Option<&StageDef>,
    global: Option<&NudgeDef>,
) -> ResolvedNudge {
    let reviewed = matches!(
        stage.map(|s| &s.mode),
        Some(StageMode::InteractivePoints(points)) if !points.is_empty()
    );
    NudgeDef::resolve(
        global,
        graph.nudge.as_ref(),
        stage.and_then(|s| s.nudge.as_ref()),
        reviewed,
    )
}

/// An output shape in the vocabulary the output tool and the prompt read.
pub(crate) fn output_spec(def: &OutputDef) -> leviath_core::output::OutputSpec {
    leviath_core::output::OutputSpec {
        format: def.format.clone(),
        instructions: def.instructions.clone(),
        example: def.example.clone(),
        schema: def.schema.as_ref().map(|s| s.value().clone()),
        validator: def.validator.as_ref().map(|c| code_key(c).to_string()),
        on_validator_error: def.on_validator_error,
        overwrite_artifacts: def.overwrite_artifacts,
        artifacts: def
            .artifacts
            .iter()
            .map(|a| leviath_core::output::ArtifactSpec {
                name: a.name.clone(),
                mime_type: a.mime_type.to_string(),
                required: a.required,
                description: a.description.clone(),
            })
            .collect(),
    }
}

/// A tool as a provider is told about it.
pub(crate) fn tool(def: &ToolDef) -> Tool {
    Tool {
        name: def.name.to_string(),
        description: def.description.clone(),
        parameters: def.schema.value().clone(),
    }
}

/// The instructions a stage is entered with: its own prompt, a fan-out's split
/// prompt, and the demand for a final output when it owes one.
fn system_prompt(
    stage: &StageDef,
    output: Option<&leviath_core::output::OutputSpec>,
) -> Option<String> {
    let base = stage.system_prompt.clone();
    let prompt = match &stage.mode {
        StageMode::FanOut(f) if !f.split_prompt.trim().is_empty() => Some(match base {
            Some(base) => format!("{base}\n\n{}", f.split_prompt),
            None => f.split_prompt.clone(),
        }),
        _ => base,
    };
    let (Some(spec), true) = (output, stage.require_output) else {
        return prompt;
    };
    let tool = leviath_core::stage_tools::SUBMIT_OUTPUT_TOOL;
    let described = leviath_core::describe_spec(spec);
    let demand = match described.is_empty() {
        true => format!(
            "Before this stage ends you must call `{tool}` with your final answer. It is \
             the only thing the caller receives."
        ),
        false => format!(
            "Before this stage ends you must call `{tool}` with your final answer. It is \
             the only thing the caller receives.\n\n{described}"
        ),
    };
    Some(match prompt {
        Some(base) => format!("{base}\n\n{demand}"),
        None => demand,
    })
}

/// How a stage is entered: its inference settings, its routing, whether it
/// takes messages, its layout, what it hides and resets, and its prompt.
pub(crate) fn stage_setup(spec: &RunSpec, idx: usize) -> StageSetup {
    let graph = &spec.graph;
    let Some(stage) = graph.stages.get(idx) else {
        return StageSetup::default();
    };
    let output = plan(spec, idx)
        .and_then(|p| p.output.as_ref())
        .map(output_spec);
    let hints = leviath_core::config::PromptHints::default();
    let params = &stage.model.params;
    StageSetup {
        inference_config: InferenceConfig {
            temperature: params.temperature.map(|t| t as f32),
            max_output_tokens: params.max_output_tokens.clone(),
            extra_params: params
                .extra
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect(),
            as_text: stage
                .input_as_text
                .iter()
                .map(ToString::to_string)
                .collect(),
            batch_tool_hint: leviath_core::taint::resolve_batch_tool_hint(
                hints.batch_tool,
                graph.batch_tool_hint,
                stage.batch_tool_hint,
            ),
            shell_hint: leviath_core::taint::resolve_shell_hint(
                hints.shell,
                graph.shell_hint,
                stage.shell_hint,
            ),
            request_timeout_secs: stage.model.request_timeout_secs,
        },
        routing: stage.tool_routing.clone(),
        accepts_messages: stage.accepts_messages,
        context_layout: stage_layout(spec, idx),
        context_hide: stage.hide.iter().map(ToString::to_string).collect(),
        context_reset: stage.reset.iter().map(ToString::to_string).collect(),
        system_prompt: system_prompt(stage, output.as_ref()),
    }
}

/// Tool lists a run has looked up again since it was spawned, by stage
/// position. A stage with an entry here advertises that list instead of its
/// plan's, so a revisit keeps what the last look found.
#[derive(bevy_ecs::component::Component, Debug, Clone, Default)]
pub(crate) struct StageToolOverrides(pub BTreeMap<usize, Vec<Tool>>);

/// Who a stage calls and with what: its provider, model, tools, fallbacks and
/// output shape, from its plan.
pub(crate) fn stage_inference(
    spec: &RunSpec,
    idx: usize,
    overrides: Option<&StageToolOverrides>,
) -> StageInference {
    let Some(plan) = plan(spec, idx) else {
        return StageInference::default();
    };
    let tools = match overrides.and_then(|o| o.0.get(&idx)) {
        Some(tools) => tools.clone(),
        None => plan.tools.iter().map(tool).collect(),
    };
    StageInference {
        provider_name: plan.provider.to_string(),
        model: plan.model.to_string(),
        tools,
        tool_filter: None,
        fallbacks: plan.fallbacks.clone(),
        output: plan.output.as_ref().map(output_spec),
    }
}

/// Whether any stage of a graph can change a file the framework would see,
/// the same test the modification gate applies.
pub(crate) fn any_stage_can_modify(graph: &RunGraph) -> bool {
    graph.stages.iter().any(|stage| {
        let gate_tools: Vec<_> = edges_from(graph, stage)
            .iter()
            .filter_map(|e| e.gate.as_ref())
            .flat_map(|g| g.tools.iter().cloned())
            .collect();
        stage.can_change_files(&gate_tools)
    })
}

#[cfg(test)]
#[path = "spec_view_tests.rs"]
pub(crate) mod tests;
