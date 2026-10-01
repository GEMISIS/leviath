//! The edges between stages: when a run may take one, what must be true
//! first, and what happens to the context on the way.

use serde::{Deserialize, Serialize};

use crate::spec::names::{EdgeName, RegionName, StageName, ToolName};

/// The name of the edge a manifest-format stage with no `transitions` table is
/// given when it is read as a graph: an `always` edge to the stage after it,
/// with nothing carried differently and no gate.
///
/// It cannot collide with a declared edge. Such a manifest names each edge
/// after the stage it enters, and only a stage that declares no edges at all
/// is given this one, so it is always the only edge leaving its stage.
pub const FALL_THROUGH_EDGE: &str = "next";

/// An edge from one stage to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EdgeDef {
    /// The edge's name, unique among the edges leaving `from`.
    pub name: EdgeName,
    /// The stage it leaves.
    pub from: StageName,
    /// The stage it enters.
    pub to: StageName,
    /// When the run takes it.
    #[serde(default)]
    pub when: EdgeCondition,
    /// What the model is told about it when it picks the next edge.
    #[serde(default)]
    pub hint: Option<String>,
    /// What happens to the context on the way.
    #[serde(default)]
    pub carry: EdgeCarry,
    /// What must be true before the run may take it.
    #[serde(default)]
    pub gate: Option<GateDef>,
    /// When the stage counts as stuck, for a `stuck` edge.
    #[serde(default)]
    pub stuck: Option<StuckDef>,
}

/// When a run takes an edge.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCondition {
    /// When the stage ends normally.
    #[default]
    Always,
    /// When the stage fails.
    Error,
    /// When the stage reaches its iteration cap.
    MaxIterations,
    /// When the model picks it.
    LlmChoice,
    /// When no other edge applies.
    DeadEnd,
    /// When the stage is stuck, by the edge's `stuck` rule.
    Stuck,
}

/// What happens to the context when a run takes an edge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCarry {
    /// Everything carries over.
    #[default]
    Direct,
    /// Every clearable region is emptied.
    Clear,
    /// The conversation is summarized.
    Compact {
        /// The summarizing prompt, over the default.
        #[serde(default)]
        prompt: Option<String>,
    },
    /// Each region is carried, compacted or cleared by name.
    Custom {
        /// Regions carried as they are.
        #[serde(default)]
        carry: Vec<RegionName>,
        /// Regions summarized.
        #[serde(default)]
        compact: Vec<RegionName>,
        /// Regions emptied.
        #[serde(default)]
        clear: Vec<RegionName>,
        /// The summarizing prompt, over the default.
        #[serde(default)]
        compact_prompt: Option<String>,
    },
}

/// What must be true before a run may take an edge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GateDef {
    /// The stage must have changed a file.
    #[serde(default)]
    pub require_modifications: bool,
    /// What the model is told when the gate holds it back.
    #[serde(default)]
    pub message: Option<String>,
    /// The region the gate's message lands in.
    #[serde(default)]
    pub region: Option<RegionName>,
    /// Tools the stage must have called.
    #[serde(default)]
    pub tools: Vec<ToolName>,
    /// How many times the gate holds the run back before letting it through.
    #[serde(default)]
    pub max_attempts: Option<u32>,
    /// A region the stage must have written to.
    #[serde(default)]
    pub require_region_updated: Option<RegionName>,
    /// Regions that must not be empty.
    #[serde(default)]
    pub require_regions: Vec<RegionName>,
    /// A checklist region that must have no open items.
    #[serde(default)]
    pub require_no_open_items: Option<RegionName>,
    /// A region that must hold at least so many entries.
    #[serde(default)]
    pub require_region_entries: Option<RegionCount>,
}

impl GateDef {
    /// How many times a gate holds the run back when it sets no
    /// `max_attempts`.
    pub const DEFAULT_MAX_ATTEMPTS: usize = 3;
}

/// The built-in tools that change files on disk, which a gate's
/// `require_modifications` counts. A gate's own `tools` add to them.
pub const MODIFYING_TOOLS: &[&str] = &["write_file", "edit_file"];

/// A region and how many entries it must hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionCount {
    /// The region.
    pub region: RegionName,
    /// The fewest entries.
    pub at_least: u32,
}

/// When a stage counts as stuck. Any one limit reached is enough.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StuckDef {
    /// After this many inference rounds.
    #[serde(default)]
    pub after_iterations: Option<u32>,
    /// After this many minutes.
    #[serde(default)]
    pub after_minutes: Option<u32>,
    /// After this many edits of one file.
    #[serde(default)]
    pub after_same_file_edits: Option<u32>,
    /// After this many tool calls.
    #[serde(default)]
    pub after_tool_calls: Option<u32>,
}
