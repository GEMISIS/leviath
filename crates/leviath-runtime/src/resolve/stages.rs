//! Steps 9 and 11: each stage decided, and what the run relies on from the
//! machine.

use std::collections::{BTreeMap, BTreeSet};

use super::{Source, output, rebase};
use crate::dynamic_interaction::BLOCKING_INTERACTION_TOOLS;
use crate::spec::env::{CodeFiles, ModelPlan, ResolveEnv};
use crate::spec::graph::{Budget, CodeRef, OutputCap, RegionKind, RunGraph, StageDef};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::RegionName;
use crate::spec::request::SpawnRequest;
use crate::spec::run_spec::{AutoAnswers, EnvFingerprint, StagePlan, ToolDef, ToolSource};

/// Below this many tokens of room after its fixed regions, a stage cannot
/// hold a conversation.
const MIN_WORKING_TOKENS: u32 = 8000;

/// The working-room floor applies only to windows at least this large; a toy
/// window is left alone.
const WORKING_ROOM_MIN_WINDOW: u32 = 20_000;

/// Plan every stage, in graph order. `None` when any stage's model could not
/// be chosen: budgets are sized against every stage's window, so none can be
/// finished without all of them.
pub(super) async fn plan_all(
    graph: &RunGraph,
    request: &SpawnRequest,
    auto: &AutoAnswers,
    code: &CodeFiles,
    src: &Source,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) -> Option<(Vec<StagePlan>, Vec<(CodeRef, Vec<u8>)>)> {
    let at = &src.at;
    let mut chosen = Vec::new();
    let mut found = Vec::new();
    for stage in &graph.stages {
        let sat = at.field("stages").key(stage.name.as_str());
        let requested = request
            .model
            .as_ref()
            .filter(|_| stage.model.allow_user_default);
        let model = match env.model(stage, requested).await {
            Ok(plan) => Some(plan),
            Err(issue) => {
                issues.push(rebase(&sat.field("model"), issue));
                None
            }
        };
        let tools = match env.tools(graph, stage, code, src.base.as_deref()).await {
            Ok(got) => {
                found.extend(got.code);
                got.tools
            }
            Err(refused) => {
                let base = sat.field("tools");
                issues.absorb(SpawnIssues(
                    refused.0.into_iter().map(|i| rebase(&base, i)).collect(),
                ));
                Vec::new()
            }
        };
        chosen.push((model, tools));
    }
    let chosen: Vec<(ModelPlan, Vec<ToolDef>)> = chosen
        .into_iter()
        .map(|(model, tools)| model.map(|m| (m, tools)))
        .collect::<Option<_>>()?;
    let windows: Vec<u32> = chosen.iter().map(|(m, _)| m.context_window).collect();
    let mut plans = Vec::new();
    for (i, (stage, (model, tools))) in graph.stages.iter().zip(chosen).enumerate() {
        let sat = at.field("stages").key(stage.name.as_str());
        let budgets = budgets(graph, i, &windows);
        working_room(graph, stage, windows[i], &budgets, at, issues);
        let tools = stage_tools(stage, tools, auto, &sat, issues);
        plans.push(plan(graph, stage, request, model, tools, budgets));
    }
    Some((plans, found))
}

fn plan(
    graph: &RunGraph,
    stage: &StageDef,
    request: &SpawnRequest,
    model: ModelPlan,
    mut tools: Vec<ToolDef>,
    region_budgets: BTreeMap<RegionName, u32>,
) -> StagePlan {
    let shape = output::cascade(
        graph.output.as_ref(),
        stage.output.as_ref(),
        request.output.as_ref(),
    );
    output::describe_submit(&mut tools, shape.as_ref());
    let max_output_tokens = output_cap(
        stage.model.params.max_output_tokens.as_ref(),
        &model,
        &region_budgets,
    );
    StagePlan {
        stage: stage.name.clone(),
        provider: model.provider,
        model: model.model,
        context_window: model.context_window,
        max_output_tokens,
        fallbacks: model.fallbacks,
        tools,
        output: shape,
        region_budgets,
        notes: model.notes,
    }
}

/// The tools a stage really gets.
///
/// A run whose questions nobody answers (unattended, under a setting that
/// does not keep them for a person) loses every tool whose only outcome is a
/// prompt for a person, unless the stage names it in `required_tools`: the
/// model never sees it, so it decides for itself instead of spending a turn to
/// be told nobody is there. This is the one place that cut is made. Then every
/// tool the stage requires must be there.
fn stage_tools(
    stage: &StageDef,
    mut tools: Vec<ToolDef>,
    auto: &AutoAnswers,
    sat: &SpecPath,
    issues: &mut SpawnIssues,
) -> Vec<ToolDef> {
    if auto.questions {
        tools.retain(|t| {
            !BLOCKING_INTERACTION_TOOLS.contains(&t.name.as_str())
                || stage.required_tools.contains(&t.name)
        });
    }
    for (j, required) in stage.required_tools.iter().enumerate() {
        if !tools.iter().any(|t| t.name == *required) {
            issues.push(
                SpawnIssue::new(
                    sat.field("required_tools").index(j),
                    IssueCode::Unresolvable,
                    format!("the stage requires the tool \"{required}\", and this machine does not offer it"),
                )
                .hint("install or connect what provides it, or take it out of required_tools")
                .known(tools.iter().map(|t| &t.name)),
            );
        }
    }
    tools
}

/// Each region's budget in a stage, in tokens.
///
/// A stage with its own layout sizes its regions against its own window. A
/// region of the graph's layout is shared by every stage that uses that
/// layout and can see it, so a percentage budget is sized against the
/// smallest of their windows: a region budgeted for a wide-window stage must
/// not overflow a narrow one. A region no such stage sees takes the first
/// stage's window, which nothing at run time consults.
fn budgets(graph: &RunGraph, index: usize, windows: &[u32]) -> BTreeMap<RegionName, u32> {
    let stage = &graph.stages[index];
    match &stage.layout {
        Some(layout) => layout
            .regions
            .iter()
            .map(|r| (r.name.clone(), budget(&r.budget, windows[index])))
            .collect(),
        None => graph
            .layout
            .regions
            .iter()
            .map(|r| {
                let window = graph
                    .stages
                    .iter()
                    .zip(windows)
                    .filter(|(s, _)| s.layout.is_none() && !s.hide.contains(&r.name))
                    .map(|(_, w)| *w)
                    .min()
                    .unwrap_or(windows[0]);
                (r.name.clone(), budget(&r.budget, window))
            })
            .collect(),
    }
}

/// A budget in tokens against `window`: a percentage is rounded, capped at
/// `max`, then floored at `min`. The floor wins when the two cross, since a
/// region starved below a usable size is worse than one slightly over its cap.
pub(super) fn budget(budget: &Budget, window: u32) -> u32 {
    match budget {
        Budget::Tokens(n) => *n,
        Budget::Percent { percent, min, max } => {
            let share = (f64::from(window) * percent).round() as u32;
            share.min(max.unwrap_or(u32::MAX)).max(min.unwrap_or(0))
        }
    }
}

/// Refuse a stage whose fixed regions leave the model too little room to
/// work in, judged against its own window over the regions it sees.
fn working_room(
    graph: &RunGraph,
    stage: &StageDef,
    window: u32,
    budgets: &BTreeMap<RegionName, u32>,
    at: &SpecPath,
    issues: &mut SpawnIssues,
) {
    let fixed: u32 = graph
        .layout_for(stage)
        .regions
        .iter()
        .filter(|r| !stage.hide.contains(&r.name))
        .filter(|r| {
            matches!(
                r.kind,
                RegionKind::Pinned
                    | RegionKind::Keyed { .. }
                    | RegionKind::CompactHistory { .. }
                    | RegionKind::Custom { pinned: true, .. }
            )
        })
        .map(|r| budgets.get(&r.name).copied().unwrap_or(0))
        .fold(0, u32::saturating_add);
    let working = window.saturating_sub(fixed);
    if window >= WORKING_ROOM_MIN_WINDOW && working < MIN_WORKING_TOKENS {
        let path = match stage.layout {
            Some(_) => at.field("stages").key(stage.name.as_str()).field("layout"),
            None => at.field("layout"),
        };
        issues.push(
            SpawnIssue::new(
                path,
                IssueCode::OutOfRange,
                format!(
                    "stage \"{}\" has only {working} tokens to work in after its fixed regions \
                     take {fixed} of its {window}-token window",
                    stage.name
                ),
            )
            .expected(format!("at least {MIN_WORKING_TOKENS} working tokens"))
            .hint("shrink the pinned, keyed and history regions, or run the stage on a model with a larger window"),
        );
    }
}

/// The cap on one reply, in tokens, as the pipeline works it out for each
/// request: a relative cap is never more than the model's own maximum reply,
/// and never less than one token. A cap on a region the stage does not carry
/// is the model's own maximum, which is what the author was reaching for.
fn output_cap(
    cap: Option<&OutputCap>,
    model: &ModelPlan,
    budgets: &BTreeMap<RegionName, u32>,
) -> Option<u32> {
    let most = model.max_output_tokens.max(1);
    let share =
        |whole: u32, fraction: f64| ((f64::from(whole) * fraction).round() as u32).clamp(1, most);
    match cap? {
        OutputCap::Tokens(n) => Some(*n),
        OutputCap::WindowPercent(fraction) => Some(share(model.context_window, *fraction)),
        OutputCap::RegionPercent { percent, region } => {
            Some(budgets.get(region).map_or(most, |b| share(*b, *percent)))
        }
    }
}

/// What the run relies on from this machine: every provider its stages,
/// fallbacks and compaction use, and every MCP server its stages connect to.
pub(super) fn fingerprint(
    graph: &RunGraph,
    plans: &[StagePlan],
    env: &dyn ResolveEnv,
) -> EnvFingerprint {
    let mut providers = BTreeSet::new();
    let mut servers = BTreeSet::new();
    for plan in plans {
        providers.insert(plan.provider.clone());
        providers.extend(plan.fallbacks.iter().filter_map(|f| f.provider.clone()));
        for tool in &plan.tools {
            if let ToolSource::Mcp { server, .. } = &tool.source {
                servers.insert(server.clone());
            }
        }
    }
    providers.extend(
        graph
            .compaction
            .iter()
            .filter_map(|c| c.model.provider.clone()),
    );
    servers.extend(
        graph
            .stages
            .iter()
            .flat_map(|s| s.connectors.iter().cloned()),
    );
    EnvFingerprint {
        providers: providers
            .into_iter()
            .filter_map(|p| env.provider_fingerprint(&p).map(|d| (p, d)))
            .collect(),
        mcp_servers: servers
            .into_iter()
            .filter_map(|s| env.mcp_fingerprint(&s).map(|d| (s, d)))
            .collect(),
        leviath_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}
