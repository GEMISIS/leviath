//! `Blueprint`: the manifest, typed.
//!
//! One type for one concept. The blueprint a run executed and the blueprint
//! installed under that name are the same kind of thing, read from different
//! files, so they are the same type here. What tells them apart is the id,
//! which carries the digest: two revisions of one name are two ids, so a
//! caching client cannot merge a run's frozen copy with whatever is installed
//! now.

use std::sync::Arc;

use async_graphql::{Enum, Object, SimpleObject};

use super::super::super::core::blueprints::BlueprintSource as CoreSource;
use leviath_core::Blueprint as CoreBlueprint;

/// How much of the digest an id carries.
///
/// Twelve hex characters is 48 bits. Enough that two manifests on one machine
/// colliding is not a thing that happens, and short enough to read in a log
/// line or a URL.
const ID_DIGEST_CHARS: usize = 12;

/// Where a blueprint was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum BlueprintSource {
    /// The run's own copy, written at spawn: what the run executed, whatever
    /// the installed file says now.
    Snapshot,
    /// The installed file. For a run, this means it kept no snapshot, so the
    /// file may have changed since it ran.
    Installed,
}

impl From<CoreSource> for BlueprintSource {
    fn from(source: CoreSource) -> Self {
        match source {
            CoreSource::Snapshot => Self::Snapshot,
            CoreSource::Installed => Self::Installed,
        }
    }
}

/// Whether a run sees tools that appear after it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ToolDiscovery {
    /// Tools are discovered once, at spawn. A tool installed mid-run reaches
    /// the next run, not this one.
    AtSpawnOnly,
    /// The run also scans its workdir's `tools/`, and re-advertises its tool
    /// set after a script is written. This decides when a run *sees* a new
    /// tool, not whether it may install one: that is what the tool permissions
    /// decide.
    RescanAfterWrites,
}

/// Whether a prompt hint is included at one manifest level.
///
/// Omitting guidance never discourages the behaviour. It leaves the paragraph
/// out of the system prompt, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum HintSetting {
    /// Defer to the level above: a stage defers to the blueprint, and the
    /// blueprint defers to the host's own setting.
    Inherit,
    /// Include the hint at this level.
    Include,
    /// Leave the hint out at this level, even where a broader level includes
    /// it.
    Omit,
}

impl From<Option<bool>> for HintSetting {
    fn from(declared: Option<bool>) -> Self {
        match declared {
            None => Self::Inherit,
            Some(true) => Self::Include,
            Some(false) => Self::Omit,
        }
    }
}

/// The prompt guidance a blueprint or a stage declares.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolUseGuidance {
    /// Whether the batch-independent-tool-calls paragraph is included.
    pub(crate) batch_independent_calls: HintSetting,
    /// Whether the platform shell paragraph is included. Even when included,
    /// it is only emitted where the host has shell guidance worth giving and
    /// the stage offers the `shell` tool.
    pub(crate) shell_for_multi_step_work: HintSetting,
}

/// How a stage runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum StageMode {
    /// The tight loop: infer, act on tool calls, repeat until a transition
    /// fires.
    Autonomous,
    /// Pauses for a person at every step.
    Interactive,
    /// Holds at each declared interaction point instead of at every step.
    InteractivePoints,
    /// Splits work across worker agents, then continues.
    FanOut,
    /// Produces the run's final answer and nothing else.
    Output,
}

impl From<&leviath_core::blueprint::StageMode> for StageMode {
    fn from(mode: &leviath_core::blueprint::StageMode) -> Self {
        use leviath_core::blueprint::StageMode as Core;
        match mode {
            Core::Autonomous => Self::Autonomous,
            Core::Interactive => Self::Interactive,
            Core::InteractivePoints { .. } => Self::InteractivePoints,
            Core::FanOut { .. } => Self::FanOut,
            Core::Output => Self::Output,
        }
    }
}

/// What a stage must produce before it may transition.
#[derive(Debug, SimpleObject)]
pub(crate) struct OutputRequirement {
    /// How many times the stage is asked again when it tries to leave without
    /// having submitted. After that the transition proceeds anyway and the
    /// run's `outputForced` counter records it: a missing answer never strands
    /// a run.
    pub(crate) reasks: i32,
}

/// One outgoing edge of a stage.
#[derive(Debug, SimpleObject)]
pub(crate) struct TransitionEdge {
    /// The stage this edge leads to.
    pub(crate) target: String,
    /// Told to the model when it is choosing where to go next.
    pub(crate) hint: Option<String>,
}

/// What a region does when it fills.
///
/// One value per kind the daemon recognises. The manifest accepts `hashmap`
/// and `hash_map` for the same kind, which is a spelling the TOML takes; the
/// API has one name for one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RegionKind {
    /// Stays in the prompt verbatim.
    Pinned,
    /// The default. Cleared or dropped as the window fills.
    Temporary,
    /// Emptied by an edge transform or an explicit clear.
    Clearable,
    /// Keeps the newest entries.
    SlidingWindow,
    /// Summarized by the compaction model as it fills.
    Compacting,
    /// Keeps compacted summaries of its own past.
    CompactHistory,
    /// Key-value entries with admission control.
    Hashmap,
    /// Open and done items a gate can require empty.
    Checklist,
    /// Behaviour defined by a Rhai script under `context_hooks/`.
    Custom,
}

impl From<&leviath_core::region::RegionKind> for RegionKind {
    fn from(kind: &leviath_core::region::RegionKind) -> Self {
        use leviath_core::region::RegionKind as Core;
        match kind {
            Core::Pinned => Self::Pinned,
            Core::Temporary => Self::Temporary,
            Core::Clearable => Self::Clearable,
            Core::SlidingWindow { .. } => Self::SlidingWindow,
            Core::Compacting { .. } => Self::Compacting,
            Core::CompactHistory { .. } => Self::CompactHistory,
            Core::HashMap { .. } => Self::Hashmap,
            Core::Checklist => Self::Checklist,
            Core::Custom { .. } => Self::Custom,
        }
    }
}

/// One context region a blueprint declares.
pub(crate) struct Region {
    /// The blueprint this region belongs to, shared rather than copied.
    pub(crate) blueprint: Arc<CoreBlueprint>,
    /// Which region, by position in the blueprint's layout.
    pub(crate) at: usize,
}

#[Object]
impl Region {
    /// Region name, unique within the blueprint.
    async fn name(&self) -> &str {
        &self.region().name
    }

    /// What the region does when it fills.
    async fn kind(&self) -> RegionKind {
        RegionKind::from(&self.region().kind)
    }

    /// Hard token ceiling for the region.
    async fn max_tokens(&self) -> i32 {
        i32::try_from(self.region().max_tokens).unwrap_or(i32::MAX)
    }

    /// One line on what this region is for.
    async fn description(&self) -> Option<&str> {
        self.region().description.as_deref()
    }

    /// Whether the run may not proceed while this region is empty.
    async fn required(&self) -> bool {
        self.region().required
    }

    /// Whether the description is also shown to the model, above the region's
    /// contents.
    async fn describe_in_prompt(&self) -> bool {
        self.region().describe_in_prompt
    }
}

impl Region {
    /// The region this object stands for.
    fn region(&self) -> &leviath_core::layout::RegionDefinition {
        &self.blueprint.context_layout.regions[self.at]
    }
}

/// One stage of a blueprint.
pub(crate) struct Stage {
    /// The blueprint this stage belongs to, shared rather than copied.
    pub(crate) blueprint: Arc<CoreBlueprint>,
    /// Which stage, by declaration order.
    pub(crate) at: usize,
}

#[Object]
impl Stage {
    /// Stage name, unique within the blueprint.
    async fn name(&self) -> &str {
        &self.stage().name
    }

    /// How the stage runs.
    async fn mode(&self) -> StageMode {
        StageMode::from(&self.stage().mode)
    }

    /// One line on what the stage is for.
    async fn description(&self) -> Option<&str> {
        self.stage().description.as_deref()
    }

    /// The only tools the model is offered here. Group tokens (`@all`,
    /// `@builtin`, `@subagent`, `@scripts`, `@mcp`) stand for whole sources and
    /// resolve at spawn.
    async fn available_tools(&self) -> &[String] {
        &self.stage().available_tools
    }

    /// Tools this stage cannot work without. These survive an unattended run,
    /// where the blocking interaction tools are otherwise withheld.
    async fn required_tools(&self) -> &[String] {
        &self.stage().required_tools
    }

    /// MCP servers whose whole tool set this stage may use.
    async fn available_connectors(&self) -> &[String] {
        &self.stage().available_connectors
    }

    /// Inference-turn bound for one visit to this stage.
    async fn max_iterations(&self) -> Option<i32> {
        self.stage()
            .max_iterations
            .map(|n| i32::try_from(n).unwrap_or(i32::MAX))
    }

    /// How many times the run may re-enter this stage.
    async fn max_revisits(&self) -> Option<i32> {
        self.stage()
            .max_revisits
            .map(|n| i32::try_from(n).unwrap_or(i32::MAX))
    }

    /// What this stage must submit before it transitions. Null when it may
    /// leave without producing anything.
    async fn output_requirement(&self) -> Option<OutputRequirement> {
        self.stage().require_output.then(|| OutputRequirement {
            reasks: i32::try_from(leviath_core::blueprint::DEFAULT_OUTPUT_REENTRY_CAP)
                .unwrap_or(i32::MAX),
        })
    }

    /// Whether `sendMessage` reaches a run parked in this stage.
    async fn accepts_messages(&self) -> bool {
        self.stage().accepts_messages
    }

    /// Whether this stage may end the run outright.
    async fn allow_complete(&self) -> bool {
        self.stage().allow_complete
    }

    /// Whether this stage may run as a fan-out worker or sub-agent.
    async fn allow_as_worker(&self) -> bool {
        self.stage().allow_as_worker
    }

    /// Whether this stage may not finish until its child runs complete.
    async fn requires_children(&self) -> bool {
        self.stage().requires_children
    }

    /// Records that the author deliberately offers the human-in-the-loop tools
    /// while this stage runs autonomously.
    ///
    /// Grants nothing and changes no behaviour. Its one consumer is
    /// `lev validate`, where it silences the
    /// `blocking-tool-in-autonomous-stage` lint. An autonomous stage that
    /// calls one of those tools with nobody attached parks in `WAITING_INPUT`
    /// until a person answers or the run is cancelled.
    async fn declares_blocking_tools(&self) -> bool {
        self.stage().allow_blocking_tools
    }

    /// The prompt guidance this stage declares, before the cascade.
    async fn tool_guidance(&self) -> ToolUseGuidance {
        ToolUseGuidance {
            batch_independent_calls: self.stage().batch_tool_hint.into(),
            shell_for_multi_step_work: self.stage().shell_hint.into(),
        }
    }

    /// Outgoing edges. An empty list marks a terminal stage.
    async fn transitions(&self) -> Vec<TransitionEdge> {
        let mut edges: Vec<TransitionEdge> = self
            .stage()
            .transitions
            .iter()
            .flatten()
            .map(|(target, edge)| TransitionEdge {
                target: target.clone(),
                hint: edge.hint.clone(),
            })
            .collect();
        // The manifest holds these in a map, so a listing sorted by target is
        // the only order two identical requests can both produce.
        edges.sort_by(|a, b| a.target.cmp(&b.target));
        edges
    }
}

impl Stage {
    /// The stage this object stands for.
    fn stage(&self) -> &leviath_core::blueprint::Stage {
        &self.blueprint.stages[self.at]
    }
}

/// An agent blueprint: the full manifest.
///
/// From a run, this is the manifest the run executed. From the blueprint
/// listing, it is the definition installed now. The `source` field says which,
/// and the digest in the id says whether they are the same bytes.
pub(crate) struct Blueprint {
    /// The parsed manifest, shared with the parse cache.
    pub(crate) parsed: Arc<CoreBlueprint>,
    /// Lowercase hex SHA-256 of the manifest text.
    pub(crate) digest: String,
    /// Which file it was read from.
    pub(crate) source: BlueprintSource,
}

#[Object]
impl Blueprint {
    /// This revision's id: `<name>@<digest prefix>`.
    ///
    /// The digest is part of the identity on purpose. A run's frozen copy and
    /// the installed blueprint share a name and may differ in every other way,
    /// and a client that caches by type and id would otherwise merge the two.
    async fn id(&self) -> String {
        let short: String = self.digest.chars().take(ID_DIGEST_CHARS).collect();
        format!("{}@{short}", self.parsed.name)
    }

    /// Unique within the installed set.
    async fn name(&self) -> &str {
        &self.parsed.name
    }

    /// Content digest of this manifest, lowercase hex SHA-256.
    async fn digest(&self) -> &str {
        &self.digest
    }

    /// Which file this was read from.
    async fn source(&self) -> BlueprintSource {
        self.source
    }

    /// From `[agent] version`.
    async fn version(&self) -> &str {
        &self.parsed.version
    }

    /// From `[agent] description`.
    async fn description(&self) -> &str {
        &self.parsed.description
    }

    /// The stage a run starts in. Defaults to the first stage declared.
    async fn entry_stage(&self) -> Option<Stage> {
        let named = self.parsed.entry_stage.as_deref();
        let at = match named {
            None => 0,
            Some(name) => self.parsed.stages.iter().position(|s| s.name == name)?,
        };
        self.parsed.stages.get(at).map(|_| Stage {
            blueprint: Arc::clone(&self.parsed),
            at,
        })
    }

    /// How deep sub-agent spawning may nest.
    async fn max_child_depth(&self) -> Option<i32> {
        self.parsed
            .max_child_depth
            .map(|n| i32::try_from(n).unwrap_or(i32::MAX))
    }

    /// Whether a run sees tools that appear after it started.
    async fn tool_discovery(&self) -> ToolDiscovery {
        match self.parsed.dynamic_tools {
            true => ToolDiscovery::RescanAfterWrites,
            false => ToolDiscovery::AtSpawnOnly,
        }
    }

    /// The prompt guidance this blueprint declares, before the cascade.
    async fn tool_guidance(&self) -> ToolUseGuidance {
        ToolUseGuidance {
            batch_independent_calls: self.parsed.batch_tool_hint.into(),
            shell_for_multi_step_work: self.parsed.shell_hint.into(),
        }
    }

    /// One entry per stage, in declaration order.
    async fn stages(&self) -> Vec<Stage> {
        (0..self.parsed.stages.len())
            .map(|at| Stage {
                blueprint: Arc::clone(&self.parsed),
                at,
            })
            .collect()
    }

    /// One entry per declared context region.
    async fn regions(&self) -> Vec<Region> {
        (0..self.parsed.context_layout.regions.len())
            .map(|at| Region {
                blueprint: Arc::clone(&self.parsed),
                at,
            })
            .collect()
    }

    /// Paths this agent declares it needs beyond its workdir. Declaring is not
    /// granting: an entry takes effect only where the host grants it.
    async fn read_paths(&self) -> &[String] {
        match self.parsed.read_paths.as_ref() {
            Some(config) => &config.allow,
            None => &[],
        }
    }
}

#[cfg(test)]
#[path = "blueprint_tests.rs"]
mod tests;
