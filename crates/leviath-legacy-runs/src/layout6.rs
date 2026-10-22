//! The spec frame of a run file in binary layout 6, which builds before a
//! run's write tools could be held to path globs wrote.
//!
//! Layout 6 and this build's layout differ in three types, all held by the
//! spec: a graph and each of its stages gained `write_paths`, and a run's
//! launch policy gained the write-path layers it inherits. Only the spec is
//! read here, through the types that hold them: the graph, the stage, the
//! launch policy and the spec itself. Every other type, and every other frame,
//! is the runtime's own, which did not change. Layouts 2 to 5 kept the same
//! launch policy, so they read it through this module too.
//!
//! A run converted from layout 6 has no write paths anywhere, which is what
//! it ran under.

use std::collections::BTreeMap;

use leviath_core::policy::ToolPolicy;
use leviath_runtime::spec::graph::{
    self, CodeRef, CompactionDef, ContextTransformDef, DependencyDef, EdgeDef, FileTrackingDef,
    McpServerDef, MimeRows, ModelChoice, NudgeDef, OutputDef, RegionLayoutDef, RepetitionDef,
    RoutingDef, SafeCommandsDef, SandboxDef, ScriptPermissionsDef, StageHooks, StageMode,
    ToolRescan, ToolRoutingDef, ToolSelector,
};
use leviath_runtime::spec::inputs::{InputDecl, InputValues};
use leviath_runtime::spec::launch::{self, DeliveryPlan, Placement, Unattended};
use leviath_runtime::spec::names::{
    Digest, McpServerName, MimePattern, ModelRef, RegionName, RunId, StageName, ToolName,
};
use leviath_runtime::spec::run_spec::{
    self, AutoAnswers, EnvFingerprint, ListedAs, SeededContent, SpecOrigin, StagePlan,
};
use serde::Deserialize;

/// The fingerprint in the header of every layout-6 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0xca, 0x81, 0x75, 0x3d, 0x6a, 0x2c, 0x96, 0xaa, 0xd9, 0xa8, 0x7f, 0x78, 0xaf, 0x8e, 0x32, 0xe1,
    0x5b, 0xb0, 0x45, 0x7e, 0x78, 0x43, 0x74, 0x0f, 0x49, 0xb4, 0x89, 0x15, 0x5a, 0xc3, 0x51, 0xd8,
];

/// What a run was trusted with, with layouts 2 to 6's fields: no write-path
/// layers.
#[derive(Debug, Deserialize)]
pub(crate) struct LaunchPolicy {
    unattended: Unattended,
    allow: Vec<ToolName>,
    max_depth: u8,
    seed_commands: bool,
    capture_model_input: bool,
}

impl From<LaunchPolicy> for launch::LaunchPolicy {
    /// The same policy, with no write paths inherited.
    fn from(p: LaunchPolicy) -> Self {
        Self {
            unattended: p.unattended,
            allow: p.allow,
            max_depth: p.max_depth,
            seed_commands: p.seed_commands,
            capture_model_input: p.capture_model_input,
            write_paths: Vec::new(),
        }
    }
}

/// One stage, with its fields in layout 6's order.
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
    routing: Option<RoutingDef>,
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
    /// The same stage, with no write paths of its own.
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
            routing: s.routing,
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

/// A run graph, with its fields in layout 6's order.
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
    routing: Option<RoutingDef>,
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
    /// The same graph, with no write paths.
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
            routing: g.routing,
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

/// A run, fully resolved, with its fields in layout 6's order.
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
            delivery: spec.delivery,
            env: spec.env,
            created_at: spec.created_at,
            listed: spec.listed,
        }
    }
}
