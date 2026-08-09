//! How a run leaves one stage for the next.
//!
//! An edge carries a condition (when it may be taken), a transform (what of the
//! context comes along), and a gate (what must be true first). Nudges and stuck
//! detection are here too, because both exist to answer the same question: this
//! stage is not finishing, so what should happen.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::layout::ContextLayout;

/// Context transform for converting between agent types.
///
/// When spawning a sub-agent with a different blueprint, transforms define
/// how to map regions from the parent agent's context to the child agent's
/// context. This enables smooth handoffs between agents with different
/// memory structures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextTransform {
    /// Source blueprint name
    pub from_blueprint: String,

    /// Target blueprint name
    pub to_blueprint: String,

    /// Region mapping rules
    pub mappings: Vec<RegionMapping>,
}

impl ContextTransform {
    /// Validate that this transform references valid regions.
    pub(super) fn validate(
        &self,
        layout: &ContextLayout,
    ) -> std::result::Result<(), ValidationError> {
        for mapping in &self.mappings {
            // We can only validate target regions against the current layout
            // (source regions belong to a different blueprint)
            if layout.get_region(&mapping.to_region).is_none() {
                return Err(ValidationError::Region {
                    region: mapping.to_region.clone(),
                    message: "transform target region not found in layout".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Mapping rule for a single region in a context transform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionMapping {
    /// Source region name
    pub from_region: String,

    /// Target region name
    pub to_region: String,

    /// Optional transformation to apply to content
    pub transform: Option<ContentTransform>,
}

/// A directed transition edge from one stage to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionEdge {
    /// Target stage name (derived from the HashMap key during parsing)
    pub target: String,

    /// When this edge is available
    #[serde(default)]
    pub condition: TransitionCondition,

    /// Human-readable hint for the LLM
    pub hint: Option<String>,

    /// How context transforms when crossing this edge
    #[serde(default)]
    pub transform: EdgeTransform,

    /// Preconditions the agent must satisfy before this edge may be followed.
    /// Absent ⇒ the edge is unconditional (beyond its `condition`).
    #[serde(default)]
    pub gate: Option<TransitionGate>,

    /// Thresholds arming a [`TransitionCondition::Stuck`] edge. `Some` iff the
    /// condition is `Stuck` - both the manifest parser and [`super::Blueprint::validate`]
    /// reject the two half-configured shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck: Option<StuckConfig>,
}

/// Thresholds that arm a [`TransitionCondition::Stuck`] edge.
///
/// At least one threshold is always set: an edge with none could never fire, so
/// both the manifest parser and [`super::Blueprint::validate`] reject that shape rather
/// than build a dead edge. Every threshold is evaluated against the *current
/// stage's* progress counters, which reset on each stage entry - so a blueprint
/// can arm different stages with different thresholds independently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StuckConfig {
    /// `stuck_after_iterations`: inferences run in this stage without finishing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_iterations: Option<usize>,

    /// `stuck_after_minutes`: wall-clock minutes spent in this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_minutes: Option<usize>,

    /// `stuck_after_same_file_edits`: `write_file`/`edit_file` calls against a
    /// single path in this stage - the "100 iterations in the wrong file" mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_same_file_edits: Option<usize>,

    /// `stuck_after_tool_calls`: total tool calls made in this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tool_calls: Option<usize>,
}

impl StuckConfig {
    /// Whether any threshold is set. `false` ⇒ the edge could never fire.
    pub fn is_armed(&self) -> bool {
        self.after_iterations.is_some()
            || self.after_minutes.is_some()
            || self.after_same_file_edits.is_some()
            || self.after_tool_calls.is_some()
    }
}

/// Preconditions an edge imposes on the stage it leaves, checked once the edge
/// has been chosen but before its transform runs. A gate that isn't satisfied
/// re-runs the stage with a `[System]` nudge instead of transitioning.
///
/// The motivating case: an agent that reads and reasons about the
/// codebase entirely through `shell` and reaches the review stage without ever
/// having called a file-writing tool, producing a run with no output at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionGate {
    /// Require at least one successful file-modifying tool call in the stage
    /// being left.
    #[serde(default)]
    pub require_modifications: bool,

    /// Nudge injected when the gate blocks. A default explaining the framework's
    /// change tracking is generated when absent.
    #[serde(default)]
    pub message: Option<String>,

    /// Region whose non-emptiness also satisfies the gate. Per-stage tool-call
    /// counters reset on stage entry and are not restored when a run resumes
    /// after a daemon restart, but context regions are - so pointing the gate at
    /// the region the write tools are routed into keeps a resumed run honest.
    #[serde(default)]
    pub region: Option<String>,

    /// Tool names counted as modifying beyond the built-in `write_file` /
    /// `edit_file` - for agents whose writes go through MCP or script tools.
    #[serde(default)]
    pub tools: Vec<String>,

    /// How many times the stage is re-run before the gate gives up and lets the
    /// transition through (with a warning). Defaults to
    /// [`DEFAULT_GATE_ATTEMPTS`].
    #[serde(default)]
    pub max_attempts: Option<usize>,
}

/// Default re-run budget for an unsatisfied [`TransitionGate`].
pub const DEFAULT_GATE_ATTEMPTS: usize = 3;

/// Built-in tools that modify files on disk, for [`TransitionGate`]'s
/// `require_modifications` accounting. Extended per-edge by
/// [`TransitionGate::tools`].
pub const MODIFYING_TOOLS: &[&str] = &["write_file", "edit_file"];

/// The tool an agent calls to hand back the run's final output.
///
/// Named here rather than in `leviath-tools` because both the blueprint
/// validator and the manifest parser need it, and neither may depend on the
/// tools crate.
pub const SUBMIT_OUTPUT_TOOL: &str = "submit_output";

/// Times a stage is re-run for a missing final output before the gate gives up
/// and lets it through with the run's `output_forced` flag set. Matches
/// [`DEFAULT_GATE_ATTEMPTS`], and is overridden by a stage's `max_revisits`.
pub const DEFAULT_OUTPUT_REENTRY_CAP: usize = 3;

/// Settings for the empty-response nudge: the `[System]` message injected when
/// a stage's model replies with text before making any tool call.
///
/// Every field is optional. A field left unset cascades stage → agent → global
/// config and finally to the built-in default, so a `[stages.<name>.nudge]`
/// block only has to name what it wants to change. An empty block is inert.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NudgeConfig {
    /// Whether the nudge fires at all. When unset at every level, the default
    /// is on - except for a stage with interaction points, whose text response
    /// is its work product and which is left alone. Setting this explicitly at
    /// any level overrides that implicit rule in both directions.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// How many text-only responses to nudge before accepting the text as
    /// final. Defaults to [`DEFAULT_MAX_NUDGES`].
    #[serde(default)]
    pub max: Option<usize>,

    /// The nudge text. Defaults to [`DEFAULT_NUDGE_TEXT`]. Supports `{stage}`
    /// (the stage's name) and `{regions}` (comma-separated names of the
    /// stage's required context regions) placeholders.
    #[serde(default)]
    pub text: Option<String>,
}

/// Default nudge injected when a model responds with text before making any
/// tool call, used when no [`NudgeConfig`] level sets `text`.
pub const DEFAULT_NUDGE_TEXT: &str = "You have tools available. Please use them to complete the task. Start by reading the relevant files in the working directory.";

/// Default number of text-only responses to nudge before accepting the text as
/// final, used when no [`NudgeConfig`] level sets `max`.
pub const DEFAULT_MAX_NUDGES: usize = 3;

/// A fully-resolved nudge policy for one stage: every [`NudgeConfig`] field
/// cascaded and defaulted. Produced by [`resolve_nudge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNudge {
    /// Whether the nudge fires for this stage.
    pub enabled: bool,
    /// Text-only responses tolerated before the text is accepted as final.
    pub max: usize,
    /// The nudge text, before placeholder interpolation.
    pub text: String,
}

/// Resolve the nudge policy for a stage, cascading each field independently
/// stage → agent → global. Narrowest level wins with no clamping - like
/// [`crate::taint::resolve_batch_tool_hint`], this is a UX knob, not a
/// permission, so a manifest may raise `max` above the global setting.
///
/// `stage_is_reviewed` feeds only the *default* for `enabled`: a stage with
/// interaction points presents its text for the user to approve, so nudging it
/// to "use your tools" is off unless some level explicitly turns it on.
pub fn resolve_nudge(
    global: Option<&NudgeConfig>,
    agent: Option<&NudgeConfig>,
    stage: Option<&NudgeConfig>,
    stage_is_reviewed: bool,
) -> ResolvedNudge {
    fn field<T: Clone>(
        global: Option<&NudgeConfig>,
        agent: Option<&NudgeConfig>,
        stage: Option<&NudgeConfig>,
        get: impl Fn(&NudgeConfig) -> Option<T>,
    ) -> Option<T> {
        stage
            .and_then(&get)
            .or_else(|| agent.and_then(&get))
            .or_else(|| global.and_then(&get))
    }
    ResolvedNudge {
        enabled: field(global, agent, stage, |c| c.enabled).unwrap_or(!stage_is_reviewed),
        max: field(global, agent, stage, |c| c.max).unwrap_or(DEFAULT_MAX_NUDGES),
        text: field(global, agent, stage, |c| c.text.clone())
            .unwrap_or_else(|| DEFAULT_NUDGE_TEXT.to_string()),
    }
}

/// Condition that determines when a transition edge is available.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionCondition {
    /// Always available (LLM chooses)
    #[default]
    Always,
    /// Only on error
    Error,
    /// Only when max_iterations hit
    MaxIterations,
    /// LLM picks from available transitions (default for multi-transition stages)
    LlmChoice,
    /// Fires when the graph would otherwise strand here: the stage finished, and
    /// every normal (`always`/`llm_choice`) edge's target has spent its
    /// `max_revisits`.
    ///
    /// Exists because the alternatives were both bad. Declaring an ordinary edge
    /// to the output stage silences the `dead-end-possible` lint by adding a
    /// route the model can take at the end of *every* visit - measured, that
    /// collapsed pipelines in 10 of 24 runs of one agent and 21 of 36 of
    /// another. Declaring nothing leaves the run to die with everything it
    /// established thrown away. This edge is reachable when stuck and invisible
    /// the rest of the time, which is what "escape" actually means.
    ///
    /// Deliberately not `max_iterations`: that fires when a stage burns its
    /// iteration budget, which is a different event and does not fire here at
    /// all.
    DeadEnd,
    /// Fires *mid-stage* when the stage's runtime metrics cross this edge's
    /// [`StuckConfig`] thresholds - the agent is burning iterations, wall clock,
    /// or edits to one file without finishing. Unlike every other condition this
    /// interrupts a stage the agent never said it had completed, so when the edge
    /// is unavailable the runtime resumes the stage rather than transitioning.
    Stuck,
}

/// How context transforms when crossing a transition edge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeTransform {
    /// Copy everything as-is (default for single-transition linear stages)
    #[default]
    Direct,

    /// Clear stage-specific regions, keep pinned/system
    Clear,

    /// LLM-compact stage content into summary
    Compact {
        /// What to ask the compaction model for, replacing the built-in
        /// instruction. `None` uses the default summary prompt.
        #[serde(default)]
        prompt: Option<String>,
    },

    /// Per-region rules
    Custom {
        /// Regions copied through untouched.
        carry: Vec<String>,
        /// Regions replaced by an LLM summary of themselves.
        compact: Vec<String>,
        /// Regions emptied on the way across.
        clear: Vec<String>,
        /// The instruction used for everything in `compact`. `None` uses the
        /// default summary prompt.
        compact_prompt: Option<String>,
    },
}

impl PartialEq for EdgeTransform {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Direct, Self::Direct) | (Self::Clear, Self::Clear) => true,
            (Self::Compact { prompt: a }, Self::Compact { prompt: b }) => a == b,
            (
                Self::Custom {
                    carry: ca,
                    compact: coa,
                    clear: cla,
                    compact_prompt: cpa,
                },
                Self::Custom {
                    carry: cb,
                    compact: cob,
                    clear: clb,
                    compact_prompt: cpb,
                },
            ) => ca == cb && coa == cob && cla == clb && cpa == cpb,
            _ => false,
        }
    }
}
impl Eq for EdgeTransform {}

/// Content transformation type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentTransform {
    /// Copy content as-is
    Direct,

    /// Summarize content to fit target region
    Summarize,

    /// Extract specific fields
    Extract {
        /// Which fields to keep, by name. Anything else is dropped.
        fields: Vec<String>,
    },
}
