//! `Blueprint`: the manifest, typed.
//!
//! One type for one concept. The blueprint a run executed and the blueprint
//! installed under that name are the same kind of thing, read from different
//! files, so they are the same type here. What tells them apart is the id,
//! which carries the digest: two revisions of one name are two ids, so a
//! caching client cannot merge a run's frozen copy with whatever is installed
//! now.

use std::sync::Arc;

use async_graphql::{Enum, ID, Object, SimpleObject};
use leviath_graphql_derive::mirror;

use super::super::super::core::blueprints::BlueprintSource as CoreSource;
use super::super::paging::page::weight;
use super::machine::mime::MimeRow;
use super::manifest::count;
use super::manifest::dependency::BlueprintDependency;
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

/// The id one blueprint revision answers to: `<name>@<digest prefix>`.
///
/// One place mints it, so the id a listing hands out and the id `node` looks
/// up cannot drift apart on how much of the digest they carry.
pub(crate) fn revision_id(name: &str, digest: &str) -> ID {
    let short: String = digest.chars().take(ID_DIGEST_CHARS).collect();
    ID(format!("{name}@{short}"))
}

/// Where a blueprint was read from.
#[mirror]
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

/// When a run looks for tools again after it started.
///
/// Discovery happens either way. What this decides is whether it happens more
/// than once - not whether the run may install a tool, which is what the tool
/// permissions decide.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ToolRescan {
    /// The set is fixed when the run starts. A tool installed mid-run reaches
    /// the next run, not this one.
    AtSpawnOnly,
    /// The run also scans its workdir's `tools/`, and looks again before its
    /// next turn once a script is written there.
    RescanAfterWrites,
    /// As `RESCAN_AFTER_WRITES`, and the run looks at the scanned directories
    /// before each batch of tool calls.
    ///
    /// `RESCAN_AFTER_WRITES` is told about a tool only when the run writes one
    /// with `write_file`, `edit_file` or `install_tool`. This notices one that
    /// arrived any other way: from a shell command, from a script tool, or from
    /// another run sharing the workdir.
    RescanBeforeDispatch,
}

impl From<leviath_core::blueprint::ToolRescan> for ToolRescan {
    fn from(setting: leviath_core::blueprint::ToolRescan) -> Self {
        use leviath_core::blueprint::ToolRescan as Core;
        match setting {
            Core::AtSpawn => Self::AtSpawnOnly,
            Core::AfterWrites => Self::RescanAfterWrites,
            Core::BeforeDispatch => Self::RescanBeforeDispatch,
        }
    }
}

/// Whether a prompt hint is included at one manifest level.
///
/// Omitting guidance never discourages the behaviour. It leaves the paragraph
/// out of the system prompt, and nothing else.
#[mirror]
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
#[mirror]
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
#[mirror]
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

impl RegionKind {
    /// The kind a run's snapshot recorded, read from its stored spelling.
    ///
    /// `None` for a word this build does not know, which is a snapshot from a
    /// newer one: a null beside a region that is plainly there beats refusing
    /// the whole window. Two spellings are older builds' own
    /// (`sliding`, `history`), and those files are still on disk.
    pub(crate) fn from_snapshot(kind: &str) -> Option<Self> {
        Some(match kind {
            "pinned" => Self::Pinned,
            "temporary" => Self::Temporary,
            "clearable" => Self::Clearable,
            "sliding_window" | "sliding" => Self::SlidingWindow,
            "compacting" => Self::Compacting,
            "compact_history" | "history" => Self::CompactHistory,
            "hashmap" => Self::Hashmap,
            "checklist" => Self::Checklist,
            "custom" => Self::Custom,
            _ => return None,
        })
    }
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

/// The resolver state behind the `Region` type.
pub(crate) struct Region {
    /// The blueprint this region belongs to, shared rather than copied.
    pub(crate) blueprint: Arc<CoreBlueprint>,
    /// The stage whose own `[context.regions]` declares it, by declaration
    /// order. `None` for the blueprint's own layout.
    pub(crate) stage: Option<usize>,
    /// Which region, by position in the layout that declares it.
    pub(crate) at: usize,
}

/// One named slice of context a blueprint declares: what it is for, how large
/// it may grow, and what it gives up when it fills.
///
/// A blueprint's context window is cut into regions so that a stage can carry
/// one part of what it knows and compact or empty another. This is the
/// declaration only. What a live run's region holds is `ContextRegion`, and
/// the two are read separately on purpose.
#[mirror(list)]
#[Object]
impl Region {
    /// Region name, unique within the layout that declares it.
    async fn name(&self) -> &str {
        &self.region().name
    }

    /// The stage whose own `[context.regions]` declares this region. Null for a
    /// region the blueprint declares run-wide, which is every region in
    /// `Blueprint.regions`.
    ///
    /// A stage may declare a layout of its own, and a name resolved from
    /// anywhere in the manifest can land in one of those. This says where the
    /// declaration was read from, so a client can tell a run-wide region from
    /// one only a single stage sets up.
    async fn declared_by_stage(&self) -> Option<Stage> {
        self.stage.map(|at| Stage {
            blueprint: Arc::clone(&self.blueprint),
            at,
        })
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
    /// empty, for the run to fill.
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

    /// The compacting region whose summaries land here.
    ///
    /// Null for every kind but `COMPACT_HISTORY`, and also where the name this
    /// region was given is one no layout in the blueprint declares, which is a
    /// history region nothing will ever write to. Read `sourceRegionName` to
    /// tell those two apart.
    async fn source_region(&self) -> Option<Region> {
        super::manifest::refs::region(&self.blueprint, self.declared_source_region()?)
    }

    /// The name this region's `source_region` was given, verbatim.
    ///
    /// Null for every kind but `COMPACT_HISTORY`. Set beside a null
    /// `sourceRegion` when the name matches no layout in the blueprint.
    async fn source_region_name(&self) -> Option<&str> {
        self.declared_source_region()
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
        &self.layout().regions[self.at]
    }

    /// The layout that declares it: one stage's own, or the blueprint's.
    fn layout(&self) -> &leviath_core::layout::ContextLayout {
        self.stage
            .and_then(|at| self.blueprint.stages[at].context_layout.as_ref())
            .unwrap_or(&self.blueprint.context_layout)
    }

    /// The `source_region` name a compacting-history region was given.
    fn declared_source_region(&self) -> Option<&str> {
        match &self.region().kind {
            leviath_core::region::RegionKind::CompactHistory { source_region } => {
                Some(source_region)
            }
            _ => None,
        }
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

impl super::super::connection::Paged for Blueprint {
    const NAME: &'static str = "Blueprint";
}

/// The resolver state behind the `Blueprint` type.
pub(crate) struct Blueprint {
    /// The parsed manifest, shared with the parse cache.
    pub(crate) parsed: Arc<CoreBlueprint>,
    /// Lowercase hex SHA-256 of the manifest text.
    pub(crate) digest: String,
    /// Which file it was read from.
    pub(crate) source: BlueprintSource,
}

/// A blueprint: the manifest that says what an agent is, whole.
///
/// Its stages, regions, tools and models are the declaration and nothing more.
/// Nothing here belongs to any one execution: that is a `Run`, which carries
/// its own frozen copy of the blueprint it started from.
///
/// So read from a run, this is the manifest that run executed; read from the
/// blueprint listing, it is the definition installed now. The `source` field
/// says which, and the digest in the id says whether they are the same bytes.
#[mirror]
#[Object]
impl Blueprint {
    /// This revision's id: `<name>@<digest prefix>`.
    ///
    /// The digest is part of the identity on purpose. A run's frozen copy and
    /// the installed blueprint share a name and may differ in every other way,
    /// and a client that caches by type and id would otherwise merge the two.
    ///
    /// `node` answers with the installed revision. A revision only a run
    /// carries is read through that run, so its id answers with nothing there.
    pub(crate) async fn id(&self) -> ID {
        revision_id(&self.parsed.name, &self.digest)
    }

    /// Unique within the installed set.
    #[filter(orderable)]
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
    #[filter(orderable)]
    async fn version(&self) -> &str {
        &self.parsed.version
    }

    /// From `[agent] description`.
    async fn description(&self) -> &str {
        &self.parsed.description
    }

    /// The stage a run starts in. Defaults to the first stage declared.
    ///
    /// Null for a blueprint that declares no stages, and for one whose
    /// `entry_stage` names a stage it does not declare, which `lev validate`
    /// refuses and the daemon will not spawn. `entryStageName` is the name it
    /// wrote.
    async fn entry_stage(&self) -> Option<Stage> {
        match self.parsed.entry_stage.as_deref() {
            Some(name) => super::manifest::refs::stage(&self.parsed, name),
            None => self.parsed.stages.first().map(|_| Stage {
                blueprint: Arc::clone(&self.parsed),
                at: 0,
            }),
        }
    }

    /// The name the manifest gave as its entry stage, verbatim. Null where it
    /// names none, which starts the run in the first stage declared.
    async fn entry_stage_name(&self) -> Option<&str> {
        self.parsed.entry_stage.as_deref()
    }

    /// How deep sub-agent spawning may nest.
    async fn max_child_depth(&self) -> Option<i32> {
        self.parsed
            .max_child_depth
            .map(|n| i32::try_from(n).unwrap_or(i32::MAX))
    }

    /// When this blueprint's runs look for tools again.
    async fn tool_rescan(&self) -> ToolRescan {
        self.parsed.tool_rescan.into()
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

    /// One entry per context region the blueprint declares run-wide.
    ///
    /// A stage may declare a layout of its own on top of this, and those regions
    /// are on `Stage.context.regions` rather than here.
    async fn regions(&self) -> Vec<Region> {
        (0..self.parsed.context_layout.regions.len())
            .map(|at| Region {
                blueprint: Arc::clone(&self.parsed),
                stage: None,
                at,
            })
            .collect()
    }

    /// Paths this blueprint declares it needs beyond its workdir. Declaring is not
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

    /// The mime rows this blueprint ships, so one that works in a file type the
    /// machine has never heard of carries the row that describes it.
    async fn mime_types(&self) -> Vec<MimeRow> {
        MimeRow::from_table(&self.parsed.mime_types, &self.parsed.name)
    }

    /// What this blueprint asks of the taint layer. Null inherits the machine's
    /// setting.
    async fn security(&self) -> Option<BlueprintSecurity> {
        self.parsed.security.as_ref().map(BlueprintSecurity::from)
    }

    /// Where this blueprint's tools run, unless a stage says otherwise. Null leaves
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

    /// Keeping the files this blueprint's runs read and write in one region, so a tool
    /// result can point at the region rather than repeating the file.
    async fn file_tracking(&self) -> Option<FileTrackingConfig> {
        self.parsed
            .file_tracking
            .as_ref()
            .map(|tracking| FileTrackingConfig::of(&self.parsed, tracking))
    }

    /// When a run of this is stopped for going round in circles. Null leaves the
    /// machine's own thresholds.
    async fn repetition_detection(&self) -> Option<RepetitionDetection> {
        self.parsed
            .repetition_detection
            .as_ref()
            .map(RepetitionDetection::from)
    }

    /// What this blueprint would like to run without being asked. A request rather
    /// than a grant: the machine's own policy decides, and this is what an
    /// operator reads when deciding whether to write it in.
    async fn safe_commands(&self) -> Option<SafeCommands> {
        self.parsed.safe_commands.as_ref().map(SafeCommands::from)
    }

    /// The shape this blueprint's answer takes, unless a stage narrows it.
    async fn output(&self) -> Option<OutputSpec> {
        self.parsed.output.as_ref().map(OutputSpec::from)
    }

    /// How this blueprint's context maps onto another's, for a handoff to a
    /// different blueprint.
    async fn transforms(&self) -> Vec<ContextTransform> {
        self.parsed
            .transforms
            .iter()
            .map(ContextTransform::from)
            .collect()
    }

    /// The tools a run of this blueprint can call.
    ///
    /// Its own `tools/` directory as well as the machine's, which is what an
    /// editor offering an `available_tools` list wants. The scope decides which
    /// directories are walked, so it is a field here rather than an argument on
    /// the root listing.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn tools(
        &self,
        ctx: &async_graphql::Context<'_>,
        #[graphql(desc = "Which tools to list. Omitted means all of them.")] filter: Option<
            super::catalog::ToolFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means name ascending.")]
        order_by: Option<Vec<super::catalog::ToolOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<
            super::super::scalars::Cursor,
        >,
    ) -> async_graphql::Result<
        super::super::connection::Connection<super::catalog::Tool, super::catalog::ToolSkips>,
    > {
        super::super::query::catalog::tool_page(
            ctx,
            Some(&self.parsed.name),
            filter,
            order_by,
            first,
            after,
        )
        .await
    }

    /// The scripts a run of this blueprint can see.
    ///
    /// Its own directory's scripts as well as the machine's. Read `scope` on
    /// each to tell the two apart.
    #[filter(skip)]
    #[graphql(complexity = "weight(first, child_complexity)")]
    async fn scripts(
        &self,
        ctx: &async_graphql::Context<'_>,
        #[graphql(desc = "Which scripts to list. Omitted means all of them.")] filter: Option<
            super::machine::ScriptFilter,
        >,
        #[graphql(desc = "Sort keys, in priority order. Omitted means id ascending.")]
        order_by: Option<Vec<super::machine::ScriptOrder>>,
        #[graphql(
            desc = "Page size; capped by the server's page-size cap.",
            default = 50
        )]
        first: i32,
        #[graphql(desc = "Cursor from the previous page.")] after: Option<
            super::super::scalars::Cursor,
        >,
    ) -> async_graphql::Result<super::super::connection::Connection<super::machine::Script>> {
        super::super::query::machine::script_page(
            ctx,
            Some(&self.parsed.name),
            filter,
            order_by,
            first,
            after,
        )
        .await
    }
}

#[cfg(test)]
#[path = "blueprint_tests.rs"]
mod tests;
