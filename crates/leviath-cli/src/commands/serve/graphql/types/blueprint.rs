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
use super::manifest::count;
use super::manifest::dependency::BlueprintDependency;
use super::manifest::mime::BlueprintMimeRow;
use super::manifest::output::OutputSpec;
use super::manifest::region::{
    RegionAdmission, RegionEviction, RegionSeed, RegionStrategy, RegionVolatility,
};
use super::manifest::runtime::{
    BlueprintSecurity, CompactionConfig, FileTrackingConfig, NudgeConfig, RepetitionDetection,
    SafeCommands, SandboxConfig,
};
use super::manifest::stage::Stage;
use super::manifest::transition::ContextTransform;
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

    /// Hard token ceiling for the region, as resolved for this layout.
    ///
    /// A region whose budget is a share of the window carries the share in
    /// `budgetPercent`, and this number is what that resolved to against the
    /// layout's own window.
    async fn max_tokens(&self) -> i32 {
        count(self.region().max_tokens)
    }

    /// The share of the model's context window this region claims, as a
    /// percentage. Null when the region names a fixed ceiling instead.
    async fn budget_percent(&self) -> Option<f64> {
        match &self.region().budget {
            leviath_core::layout::BudgetSpec::Percent { percent, .. } => Some(percent * 100.0),
            leviath_core::layout::BudgetSpec::Absolute(_) => None,
        }
    }

    /// The floor a percentage budget resolves no lower than, so a small-context
    /// model does not starve the region.
    async fn min_tokens(&self) -> Option<i32> {
        match &self.region().budget {
            leviath_core::layout::BudgetSpec::Percent { min, .. } => min.map(count),
            leviath_core::layout::BudgetSpec::Absolute(_) => None,
        }
    }

    /// The ceiling a percentage budget resolves no higher than, so a share of a
    /// very large window does not balloon.
    async fn budget_max_tokens(&self) -> Option<i32> {
        match &self.region().budget {
            leviath_core::layout::BudgetSpec::Percent { max, .. } => max.map(count),
            leviath_core::layout::BudgetSpec::Absolute(_) => None,
        }
    }

    /// One line on what this region is for.
    async fn description(&self) -> Option<&str> {
        self.region().description.as_deref()
    }

    /// Whether the run may not proceed while this region is empty.
    async fn required(&self) -> bool {
        self.region().required
    }

    /// Shown when a required region is empty. `{region}` is filled in.
    async fn required_message(&self) -> Option<&str> {
        self.region().required_message.as_deref()
    }

    /// Whether the description is also shown to the model, above the region's
    /// contents.
    async fn describe_in_prompt(&self) -> bool {
        self.region().describe_in_prompt
    }

    /// Whether an edge that compacts the context may hand this region to the
    /// summarizer.
    async fn summarizable(&self) -> bool {
        self.region().summarizable
    }

    /// How much the contents move between requests, which is what decides where
    /// the region sits in the assembled prompt and so what the prompt cache can
    /// keep.
    async fn volatility(&self) -> RegionVolatility {
        RegionVolatility::from(self.region().volatility)
    }

    /// What happens to a write that does not fit.
    async fn admission(&self) -> RegionAdmission {
        RegionAdmission::from(self.region().admission)
    }

    /// The fraction of the budget at which a compacting region compacts.
    async fn compact_at(&self) -> Option<f64> {
        self.region().compact_at
    }

    /// The mime patterns this region takes as parts. Empty means anything.
    async fn accepts(&self) -> &[String] {
        &self.region().accepts
    }

    /// What fills this region before the first inference. Null means it starts
    /// empty, for the agent to fill.
    async fn seed(&self) -> Option<RegionSeed> {
        self.region().seed.as_ref().map(RegionSeed::from)
    }

    /// The most entries a sliding region keeps.
    async fn max_items(&self) -> Option<i32> {
        match &self.region().kind {
            leviath_core::region::RegionKind::SlidingWindow { max_items, .. } => {
                Some(count(*max_items))
            }
            _ => None,
        }
    }

    /// How a sliding region makes room. Null for every other kind.
    async fn strategy(&self) -> Option<RegionStrategy> {
        self.eviction().map(|eviction| eviction.strategy)
    }

    /// How many entries over its ceiling a bulk eviction waits for.
    async fn overflow(&self) -> Option<i32> {
        self.eviction().and_then(|eviction| eviction.overflow)
    }

    /// How many of the oldest entries one compaction pass takes.
    async fn compact_count(&self) -> Option<i32> {
        self.eviction().and_then(|eviction| eviction.compact_count)
    }

    /// The token count at which a compacting region compacts.
    async fn threshold_tokens(&self) -> Option<i32> {
        match &self.region().kind {
            leviath_core::region::RegionKind::Compacting { threshold_tokens } => {
                Some(count(*threshold_tokens))
            }
            _ => None,
        }
    }

    /// The compacting region whose summaries land here, by name.
    async fn source_region(&self) -> Option<&str> {
        match &self.region().kind {
            leviath_core::region::RegionKind::CompactHistory { source_region } => {
                Some(source_region)
            }
            _ => None,
        }
    }

    /// The most keys a key-value region holds.
    async fn max_entries(&self) -> Option<i32> {
        match &self.region().kind {
            leviath_core::region::RegionKind::HashMap { max_entries } => max_entries.map(count),
            _ => None,
        }
    }

    /// The Rhai script that owns this region, for a custom one.
    async fn script(&self) -> Option<&str> {
        match &self.region().kind {
            leviath_core::region::RegionKind::Custom { script, .. } => Some(script),
            _ => None,
        }
    }

    /// Whether a custom region is never evicted, like a pinned one, rather than
    /// first out, like a temporary one. Null for every other kind.
    async fn pinned(&self) -> Option<bool> {
        match &self.region().kind {
            leviath_core::region::RegionKind::Custom { pinned, .. } => Some(*pinned),
            _ => None,
        }
    }
}

impl Region {
    /// The region this object stands for.
    fn region(&self) -> &leviath_core::layout::RegionDefinition {
        &self.blueprint.context_layout.regions[self.at]
    }

    /// How this region makes room, for the kinds that slide.
    fn eviction(&self) -> Option<RegionEviction> {
        match &self.region().kind {
            leviath_core::region::RegionKind::SlidingWindow {
                eviction_strategy, ..
            } => Some(RegionEviction::from(*eviction_strategy)),
            _ => None,
        }
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

    /// What must be on the machine before a run of this will start. A required
    /// one missing fails the spawn with its remedy; the rest are warnings.
    async fn dependencies(&self) -> Vec<BlueprintDependency> {
        self.parsed
            .dependencies
            .iter()
            .map(BlueprintDependency::from)
            .collect()
    }

    /// The mime rows this blueprint ships, so an agent that works in a file type
    /// the machine has never heard of carries the row that describes it.
    async fn mime_types(&self) -> Vec<BlueprintMimeRow> {
        BlueprintMimeRow::from_table(&self.parsed.mime_types)
    }

    /// What this blueprint asks of the taint layer. Null inherits the machine's
    /// setting.
    async fn security(&self) -> Option<BlueprintSecurity> {
        self.parsed.security.as_ref().map(BlueprintSecurity::from)
    }

    /// Where this agent's tools run, unless a stage says otherwise. Null leaves
    /// the machine's own setting.
    async fn sandbox(&self) -> Option<SandboxConfig> {
        self.parsed.sandbox.as_ref().map(SandboxConfig::from)
    }

    /// What happens when a model answers with text before calling any tool,
    /// unless a stage says otherwise. Null leaves the machine's own setting.
    async fn nudge(&self) -> Option<NudgeConfig> {
        self.parsed.nudge.as_ref().map(NudgeConfig::from)
    }

    /// The model that summarizes a region when it fills. Null leaves the
    /// machine's own summarizer.
    async fn compaction(&self) -> Option<CompactionConfig> {
        self.parsed
            .compaction_config
            .as_ref()
            .map(CompactionConfig::from)
    }

    /// Keeping the files this agent reads and writes in one region, so a tool
    /// result can point at the region rather than repeating the file.
    async fn file_tracking(&self) -> Option<FileTrackingConfig> {
        self.parsed
            .file_tracking
            .as_ref()
            .map(FileTrackingConfig::from)
    }

    /// When a run of this is stopped for going round in circles. Null leaves the
    /// machine's own thresholds.
    async fn repetition_detection(&self) -> Option<RepetitionDetection> {
        self.parsed
            .repetition_detection
            .as_ref()
            .map(RepetitionDetection::from)
    }

    /// What this agent would like to run without being asked. A request rather
    /// than a grant: the machine's own policy decides, and this is what an
    /// operator reads when deciding whether to write it in.
    async fn safe_commands(&self) -> Option<SafeCommands> {
        self.parsed.safe_commands.as_ref().map(SafeCommands::from)
    }

    /// The shape this agent's answer takes, unless a stage narrows it.
    async fn output(&self) -> Option<OutputSpec> {
        self.parsed.output.as_ref().map(OutputSpec::from)
    }

    /// How this agent's context maps onto another's, for a handoff to a
    /// different blueprint.
    async fn transforms(&self) -> Vec<ContextTransform> {
        self.parsed
            .transforms
            .iter()
            .map(ContextTransform::from)
            .collect()
    }
}

#[cfg(test)]
#[path = "blueprint_tests.rs"]
mod tests;
