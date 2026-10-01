//! The context layout: the regions a run's context is made of, how big each
//! may grow, and what fills them at spawn.

use leviath_core::region::{Admission, Volatility};
use serde::{Deserialize, Serialize};

use super::stage::{CodeRef, SeedToolCall};
use crate::spec::names::{MimePattern, RegionName, WorkdirPath};

/// A context layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionLayoutDef {
    /// The regions, in the order the model reads them.
    pub regions: Vec<RegionDef>,
    /// The token budget the whole layout shares.
    pub total_budget_tokens: u32,
    /// Which regions give up entries first when the context is full.
    #[serde(default)]
    pub eviction_order: Vec<RegionName>,
}

/// One region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionDef {
    /// The region's name, unique in its layout.
    pub name: RegionName,
    /// How it keeps and drops entries.
    pub kind: RegionKind,
    /// How big it may grow.
    pub budget: Budget,
    /// The fraction of its budget at which it is summarized, when it is one
    /// that can be.
    #[serde(default)]
    pub compact_at: Option<f64>,
    /// What it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the description is shown to the model.
    #[serde(default)]
    pub describe_in_prompt: bool,
    /// Whether a spawn that leaves it empty is refused.
    #[serde(default)]
    pub required: bool,
    /// What that refusal says.
    #[serde(default)]
    pub required_message: Option<String>,
    /// Whether it may be summarized when the context is compacted.
    #[serde(default = "leviath_core::default_true")]
    pub summarizable: bool,
    /// What happens to an entry that does not fit.
    #[serde(default)]
    pub admission: Admission,
    /// How often its contents change, for prompt caching.
    #[serde(default)]
    pub volatility: Volatility,
    /// What fills it at spawn, besides inputs bound to it.
    #[serde(default)]
    pub seed: Option<Seed>,
    /// Mime types a part placed in it may carry. Empty accepts any.
    #[serde(default)]
    pub accepts: Vec<MimePattern>,
}

/// How a region keeps and drops entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum RegionKind {
    /// Kept for the whole run.
    Pinned,
    /// The latest entries, up to a count.
    SlidingWindow {
        /// The most entries kept.
        max_items: u32,
        /// How old entries leave.
        #[serde(default)]
        eviction: Eviction,
    },
    /// Emptied after each turn.
    Temporary,
    /// Summarized once it passes a size.
    Compacting {
        /// The size, in tokens. `None` takes the region's `compact_at`
        /// fraction of its budget, or 80% of it.
        #[serde(default)]
        threshold_tokens: Option<u32>,
    },
    /// Kept until a stage or edge clears it.
    Clearable,
    /// Summaries of another region's evicted entries.
    CompactHistory {
        /// The region whose entries it summarizes.
        source: RegionName,
    },
    /// Entries replaced by key.
    Keyed {
        /// The most keys kept.
        #[serde(default)]
        max_entries: Option<u32>,
    },
    /// A checklist the model ticks off.
    Checklist,
    /// Rendered and maintained by code.
    Custom {
        /// The code.
        code: CodeRef,
        /// Whether its entries survive the context being cleared.
        #[serde(default)]
        pinned: bool,
    },
}

/// How a sliding window drops old entries.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Eviction {
    /// One turn at a time.
    #[default]
    PerItem,
    /// In bulk, once the window is this many entries over its cap, back down
    /// to the cap. The prefix stays stable in between, for prompt caching.
    Bulk(u32),
    /// Summarize this many of the oldest entries once the cap is reached.
    Compact(u32),
}

/// How big a region may grow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum Budget {
    /// A fixed number of tokens.
    Tokens(u32),
    /// A fraction (above 0, at most 1) of the model's context window,
    /// optionally clamped.
    Percent {
        /// The fraction.
        percent: f64,
        /// The fewest tokens.
        #[serde(default)]
        min: Option<u32>,
        /// The most tokens.
        #[serde(default)]
        max: Option<u32>,
    },
}

/// What fills a region at spawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Seed {
    /// Workdir files matching a glob, concatenated.
    Glob(String),
    /// Workdir files, concatenated.
    Files(Vec<WorkdirPath>),
    /// Fixed text.
    Literal(String),
    /// The text some code returns.
    Code(CodeRef),
    /// The output of a shell command run in the workdir.
    Command(String),
    /// The results of tool calls.
    Tools {
        /// The calls.
        calls: Vec<SeedToolCall>,
        /// When they run again.
        #[serde(default)]
        refresh: SeedRefresh,
    },
}

/// When a tool seed runs again.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SeedRefresh {
    /// Only at spawn.
    #[default]
    Once,
    /// At the start of every stage.
    EachStage,
}
