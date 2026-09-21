//! How a run leaves one stage for the next: the edge, its condition, what it
//! carries, and what has to be true first.

use std::sync::Arc;

use async_graphql::{Enum, Object, SimpleObject};

use leviath_core::Blueprint as CoreBlueprint;

use super::super::blueprint::Region;
use super::count;
use super::refs;
use super::stage::Stage;

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

/// The three region lists a `CUSTOM` transform carries, as the edge wrote them.
#[derive(Debug, Default, Clone)]
pub(crate) struct TransformRegions {
    /// Names carried over verbatim.
    pub(crate) carry: Vec<String>,
    /// Names handed to the summarizer.
    pub(crate) compact: Vec<String>,
    /// Names emptied.
    pub(crate) clear: Vec<String>,
}

/// The resolver state behind the `TransformConfig` type.
pub(crate) struct TransformConfig {
    /// The blueprint the region names resolve in.
    blueprint: Arc<CoreBlueprint>,
    /// The names the edge wrote.
    named: TransformRegions,
    /// A prompt for the summarizer on this edge.
    compact_prompt: Option<String>,
}

/// What a transform does in detail: which regions it carries, compacts and
/// clears, and what it asks the summarizer for.
///
/// Set for `COMPACT`, which carries the prompt and nothing else, and for
/// `CUSTOM`, which carries the per-region lists. Null for `DIRECT` and `CLEAR`,
/// which have nothing to say.
#[Object]
impl TransformConfig {
    /// Regions carried over verbatim.
    ///
    /// One entry per name the edge wrote that a layout in this blueprint
    /// declares. A name with no declaration is in `carryNames` and not here:
    /// either a later edit removed the region, or the edge names one of the four
    /// the runtime carries whatever a manifest says - `conversation`,
    /// `tool_results`, `final_output` and `stage_instructions` - which exist at
    /// run time with nothing declared to read.
    async fn carry(&self) -> Vec<Region> {
        refs::regions(&self.blueprint, &self.named.carry)
    }

    /// Every name the edge wrote in `carry`, verbatim and in order, declared or
    /// not.
    async fn carry_names(&self) -> &[String] {
        &self.named.carry
    }

    /// Regions handed to the summarizer. Declared names only; `compactNames`
    /// carries the whole list.
    async fn compact(&self) -> Vec<Region> {
        refs::regions(&self.blueprint, &self.named.compact)
    }

    /// Every name the edge wrote in `compact`, verbatim and in order.
    async fn compact_names(&self) -> &[String] {
        &self.named.compact
    }

    /// Regions emptied. Declared names only; `clearNames` carries the whole
    /// list.
    async fn clear(&self) -> Vec<Region> {
        refs::regions(&self.blueprint, &self.named.clear)
    }

    /// Every name the edge wrote in `clear`, verbatim and in order.
    async fn clear_names(&self) -> &[String] {
        &self.named.clear
    }

    /// A prompt for the summarizer on this edge, in place of the default.
    async fn compact_prompt(&self) -> Option<&str> {
        self.compact_prompt.as_deref()
    }
}

/// The resolver state behind the `RegionEntryRequirement` type.
pub(crate) struct RegionEntryRequirement {
    /// The blueprint the region name resolves in.
    blueprint: Arc<CoreBlueprint>,
    /// The name the gate wrote.
    region: String,
    /// The fewest entries that satisfy the gate.
    at_least: i32,
}

/// A region and the fewest entries it must hold.
#[Object]
impl RegionEntryRequirement {
    /// The region counted.
    ///
    /// Null where no layout in this blueprint declares that name.
    /// `regionName` carries the name either way.
    async fn region(&self) -> Option<Region> {
        refs::region(&self.blueprint, &self.region)
    }

    /// The region name the gate wrote, verbatim.
    async fn region_name(&self) -> &str {
        &self.region
    }

    /// The fewest entries that satisfy the gate.
    async fn at_least(&self) -> i32 {
        self.at_least
    }
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

/// The resolver state behind the `TransitionGate` type.
pub(crate) struct TransitionGate {
    /// The blueprint the region names resolve in.
    blueprint: Arc<CoreBlueprint>,
    /// The gate as the edge wrote it.
    gate: leviath_core::blueprint::TransitionGate,
}

/// What a stage must have done before an edge may be taken.
///
/// A gate that is not satisfied re-runs the stage with `message` instead of
/// transitioning, up to `maxAttempts` times, and then lets the run through: an
/// unmet gate slows a run down, it never strands one.
#[Object]
impl TransitionGate {
    /// The stage must have modified something.
    async fn require_modifications(&self) -> bool {
        self.gate.require_modifications
    }

    /// A second way to satisfy `requireModifications`: this region holding
    /// anything also passes. It is an alternative rather than a requirement,
    /// because per-stage tool counters do not survive a daemon restart and a
    /// region does.
    ///
    /// Null when the gate names no region, and also when it names one no layout
    /// in this blueprint declares. `regionName` tells those apart: it is null
    /// only in the first case.
    async fn region(&self) -> Option<Region> {
        refs::region(&self.blueprint, self.gate.region.as_deref()?)
    }

    /// The region name the gate wrote for its `requireModifications`
    /// alternative, verbatim. Null when it names none.
    async fn region_name(&self) -> Option<&str> {
        self.gate.region.as_deref()
    }

    /// Tools counted as modifying, beyond `write_file` and `edit_file`. For an
    /// blueprint whose writes go through MCP or a script.
    ///
    /// Names rather than `Tool`s, because an MCP server's tool is exactly what
    /// this list is for and an inventory does not describe one.
    async fn tools(&self) -> &[String] {
        &self.gate.tools
    }

    /// Regions that must all hold something. Conjunctive, unlike `region`.
    ///
    /// Declared names only. `requireRegionNames` carries every name the gate
    /// wrote, which is where a name with no declaration in this blueprint stays
    /// readable.
    async fn require_regions(&self) -> Vec<Region> {
        refs::regions(&self.blueprint, &self.gate.require_regions)
    }

    /// Every name the gate wrote in `requireRegions`, verbatim and in order,
    /// declared or not.
    async fn require_region_names(&self) -> &[String] {
        &self.gate.require_regions
    }

    /// A region that must have changed during this stage, not merely be
    /// present. What a revise loop needs: re-emitting the same content
    /// satisfies a presence check.
    ///
    /// Null when the gate asks for none, and also when it names one no layout
    /// declares. `requireRegionUpdatedName` tells those apart.
    async fn require_region_updated(&self) -> Option<Region> {
        refs::region(
            &self.blueprint,
            self.gate.require_region_updated.as_deref()?,
        )
    }

    /// The name the gate wrote for `requireRegionUpdated`, verbatim. Null when
    /// it asks for none.
    async fn require_region_updated_name(&self) -> Option<&str> {
        self.gate.require_region_updated.as_deref()
    }

    /// A checklist region that must have no open items left.
    ///
    /// Null when the gate asks for none, and also when it names one no layout
    /// declares. `requireNoOpenItemsName` tells those apart.
    async fn require_no_open_items(&self) -> Option<Region> {
        refs::region(&self.blueprint, self.gate.require_no_open_items.as_deref()?)
    }

    /// The name the gate wrote for `requireNoOpenItems`, verbatim. Null when it
    /// asks for none.
    async fn require_no_open_items_name(&self) -> Option<&str> {
        self.gate.require_no_open_items.as_deref()
    }

    /// A region that must hold at least so many entries.
    async fn require_region_entries(&self) -> Option<RegionEntryRequirement> {
        self.gate
            .require_region_entries
            .as_ref()
            .map(|needed| RegionEntryRequirement {
                blueprint: Arc::clone(&self.blueprint),
                region: needed.region.clone(),
                at_least: count(needed.at_least),
            })
    }

    /// Sent back to the stage while the gate holds it. A default explaining the
    /// framework's change tracking is generated when this is absent.
    async fn message(&self) -> Option<&str> {
        self.gate.message.as_deref()
    }

    /// How many times the stage is re-asked before the gate gives up and lets
    /// the transition through with a warning.
    async fn max_attempts(&self) -> Option<i32> {
        self.gate.max_attempts.map(count)
    }
}

/// The resolver state behind the `TransitionEdge` type.
pub(crate) struct TransitionEdge {
    /// The blueprint the target stage resolves in.
    blueprint: Arc<CoreBlueprint>,
    /// The stage name this edge leads to, as the manifest keyed it.
    pub(crate) target: String,
    /// The edge as the manifest wrote it.
    edge: leviath_core::blueprint::TransitionEdge,
}

/// One outgoing edge of a stage.
///
/// A stage with no edges is terminal.
#[Object]
impl TransitionEdge {
    /// The stage this edge leads to.
    ///
    /// Null only where the blueprint names a stage it does not declare, which
    /// `lev validate` refuses and the daemon will not spawn: an installed
    /// manifest can still be in that state, and `targetName` is the name it
    /// wrote.
    async fn target(&self) -> Option<Stage> {
        refs::stage(&self.blueprint, &self.target)
    }

    /// The stage name this edge was keyed by, verbatim.
    async fn target_name(&self) -> &str {
        &self.target
    }

    /// Told to the model when it is choosing where to go next.
    async fn hint(&self) -> Option<&str> {
        self.edge.hint.as_deref()
    }

    /// When this edge may be taken.
    async fn condition(&self) -> TransitionCondition {
        TransitionCondition::from(&self.edge.condition)
    }

    /// What happens to the context on the way through.
    async fn transform(&self) -> TransitionTransform {
        TransitionTransform::from(&self.edge.transform)
    }

    /// The transform in detail: a `COMPACT` edge's prompt, or a `CUSTOM` edge's
    /// per-region lists. Null for `DIRECT` and `CLEAR`.
    async fn transform_config(&self) -> Option<TransformConfig> {
        use leviath_core::blueprint::EdgeTransform;
        let (named, compact_prompt) = match &self.edge.transform {
            EdgeTransform::Custom {
                carry,
                compact,
                clear,
                compact_prompt,
            } => (
                TransformRegions {
                    carry: carry.clone(),
                    compact: compact.clone(),
                    clear: clear.clone(),
                },
                compact_prompt.clone(),
            ),
            // A compact edge takes the whole context, so it has no lists to
            // report: only what it asks the summarizer for.
            EdgeTransform::Compact { prompt } => (TransformRegions::default(), prompt.clone()),
            EdgeTransform::Direct | EdgeTransform::Clear => return None,
        };
        Some(TransformConfig {
            blueprint: Arc::clone(&self.blueprint),
            named,
            compact_prompt,
        })
    }

    /// What must be true before this edge is taken. Null when the edge asks for
    /// nothing beyond its condition.
    async fn gate(&self) -> Option<TransitionGate> {
        self.edge.gate.as_ref().map(|gate| TransitionGate {
            blueprint: Arc::clone(&self.blueprint),
            gate: gate.clone(),
        })
    }

    /// What arms this edge, for a `STUCK` condition. Null for every other
    /// condition, and never null for that one.
    async fn stuck(&self) -> Option<StuckThresholds> {
        self.edge.stuck.as_ref().map(StuckThresholds::from)
    }
}

impl TransitionEdge {
    /// Describe one edge of a stage.
    ///
    /// The manifest keys these by target and the parser fills the edge's own
    /// copy from that key, so the map key is passed in as the authority: an
    /// older record can carry an empty one.
    pub(crate) fn of(
        blueprint: &Arc<CoreBlueprint>,
        target: &str,
        edge: &leviath_core::blueprint::TransitionEdge,
    ) -> Self {
        Self {
            blueprint: Arc::clone(blueprint),
            target: target.to_string(),
            edge: edge.clone(),
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
///
/// Both regions are names. Each belongs to a blueprint the mapping names rather
/// than to this one, and the receiving blueprint has to be installed for the
/// handoff to happen at all, so neither side has a declaration to read here.
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
