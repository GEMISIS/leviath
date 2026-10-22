//! The spec frame of a run file in binary layout 4, which builds before a
//! graph's compaction models were resolved as a stage's are wrote.
//!
//! Layout 4 and this build's layout differ in one type: a graph's compaction
//! settings. Layout 4 named exactly one model and required the summary's
//! length and temperature; this build names a model, a list of models, or
//! none (the current stage's). Only the spec holds a graph, so only the spec
//! is read here, through the two types that hold the compaction settings:
//! the graph and the spec itself. Its stages and its delivery are read as
//! layout 5 still wrote them (see [`crate::layout5`]). Every other type, and
//! every other frame, is the runtime's own, which did not change.
//!
//! The one model converts to `model`, with no `models`: a run converted from
//! layout 4 summarizes exactly where it did.

use std::collections::BTreeMap;

use leviath_core::policy::ToolPolicy;
use leviath_runtime::spec::graph::{
    self, CodeRef, ContextTransformDef, DependencyDef, EdgeDef, FileTrackingDef, McpServerDef,
    MimeRows, NudgeDef, OutputDef, RegionLayoutDef, RepetitionDef, SafeCommandsDef, SandboxDef,
    ScriptPermissionsDef, ToolRescan,
};
use leviath_runtime::spec::inputs::{InputDecl, InputValues};
use leviath_runtime::spec::launch::Placement;
use leviath_runtime::spec::names::{Digest, ModelRef, RegionName, RunId, StageName, ToolName};
use leviath_runtime::spec::run_spec::{
    self, AutoAnswers, EnvFingerprint, ListedAs, SeededContent, SpecOrigin, StagePlan,
};
use serde::Deserialize;

/// The fingerprint in the header of every layout-4 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0x02, 0xc9, 0xe6, 0x0f, 0x25, 0xc0, 0xe2, 0x97, 0xb4, 0xde, 0xcd, 0x90, 0x28, 0x2b, 0x25, 0x63,
    0x74, 0x91, 0xa2, 0xad, 0x60, 0xa5, 0xb4, 0x64, 0x8f, 0x9e, 0x13, 0x89, 0x75, 0xed, 0xf5, 0xe2,
];

/// A graph's compaction settings, with layout 4's one model.
#[derive(Debug, Deserialize)]
pub(crate) struct CompactionDef {
    model: ModelRef,
    system_prompt: Option<String>,
    user_prompt_template: Option<String>,
    max_summary_tokens: u32,
    temperature: f64,
}

impl From<CompactionDef> for graph::CompactionDef {
    /// The same settings, the one model as `model`.
    fn from(c: CompactionDef) -> Self {
        Self {
            model: Some(c.model),
            models: Vec::new(),
            system_prompt: c.system_prompt,
            user_prompt_template: c.user_prompt_template,
            max_summary_tokens: c.max_summary_tokens,
            temperature: c.temperature,
        }
    }
}

/// A run graph, with its fields in layout 4's order.
#[derive(Debug, Deserialize)]
pub(crate) struct RunGraph {
    title: Option<String>,
    description: Option<String>,
    entry: Option<StageName>,
    stages: Vec<crate::layout5::StageDef>,
    edges: Vec<EdgeDef>,
    layout: RegionLayoutDef,
    inputs: Vec<InputDecl>,
    output: Option<OutputDef>,
    compaction: Option<CompactionDef>,
    max_child_depth: Option<u8>,
    taint_tracking: Option<bool>,
    tool_permissions: BTreeMap<ToolName, ToolPolicy>,
    sandbox: Option<SandboxDef>,
    read_paths: Vec<String>,
    safe_commands: SafeCommandsDef,
    batch_tool_hint: Option<bool>,
    shell_hint: Option<bool>,
    nudge: Option<NudgeDef>,
    repetition: Option<RepetitionDef>,
    file_tracking: Option<FileTrackingDef>,
    tool_rescan: ToolRescan,
    transforms: Vec<ContextTransformDef>,
    mime_types: MimeRows,
    dependencies: Vec<DependencyDef>,
    mcp_servers: Vec<McpServerDef>,
    script_permissions: ScriptPermissionsDef,
}

impl From<RunGraph> for graph::RunGraph {
    fn from(g: RunGraph) -> Self {
        Self {
            routing: None,
            title: g.title,
            description: g.description,
            entry: g.entry,
            stages: g.stages.into_iter().map(Into::into).collect(),
            edges: g.edges,
            layout: g.layout,
            inputs: g.inputs,
            output: g.output,
            compaction: g.compaction.map(Into::into),
            max_child_depth: g.max_child_depth,
            taint_tracking: g.taint_tracking,
            tool_permissions: g.tool_permissions,
            sandbox: g.sandbox,
            read_paths: g.read_paths,
            write_paths: Vec::new(),
            safe_commands: g.safe_commands,
            batch_tool_hint: g.batch_tool_hint,
            shell_hint: g.shell_hint,
            nudge: g.nudge,
            repetition: g.repetition,
            file_tracking: g.file_tracking,
            tool_rescan: g.tool_rescan,
            transforms: g.transforms,
            mime_types: g.mime_types,
            dependencies: g.dependencies,
            mcp_servers: g.mcp_servers,
            script_permissions: g.script_permissions,
            continue_at: None,
        }
    }
}

/// A run, fully resolved, with its fields in layout 4's order.
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
    launch: crate::layout6::LaunchPolicy,
    auto_answers: AutoAnswers,
    placement: Placement,
    delivery: crate::layout5::DeliveryPlan,
    env: EnvFingerprint,
    created_at: i64,
    listed: Option<ListedAs>,
}

impl From<RunSpec> for run_spec::RunSpec {
    fn from(spec: RunSpec) -> Self {
        Self {
            run_id: spec.run_id,
            origin: spec.origin,
            graph: spec.graph.into(),
            inputs: spec.inputs,
            stages: spec.stages,
            seeded: spec.seeded,
            code: spec.code,
            requested_output: spec.requested_output,
            requested_model: spec.requested_model,
            launch: spec.launch.into(),
            auto_answers: spec.auto_answers,
            placement: spec.placement,
            delivery: spec.delivery.into(),
            env: spec.env,
            created_at: spec.created_at,
            listed: spec.listed,
        }
    }
}
