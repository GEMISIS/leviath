//! The spec frame of a run file in binary layout 5, which builds before a
//! spawn's idempotency key and a graph's `routing` were kept wrote.
//!
//! Layout 5 and this build's layout differ in three types, all held by the
//! spec: who hears about a run, which this build keeps the key a run was
//! spawned under in, and a graph and each of its stages, which this build
//! writes `routing` for. Layout 5 wrote nothing for `routing`, so the two
//! types below leave the field out, and read exactly what layout 5 wrote.
//! Every other type, and the code and owner frames, are the runtime's own,
//! which did not change. Its states and deltas are read as layout 7 still
//! wrote them (see [`crate::layout7`]).
//!
//! A run written in layout 5 was spawned under no key, since nothing kept
//! one, and its routing was not recorded, so it upgrades with neither: a
//! resumed run takes the operator's routing, as it did before. Layouts 2 to
//! 4 kept the same delivery, and layout 4 the same stages, so they read
//! those through this module too.

use std::collections::BTreeMap;

use leviath_core::policy::ToolPolicy;
use leviath_runtime::spec::graph::{
    self, CodeRef, CompactionDef, ContextTransformDef, DependencyDef, EdgeDef, FileTrackingDef,
    McpServerDef, MimeRows, ModelChoice, NudgeDef, OutputDef, RegionLayoutDef, RepetitionDef,
    SafeCommandsDef, SandboxDef, ScriptPermissionsDef, StageHooks, StageMode, ToolRescan,
    ToolRoutingDef, ToolSelector,
};
use leviath_runtime::spec::inputs::{InputDecl, InputValues};
use leviath_runtime::spec::launch::{self, CallbackPlan, Placement};
use leviath_runtime::spec::names::{
    Digest, McpServerName, MimePattern, ModelRef, RegionName, RunId, StageName, ToolName,
};
use leviath_runtime::spec::run_spec::{
    self, AutoAnswers, EnvFingerprint, ListedAs, SeededContent, SpecOrigin, StagePlan,
};
use serde::Deserialize;

/// The fingerprint in the header of every layout-5 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0xc7, 0xcd, 0x41, 0x6c, 0xf7, 0xa8, 0x7b, 0x1e, 0xce, 0x8b, 0x17, 0x17, 0xe0, 0xf6, 0x58, 0x26,
    0xc0, 0x75, 0xb2, 0xa9, 0x01, 0x53, 0x0a, 0x0f, 0x25, 0xde, 0x93, 0x77, 0x90, 0xd5, 0xb2, 0x15,
];

/// Who hears about a run, with layouts 2 to 5's fields.
#[derive(Debug, Deserialize)]
pub(crate) struct DeliveryPlan {
    callback: Option<CallbackPlan>,
    metadata: BTreeMap<String, String>,
}

impl From<DeliveryPlan> for launch::DeliveryPlan {
    /// The same webhook and labels, under no key.
    fn from(delivery: DeliveryPlan) -> Self {
        Self {
            callback: delivery.callback,
            metadata: delivery.metadata,
            idempotency: None,
        }
    }
}

/// One stage, with its fields in layouts 4 and 5's order and no `routing`.
#[derive(Debug, Deserialize)]
pub(crate) struct StageDef {
    name: StageName,
    description: Option<String>,
    system_prompt: Option<String>,
    model: ModelChoice,
    tools: Vec<ToolSelector>,
    required_tools: Vec<ToolName>,
    connectors: Vec<McpServerName>,
    max_iterations: Option<u32>,
    mode: StageMode,
    layout: Option<RegionLayoutDef>,
    hide: Vec<RegionName>,
    reset: Vec<RegionName>,
    tool_permissions: BTreeMap<ToolName, ToolPolicy>,
    requires_children: bool,
    max_revisits: Option<u32>,
    transition_prompt: Option<String>,
    accepts_messages: bool,
    allow_complete: bool,
    allow_as_worker: bool,
    allow_blocking_tools: bool,
    taint_tracking: Option<bool>,
    batch_tool_hint: Option<bool>,
    shell_hint: Option<bool>,
    nudge: Option<NudgeDef>,
    sandbox: Option<SandboxDef>,
    tool_routing: Option<ToolRoutingDef>,
    output_routing: BTreeMap<String, RegionName>,
    output: Option<OutputDef>,
    require_output: bool,
    input_accepts: Vec<MimePattern>,
    input_as_text: Vec<MimePattern>,
    tool_accepts: BTreeMap<ToolName, Vec<MimePattern>>,
    hooks: StageHooks,
}

impl From<StageDef> for graph::StageDef {
    /// The same stage, with no routing of its own.
    fn from(s: StageDef) -> Self {
        Self {
            name: s.name,
            description: s.description,
            system_prompt: s.system_prompt,
            model: s.model,
            tools: s.tools,
            required_tools: s.required_tools,
            connectors: s.connectors,
            max_iterations: s.max_iterations,
            mode: s.mode,
            layout: s.layout,
            hide: s.hide,
            reset: s.reset,
            tool_permissions: s.tool_permissions,
            requires_children: s.requires_children,
            max_revisits: s.max_revisits,
            transition_prompt: s.transition_prompt,
            routing: None,
            accepts_messages: s.accepts_messages,
            allow_complete: s.allow_complete,
            allow_as_worker: s.allow_as_worker,
            allow_blocking_tools: s.allow_blocking_tools,
            taint_tracking: s.taint_tracking,
            batch_tool_hint: s.batch_tool_hint,
            shell_hint: s.shell_hint,
            nudge: s.nudge,
            sandbox: s.sandbox,
            tool_routing: s.tool_routing,
            output_routing: s.output_routing,
            output: s.output,
            require_output: s.require_output,
            input_accepts: s.input_accepts,
            input_as_text: s.input_as_text,
            tool_accepts: s.tool_accepts,
            write_paths: Vec::new(),
            hooks: s.hooks,
        }
    }
}

/// A run graph, with its fields in layout 5's order and no `routing`.
#[derive(Debug, Deserialize)]
pub(crate) struct RunGraph {
    title: Option<String>,
    description: Option<String>,
    entry: Option<StageName>,
    stages: Vec<StageDef>,
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
    /// The same graph, with no routing of its own.
    fn from(g: RunGraph) -> Self {
        Self {
            title: g.title,
            description: g.description,
            entry: g.entry,
            stages: g.stages.into_iter().map(Into::into).collect(),
            edges: g.edges,
            layout: g.layout,
            inputs: g.inputs,
            output: g.output,
            compaction: g.compaction,
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
            routing: None,
            repetition: g.repetition,
            file_tracking: g.file_tracking,
            tool_rescan: g.tool_rescan,
            transforms: g.transforms,
            mime_types: g.mime_types,
            dependencies: g.dependencies,
            mcp_servers: g.mcp_servers,
            script_permissions: g.script_permissions,
        }
    }
}

/// A run, fully resolved, with its fields in layout 5's order.
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
