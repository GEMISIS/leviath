//! `RunSpec`: a run as it was resolved, the first frame of its run file.
//!
//! Also what a dry run answers with, `SpawnSummary`, which is the same run in
//! brief: the two share their origin, launch and input types so a client
//! reads a summary and a spec with the same code.

use async_graphql::{Enum, ID, SimpleObject};
use leviath_graphql_derive::mirror;
use leviath_runtime::spec::graph::{CodeRef as CoreCodeRef, OutputDef};
use leviath_runtime::spec::launch::{
    DeliveryPlan, LaunchPolicy as CorePolicy, Placement, Unattended,
};
use leviath_runtime::spec::run_spec::{
    AutoAnswers as CoreAnswers, EnvFingerprint as CoreEnv, RunSpec as CoreSpec, SpecOrigin,
    StagePlan as CorePlan, ToolDef as CoreTool, ToolSource,
};
use leviath_runtime::spec::summary::{SpawnSummary as CoreSummary, StageSummary as CoreStage};

use super::super::super::scalars::{Json, Timestamp};
use super::super::manifest::output::ValidatorErrorPolicy;
use super::super::run::MetadataEntry;
use super::state::StatePart;
use super::values::{InputEntry, entries};
use super::{json_of, saturating};

/// Where a run's graph came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SpecOriginKind {
    /// An installed blueprint.
    Blueprint,
    /// A blueprint read from its directory on the daemon's machine.
    BlueprintFile,
    /// A graph the caller wrote.
    Raw,
    /// The graph a run from an earlier release recorded, its blueprint gone
    /// when it was converted. Such a run never resumes.
    Recorded,
}

/// Where a run's graph came from, and which revision of it ran.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunOrigin {
    /// Which of the four it was.
    pub(crate) kind: SpecOriginKind,
    /// The blueprint's name. Null for a graph the caller wrote.
    pub(crate) blueprint_name: Option<String>,
    /// The revision that ran, as the lowercase hex SHA-256 of its manifest.
    pub(crate) digest: Option<String>,
    /// The blueprint's declared version.
    pub(crate) version: Option<String>,
    /// The directory a `BLUEPRINT_FILE` was read from.
    pub(crate) path: Option<String>,
}

impl From<&SpecOrigin> for RunOrigin {
    fn from(origin: &SpecOrigin) -> Self {
        let digest = origin.digest().map(ToString::to_string);
        let blueprint_name = origin.blueprint_name().map(str::to_string);
        match origin {
            SpecOrigin::Blueprint { version, .. } => Self {
                kind: SpecOriginKind::Blueprint,
                blueprint_name,
                digest,
                version: Some(version.clone()),
                path: None,
            },
            SpecOrigin::BlueprintFile { path, version, .. } => Self {
                kind: SpecOriginKind::BlueprintFile,
                blueprint_name,
                digest,
                version: Some(version.clone()),
                path: Some(path.to_string()),
            },
            SpecOrigin::Recorded { .. } => Self {
                kind: SpecOriginKind::Recorded,
                blueprint_name,
                digest,
                version: None,
                path: None,
            },
            SpecOrigin::Raw => Self {
                kind: SpecOriginKind::Raw,
                blueprint_name,
                digest,
                version: None,
                path: None,
            },
        }
    }
}

/// How much of a run goes ahead without a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum UnattendedMode {
    /// A person answers every approval and question.
    Off,
    /// Nothing waits for a person.
    All,
    /// The named yolo profile says which prompts still reach a person.
    Profile,
}

/// What a run is trusted with, once decided.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct LaunchPolicy {
    /// How much goes ahead without a person.
    pub(crate) unattended: UnattendedMode,
    /// The yolo profile, when `unattended` is `PROFILE`.
    pub(crate) profile: Option<String>,
    /// Tools approved without asking.
    pub(crate) allow: Vec<String>,
    /// How many more levels of child runs this run may start.
    pub(crate) max_depth: i32,
    /// Whether region seeds that run a shell command may run.
    pub(crate) seed_commands: bool,
    /// Whether every model request is recorded in the run's journal.
    pub(crate) capture_model_input: bool,
}

impl From<&CorePolicy> for LaunchPolicy {
    fn from(policy: &CorePolicy) -> Self {
        let (unattended, profile) = match &policy.unattended {
            Unattended::Off => (UnattendedMode::Off, None),
            Unattended::All => (UnattendedMode::All, None),
            Unattended::Profile(name) => (UnattendedMode::Profile, Some(name.to_string())),
        };
        Self {
            unattended,
            profile,
            allow: policy.allow.iter().map(ToString::to_string).collect(),
            max_depth: i32::from(policy.max_depth),
            seed_commands: policy.seed_commands,
            capture_model_input: policy.capture_model_input,
        }
    }
}

/// One stage of a resolved run, in brief.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StageSummary {
    /// The stage.
    pub(crate) stage: String,
    /// The provider that would serve it.
    pub(crate) provider: String,
    /// The model it would run on.
    pub(crate) model: String,
    /// The tools it would be given, by name.
    pub(crate) tools: Vec<String>,
}

impl From<&CoreStage> for StageSummary {
    fn from(stage: &CoreStage) -> Self {
        Self {
            stage: stage.stage.to_string(),
            provider: stage.provider.to_string(),
            model: stage.model.to_string(),
            tools: stage.tools.iter().map(ToString::to_string).collect(),
        }
    }
}

/// A run a request would start, in brief: what a dry run answers with when it
/// found nothing wrong.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct SpawnSummary {
    /// What the run is called: its blueprint's name, or a raw graph's title.
    pub(crate) title: String,
    /// Where its graph came from.
    pub(crate) origin: RunOrigin,
    /// The stage it starts in.
    pub(crate) entry_stage: String,
    /// Each stage, in the graph's order.
    pub(crate) stages: Vec<StageSummary>,
    /// The inputs, checked and with their defaults filled in.
    pub(crate) inputs: Vec<InputEntry>,
    /// What it would be trusted with.
    pub(crate) launch: LaunchPolicy,
    /// The directory its tools would work in.
    pub(crate) workdir: String,
    /// What may keep the run from ever finishing: stages it can reach and
    /// never leave, each named. Warnings, never refusals: the run would
    /// start. Empty for most runs.
    pub(crate) warnings: Vec<crate::commands::serve::graphql::mutation::spawn::SpawnIssue>,
}

impl From<&CoreSummary> for SpawnSummary {
    fn from(summary: &CoreSummary) -> Self {
        Self {
            title: summary.title.clone(),
            origin: RunOrigin::from(&summary.origin),
            entry_stage: summary.entry_stage.to_string(),
            stages: summary.stages.iter().map(StageSummary::from).collect(),
            inputs: entries(&summary.inputs),
            launch: LaunchPolicy::from(&summary.launch),
            workdir: summary.workdir.display().to_string(),
            warnings: summary
                .warnings
                .iter()
                .map(crate::commands::serve::graphql::mutation::spawn::SpawnIssue::from)
                .collect(),
        }
    }
}

/// Where a tool a stage gets comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ToolDefSource {
    /// Compiled into Leviath.
    Builtin,
    /// A child-run tool.
    Subagent,
    /// A stage-control tool the engine handles itself.
    StageControl,
    /// A script tool.
    Script,
    /// An MCP server's tool.
    Mcp,
}

/// A tool as a stage gets it, frozen when the run was resolved.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolDef {
    /// Its name, as the model calls it.
    pub(crate) name: String,
    /// What it does, as the model reads it.
    pub(crate) description: String,
    /// The JSON Schema of its arguments.
    pub(crate) schema: Json,
    /// Where it comes from.
    pub(crate) source: ToolDefSource,
    /// A script tool's code, by digest.
    pub(crate) script_digest: Option<String>,
    /// An MCP tool's server.
    pub(crate) mcp_server: Option<String>,
    /// An MCP tool's name on that server.
    pub(crate) mcp_tool: Option<String>,
}

impl From<&CoreTool> for ToolDef {
    fn from(tool: &CoreTool) -> Self {
        let (source, script_digest, mcp_server, mcp_tool) = match &tool.source {
            ToolSource::Builtin => (ToolDefSource::Builtin, None, None, None),
            ToolSource::Subagent => (ToolDefSource::Subagent, None, None, None),
            ToolSource::StageControl => (ToolDefSource::StageControl, None, None, None),
            ToolSource::Script(digest) => {
                (ToolDefSource::Script, Some(digest.to_string()), None, None)
            }
            ToolSource::Mcp { server, tool } => (
                ToolDefSource::Mcp,
                None,
                Some(server.to_string()),
                Some(tool.clone()),
            ),
        };
        Self {
            name: tool.name.to_string(),
            description: tool.description.clone(),
            schema: Json(tool.schema.value().clone()),
            source,
            script_digest,
            mcp_server,
            mcp_tool,
        }
    }
}

/// Whether some code is a file beside the blueprint or text written inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum CodeRefKind {
    /// A file, by its path relative to the blueprint.
    File,
    /// Source written inline in the graph.
    Inline,
}

/// Code a run's graph names: a hook, a validator, a script.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct CodeRef {
    /// Which of the two it is.
    pub(crate) kind: CodeRefKind,
    /// The path, or the source itself.
    pub(crate) text: String,
}

impl From<&CoreCodeRef> for CodeRef {
    fn from(code: &CoreCodeRef) -> Self {
        match code {
            CoreCodeRef::File(path) => Self {
                kind: CodeRefKind::File,
                text: path.clone(),
            },
            CoreCodeRef::Inline(source) => Self {
                kind: CodeRefKind::Inline,
                text: source.clone(),
            },
        }
    }
}

/// One piece of code a run uses, and the digest its bytes are stored under in
/// the run file.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct CodeEntry {
    /// The graph's reference to it.
    pub(crate) code: CodeRef,
    /// The lowercase hex SHA-256 of the code that ran.
    pub(crate) digest: String,
}

/// One file the final output hands back.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct OutputShapeArtifact {
    /// The file's name.
    pub(crate) name: String,
    /// Its mime type or pattern.
    pub(crate) mime_type: String,
    /// Whether the answer must include it.
    pub(crate) required: bool,
    /// What it is.
    pub(crate) description: Option<String>,
}

/// The shape a run's or a stage's final output must take.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct OutputShape {
    /// The format, as an opaque label the model is told.
    pub(crate) format: Option<String>,
    /// How to write it.
    pub(crate) instructions: Option<String>,
    /// An example answer.
    pub(crate) example: Option<String>,
    /// A JSON Schema the answer must meet.
    pub(crate) schema: Option<Json>,
    /// Code that checks the answer.
    pub(crate) validator: Option<CodeRef>,
    /// What happens when that code refuses an answer. Null is `REJECT`.
    pub(crate) on_validator_error: Option<ValidatorErrorPolicy>,
    /// Whether a later answer may replace files an earlier one wrote.
    pub(crate) overwrite_artifacts: Option<bool>,
    /// Files the answer hands back beside its text.
    pub(crate) artifacts: Vec<OutputShapeArtifact>,
}

impl From<&OutputDef> for OutputShape {
    fn from(output: &OutputDef) -> Self {
        Self {
            format: output.format.clone(),
            instructions: output.instructions.clone(),
            example: output.example.clone(),
            schema: output.schema.as_ref().map(|doc| Json(doc.value().clone())),
            validator: output.validator.as_ref().map(CodeRef::from),
            on_validator_error: output.on_validator_error.map(ValidatorErrorPolicy::from),
            overwrite_artifacts: output.overwrite_artifacts,
            artifacts: output
                .artifacts
                .iter()
                .map(|artifact| OutputShapeArtifact {
                    name: artifact.name.clone(),
                    mime_type: artifact.mime_type.to_string(),
                    required: artifact.required,
                    description: artifact.description.clone(),
                })
                .collect(),
        }
    }
}

/// A region's budget in one stage.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionBudget {
    /// The region.
    pub(crate) region: String,
    /// Its budget, in tokens.
    pub(crate) tokens: i32,
}

/// One stage, as it was decided when the run was resolved.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StagePlan {
    /// The stage.
    pub(crate) stage: String,
    /// The provider that serves it.
    pub(crate) provider: String,
    /// The model it runs.
    pub(crate) model: String,
    /// That model's context window, in tokens.
    pub(crate) context_window: i32,
    /// The cap on one reply, in tokens, when there is one.
    pub(crate) max_output_tokens: Option<i32>,
    /// Where to go if the provider fails, best first, as `provider/model`.
    pub(crate) fallbacks: Vec<String>,
    /// The tools it gets.
    pub(crate) tools: Vec<ToolDef>,
    /// The final-output shape it asks for, with the caller's request applied.
    pub(crate) output: Option<OutputShape>,
    /// Each region's budget in this stage.
    pub(crate) region_budgets: Vec<RegionBudget>,
    /// Lines worth logging about how the stage was decided.
    pub(crate) notes: Vec<String>,
}

impl From<&CorePlan> for StagePlan {
    fn from(plan: &CorePlan) -> Self {
        Self {
            stage: plan.stage.to_string(),
            provider: plan.provider.to_string(),
            model: plan.model.to_string(),
            context_window: saturating(plan.context_window),
            max_output_tokens: plan.max_output_tokens.map(saturating),
            fallbacks: plan.fallbacks.iter().map(ToString::to_string).collect(),
            tools: plan.tools.iter().map(ToolDef::from).collect(),
            output: plan.output.as_ref().map(OutputShape::from),
            region_budgets: plan
                .region_budgets
                .iter()
                .map(|(region, tokens)| RegionBudget {
                    region: region.to_string(),
                    tokens: saturating(*tokens),
                })
                .collect(),
            notes: plan.notes.clone(),
        }
    }
}

/// What one region held when the run started: its seeds and bound inputs,
/// resolved.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct SeededRegion {
    /// The region.
    pub(crate) region: String,
    /// The text it starts with.
    pub(crate) text: String,
    /// The parts it starts with.
    pub(crate) parts: Vec<StatePart>,
}

/// What a run answers for itself instead of asking a person.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct AutoAnswers {
    /// The model's own questions are answered for it.
    pub(crate) questions: bool,
    /// Stage checkpoints approve themselves.
    pub(crate) checkpoints: bool,
    /// Taint-gate prompts approve themselves.
    pub(crate) gate: bool,
}

impl From<&CoreAnswers> for AutoAnswers {
    fn from(answers: &CoreAnswers) -> Self {
        Self {
            questions: answers.questions,
            checkpoints: answers.checkpoints,
            gate: answers.gate,
        }
    }
}

/// Where a run sits: its working directory and its place in a tree of runs.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunPlacement {
    /// The directory its tools work in.
    pub(crate) workdir: String,
    /// The run that started this one, when one did.
    pub(crate) parent_id: Option<ID>,
    /// How far below the top of its tree this run is. A top-level run is 0.
    pub(crate) depth: i32,
    /// For a fan-out worker, the stage of the parent's graph it runs.
    pub(crate) worker_stage: Option<String>,
    /// For a fan-out worker, the id of the work item it runs.
    pub(crate) work_item: Option<String>,
}

impl From<&Placement> for RunPlacement {
    fn from(placement: &Placement) -> Self {
        Self {
            workdir: placement.workdir.display().to_string(),
            parent_id: placement.parent.as_ref().map(|id| ID(id.to_string())),
            depth: i32::from(placement.depth),
            worker_stage: placement.worker_stage.as_ref().map(ToString::to_string),
            work_item: placement.work_item.clone(),
        }
    }
}

/// Who hears about a run, and the caller's labels for it.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunDelivery {
    /// The URL the daemon posts to when the run finishes.
    pub(crate) callback_url: Option<String>,
    /// Whether that post is signed. The secret itself is never read back.
    pub(crate) callback_signed: bool,
    /// The caller's labels, carried through untouched.
    pub(crate) metadata: Vec<MetadataEntry>,
}

impl From<&DeliveryPlan> for RunDelivery {
    fn from(delivery: &DeliveryPlan) -> Self {
        Self {
            callback_url: delivery.callback.as_ref().map(|c| c.url.to_string()),
            callback_signed: delivery.callback_secret().is_some(),
            metadata: delivery
                .metadata
                .iter()
                .map(|(key, value)| MetadataEntry {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        }
    }
}

/// A name and the digest of what it stood for.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct NamedDigest {
    /// The provider or MCP server.
    pub(crate) name: String,
    /// The digest of its configuration or tool list.
    pub(crate) digest: String,
}

/// What a run relied on from the machine it was resolved on, so a resume can
/// tell when that has changed.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct EnvFingerprint {
    /// Each provider the run uses, by the digest of its configuration.
    pub(crate) providers: Vec<NamedDigest>,
    /// Each MCP server the run uses, by the digest of its tool list.
    pub(crate) mcp_servers: Vec<NamedDigest>,
    /// The Leviath version that resolved the run.
    pub(crate) leviath_version: String,
}

impl From<&CoreEnv> for EnvFingerprint {
    fn from(env: &CoreEnv) -> Self {
        Self {
            providers: env
                .providers
                .iter()
                .map(|(name, digest)| NamedDigest {
                    name: name.to_string(),
                    digest: digest.to_string(),
                })
                .collect(),
            mcp_servers: env
                .mcp_servers
                .iter()
                .map(|(name, digest)| NamedDigest {
                    name: name.to_string(),
                    digest: digest.to_string(),
                })
                .collect(),
            leviath_version: env.leviath_version.clone(),
        }
    }
}

/// A run as it was resolved: the request decided against the machine it
/// started on, and the first frame of its run file.
///
/// Nothing in it is looked up again. A resume binds to it, and a run's
/// `state` and `deltas` are what happened next.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunSpec {
    /// The run.
    pub(crate) run_id: ID,
    /// Where its graph came from.
    pub(crate) origin: RunOrigin,
    /// The graph, with the request's input slots applied, in the same JSON
    /// form `spawnRun` takes a graph in.
    pub(crate) graph: Json,
    /// The checked inputs.
    pub(crate) inputs: Vec<InputEntry>,
    /// One plan per stage, in the graph's stage order.
    pub(crate) stages: Vec<StagePlan>,
    /// What each region held at spawn.
    pub(crate) seeded: Vec<SeededRegion>,
    /// The code the run uses.
    pub(crate) code: Vec<CodeEntry>,
    /// The output shape the caller asked for, as asked.
    pub(crate) requested_output: Option<OutputShape>,
    /// The model the caller asked for, as asked.
    pub(crate) requested_model: Option<String>,
    /// What the run is trusted with.
    pub(crate) launch: LaunchPolicy,
    /// What its unattended setting answers without a person.
    pub(crate) auto_answers: AutoAnswers,
    /// Where it runs.
    pub(crate) placement: RunPlacement,
    /// Who hears about it.
    pub(crate) delivery: RunDelivery,
    /// What it relied on from the machine.
    pub(crate) env: EnvFingerprint,
    /// When it was resolved.
    pub(crate) created_at: Timestamp,
}

impl From<&CoreSpec> for RunSpec {
    fn from(spec: &CoreSpec) -> Self {
        Self {
            run_id: ID(spec.run_id.to_string()),
            origin: RunOrigin::from(&spec.origin),
            graph: Json(json_of(&spec.graph)),
            inputs: entries(&spec.inputs),
            stages: spec.stages.iter().map(StagePlan::from).collect(),
            seeded: spec
                .seeded
                .iter()
                .map(|(region, content)| SeededRegion {
                    region: region.to_string(),
                    text: content.text.clone(),
                    parts: content.parts.iter().map(StatePart::from).collect(),
                })
                .collect(),
            code: spec
                .code
                .iter()
                .map(|(code, digest)| CodeEntry {
                    code: CodeRef::from(code),
                    digest: digest.to_string(),
                })
                .collect(),
            requested_output: spec.requested_output.as_ref().map(OutputShape::from),
            requested_model: spec.requested_model.as_ref().map(ToString::to_string),
            launch: LaunchPolicy::from(&spec.launch),
            auto_answers: AutoAnswers::from(&spec.auto_answers),
            placement: RunPlacement::from(&spec.placement),
            delivery: RunDelivery::from(&spec.delivery),
            env: EnvFingerprint::from(&spec.env),
            created_at: Timestamp(spec.created_at),
        }
    }
}
