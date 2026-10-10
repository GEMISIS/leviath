//! The context layout of an old manifest as its parser reads it: each region,
//! its kind, its budget (a token count or a share of the model's window), and
//! what fills it at spawn.

use leviath_core::region::{EvictionStrategy, RegionKind};
use serde::{Deserialize, Serialize};

/// How a region's token ceiling is expressed before it is resolved against a
/// concrete model context window.
///
/// Blueprint authors think in **proportions** (`budget = "35%"`) so their intent
/// stays correct regardless of the model's context size, while power users can
/// still pin an exact count. The percentage denominator - the model's context
/// window - is not known at parse time, so the spec is stored unresolved here and
/// carried into the run graph as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSpec {
    /// A fixed token ceiling, independent of the model. Resolving is a no-op.
    Absolute(usize),

    /// A ceiling expressed as a fraction of the model's context window, with
    /// optional absolute guard-rails. `percent` is a fraction (`0.35` for
    /// `"35%"`). `max` caps the resolved value (so e.g. 2% of a 1M window can't
    /// balloon a task region to 20K tokens); `min` floors it (so a small-context
    /// model doesn't starve the region below a usable size).
    Percent {
        /// Fraction of the model context window (0.35 == "35%").
        percent: f64,
        /// Absolute floor for the resolved value, if any.
        min: Option<usize>,
        /// Absolute cap for the resolved value, if any.
        max: Option<usize>,
    },
}

impl Default for BudgetSpec {
    /// Only a serde-deserialize fallback for older persisted blueprints; live
    /// code always sets the budget explicitly via [`RegionDefinition::new`].
    fn default() -> Self {
        BudgetSpec::Absolute(0)
    }
}

impl BudgetSpec {
    /// Parse a percentage string like `"35%"` into its fraction (`0.35`).
    ///
    /// Surrounding whitespace is trimmed and decimals are allowed (`"0.6%"`).
    /// Rejects a missing `%`, a non-numeric value, and anything outside the
    /// `(0, 100]` range - a single region can't sensibly claim ≤0% or more than
    /// the whole window (region budgets may *sum* past 100%, but each is a
    /// fraction of one window). Returns the human-readable reason on failure so
    /// the caller can surface it at load time.
    pub fn parse_budget(s: &str) -> std::result::Result<f64, String> {
        let trimmed = s.trim();
        let Some(num) = trimmed.strip_suffix('%') else {
            return Err(format!("budget '{s}' must end with '%' (e.g. \"35%\")"));
        };
        let value: f64 = num
            .trim()
            .parse()
            .map_err(|_| format!("budget '{s}' is not a valid number"))?;
        if !(value > 0.0 && value <= 100.0) {
            return Err(format!(
                "budget '{s}' must be greater than 0% and at most 100%"
            ));
        }
        Ok(value / 100.0)
    }
}

/// A ContextLayout defines the complete memory map for an agent.
///
/// Like SNES VRAM layout - every region has a defined purpose, size, and policy.
/// The layout specifies:
/// - Which regions exist and their configurations
/// - Total token budget across all regions
/// - Eviction order when space is needed
///
/// Layouts are typically defined in an agent's blueprint and remain constant
/// throughout the agent's lifecycle, though the content within regions changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLayout {
    /// All regions in this layout
    pub regions: Vec<RegionDefinition>,

    /// Total token budget across all regions
    pub total_budget_tokens: usize,

    /// Region names in eviction priority order (first = evicted first)
    ///
    /// When the context window fills up, regions are processed in this order:
    /// 1. Temporary regions: evict oldest entries
    /// 2. Compacting regions: trigger summarization
    /// 3. SlidingWindow regions: reduce window size
    /// 4. Pinned regions: NEVER touched (if these fill up, it's a config error)
    pub eviction_order: Vec<String>,
}

impl ContextLayout {
    /// Create a new layout with the specified configuration.
    pub fn new(regions: Vec<RegionDefinition>, total_budget_tokens: usize) -> Self {
        Self {
            regions,
            total_budget_tokens,
            eviction_order: Vec::new(),
        }
    }
}

/// Where a region's initial content comes from at run start.
///
/// A region without a seed starts empty and is populated by the agent. A seeded
/// region is filled before the first inference: `CallerInput` regions are filled
/// by the run's caller (a CLI `--<name>` flag, an ACP `---region:<name>---`
/// marker, or the API `regions` map); the remaining variants are resolved by the
/// daemon from the run's workdir (which is why this type only *declares* the
/// source - `leviath-core` stays filesystem-agnostic; resolution lives in the
/// CLI daemon's spawner).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegionSeed {
    /// Filled at run time by the caller, keyed by `name` (defaults to the
    /// region's own name; the sentinel `task` maps to the `--task`/prompt text).
    /// When the owning region is `required`, a missing value is a hard error
    /// before any inference runs.
    CallerInput {
        /// The caller-input key this region is filled from.
        name: String,
    },
    /// Concatenated contents of the workdir files matching a glob pattern.
    Glob {
        /// Glob pattern, resolved relative to the run's workdir.
        pattern: String,
    },
    /// Concatenated contents of an explicit list of workdir-relative files.
    Files {
        /// File paths, resolved relative to the run's workdir.
        paths: Vec<String>,
    },
    /// A static literal string baked into the blueprint.
    Literal {
        /// The verbatim seed text.
        text: String,
    },
    /// The `String` returned by running a Rhai script from the workdir.
    Rhai {
        /// Script path, resolved relative to the run's workdir.
        script: String,
    },
    /// The combined stdout/stderr of a shell command run in the workdir at spawn.
    ///
    /// Unlike every other variant this *executes* something, and it does so
    /// before the first inference - so before any tool-approval prompt. The
    /// daemon runs it inside the entry stage's sandbox when one is configured,
    /// caps its runtime and output, and honours the `[security]
    /// allow_seed_commands` kill switch. A failure is non-fatal unless the
    /// owning region is `required`.
    Command {
        /// The shell command line, run with the platform shell in the workdir.
        command: String,
    },
    /// The combined output of one or more tool calls, run at spawn.
    ///
    /// Like [`Command`](Self::Command) this *executes* something, but through
    /// the run's own tool layer rather than a shell: any tool the agent could
    /// call is callable here - a built-in, an MCP server's, a Rhai script's -
    /// and each call answers to the same `tool_permissions` and taint rules it
    /// would answer to mid-run. That is what makes an unrestricted list safe:
    /// a seed can reach nothing the agent was not already granted.
    ///
    /// Several calls write into one region, in the order given, each under its
    /// own heading. A failed call is skipped with a warning unless the region is
    /// `required`, so one unavailable tool does not cost the others.
    Tools {
        /// The calls to run, in order.
        calls: Vec<SeedToolCall>,
        /// Whether the calls run once, or again on every stage entry.
        refresh: SeedRefresh,
    },
}

/// When a [`RegionSeed::Tools`] seed runs again.
///
/// Every other seed kind resolves once, at spawn, and this defaults to the
/// same: a region seeded from the filesystem or a literal has no reason to be
/// re-read, and re-running a call on every stage entry costs a tool call and
/// rewrites a region the cache was holding still.
///
/// [`EachStage`](Self::EachStage) is for the seeds where the answer moves.
/// A clock is the clear case: a run that spends an hour in one stage and then
/// enters another should date the second stage from when it started, not from
/// when the run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedRefresh {
    /// Resolve once, at spawn. The default, and what every other seed does.
    #[default]
    Once,
    /// Resolve again whenever a stage is entered, replacing the region.
    EachStage,
}

impl SeedRefresh {
    /// Parse the manifest spelling, or `None` for a word that is neither.
    ///
    /// A wrong spelling is rejected rather than defaulted, so
    /// `refresh = "each stage"` is reported instead of quietly meaning `once` -
    /// which would read as the feature not working.
    pub fn from_str_loose(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "once" | "spawn" => Some(Self::Once),
            "each_stage" | "stage" => Some(Self::EachStage),
            _ => None,
        }
    }
}

/// One tool call in a [`RegionSeed::Tools`] seed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeedToolCall {
    /// The tool to call, spelled as the agent would spell it (so an MCP tool
    /// keeps its `<server>__<tool>` qualification).
    pub name: String,
    /// The arguments object. Empty for the many tools that take none.
    pub args: serde_json::Value,
}

impl SeedToolCall {
    /// A call with no arguments.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// A call with an arguments object.
    pub fn with_args(name: impl Into<String>, args: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            args,
        }
    }
}

#[cfg(test)]
mod seed_refresh_tests {
    use super::*;

    #[test]
    fn both_spellings_and_their_aliases_parse() {
        assert_eq!(SeedRefresh::from_str_loose("once"), Some(SeedRefresh::Once));
        assert_eq!(
            SeedRefresh::from_str_loose("spawn"),
            Some(SeedRefresh::Once)
        );
        assert_eq!(
            SeedRefresh::from_str_loose("each_stage"),
            Some(SeedRefresh::EachStage)
        );
        assert_eq!(
            SeedRefresh::from_str_loose("stage"),
            Some(SeedRefresh::EachStage)
        );
        // Case and surrounding space are not the author's problem.
        assert_eq!(
            SeedRefresh::from_str_loose("  EACH_STAGE "),
            Some(SeedRefresh::EachStage)
        );
    }

    /// A word that is neither is rejected rather than defaulted. Defaulting
    /// would make `refresh = "each stage"` silently mean `once`, which reads as
    /// the feature not working rather than as a typo.
    #[test]
    fn an_unrecognised_word_is_not_quietly_once() {
        assert_eq!(SeedRefresh::from_str_loose("each stage"), None);
        assert_eq!(SeedRefresh::from_str_loose("always"), None);
        assert_eq!(SeedRefresh::from_str_loose(""), None);
    }

    /// The default matches every other seed kind: resolve at spawn, once.
    #[test]
    fn the_default_is_once() {
        assert_eq!(SeedRefresh::default(), SeedRefresh::Once);
    }
}

/// How an old manifest's region kind was written: the shape of
/// [`RegionKind`] as [`RegionDefinition`] carries it, kept here because
/// nothing but an old manifest reads or writes it.
#[derive(Serialize, Deserialize)]
#[serde(remote = "RegionKind")]
enum OldRegionKind {
    Pinned,
    SlidingWindow {
        max_items: usize,
        eviction_strategy: EvictionStrategy,
    },
    Temporary,
    Compacting {
        threshold_tokens: usize,
    },
    Clearable,
    CompactHistory {
        source_region: String,
    },
    HashMap {
        max_entries: Option<usize>,
    },
    Checklist,
    Custom {
        script: String,
        /// Written `persistent` by the oldest manifests; both spellings parse.
        #[serde(alias = "persistent")]
        pinned: bool,
    },
}

/// Definition of a region in a layout.
///
/// This is the blueprint for creating a Region instance. It specifies the
/// region's configuration but doesn't contain actual content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionDefinition {
    /// Unique name for this region
    pub name: String,

    /// Region lifecycle policy
    #[serde(with = "OldRegionKind")]
    pub kind: RegionKind,

    /// The region's token ceiling as written; for a percentage budget, its cap
    /// or 0. [`Self::budget`] is the source of truth.
    pub max_tokens: usize,

    /// How this region's ceiling is expressed. Defaults (via [`Self::new`]) to
    /// [`BudgetSpec::Absolute`] holding `max_tokens`, so a region that names a
    /// token count directly gets an absolute ceiling. A percentage budget is
    /// resolved against the model context window at window-build time.
    #[serde(default)]
    pub budget: BudgetSpec,

    /// For [`RegionKind::Compacting`] regions only: compact when the region
    /// reaches this fraction of its resolved budget (`0.80` for `compact_at =
    /// "80%"`). `None` keeps the absolute `threshold_tokens` carried on the kind.
    #[serde(default)]
    pub compact_at: Option<f64>,

    /// Human-readable description of this region's purpose
    pub description: Option<String>,

    /// Whether `description` is also shown to the model, under the region's
    /// name. Off by default - see [`leviath_core::region::Region::describe_in_prompt`].
    #[serde(default)]
    pub describe_in_prompt: bool,

    /// When true, this region must be non-empty before a stage that can write
    /// to it is allowed to complete. Guards against an agent skipping a
    /// context-population step (e.g. never writing the `plan` region). Enforced
    /// in the run loop, which re-runs the stage with [`Self::required_message`]
    /// until the region is populated.
    #[serde(default)]
    pub required: bool,

    /// Whether an edge transform may hand this region to the summarizer.
    ///
    /// `transform = "compact"` reads as "summarize the transcript on the way
    /// out" and means "summarize every region that is not pinned", which
    /// includes the ones holding the run's results. Figures that survive a
    /// paraphrase are no longer figures: without this, a `results` region
    /// carrying computed values is rewritten into prose before the stage that
    /// reports them ever sees it.
    ///
    /// Setting this false protects the region wherever it is used, rather than
    /// at each of the N edges that might touch it. `clear` still applies - this
    /// says "do not paraphrase my content", not "keep it forever".
    #[serde(default = "leviath_core::default_true")]
    pub summarizable: bool,

    /// What this region does when a write does not fit.
    ///
    /// Declared per region rather than per stage: whether losing the oldest
    /// entry is acceptable is a property of what the region holds, and does not
    /// change depending on which stage is writing to it.
    #[serde(default)]
    pub admission: leviath_core::region::Admission,
    /// How much this region's contents move between requests. See
    /// [`leviath_core::region::Volatility`].
    ///
    /// Defaulted on the wire so a definition written before this existed still
    /// loads, and loads as the pessimistic value - which is what an unclassified
    /// region should be.
    #[serde(default)]
    pub volatility: leviath_core::region::Volatility,

    /// Optional custom message shown to the agent when this region is required
    /// but empty. Falls back to a generated default when `None`.
    #[serde(default)]
    pub required_message: Option<String>,

    /// Where this region's initial content comes from at run start. `None`
    /// means the region starts empty (the agent populates it). See
    /// [`RegionSeed`].
    #[serde(default)]
    pub seed: Option<RegionSeed>,

    /// Mime type patterns this region takes; empty means anything. See
    /// [`leviath_core::region::Region::accepts`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepts: Vec<String>,
}

impl RegionDefinition {
    /// Create a new region definition with an absolute token ceiling.
    ///
    /// The `budget` is set to [`BudgetSpec::Absolute`] holding `max_tokens` and
    /// `compact_at` to `None`, so every existing caller (and every region without
    /// a percentage budget) is unaffected - resolving such a layout is a no-op.
    pub fn new(name: String, kind: RegionKind, max_tokens: usize) -> Self {
        Self {
            name,
            kind,
            max_tokens,
            budget: BudgetSpec::Absolute(max_tokens),
            compact_at: None,
            description: None,
            describe_in_prompt: false,
            required: false,
            required_message: None,
            summarizable: true,
            admission: leviath_core::region::Admission::default(),
            volatility: leviath_core::region::Volatility::default(),
            seed: None,
            accepts: Vec::new(),
        }
    }

    /// Set this region's budget spec (e.g. a percentage of the model window).
    /// `max_tokens` is left as the provisional/resolved value; it is (re)computed
    /// from the budget when the owning layout is resolved.
    pub fn with_budget(mut self, budget: BudgetSpec) -> Self {
        self.budget = budget;
        self
    }

    /// Set the compaction trigger fraction for a [`RegionKind::Compacting`]
    /// region (`0.80` == compact at 80% of the resolved budget).
    pub fn with_compact_at(mut self, fraction: f64) -> Self {
        self.compact_at = Some(fraction);
        self
    }

    /// Set this region's seed source.
    pub fn with_seed(mut self, seed: RegionSeed) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Mark this region as required, with an optional custom nudge message.
    pub fn with_required(mut self, required: bool, message: Option<String>) -> Self {
        self.required = required;
        self.required_message = message;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_creation() {
        let regions = vec![
            RegionDefinition::new("pinned".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("temp".to_string(), RegionKind::Temporary, 10000),
        ];
        let layout = ContextLayout::new(regions, 20000);
        assert_eq!(layout.regions.len(), 2);
        assert_eq!(layout.total_budget_tokens, 20000);
    }

    #[test]
    fn parse_budget_accepts_plain_and_decimal_percentages() {
        assert_eq!(BudgetSpec::parse_budget("35%").unwrap(), 0.35);
        assert_eq!(BudgetSpec::parse_budget("100%").unwrap(), 1.0);
        assert!((BudgetSpec::parse_budget("0.6%").unwrap() - 0.006).abs() < 1e-9);
    }

    #[test]
    fn parse_budget_trims_surrounding_and_inner_whitespace() {
        assert_eq!(BudgetSpec::parse_budget("  35 %  ").unwrap(), 0.35);
    }

    #[test]
    fn parse_budget_rejects_missing_percent_sign() {
        let err = BudgetSpec::parse_budget("35").unwrap_err();
        assert!(err.contains("must end with '%'"), "{err}");
    }

    #[test]
    fn parse_budget_rejects_non_numeric() {
        let err = BudgetSpec::parse_budget("abc%").unwrap_err();
        assert!(err.contains("not a valid number"), "{err}");
    }

    #[test]
    fn parse_budget_rejects_zero_and_negative() {
        let zero = BudgetSpec::parse_budget("0%").unwrap_err();
        assert!(zero.contains("greater than 0%"), "{zero}");
        let neg = BudgetSpec::parse_budget("-10%").unwrap_err();
        assert!(neg.contains("greater than 0%"), "{neg}");
    }

    #[test]
    fn parse_budget_rejects_over_one_hundred() {
        let err = BudgetSpec::parse_budget("150%").unwrap_err();
        assert!(err.contains("at most 100%"), "{err}");
    }

    #[test]
    fn region_definition_default_budget_matches_max_tokens() {
        let def = RegionDefinition::new("a".to_string(), RegionKind::Pinned, 5000);
        assert_eq!(def.budget, BudgetSpec::Absolute(5000));
        assert_eq!(def.compact_at, None);
    }

    #[test]
    fn budget_spec_default_is_absolute_zero() {
        assert_eq!(BudgetSpec::default(), BudgetSpec::Absolute(0));
    }

    /// Every kind round-trips in the old shape, a custom region's `pinned`
    /// also reads under its oldest name, and a definition still carrying the
    /// `schema` key old manifests wrote loads without it.
    #[test]
    fn a_region_definition_reads_and_writes_every_kind_in_the_old_shape() {
        let kinds = [
            RegionKind::Pinned,
            RegionKind::SlidingWindow {
                max_items: 3,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            RegionKind::Temporary,
            RegionKind::Compacting {
                threshold_tokens: 10,
            },
            RegionKind::Clearable,
            RegionKind::CompactHistory {
                source_region: "notes".to_string(),
            },
            RegionKind::HashMap {
                max_entries: Some(4),
            },
            RegionKind::Checklist,
            RegionKind::Custom {
                script: "r.rhai".to_string(),
                pinned: true,
            },
        ];
        for kind in kinds {
            let def = RegionDefinition::new("r".to_string(), kind.clone(), 100);
            let json = serde_json::to_string(&def).unwrap();
            let back: RegionDefinition = serde_json::from_str(&json).unwrap();
            assert_eq!(back.kind, kind, "{json}");
        }

        let old = r#"{"name":"r","kind":{"Custom":{"script":"r.rhai","persistent":true}},
            "max_tokens":100,"schema":null,"description":null}"#;
        let def: RegionDefinition = serde_json::from_str(old).unwrap();
        assert_eq!(
            def.kind,
            RegionKind::Custom {
                script: "r.rhai".to_string(),
                pinned: true
            }
        );
    }
}
