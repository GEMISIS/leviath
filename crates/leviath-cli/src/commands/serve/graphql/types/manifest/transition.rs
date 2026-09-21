//! How a run leaves one stage for the next: the edge, its condition, what it
//! carries, and what has to be true first.

use async_graphql::{Enum, SimpleObject};

use super::count;

/// When an edge may be taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum TransitionCondition {
    /// Taken as soon as the stage finishes.
    Always,
    /// The model chooses this edge over the stage's others.
    LlmChoice,
    /// Taken when the stage ends in an error.
    Error,
    /// Taken when the stage runs out of iterations.
    MaxIterations,
    /// Taken when the stage is detected stuck. The thresholds that arm it are
    /// on `stuck`.
    Stuck,
    /// Taken when no other edge can fire.
    DeadEnd,
}

impl From<&leviath_core::blueprint::TransitionCondition> for TransitionCondition {
    fn from(condition: &leviath_core::blueprint::TransitionCondition) -> Self {
        use leviath_core::blueprint::TransitionCondition as Core;
        match condition {
            Core::Always => Self::Always,
            Core::LlmChoice => Self::LlmChoice,
            Core::Error => Self::Error,
            Core::MaxIterations => Self::MaxIterations,
            Core::Stuck => Self::Stuck,
            Core::DeadEnd => Self::DeadEnd,
        }
    }
}

/// What happens to the context on the way through an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum TransitionTransform {
    /// Everything carries over untouched.
    Direct,
    /// The context is cleared.
    Clear,
    /// The context is compacted: the summarizer replaces it with a summary of
    /// itself. `transformConfig.compactPrompt` carries the instruction when the
    /// edge names one.
    Compact,
    /// Per-region, as `transformConfig` spells out.
    Custom,
}

impl From<&leviath_core::blueprint::EdgeTransform> for TransitionTransform {
    fn from(transform: &leviath_core::blueprint::EdgeTransform) -> Self {
        use leviath_core::blueprint::EdgeTransform as Core;
        match transform {
            Core::Direct => Self::Direct,
            Core::Clear => Self::Clear,
            Core::Compact { .. } => Self::Compact,
            Core::Custom { .. } => Self::Custom,
        }
    }
}

/// What a transform does in detail: which regions it carries, compacts and
/// clears, and what it asks the summarizer for.
///
/// Set for `COMPACT`, which carries the prompt and nothing else, and for
/// `CUSTOM`, which carries the per-region lists. Null for `DIRECT` and `CLEAR`,
/// which have nothing to say.
///
/// Region names rather than regions: a transform may name one a later edit
/// removed, and dropping it would hide the instruction rather than the problem.
#[derive(Debug, SimpleObject)]
pub(crate) struct TransformConfig {
    /// Regions carried over verbatim.
    pub(crate) carry: Vec<String>,
    /// Regions handed to the summarizer.
    pub(crate) compact: Vec<String>,
    /// Regions emptied.
    pub(crate) clear: Vec<String>,
    /// A prompt for the summarizer on this edge, in place of the default.
    pub(crate) compact_prompt: Option<String>,
}

/// A region and the fewest entries it must hold.
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionEntryRequirement {
    /// The region counted, by name.
    pub(crate) region: String,
    /// The fewest entries that satisfy the gate.
    pub(crate) at_least: i32,
}

/// What arms a `STUCK` edge.
///
/// At least one threshold is always set: an edge with none could never fire, so
/// the manifest parser refuses that shape rather than building a dead edge.
/// Every one counts against the current visit to the stage, so the same
/// blueprint can arm two stages differently.
#[derive(Debug, SimpleObject)]
pub(crate) struct StuckThresholds {
    /// Inferences run in this stage without finishing it.
    pub(crate) after_iterations: Option<i32>,
    /// Wall-clock minutes spent in this stage.
    pub(crate) after_minutes: Option<i32>,
    /// Writes against a single path in this stage: the "a hundred iterations in
    /// the wrong file" case.
    pub(crate) after_same_file_edits: Option<i32>,
    /// Tool calls made in this stage.
    pub(crate) after_tool_calls: Option<i32>,
}

impl From<&leviath_core::blueprint::StuckConfig> for StuckThresholds {
    fn from(stuck: &leviath_core::blueprint::StuckConfig) -> Self {
        Self {
            after_iterations: stuck.after_iterations.map(count),
            after_minutes: stuck.after_minutes.map(count),
            after_same_file_edits: stuck.after_same_file_edits.map(count),
            after_tool_calls: stuck.after_tool_calls.map(count),
        }
    }
}

/// What a stage must have done before an edge may be taken.
///
/// A gate that is not satisfied re-runs the stage with `message` instead of
/// transitioning, up to `maxAttempts` times, and then lets the run through: an
/// unmet gate slows a run down, it never strands one.
#[derive(Debug, SimpleObject)]
pub(crate) struct TransitionGate {
    /// The stage must have modified something.
    pub(crate) require_modifications: bool,
    /// A second way to satisfy `requireModifications`: this region holding
    /// anything also passes. It is an alternative rather than a requirement,
    /// because per-stage tool counters do not survive a daemon restart and a
    /// region does.
    pub(crate) region: Option<String>,
    /// Tools counted as modifying, beyond `write_file` and `edit_file`. For an
    /// blueprint whose writes go through MCP or a script.
    pub(crate) tools: Vec<String>,
    /// Regions that must all hold something. Conjunctive, unlike `region`.
    pub(crate) require_regions: Vec<String>,
    /// A region that must have changed during this stage, not merely be
    /// present. What a revise loop needs: re-emitting the same content
    /// satisfies a presence check.
    pub(crate) require_region_updated: Option<String>,
    /// A checklist region that must have no open items left.
    pub(crate) require_no_open_items: Option<String>,
    /// A region that must hold at least so many entries.
    pub(crate) require_region_entries: Option<RegionEntryRequirement>,
    /// Sent back to the stage while the gate holds it. A default explaining the
    /// framework's change tracking is generated when this is absent.
    pub(crate) message: Option<String>,
    /// How many times the stage is re-asked before the gate gives up and lets
    /// the transition through with a warning.
    pub(crate) max_attempts: Option<i32>,
}

impl From<&leviath_core::blueprint::TransitionGate> for TransitionGate {
    fn from(gate: &leviath_core::blueprint::TransitionGate) -> Self {
        Self {
            require_modifications: gate.require_modifications,
            region: gate.region.clone(),
            tools: gate.tools.clone(),
            require_regions: gate.require_regions.clone(),
            require_region_updated: gate.require_region_updated.clone(),
            require_no_open_items: gate.require_no_open_items.clone(),
            require_region_entries: gate.require_region_entries.as_ref().map(|needed| {
                RegionEntryRequirement {
                    region: needed.region.clone(),
                    at_least: count(needed.at_least),
                }
            }),
            message: gate.message.clone(),
            max_attempts: gate.max_attempts.map(count),
        }
    }
}

/// One outgoing edge of a stage.
///
/// A stage with no edges is terminal. The target is an object because the
/// manifest cannot name a stage it does not declare: that is refused at load.
#[derive(Debug, SimpleObject)]
pub(crate) struct TransitionEdge {
    /// The stage this edge leads to, by name. Look it up in the blueprint's
    /// `stages`.
    pub(crate) target: String,
    /// Told to the model when it is choosing where to go next.
    pub(crate) hint: Option<String>,
    /// When this edge may be taken.
    pub(crate) condition: TransitionCondition,
    /// What happens to the context on the way through.
    pub(crate) transform: TransitionTransform,
    /// The transform in detail: a `COMPACT` edge's prompt, or a `CUSTOM` edge's
    /// per-region lists. Null for `DIRECT` and `CLEAR`.
    pub(crate) transform_config: Option<TransformConfig>,
    /// What must be true before this edge is taken. Null when the edge asks for
    /// nothing beyond its condition.
    pub(crate) gate: Option<TransitionGate>,
    /// What arms this edge, for a `STUCK` condition. Null for every other
    /// condition, and never null for that one.
    pub(crate) stuck: Option<StuckThresholds>,
}

impl TransitionEdge {
    /// Describe one edge of a stage.
    pub(crate) fn from_core(edge: &leviath_core::blueprint::TransitionEdge) -> Self {
        use leviath_core::blueprint::EdgeTransform;
        let transform_config = match &edge.transform {
            EdgeTransform::Custom {
                carry,
                compact,
                clear,
                compact_prompt,
            } => Some(TransformConfig {
                carry: carry.clone(),
                compact: compact.clone(),
                clear: clear.clone(),
                compact_prompt: compact_prompt.clone(),
            }),
            // A compact edge takes the whole context, so it has no lists to
            // report: only what it asks the summarizer for.
            EdgeTransform::Compact { prompt } => Some(TransformConfig {
                carry: Vec::new(),
                compact: Vec::new(),
                clear: Vec::new(),
                compact_prompt: prompt.clone(),
            }),
            EdgeTransform::Direct | EdgeTransform::Clear => None,
        };
        Self {
            target: edge.target.clone(),
            hint: edge.hint.clone(),
            condition: TransitionCondition::from(&edge.condition),
            transform: TransitionTransform::from(&edge.transform),
            transform_config,
            gate: edge.gate.as_ref().map(TransitionGate::from),
            stuck: edge.stuck.as_ref().map(StuckThresholds::from),
        }
    }
}

/// What happens to one region's content as it crosses between blueprints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum MappingTransform {
    /// Carried verbatim.
    Direct,
    /// Summarized on the way.
    Summarize,
    /// Narrowed to named fields, and the rest dropped.
    Extract,
}

/// One region's route into another blueprint's layout.
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionMapping {
    /// The region it comes from, by name in the handing-off blueprint.
    pub(crate) from_region: String,
    /// The region it goes to, by name in the receiving blueprint.
    pub(crate) to_region: String,
    /// What happens to the content on the way. Null leaves it verbatim.
    pub(crate) transform: Option<MappingTransform>,
    /// The fields kept, for an `EXTRACT` transform. Empty for the others.
    pub(crate) fields: Vec<String>,
}

/// How this blueprint's context maps onto another's.
///
/// What a handoff needs: two blueprints with different memory structures cannot
/// simply pass a context along, so the blueprint that hands off says which of
/// its regions becomes which of the other's.
///
/// The blueprints are named rather than resolved. The receiving one has to be
/// installed for the handoff to happen, and it may not be installed now, so
/// naming it is the answer that stays true.
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextTransform {
    /// The blueprint handing off, by name.
    pub(crate) from_blueprint: String,
    /// The blueprint receiving, by name.
    pub(crate) to_blueprint: String,
    /// The region-to-region mappings.
    pub(crate) mappings: Vec<RegionMapping>,
}

impl From<&leviath_core::blueprint::ContextTransform> for ContextTransform {
    fn from(transform: &leviath_core::blueprint::ContextTransform) -> Self {
        use leviath_core::blueprint::ContentTransform as Content;
        Self {
            from_blueprint: transform.from_blueprint.clone(),
            to_blueprint: transform.to_blueprint.clone(),
            mappings: transform
                .mappings
                .iter()
                .map(|mapping| RegionMapping {
                    from_region: mapping.from_region.clone(),
                    to_region: mapping.to_region.clone(),
                    transform: mapping.transform.as_ref().map(|content| match content {
                        Content::Direct => MappingTransform::Direct,
                        Content::Summarize => MappingTransform::Summarize,
                        Content::Extract { .. } => MappingTransform::Extract,
                    }),
                    fields: match &mapping.transform {
                        Some(Content::Extract { fields }) => fields.clone(),
                        _ => Vec::new(),
                    },
                })
                .collect(),
        }
    }
}
