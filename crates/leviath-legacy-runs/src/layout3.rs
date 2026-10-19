//! The frames of a run file in binary layout 3, which builds before fan-out
//! worker choices wrote.
//!
//! Layout 3 and this build's layout differ in one setting and two records: a
//! fan-out named one `worker` where this build keeps the `stage` it belongs
//! to and its `workers_available`, and a fan-out in progress kept its items
//! without the worker each one went to. Everything that holds one of those
//! is read here as layout 3 wrote it: the spec (through its graph, its
//! stages and their modes), the state, and the delta. The graph's compaction
//! settings are read as layout 4 still wrote them (see [`crate::layout4`]).
//! Every other type is the runtime's own, which did not change.
//!
//! An old fan-out had one worker, which took every item, so each converts to
//! a fan-out of its stage with that one worker, named after what it runs.

use std::collections::BTreeMap;

use leviath_core::policy::ToolPolicy;
use leviath_runtime::fanout::FanOutOrigin;
use leviath_runtime::spec::graph::{
    self, CodeRef, ContextTransformDef, DependencyDef, EVERY_ITEM, EdgeDef, FileTrackingDef,
    InteractionPointDef, McpServerDef, MimeRows, ModelChoice, NudgeDef, OutputDef, RegionLayoutDef,
    RepetitionDef, SafeCommandsDef, SandboxDef, ScriptPermissionsDef, StageHooks, ToolRescan,
    ToolRoutingDef, ToolSelector, WorkerFailure, WorkerOption, WorkerSource,
};
use leviath_runtime::spec::inputs::{InputDecl, InputValues};
use leviath_runtime::spec::launch::Placement;
use leviath_runtime::spec::names::{
    Digest, McpServerName, MimePattern, ModelRef, RegionName, RunId, StageName, ToolName,
};
use leviath_runtime::spec::run_spec::{
    self, AutoAnswers, EnvFingerprint, ListedAs, SeededContent, SpecOrigin, StagePlan,
};
use leviath_runtime::state;
use leviath_runtime::state::context::PartState;
use serde::Deserialize;

/// The fingerprint in the header of every layout-3 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0xfd, 0x1f, 0x98, 0x95, 0x06, 0x39, 0x2d, 0xf3, 0x90, 0xe4, 0x2a, 0xa6, 0x0c, 0xca, 0xa7, 0x90,
    0x37, 0xe9, 0x4b, 0xe2, 0x3c, 0xe4, 0xc3, 0x5a, 0x8e, 0xc9, 0xf7, 0x42, 0x5c, 0x65, 0x7c, 0xb6,
];

/// A fan-out stage's settings, with layout 3's one worker.
#[derive(Debug, Deserialize)]
pub(crate) struct FanOutDef {
    worker: WorkerSource,
    merge_stage: Option<StageName>,
    max_workers: Option<u32>,
    on_worker_failure: WorkerFailure,
    split_prompt: String,
    results_region: Option<RegionName>,
    max_items: Option<u32>,
    max_attempts: Option<u32>,
}

impl FanOutDef {
    /// The same fan-out as `stage`'s, its one worker named after what it runs.
    fn upgraded(self, stage: &StageName) -> graph::FanOutDef {
        graph::FanOutDef {
            stage: stage.clone(),
            workers_available: vec![WorkerOption::named_after(self.worker, EVERY_ITEM)],
            merge_stage: self.merge_stage,
            max_workers: self.max_workers,
            on_worker_failure: self.on_worker_failure,
            split_prompt: self.split_prompt,
            results_region: self.results_region,
            max_items: self.max_items,
            max_attempts: self.max_attempts,
        }
    }
}

/// How a stage runs, in layout 3's order of variants.
#[derive(Debug, Deserialize)]
pub(crate) enum StageMode {
    Autonomous,
    Interactive,
    InteractivePoints(Vec<InteractionPointDef>),
    FanOut(FanOutDef),
    Output,
}

impl StageMode {
    fn upgraded(self, stage: &StageName) -> graph::StageMode {
        match self {
            Self::Autonomous => graph::StageMode::Autonomous,
            Self::Interactive => graph::StageMode::Interactive(graph::InteractiveDef::default()),
            Self::InteractivePoints(points) => graph::StageMode::InteractivePoints(points),
            Self::FanOut(fan) => graph::StageMode::FanOut(fan.upgraded(stage)),
            Self::Output => graph::StageMode::Output,
        }
    }
}

/// One stage, with its fields in layout 3's order.
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
    fn from(s: StageDef) -> Self {
        Self {
            routing: None,
            mode: s.mode.upgraded(&s.name),
            name: s.name,
            description: s.description,
            system_prompt: s.system_prompt,
            model: s.model,
            tools: s.tools,
            required_tools: s.required_tools,
            connectors: s.connectors,
            max_iterations: s.max_iterations,
            layout: s.layout,
            hide: s.hide,
            reset: s.reset,
            tool_permissions: s.tool_permissions,
            requires_children: s.requires_children,
            max_revisits: s.max_revisits,
            transition_prompt: s.transition_prompt,
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

/// A run graph, with its fields in layout 3's order.
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
    compaction: Option<crate::layout4::CompactionDef>,
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
        }
    }
}

/// A run, fully resolved, with its fields in layout 3's order.
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

/// One queued fan-out item, which layout 3 kept without its worker.
#[derive(Debug, Deserialize)]
pub(crate) struct WorkItemState {
    id: String,
    inputs: InputValues,
}

/// A fan-out in progress, with its fields in layout 3's order.
#[derive(Debug, Deserialize)]
pub(crate) struct FanOutState {
    stage: StageName,
    config: FanOutDef,
    max_workers: Option<u32>,
    queued: Vec<WorkItemState>,
    active: Vec<(String, RunId)>,
    done: Vec<(String, String)>,
    failed: Vec<(String, String)>,
    paused: bool,
    origin: FanOutOrigin,
    parts: Vec<PartState>,
}

impl From<FanOutState> for state::FanOutState {
    /// The same fan-out, every item, queued or running, with its one worker.
    fn from(f: FanOutState) -> Self {
        let config = f.config.upgraded(&f.stage);
        let worker = config.workers_available[0].name.clone();
        Self {
            stage: f.stage,
            max_workers: f.max_workers,
            queued: f
                .queued
                .into_iter()
                .map(|item| state::WorkItemState {
                    id: item.id,
                    worker: worker.clone(),
                    inputs: item.inputs,
                })
                .collect(),
            active: f
                .active
                .into_iter()
                .map(|(id, run)| (id, worker.clone(), run))
                .collect(),
            config,
            done: f.done,
            failed: f.failed,
            paused: f.paused,
            origin: f.origin,
            parts: f.parts,
        }
    }
}

/// A run's state as layouts 2 and 3 wrote it: layout 7's, with this
/// layout's fan-out in progress.
pub(crate) type RunState = crate::layout7::RunState<FanOutState>;

/// One step of a run's history as layouts 2 and 3 wrote it: layout 7's, with
/// this layout's fan-out in progress.
pub(crate) type StateDelta = crate::layout7::StateDelta<FanOutState>;

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::spec::names::WorkerName;

    /// A fan-out to one stage worker, as layout 3 wrote it.
    fn old_fan_out() -> FanOutDef {
        FanOutDef {
            worker: WorkerSource::Stage(StageName::new("w").unwrap()),
            merge_stage: None,
            max_workers: None,
            on_worker_failure: WorkerFailure::Continue,
            split_prompt: String::new(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        }
    }

    /// A stage that does not fan out keeps its mode as it was, and one that
    /// does becomes a fan-out of that stage. Every mode is driven here, in one
    /// place: coverage counts a function by its best-covered build, and the
    /// integration tests' build sees only some of them.
    #[test]
    fn every_mode_comes_through() {
        let stage = StageName::new("s").unwrap();
        // Compared as text: a match would leave an arm only a failure reaches.
        let shown = |mode: StageMode| format!("{:?}", mode.upgraded(&stage));
        assert_eq!(shown(StageMode::Autonomous), "Autonomous");
        assert_eq!(
            shown(StageMode::Interactive),
            "Interactive(InteractiveDef { idle_timeout_secs: None })"
        );
        assert_eq!(shown(StageMode::Output), "Output");
        assert_eq!(
            shown(StageMode::InteractivePoints(Vec::new())),
            "InteractivePoints([])"
        );
        let fan = shown(StageMode::FanOut(old_fan_out()));
        assert!(
            fan.starts_with("FanOut(FanOutDef { stage: StageName(\"s\")"),
            "{fan}"
        );
    }

    /// A worker already running when layout 3 stopped keeps running under the
    /// fan-out's one worker, as a queued item does.
    #[test]
    fn a_running_item_keeps_its_run_under_the_one_worker() {
        let fan = FanOutState {
            stage: StageName::new("split").unwrap(),
            config: old_fan_out(),
            max_workers: None,
            queued: vec![WorkItemState {
                id: "b".into(),
                inputs: InputValues::default(),
            }],
            active: vec![("a".into(), RunId::new("run-a").unwrap())],
            done: Vec::new(),
            failed: Vec::new(),
            paused: false,
            origin: FanOutOrigin::Stage,
            parts: Vec::new(),
        };
        let upgraded = state::FanOutState::from(fan);
        let w = WorkerName::new("w").unwrap();
        assert_eq!(
            upgraded.active,
            [("a".to_string(), w.clone(), RunId::new("run-a").unwrap())]
        );
        assert_eq!(upgraded.queued[0].worker, w);
        assert_eq!(upgraded.config.stage.as_str(), "split");
    }
}
