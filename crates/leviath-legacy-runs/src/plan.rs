//! Each stage's plan: the model it runs on, that model's window, and the
//! tools it gets.
//!
//! An old run recorded which models it ran and the names of its tools, not a
//! model's window or a tool's definition. Both are looked up on this machine
//! through the host's [`StageLookup`], the way a new run's are when it is
//! resolved. Without one, a stage keeps the model its run would have used,
//! sized to the context budget it last ran with, and is given no tools; the
//! report says so.

use std::collections::BTreeMap;
use std::path::Path;

use leviath_core::output::{ArtifactSpec, OutputSpec};
use leviath_core::run_meta::{ContextSnapshot, StageModelUse, StageRecord as OldStage};
use leviath_runtime::dynamic_interaction::BLOCKING_INTERACTION_TOOLS;
use leviath_runtime::spec::env::{CodeFiles, ModelPlan};
use leviath_runtime::spec::graph::{CodeRef, OutputCap, OutputDef, RunGraph, StageDef};
use leviath_runtime::spec::names::{ModelId, ModelRef, ProviderName, RegionName};
use leviath_runtime::spec::run_spec::{AutoAnswers, StagePlan, ToolDef};

use crate::StageLookup;
use crate::context::n32;
use crate::report::Report;

/// Why the tool definitions are left out of every stage plan when nothing
/// looks them up.
const NO_TOOL_DEFS: &str = "an old run kept the names of its tools but not their definitions, and this conversion had nothing to look them up in";

/// What every stage is planned from.
pub(crate) struct Stages<'a> {
    pub(crate) graph: &'a RunGraph,
    /// The old run's stage ledger.
    pub(crate) ledger: &'a [OldStage],
    /// The context the run was last in, whose budgets the plans keep.
    pub(crate) context: &'a ContextSnapshot,
    /// The model the run was launched with.
    pub(crate) requested: Option<&'a ModelRef>,
    pub(crate) auto: AutoAnswers,
    pub(crate) lookup: Option<&'a dyn StageLookup>,
    /// The code the run holds, by digest.
    pub(crate) code: &'a CodeFiles,
    /// The installed agent's directory, whose script tools a stage may use.
    pub(crate) base: Option<&'a Path>,
    pub(crate) workdir: Option<&'a Path>,
}

/// A model the stage ledger recorded, as a checked reference.
pub(crate) fn ledger_model(m: &StageModelUse) -> Option<ModelRef> {
    ModelRef::parse(&format!("{}/{}", m.provider, m.model)).ok()
}

/// Every stage's plan, in graph order, and the code of any script tool the
/// lookup found that the run did not hold.
pub(crate) fn plan_all(
    cx: &Stages<'_>,
    report: &mut Report,
) -> (Vec<StagePlan>, Vec<(CodeRef, Vec<u8>)>) {
    let mut found = Vec::new();
    let plans = cx
        .graph
        .stages
        .iter()
        .map(|s| plan(cx, s, &mut found, report))
        .collect();
    report.fill(
        "stages.*.region_budgets",
        "the budgets of the stage the run was last in",
        "an old run recorded region budgets only for the stage it was in",
    );
    if cx.lookup.is_none() {
        report.fill("stages.*.tools", "[]", NO_TOOL_DEFS);
    }
    (plans, found)
}

/// The model the old run would have run `stage` on: the model it was
/// launched with when the stage takes the caller's, else the one its ledger
/// recorded the stage running on, else the stage's own first model. `true`
/// with a model the lookup is asked for by name; among the stage's own
/// models the lookup chooses itself, as it does for a new run.
fn wanted(cx: &Stages<'_>, stage: &StageDef) -> Option<(ModelRef, bool)> {
    if let Some(r) = cx.requested.filter(|_| stage.model.allow_user_default) {
        return Some((r.clone(), true));
    }
    let ran = cx
        .ledger
        .iter()
        .find(|r| r.name == stage.name.as_str())
        .and_then(|r| r.models.last())
        .and_then(ledger_model);
    match ran {
        Some(m) => Some((m, true)),
        None => stage.model.models.first().map(|m| (m.clone(), false)),
    }
}

/// The model a stage runs on, without a lookup's answer: what the run would
/// have used, sized to the context budget it last ran with.
fn recorded_model(
    cx: &Stages<'_>,
    stage: &StageDef,
    wanted: Option<ModelRef>,
    report: &mut Report,
) -> ModelPlan {
    let at = format!("stages.{}", stage.name);
    let window = n32(cx.context.max_tokens);
    let (provider, model) = match wanted {
        Some(ModelRef {
            provider: Some(provider),
            model,
        }) => (provider, model),
        _ => {
            report.fill(
                format!("{at}.model"),
                "unknown/unknown",
                "the run recorded no model this stage would have run on, and nothing looked one up",
            );
            (
                ProviderName::new("unknown").expect("a plain word is a provider name"),
                ModelId::new("unknown").expect("a plain word is a model id"),
            )
        }
    };
    report.fill(
        format!("{at}.context_window"),
        window,
        "an old run did not record its models' windows; this is the context budget it last ran with",
    );
    let current = ModelRef {
        provider: Some(provider.clone()),
        model: model.clone(),
    };
    ModelPlan {
        fallbacks: stage
            .model
            .models
            .iter()
            .filter(|m| **m != current)
            .cloned()
            .collect(),
        provider,
        model,
        context_window: window,
        max_output_tokens: 0,
        notes: Vec::new(),
    }
}

fn plan(
    cx: &Stages<'_>,
    stage: &StageDef,
    found: &mut Vec<(CodeRef, Vec<u8>)>,
    report: &mut Report,
) -> StagePlan {
    let at = format!("stages.{}", stage.name);
    let wanted = wanted(cx, stage);
    let asked = wanted.as_ref().filter(|(_, ask)| *ask).map(|(m, _)| m);
    let looked = cx.lookup.map(|l| l.model(cx.graph, stage, asked));
    let model = match looked {
        Some(Ok(plan)) => plan,
        Some(Err(why)) => {
            report.note(format!(
                "{at}.model could not be looked up on this machine: {why}"
            ));
            recorded_model(cx, stage, wanted.map(|(m, _)| m), report)
        }
        None => recorded_model(cx, stage, wanted.map(|(m, _)| m), report),
    };
    let region_budgets: BTreeMap<RegionName, u32> = cx
        .context
        .regions
        .iter()
        .filter_map(|r| Some((RegionName::new(r.name.as_str()).ok()?, n32(r.max_tokens))))
        .collect();
    let max_output_tokens = output_cap(stage, &model, &region_budgets, &at, report);
    let mut tools = tools(cx, stage, found, &at, report);
    describe_submit(&mut tools, stage.output.as_ref());
    let mut notes = vec!["converted from an old run directory".to_string()];
    notes.extend(model.notes);
    StagePlan {
        stage: stage.name.clone(),
        provider: model.provider,
        model: model.model,
        context_window: model.context_window,
        max_output_tokens,
        fallbacks: model.fallbacks,
        tools,
        output: stage.output.clone(),
        region_budgets,
        notes,
    }
}

/// The cap on one reply, worked out as a new run's is. A cap relative to the
/// model needs the model's own maximum reply, which only a lookup knows.
fn output_cap(
    stage: &StageDef,
    model: &ModelPlan,
    budgets: &BTreeMap<RegionName, u32>,
    at: &str,
    report: &mut Report,
) -> Option<u32> {
    let most = model.max_output_tokens;
    let share =
        |whole: u32, fraction: f64| ((f64::from(whole) * fraction).round() as u32).clamp(1, most);
    match stage.model.params.max_output_tokens.as_ref()? {
        OutputCap::Tokens(n) => Some(*n),
        _ if most == 0 => {
            report.fill(
                format!("{at}.max_output_tokens"),
                "None",
                "the cap was relative to a model nothing looked up",
            );
            None
        }
        OutputCap::WindowPercent(fraction) => Some(share(model.context_window, *fraction)),
        OutputCap::RegionPercent { percent, region } => {
            Some(budgets.get(region).map_or(most, |b| share(*b, *percent)))
        }
    }
}

/// The tools a stage gets, as the lookup finds them, with the ones that only
/// ask a person left out of a run nobody answers (unless the stage requires
/// them), as a new run's are.
fn tools(
    cx: &Stages<'_>,
    stage: &StageDef,
    found: &mut Vec<(CodeRef, Vec<u8>)>,
    at: &str,
    report: &mut Report,
) -> Vec<ToolDef> {
    let Some(lookup) = cx.lookup else {
        return Vec::new();
    };
    match lookup.tools(cx.graph, stage, cx.code, cx.base, cx.workdir) {
        Ok(got) => {
            found.extend(got.code);
            let mut tools = got.tools;
            if cx.auto.questions {
                tools.retain(|t| {
                    !BLOCKING_INTERACTION_TOOLS.contains(&t.name.as_str())
                        || stage.required_tools.contains(&t.name)
                });
            }
            tools
        }
        Err(why) => {
            report.fill(
                format!("{at}.tools"),
                "[]",
                format!("the stage's tools could not be looked up on this machine: {why}"),
            );
            Vec::new()
        }
    }
}

/// Tell the model the shape it is asked for in the `submit_output` tool's
/// description, as a new run's stage does.
fn describe_submit(tools: &mut [ToolDef], output: Option<&OutputDef>) {
    let described = output
        .map(|o| leviath_core::describe_spec(&spec_of(o)))
        .unwrap_or_default();
    if described.is_empty() {
        return;
    }
    for tool in tools
        .iter_mut()
        .filter(|t| t.name.as_str() == leviath_tools::SUBMIT_OUTPUT_TOOL)
    {
        tool.description = leviath_tools::submit_output_description(&described);
    }
}

/// The parts of a shape the model is told about.
fn spec_of(o: &OutputDef) -> OutputSpec {
    OutputSpec {
        format: o.format.clone(),
        instructions: o.instructions.clone(),
        example: o.example.clone(),
        schema: o.schema.as_ref().map(|s| s.value().clone()),
        artifacts: o
            .artifacts
            .iter()
            .map(|a| ArtifactSpec {
                name: a.name.clone(),
                mime_type: a.mime_type.to_string(),
                required: a.required,
                description: a.description.clone(),
            })
            .collect(),
        ..OutputSpec::default()
    }
}
