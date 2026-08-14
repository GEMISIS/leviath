//! Context window layouts and memory maps.
//!
//! A layout defines the complete memory structure for an agent's context window,
//! including all regions, their sizes, and eviction priorities. This is analogous
//! to a hardware memory map that defines where different types of data live and
//! how they're managed.

use crate::error::ValidationError;
use crate::region::{RegionKind, RegionSchema};
use serde::{Deserialize, Serialize};

/// The region a stage's `system_prompt` is written into, when a blueprint
/// declares one by this name.
///
/// Stage instructions have always been pinned context - that is why they read
/// as instruction rather than history - but the region holding them was chosen
/// by accident: whichever pinned region happened to be declared first. That
/// region then carried the prompt's tokens in the stage ledger under its own
/// name, could not be sized or scoped, and sat wherever it sat in the cached
/// prefix (#366).
///
/// Declaring a region by this name gives the prompt a handle:
///
/// ```toml
/// [context.regions]
/// stage_instructions = { kind = "pinned", budget = "3%" }
/// ```
///
/// A blueprint that declares nothing by this name keeps the old behaviour
/// exactly, so this costs no existing agent anything.
pub const STAGE_INSTRUCTIONS_REGION: &str = "stage_instructions";

/// Serde default for a flag that is on unless a blueprint turns it off.
fn default_true() -> bool {
    true
}

/// How a region's token ceiling is expressed before it is resolved against a
/// concrete model context window.
///
/// Blueprint authors think in **proportions** (`budget = "35%"`) so their intent
/// stays correct regardless of the model's context size, while power users can
/// still pin an exact count. The percentage denominator - the model's context
/// window - is not known at parse time, so the spec is stored unresolved here and
/// turned into a concrete token count at window-build time (see
/// [`BudgetSpec::resolve`] and [`ContextLayout::resolved`]).
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

    /// Resolve this spec to a concrete token count against a model context
    /// `window`.
    ///
    /// [`Absolute`](BudgetSpec::Absolute) ignores the window (idempotent - a
    /// fully-absolute layout resolves to itself). [`Percent`](BudgetSpec::Percent)
    /// rounds `window * percent`, then applies the `max` cap, then the `min`
    /// floor. The floor is applied **last** so that when `min > max` the floor
    /// wins: a region starved below a usable size is worse than one slightly over
    /// its cap.
    pub fn resolve(&self, window: usize) -> usize {
        match self {
            BudgetSpec::Absolute(n) => *n,
            BudgetSpec::Percent { percent, min, max } => {
                let mut v = (window as f64 * percent).round() as usize;
                if let Some(max) = max {
                    v = v.min(*max);
                }
                if let Some(min) = min {
                    v = v.max(*min);
                }
                v
            }
        }
    }

    /// Whether this is a percentage budget (needs a model window to resolve).
    pub fn is_percent(&self) -> bool {
        matches!(self, BudgetSpec::Percent { .. })
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

    /// Set the eviction order for this layout.
    pub fn with_eviction_order(mut self, order: Vec<String>) -> Self {
        self.eviction_order = order;
        self
    }

    /// Validate that the layout is well-formed.
    ///
    /// Checks:
    /// - Sum of max_tokens doesn't exceed total_budget_tokens
    /// - All region names in eviction_order exist
    /// - No duplicate region names
    pub fn validate(&self) -> std::result::Result<(), ValidationError> {
        // Check for duplicate region names
        let mut names = std::collections::HashSet::new();
        for region in &self.regions {
            if !names.insert(region.name.as_str()) {
                return Err(ValidationError::Region {
                    region: region.name.clone(),
                    message: "duplicate region name".to_string(),
                });
            }
        }

        // Check that eviction_order regions exist
        for name in &self.eviction_order {
            if !names.contains(name.as_str()) {
                return Err(ValidationError::Layout(format!(
                    "eviction order references unknown region: {}",
                    name
                )));
            }
        }

        // Reject a Custom region whose script path is empty - it could never
        // resolve to a file, and the runtime would silently fall back to
        // Temporary-style rendering on every inference.
        for region in &self.regions {
            if let RegionKind::Custom { script, .. } = &region.kind
                && script.trim().is_empty()
            {
                return Err(ValidationError::Region {
                    region: region.name.clone(),
                    message: "custom region requires a non-empty script path".to_string(),
                });
            }
        }

        // Warn if sum of max tokens exceeds budget (not necessarily an error,
        // since not all regions will be full simultaneously)
        // Warn if no SlidingWindow region exists - agents should have a
        // conversation region for typed message entries, but some agents
        // (e.g., deep-researcher) use other region kinds exclusively. A Custom
        // region counts: its script can render typed entries as messages.
        let has_message_region = self.regions.iter().any(|r| {
            matches!(
                r.kind,
                RegionKind::SlidingWindow { .. } | RegionKind::Custom { .. }
            )
        });
        if !has_message_region {
            tracing::warn!(
                "Layout has no SlidingWindow (or custom scripted) region - typed \
                 conversation entries require one"
            );
        }

        // The token-sum warning and the fixed-working-budget hard error below
        // operate on concrete `max_tokens` values. When percentage budgets are
        // present those values are provisional placeholders until the layout is
        // resolved against a model window, so the checks are meaningless here -
        // skip them and rely on the post-resolution `validate()` call at spawn.
        if self.has_percent_budgets() {
            return Ok(());
        }

        let total_max: usize = self.regions.iter().map(|r| r.max_tokens).sum();
        if total_max > self.total_budget_tokens {
            tracing::warn!(
                "Sum of region max tokens ({}) exceeds total budget ({})",
                total_max,
                self.total_budget_tokens
            );
        }

        // Ensure the layout leaves a minimum working budget once the fixed,
        // non-evictable regions are full. Pinned / HashMap / CompactHistory
        // regions persist for the whole run and consume budget; if they leave
        // too little room, the conversation/tool-result (evictable) regions have
        // almost no space and the agent operates "blind". Fail loudly at load
        // instead of degrading silently at runtime.
        let fixed_tokens: usize = self
            .regions
            .iter()
            .filter(|r| {
                matches!(
                    r.kind,
                    RegionKind::Pinned
                        | RegionKind::HashMap { .. }
                        | RegionKind::CompactHistory { .. }
                        | RegionKind::Custom {
                            persistent: true,
                            ..
                        }
                )
            })
            .map(|r| r.max_tokens)
            .sum();
        // Only enforce the absolute working-budget floor on realistically-sized
        // layouts. Tiny illustrative layouts (toy examples, unit-test fixtures)
        // have small budgets by design and are not real agent runs; applying an
        // absolute floor to them would be nonsensical.
        let working_tokens = self.total_budget_tokens.saturating_sub(fixed_tokens);
        if self.total_budget_tokens >= Self::BUDGET_CHECK_MIN_TOTAL
            && working_tokens < Self::MIN_WORKING_TOKENS
        {
            return Err(ValidationError::Layout(format!(
                "context layout leaves only {working_tokens} working tokens after fixed \
                 regions (pinned/hashmap/compact_history/persistent custom) consume \
                 {fixed_tokens} of the {} \
                 total budget; at least {} are needed for the agent to operate. Reduce the \
                 fixed regions' max_tokens or increase the total budget.",
                self.total_budget_tokens,
                Self::MIN_WORKING_TOKENS
            )));
        }

        Ok(())
    }

    /// Minimum token budget that must remain for evictable/working regions
    /// (conversation, tool results, scratch) after the fixed regions are full,
    /// so the agent has room to hold recent context and generate. Below this a
    /// run would operate with almost no working space.
    const MIN_WORKING_TOKENS: usize = 8000;

    /// The working-budget floor is only enforced when the layout's total budget
    /// is at least this large - i.e. it's a realistically-sized agent, not a
    /// toy/illustrative layout where an absolute floor wouldn't make sense.
    const BUDGET_CHECK_MIN_TOTAL: usize = 20_000;

    /// Get a region definition by name.
    pub fn get_region(&self, name: &str) -> Option<&RegionDefinition> {
        self.regions.iter().find(|r| r.name == name)
    }

    /// Whether any region uses a percentage budget (and therefore needs a model
    /// context window to resolve to concrete token counts).
    pub fn has_percent_budgets(&self) -> bool {
        self.regions.iter().any(|r| r.budget.is_percent())
    }

    /// Resolve every region's percentage budget against a concrete model context
    /// `window`, returning a fully-absolute layout.
    ///
    /// Each region's `max_tokens` becomes `budget.resolve(window)`, and each
    /// [`RegionKind::Compacting`] region's `threshold_tokens` is recomputed from
    /// its [`compact_at`](RegionDefinition::compact_at) fraction (via the private
    /// `resolve_compacting_threshold` helper). `eviction_order` is preserved. The
    /// total budget becomes the model `window` when any percentage budget is
    /// present (percentage ceilings are relative to the whole window and may sum
    /// past 100%); a pure-absolute layout keeps its legacy summed total unchanged.
    ///
    /// Resolving an already-absolute layout is a no-op, so this is safe to call
    /// unconditionally at window-build time.
    pub fn resolved(&self, window: usize) -> ContextLayout {
        let regions = self
            .regions
            .iter()
            .map(|r| {
                let max_tokens = r.budget.resolve(window);
                let kind = match &r.kind {
                    RegionKind::Compacting { threshold_tokens } => RegionKind::Compacting {
                        threshold_tokens: Self::resolve_compacting_threshold(
                            r.compact_at,
                            *threshold_tokens,
                            max_tokens,
                        ),
                    },
                    other => other.clone(),
                };
                // Emit a fully-absolute region: the percentage has been baked
                // into `max_tokens` and the compaction threshold into `kind`, so
                // the resolved layout carries no `Percent` budgets. This makes
                // `has_percent_budgets()` false on the result, so a post-resolution
                // `validate()` runs the real token/working-budget checks.
                RegionDefinition {
                    kind,
                    max_tokens,
                    budget: BudgetSpec::Absolute(max_tokens),
                    compact_at: None,
                    ..r.clone()
                }
            })
            .collect();

        let total_budget_tokens = if self.has_percent_budgets() {
            window
        } else {
            self.total_budget_tokens
        };

        ContextLayout {
            regions,
            total_budget_tokens,
            eviction_order: self.eviction_order.clone(),
        }
    }

    /// Compute a Compacting region's concrete compaction threshold from its
    /// `compact_at` fraction, the absolute `threshold_tokens` guard carried on
    /// the kind, and the region's resolved budget.
    ///
    /// - `compact_at = Some(f)` with an explicit `threshold_tokens` cap (any
    ///   value below the [`usize::MAX`] sentinel) → `min(round(budget * f), cap)`:
    ///   compact at the percentage, but never later than the absolute guard-rail.
    /// - `compact_at = Some(f)` with no cap (`threshold_tokens == usize::MAX`
    ///   sentinel) → `round(budget * f)`.
    /// - `compact_at = None` → the absolute `threshold_tokens` as-is (back-compat,
    ///   including the parser's `max_tokens * 8 / 10` default).
    ///
    /// The `usize::MAX` sentinel is safe: a layout is always resolved before any
    /// [`Region::needs_compaction`](crate::region::Region::needs_compaction) check.
    fn resolve_compacting_threshold(
        compact_at: Option<f64>,
        threshold_tokens: usize,
        resolved_budget: usize,
    ) -> usize {
        match compact_at {
            Some(fraction) => {
                let pct = (resolved_budget as f64 * fraction).round() as usize;
                pct.min(threshold_tokens)
            }
            None => threshold_tokens,
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
    pub kind: RegionKind,

    /// **Resolved** maximum tokens for this region. This is the concrete ceiling
    /// every downstream consumer reads; for a percentage budget it is populated
    /// when the layout is resolved against a model window (see
    /// [`ContextLayout::resolved`]). [`Self::budget`] is the source of truth for
    /// how this value is derived.
    pub max_tokens: usize,

    /// How this region's ceiling is expressed. Defaults (via [`Self::new`]) to
    /// [`BudgetSpec::Absolute`] holding `max_tokens`, so a region built the old
    /// way behaves exactly as before. A percentage budget is resolved against the
    /// model context window at window-build time.
    #[serde(default)]
    pub budget: BudgetSpec,

    /// For [`RegionKind::Compacting`] regions only: compact when the region
    /// reaches this fraction of its resolved budget (`0.80` for `compact_at =
    /// "80%"`). `None` keeps the absolute `threshold_tokens` carried on the kind.
    /// See [`ContextLayout::resolved`] for how this becomes a concrete threshold.
    #[serde(default)]
    pub compact_at: Option<f64>,

    /// Optional validation schema
    pub schema: Option<RegionSchema>,

    /// Human-readable description of this region's purpose
    pub description: Option<String>,

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
    /// paraphrase are no longer figures: a `results` region carrying computed
    /// values was rewritten into prose before the stage that reports them saw
    /// it (#369).
    ///
    /// Setting this false protects the region wherever it is used, rather than
    /// at each of the N edges that might touch it. `clear` still applies - this
    /// says "do not paraphrase my content", not "keep it forever".
    #[serde(default = "default_true")]
    pub summarizable: bool,

    /// What this region does when a write does not fit.
    ///
    /// Declared per region rather than per stage: whether losing the oldest
    /// entry is acceptable is a property of what the region holds, and does not
    /// change depending on which stage is writing to it.
    #[serde(default)]
    pub admission: crate::region::Admission,

    /// Optional custom message shown to the agent when this region is required
    /// but empty. Falls back to a generated default when `None`.
    #[serde(default)]
    pub required_message: Option<String>,

    /// Where this region's initial content comes from at run start. `None`
    /// means the region starts empty (the agent populates it). See
    /// [`RegionSeed`].
    #[serde(default)]
    pub seed: Option<RegionSeed>,
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
            schema: None,
            description: None,
            required: false,
            required_message: None,
            summarizable: true,
            admission: crate::region::Admission::default(),
            seed: None,
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

    /// Add a schema to this region definition.
    pub fn with_schema(mut self, schema: RegionSchema) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Add a description to this region definition.
    pub fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_testkit::with_tracing;

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
    fn test_layout_validation() {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout =
            ContextLayout::new(regions, 10000).with_eviction_order(vec!["test".to_string()]);

        assert!(layout.validate().is_ok());
    }

    #[test]
    fn test_duplicate_region_names() {
        let regions = vec![
            RegionDefinition::new("test".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("test".to_string(), RegionKind::Temporary, 3000),
        ];
        let layout = ContextLayout::new(regions, 10000);

        assert!(layout.validate().is_err());
    }

    #[test]
    fn test_eviction_order_unknown_region_is_error() {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout =
            ContextLayout::new(regions, 10000).with_eviction_order(vec!["nonexistent".to_string()]);

        let err = layout.validate().unwrap_err();
        assert_eq!(
            err,
            ValidationError::Layout(
                "eviction order references unknown region: nonexistent".to_string()
            )
        );
    }

    #[test]
    fn test_validate_warns_but_does_not_error_when_max_tokens_exceed_budget() {
        // Sum of region max_tokens (5000 + 10000 = 15000) exceeds the total
        // budget (10000) - this should only warn, not fail validation, since
        // not all regions are full simultaneously.
        let regions = vec![
            RegionDefinition::new("a".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("b".to_string(), RegionKind::Temporary, 10000),
        ];
        let layout = ContextLayout::new(regions, 10000);
        with_tracing(|| {
            assert!(layout.validate().is_ok());
        });
    }

    #[test]
    fn validate_errors_when_fixed_regions_starve_working_budget() {
        // Realistically-sized layout (>= 20k) where a huge fixed (pinned) region
        // leaves < 8000 working tokens for conversation/tool-results → hard error.
        let regions = vec![
            RegionDefinition::new("big_pinned".to_string(), RegionKind::Pinned, 95_000),
            RegionDefinition::new("work".to_string(), RegionKind::Temporary, 5_000),
        ];
        let layout = ContextLayout::new(regions, 100_000);
        with_tracing(|| {
            let err = layout.validate().unwrap_err();
            assert!(
                err.to_string().contains("working tokens"),
                "actionable budget error: {err}"
            );
        });
    }

    #[test]
    fn validate_ok_for_realistic_layout_with_working_room() {
        let regions = vec![
            RegionDefinition::new("task".to_string(), RegionKind::Pinned, 4_000),
            RegionDefinition::new("conversation".to_string(), RegionKind::Temporary, 40_000),
        ];
        let layout = ContextLayout::new(regions, 44_000);
        with_tracing(|| {
            assert!(layout.validate().is_ok());
        });
    }

    fn custom_kind(script: &str, persistent: bool) -> RegionKind {
        RegionKind::Custom {
            script: script.to_string(),
            persistent,
        }
    }

    #[test]
    fn validate_rejects_custom_region_with_empty_script() {
        // Whitespace-only counts as empty: it could never resolve to a file
        // and the runtime would silently fall back on every inference.
        let regions = vec![RegionDefinition::new(
            "brain".to_string(),
            custom_kind("   ", false),
            5000,
        )];
        let layout = ContextLayout::new(regions, 10_000);
        let err = with_tracing(|| layout.validate().unwrap_err());
        assert!(
            err.to_string().contains("non-empty script path"),
            "actionable error: {err}"
        );
    }

    #[test]
    fn validate_counts_persistent_custom_as_fixed_budget() {
        // A persistent custom region is Pinned-like: protected from eviction,
        // so it must count toward the fixed budget that can starve the
        // working room.
        let regions = vec![
            RegionDefinition::new("vault".to_string(), custom_kind("v.rhai", true), 95_000),
            RegionDefinition::new("work".to_string(), RegionKind::Temporary, 5_000),
        ];
        let layout = ContextLayout::new(regions, 100_000);
        let err = with_tracing(|| layout.validate().unwrap_err());
        assert!(err.to_string().contains("working tokens"), "{err}");
    }

    #[test]
    fn validate_counts_non_persistent_custom_as_working_budget() {
        // Same shape, but the custom region is evictable - it IS the working
        // room, so validation passes.
        let regions = vec![
            RegionDefinition::new("brain".to_string(), custom_kind("b.rhai", false), 95_000),
            RegionDefinition::new("task".to_string(), RegionKind::Pinned, 4_000),
        ];
        let layout = ContextLayout::new(regions, 100_000);
        with_tracing(|| {
            assert!(layout.validate().is_ok());
        });
    }

    #[test]
    fn custom_region_satisfies_the_message_region_check() {
        // A layout whose only region is custom must not trip the "no
        // SlidingWindow region" warning path - its script can render typed
        // entries as messages. (Mirrors the sliding-window-present test: the
        // skip branch is exercised, validation succeeds.)
        let regions = vec![RegionDefinition::new(
            "everything".to_string(),
            custom_kind("all.rhai", false),
            9_000,
        )];
        let layout = ContextLayout::new(regions, 10_000);
        with_tracing(|| {
            assert!(layout.validate().is_ok());
        });
    }

    #[test]
    fn resolved_percent_budget_applies_to_custom_region() {
        // The "recreate built-ins in Rhai" guarantee: percentage budgets work
        // on custom regions exactly as on built-in kinds, resolved against
        // the stage model's context window at spawn.
        let def = RegionDefinition::new("brain".to_string(), custom_kind("b.rhai", false), 0)
            .with_budget(BudgetSpec::Percent {
                percent: 0.40,
                min: Some(10_000),
                max: None,
            });
        let layout = ContextLayout::new(vec![def], 0);
        let resolved = layout.resolved(200_000);
        assert_eq!(resolved.regions[0].max_tokens, 80_000);
        assert!(matches!(
            resolved.regions[0].kind,
            RegionKind::Custom { ref script, persistent: false } if script == "b.rhai"
        ));
        // The min floor wins on a small window.
        let small = layout.resolved(8_192);
        assert_eq!(small.regions[0].max_tokens, 10_000);
    }

    #[test]
    fn test_get_region_found() {
        let regions = vec![
            RegionDefinition::new("a".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("b".to_string(), RegionKind::Temporary, 3000),
        ];
        let layout = ContextLayout::new(regions, 10000);

        let found = layout.get_region("b").unwrap();
        assert_eq!(found.name, "b");
        assert_eq!(found.max_tokens, 3000);
    }

    #[test]
    fn test_get_region_not_found() {
        let regions = vec![RegionDefinition::new(
            "a".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout = ContextLayout::new(regions, 10000);
        assert!(layout.get_region("missing").is_none());
    }

    #[test]
    fn test_region_definition_with_schema() {
        let schema = crate::region::RegionSchema::new(crate::region::ContentFormat::Json);
        let def =
            RegionDefinition::new("a".to_string(), RegionKind::Pinned, 5000).with_schema(schema);
        assert_eq!(
            def.schema.as_ref().unwrap().format,
            crate::region::ContentFormat::Json
        );
    }

    #[test]
    fn test_region_definition_with_description() {
        let def = RegionDefinition::new("a".to_string(), RegionKind::Pinned, 5000)
            .with_description("holds architecture notes".to_string());
        assert_eq!(def.description.as_deref(), Some("holds architecture notes"));
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
    fn resolve_absolute_ignores_window() {
        assert_eq!(BudgetSpec::Absolute(4000).resolve(1_000_000), 4000);
        assert!(!BudgetSpec::Absolute(4000).is_percent());
    }

    #[test]
    fn resolve_percent_of_window() {
        let spec = BudgetSpec::Percent {
            percent: 0.35,
            min: None,
            max: None,
        };
        assert_eq!(spec.resolve(1_000_000), 350_000);
        assert!(spec.is_percent());
    }

    #[test]
    fn resolve_percent_applies_max_cap() {
        let spec = BudgetSpec::Percent {
            percent: 0.02,
            min: None,
            max: Some(4000),
        };
        // 2% of 1M = 20_000, capped to 4000.
        assert_eq!(spec.resolve(1_000_000), 4000);
    }

    #[test]
    fn resolve_percent_applies_min_floor() {
        let spec = BudgetSpec::Percent {
            percent: 0.02,
            min: Some(2000),
            max: None,
        };
        // 2% of 8000 = 160, floored to 2000.
        assert_eq!(spec.resolve(8000), 2000);
    }

    #[test]
    fn resolve_percent_within_bounds_takes_neither_clamp() {
        let spec = BudgetSpec::Percent {
            percent: 0.10,
            min: Some(1000),
            max: Some(50_000),
        };
        // 10% of 200k = 20_000, between the floor and cap.
        assert_eq!(spec.resolve(200_000), 20_000);
    }

    #[test]
    fn resolve_percent_floor_wins_when_min_exceeds_max() {
        let spec = BudgetSpec::Percent {
            percent: 0.10,
            min: Some(9000),
            max: Some(4000),
        };
        // 10% of 200k = 20_000 → capped to 4000 → floored up to 9000 (floor wins).
        assert_eq!(spec.resolve(200_000), 9000);
    }

    #[test]
    fn has_percent_budgets_detects_percentage_regions() {
        let absolute = ContextLayout::new(
            vec![RegionDefinition::new(
                "a".to_string(),
                RegionKind::Pinned,
                5000,
            )],
            5000,
        );
        assert!(!absolute.has_percent_budgets());

        let percent = ContextLayout::new(
            vec![
                RegionDefinition::new("a".to_string(), RegionKind::Pinned, 5000).with_budget(
                    BudgetSpec::Percent {
                        percent: 0.05,
                        min: None,
                        max: None,
                    },
                ),
            ],
            5000,
        );
        assert!(percent.has_percent_budgets());
    }

    #[test]
    fn resolved_is_noop_for_absolute_layout() {
        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "a".to_string(),
                RegionKind::Pinned,
                5000,
            )],
            5000,
        );
        let resolved = layout.resolved(1_000_000);
        assert_eq!(resolved.regions[0].max_tokens, 5000);
        // Absolute layout keeps its legacy summed total, not the window.
        assert_eq!(resolved.total_budget_tokens, 5000);
    }

    #[test]
    fn resolved_percent_layout_uses_window_as_total() {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("a".to_string(), RegionKind::Pinned, 0).with_budget(
                    BudgetSpec::Percent {
                        percent: 0.10,
                        min: None,
                        max: None,
                    },
                ),
            ],
            0,
        )
        .with_eviction_order(vec!["a".to_string()]);
        let resolved = layout.resolved(1_000_000);
        assert_eq!(resolved.regions[0].max_tokens, 100_000);
        assert_eq!(resolved.total_budget_tokens, 1_000_000);
        // eviction order carried through.
        assert_eq!(resolved.eviction_order, vec!["a".to_string()]);
    }

    #[test]
    fn resolved_compacting_threshold_all_cases() {
        // compact_at + explicit threshold cap → min(pct, cap).
        let both = RegionDefinition::new(
            "c".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 25_000,
            },
            0,
        )
        .with_budget(BudgetSpec::Percent {
            percent: 0.20,
            min: None,
            max: None,
        })
        .with_compact_at(0.80);
        let r = ContextLayout::new(vec![both], 0).resolved(200_000);
        // budget = 40_000; 80% = 32_000; capped to 25_000.
        assert_eq!(
            r.regions[0].kind,
            RegionKind::Compacting {
                threshold_tokens: 25_000
            }
        );

        // compact_at with no cap (usize::MAX sentinel) → pct only.
        let pct_only = RegionDefinition::new(
            "c".to_string(),
            RegionKind::Compacting {
                threshold_tokens: usize::MAX,
            },
            0,
        )
        .with_budget(BudgetSpec::Percent {
            percent: 0.20,
            min: None,
            max: None,
        })
        .with_compact_at(0.80);
        let r = ContextLayout::new(vec![pct_only], 0).resolved(200_000);
        assert_eq!(
            r.regions[0].kind,
            RegionKind::Compacting {
                threshold_tokens: 32_000
            }
        );

        // compact_at = None → absolute threshold passes through unchanged.
        let absolute = RegionDefinition::new(
            "c".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 8000,
            },
            10_000,
        );
        let r = ContextLayout::new(vec![absolute], 10_000).resolved(1_000_000);
        assert_eq!(
            r.regions[0].kind,
            RegionKind::Compacting {
                threshold_tokens: 8000
            }
        );
    }

    #[test]
    fn validate_skips_token_checks_for_percent_layouts() {
        // A percentage layout whose provisional max_tokens are tiny/zero must not
        // trip the fixed-working-budget hard error - that check is deferred to
        // post-resolution. Wrap in tracing so no warn-arg lines read uncovered.
        let regions = vec![
            RegionDefinition::new("big_pinned".to_string(), RegionKind::Pinned, 0).with_budget(
                BudgetSpec::Percent {
                    percent: 0.95,
                    min: None,
                    max: None,
                },
            ),
        ];
        let layout = ContextLayout::new(regions, 100_000);
        with_tracing(|| {
            assert!(layout.validate().is_ok());
        });
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

    #[test]
    fn test_validate_with_sliding_window_present() {
        // A layout that DOES contain a SlidingWindow region exercises the
        // has_sliding_window detection returning true, so the "no sliding
        // window" warning branch is skipped.
        let regions = vec![
            RegionDefinition::new("pinned".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new(
                "conv".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: crate::region::EvictionStrategy::PerItem,
                },
                5000,
            ),
        ];
        let layout = ContextLayout::new(regions, 20000);
        with_tracing(|| {
            assert!(layout.validate().is_ok());
        });
    }
}
