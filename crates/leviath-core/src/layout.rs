//! Context window layouts and memory maps.
//!
//! A layout defines the complete memory structure for an agent's context window,
//! including all regions, their sizes, and eviction priorities. This is analogous
//! to a hardware memory map that defines where different types of data live and
//! how they're managed.

use crate::error::ValidationError;
use crate::region::{RegionKind, RegionSchema};
use serde::{Deserialize, Serialize};

/// A ContextLayout defines the complete memory map for an agent.
///
/// Like SNES VRAM layout — every region has a defined purpose, size, and policy.
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

        // Warn if sum of max tokens exceeds budget (not necessarily an error,
        // since not all regions will be full simultaneously)
        // Warn if no SlidingWindow region exists — agents should have a
        // conversation region for typed message entries, but some agents
        // (e.g., deep-researcher) use other region kinds exclusively.
        let has_sliding_window = self
            .regions
            .iter()
            .any(|r| matches!(r.kind, RegionKind::SlidingWindow { .. }));
        if !has_sliding_window {
            tracing::warn!(
                "Layout has no SlidingWindow region — typed conversation entries require one"
            );
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
                 regions (pinned/hashmap/compact_history) consume {fixed_tokens} of the {} \
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
    /// is at least this large — i.e. it's a realistically-sized agent, not a
    /// toy/illustrative layout where an absolute floor wouldn't make sense.
    const BUDGET_CHECK_MIN_TOTAL: usize = 20_000;

    /// Get a region definition by name.
    pub fn get_region(&self, name: &str) -> Option<&RegionDefinition> {
        self.regions.iter().find(|r| r.name == name)
    }
}

/// Where a region's initial content comes from at run start.
///
/// A region without a seed starts empty and is populated by the agent. A seeded
/// region is filled before the first inference: `CallerInput` regions are filled
/// by the run's caller (a CLI `--<name>` flag, an ACP `---region:<name>---`
/// marker, or the API `regions` map); the remaining variants are resolved by the
/// daemon from the run's workdir (which is why this type only *declares* the
/// source — `leviath-core` stays filesystem-agnostic; resolution lives in the
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

    /// Maximum tokens for this region
    pub max_tokens: usize,

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
    /// Create a new region definition.
    pub fn new(name: String, kind: RegionKind, max_tokens: usize) -> Self {
        Self {
            name,
            kind,
            max_tokens,
            schema: None,
            description: None,
            required: false,
            required_message: None,
            seed: None,
        }
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

    /// Minimal no-op `Subscriber` that reports every callsite as enabled.
    ///
    /// Without an active subscriber, `tracing::warn!`/`info!`/`debug!` calls
    /// short-circuit their field-argument evaluation before ever reaching it
    /// (no subscriber means the "is this level enabled" check fails first) --
    /// so a multi-line `tracing::warn!(...)` call's field-list lines show as
    /// uncovered by `cargo llvm-cov` even when the surrounding branch
    /// genuinely executes and is asserted on. This bare `Subscriber` impl is
    /// the proven-working pattern used across this workspace.
    struct AlwaysOnSubscriber;

    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn register_callsite(
            &self,
            _metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            // `register_callsite` always returns `Interest::always()`, so
            // tracing's dispatch macros cache every callsite as
            // "always enabled" and never call `enabled` again afterward.
            // Call it directly here (with real metadata from a live event)
            // so this trait-impl boilerplate method isn't itself left
            // uncovered.
            assert!(self.enabled(event.metadata()));
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
        fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
            Some(tracing::metadata::LevelFilter::TRACE)
        }
    }

    fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
        static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INSTALLED.get_or_init(|| {
            let _ = tracing::subscriber::set_global_default(AlwaysOnSubscriber);
            tracing::callsite::rebuild_interest_cache();
        });
        f()
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        // This file only ever uses `tracing::warn!` event macros, never
        // `tracing::span!`, so the span-related trait methods above are
        // otherwise dead code from `with_tracing`'s callers. Exercise them
        // directly via a real span so they're not left uncovered themselves.
        with_tracing(|| {
            let span = tracing::info_span!("test-span", field = tracing::field::Empty);
            span.record("field", 1);
            let other = tracing::info_span!("other-span");
            span.follows_from(&other);
            let _enter = span.enter();
            tracing::info!(parent: &span, "inside span");
        });
    }

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
        // budget (10000) — this should only warn, not fail validation, since
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
