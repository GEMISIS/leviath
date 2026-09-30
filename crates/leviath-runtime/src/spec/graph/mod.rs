//! The run graph: everything a run is made of, written down whole.
//!
//! A [`RunGraph`] is stages, the edges between them, the context layout, the
//! inputs the run takes, and the graph-wide settings. A request can carry one
//! directly, and a blueprint is a named graph with some of it filled in. Every
//! reference inside it (an edge's stages, a gate's region, an input's slot) is
//! a checked name, and [`RunGraph::validate`] confirms each one points at
//! something declared.

use std::collections::BTreeMap;

use leviath_core::policy::ToolPolicy;
use serde::{Deserialize, Serialize};

pub mod edge;
mod from_blueprint;
pub mod policy;
pub mod region;
pub mod stage;
mod validate;

pub use edge::{EdgeCarry, EdgeCondition, EdgeDef, GateDef, RegionCount, StuckDef};
pub use policy::{
    ArtifactDef, CompactionDef, ContentTransform, ContextTransformDef, DependencyDef,
    FileTrackingDef, InstallDef, McpServerTemplate, MimeRowDef, MimeRows, Needs, NudgeDef,
    OutputDef, RegionMappingDef, RepetitionDef, SafeCommandsDef, SandboxDef, TokenRule, ToolRescan,
};
pub use region::{Budget, Eviction, RegionDef, RegionKind, RegionLayoutDef, Seed, SeedRefresh};
pub use stage::{
    AnswerStyle, CodeRef, FanOutDef, InteractionPointDef, ModelChoice, ModelParams, OutputCap,
    ParamScalar, SeedToolCall, StageDef, StageHooks, StageMode, ToolGroup, ToolRoutingDef,
    ToolSelector, UnattendedPoint, WorkerFailure, WorkerSource,
};

use super::inputs::InputDecl;
use super::names::{StageName, ToolName};

/// A whole run, written down.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunGraph {
    /// What the run is called where runs are listed.
    #[serde(default)]
    pub title: Option<String>,
    /// What the run does.
    #[serde(default)]
    pub description: Option<String>,
    /// The stage the run starts in. `None` starts in the first stage.
    #[serde(default)]
    pub entry: Option<StageName>,
    /// The stages.
    pub stages: Vec<StageDef>,
    /// The edges between them.
    #[serde(default)]
    pub edges: Vec<EdgeDef>,
    /// The context layout every stage uses unless it has its own.
    pub layout: RegionLayoutDef,
    /// The inputs the run takes.
    #[serde(default)]
    pub inputs: Vec<InputDecl>,
    /// The shape of the run's final output.
    #[serde(default)]
    pub output: Option<OutputDef>,
    /// How the conversation is summarized when the context fills.
    #[serde(default)]
    pub compaction: Option<CompactionDef>,
    /// How deep the run's tree of child runs may grow.
    #[serde(default)]
    pub max_child_depth: Option<u8>,
    /// Whether taint tracking is on.
    #[serde(default)]
    pub taint_tracking: Option<bool>,
    /// Per-tool approval policy for every stage.
    #[serde(default)]
    pub tool_permissions: BTreeMap<ToolName, ToolPolicy>,
    /// Where tools run.
    #[serde(default)]
    pub sandbox: Option<SandboxDef>,
    /// Paths outside the workdir the run may read (globs and `re:` patterns).
    #[serde(default)]
    pub read_paths: Vec<String>,
    /// Commands and tools that only read, so they run without approval.
    #[serde(default)]
    pub safe_commands: SafeCommandsDef,
    /// The batch-tool-calls hint. `None` takes the operator's.
    #[serde(default)]
    pub batch_tool_hint: Option<bool>,
    /// The platform shell hint. `None` takes the operator's.
    #[serde(default)]
    pub shell_hint: Option<bool>,
    /// The empty-response nudge.
    #[serde(default)]
    pub nudge: Option<NudgeDef>,
    /// Loop detection.
    #[serde(default)]
    pub repetition: Option<RepetitionDef>,
    /// The list of files read and written, kept in a region.
    #[serde(default)]
    pub file_tracking: Option<FileTrackingDef>,
    /// When the tool list is looked at again.
    #[serde(default)]
    pub tool_rescan: ToolRescan,
    /// How child runs' contexts are built from this run's.
    #[serde(default)]
    pub transforms: Vec<ContextTransformDef>,
    /// Mime registry rows the run adds.
    #[serde(default)]
    pub mime_types: MimeRows,
    /// What the run needs from the machine.
    #[serde(default)]
    pub dependencies: Vec<DependencyDef>,
}

impl RunGraph {
    /// The stage the run starts in: `entry`, or the first stage.
    pub fn entry_stage(&self) -> Option<&StageDef> {
        match &self.entry {
            Some(name) => self.stage(name.as_str()),
            None => self.stages.first(),
        }
    }

    /// A stage by name.
    pub fn stage(&self, name: &str) -> Option<&StageDef> {
        self.stages.iter().find(|s| s.name.as_str() == name)
    }

    /// The edges leaving a stage, in declaration order.
    pub fn edges_from<'a>(&'a self, stage: &'a str) -> impl Iterator<Item = &'a EdgeDef> {
        self.edges.iter().filter(move |e| e.from.as_str() == stage)
    }

    /// The layout a stage uses: its own, or the graph's.
    pub fn layout_for<'a>(&'a self, stage: &'a StageDef) -> &'a RegionLayoutDef {
        stage.layout.as_ref().unwrap_or(&self.layout)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
pub(crate) mod tests;
