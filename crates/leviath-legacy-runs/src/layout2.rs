//! The spec frame of a run file in binary layout 2, which alpha builds wrote.
//!
//! Layout 2 and this build's layout differ in one type: a stage's plan. In
//! layout 2 it named its provider, model, window, reply cap and fallbacks one
//! after another; this build keeps the model as one [`ChosenModel`] and the
//! cap as the stage's `reply_cap`. Every other frame (code, states, deltas,
//! owners) and every other field of the spec has the same binary shape in
//! both, so only the spec is read here, into the two types below, which use
//! the runtime's own types for everything that did not change.

use std::collections::BTreeMap;

use leviath_runtime::spec::graph::{CodeRef, OutputDef, RunGraph};
use leviath_runtime::spec::inputs::InputValues;
use leviath_runtime::spec::launch::{DeliveryPlan, LaunchPolicy, Placement};
use leviath_runtime::spec::names::{
    Digest, ModelId, ModelRef, ProviderName, RegionName, RunId, StageName,
};
use leviath_runtime::spec::run_spec::{
    self, AutoAnswers, ChosenModel, EnvFingerprint, ListedAs, SeededContent, SpecOrigin, ToolDef,
};
use serde::Deserialize;

/// The fingerprint in the header of every layout-2 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0x49, 0x3f, 0x19, 0x26, 0x17, 0x99, 0x9b, 0x61, 0x46, 0x00, 0xb7, 0xa2, 0x03, 0x00, 0x53, 0xb8,
    0xd3, 0x30, 0x50, 0xa4, 0x6a, 0x55, 0xe2, 0x22, 0x32, 0xfd, 0x8c, 0x1c, 0x71, 0xe4, 0xef, 0xd5,
];

/// One stage, decided, with its fields in layout 2's order.
#[derive(Debug, Deserialize)]
pub(crate) struct StagePlan {
    stage: StageName,
    provider: ProviderName,
    model: ModelId,
    context_window: u32,
    max_output_tokens: Option<u32>,
    fallbacks: Vec<ModelRef>,
    tools: Vec<ToolDef>,
    output: Option<OutputDef>,
    region_budgets: BTreeMap<RegionName, u32>,
    notes: Vec<String>,
}

impl From<StagePlan> for run_spec::StagePlan {
    /// The same stage: its model whole, and the cap it ran under as its
    /// reply cap, which is what layout 2 kept there.
    fn from(plan: StagePlan) -> Self {
        Self {
            stage: plan.stage,
            model: ChosenModel {
                provider: plan.provider,
                id: plan.model,
                context_window: plan.context_window,
                fallbacks: plan.fallbacks,
            },
            reply_cap: plan.max_output_tokens,
            tools: plan.tools,
            output: plan.output,
            region_budgets: plan.region_budgets,
            notes: plan.notes,
        }
    }
}

/// A run, fully resolved, with its fields in layout 2's order.
#[derive(Debug, Deserialize)]
pub(crate) struct RunSpec {
    run_id: RunId,
    origin: SpecOrigin,
    graph: RunGraph,
    inputs: InputValues,
    stages: Vec<StagePlan>,
    seeded: BTreeMap<RegionName, SeededContent>,
    code: Vec<(CodeRef, Digest)>,
    requested_output: Option<OutputDef>,
    requested_model: Option<ModelRef>,
    launch: LaunchPolicy,
    auto_answers: AutoAnswers,
    placement: Placement,
    delivery: DeliveryPlan,
    env: EnvFingerprint,
    created_at: i64,
    listed: Option<ListedAs>,
}

impl From<RunSpec> for run_spec::RunSpec {
    fn from(spec: RunSpec) -> Self {
        Self {
            run_id: spec.run_id,
            origin: spec.origin,
            graph: spec.graph,
            inputs: spec.inputs,
            stages: spec.stages.into_iter().map(Into::into).collect(),
            seeded: spec.seeded,
            code: spec.code,
            requested_output: spec.requested_output,
            requested_model: spec.requested_model,
            launch: spec.launch,
            auto_answers: spec.auto_answers,
            placement: spec.placement,
            delivery: spec.delivery,
            env: spec.env,
            created_at: spec.created_at,
            listed: spec.listed,
        }
    }
}
