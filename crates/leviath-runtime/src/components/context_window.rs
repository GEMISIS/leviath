//! The agent's context window: what it remembers, and what it forgets first.
//!
//! Every region an agent holds lives here, along with the assembly that turns
//! them into a request and the eviction that keeps them inside a budget. Split
//! out of `components` because it was two thirds of that file on its own, and
//! because "what an agent *is*" and "what an agent *remembers*" are different
//! questions to arrive with.

use super::block_cache::{
    CACHE_CHUNK_TOKENS, append_only_chunks, cache_hint_sort_priority, mark_breakpoint_eligibility,
    mark_recently_changed_run, push_chunked, system_prefix_hash,
};
use super::*;

/// Result of an eviction attempt, including tokens freed and regions needing LLM compaction.
#[derive(Debug, Clone)]
pub struct EvictionResult {
    /// Number of tokens freed by eviction phases 1-2 (Clearable + Temporary).
    pub tokens_freed: usize,
    /// Region names that need LLM-based compaction (phase 3).
    pub needs_compaction: Vec<String>,
}

/// Per-stage inference configuration overrides.
///
/// Set on the agent entity before each stage to override default inference
/// parameters like temperature and max output tokens. When absent, defaults
/// are used (temperature 0.7, max output 4096).
#[derive(Component, Debug, Clone, Default)]
pub struct InferenceConfig {
    /// Temperature override. If None, uses 0.7 (or 0.0 if model doesn't support it).
    pub temperature: Option<f32>,
    /// Max output tokens override. If None, caps at model's max_output_tokens capability.
    pub max_output_tokens: Option<usize>,
    /// Extra provider parameters from `[stages.<name>.model.parameters]` beyond
    /// `temperature`/`max_output_tokens` (e.g. `top_p`, `stop`, `seed`,
    /// `frequency_penalty`). Passed through to the provider request so models can
    /// be tuned from the manifest. Empty when none are set.
    pub extra_params: serde_json::Map<String, serde_json::Value>,
    /// Whether to prepend the batch-tool-calls hint to this stage's system
    /// prompt. Resolved from the global config → agent → stage cascade at spawn
    /// (see [`leviath_core::taint::resolve_batch_tool_hint`]); `false` by default
    /// so an unset config is a no-op.
    pub batch_tool_hint: bool,
    /// Whether this stage is eligible for the platform shell hint. Resolved from
    /// the global config → agent → stage cascade at spawn (see
    /// [`leviath_core::taint::resolve_shell_hint`]); `false` by default so an
    /// unset config is a no-op. Eligibility is not emission: the hint also needs
    /// a platform worth describing and a stage that advertises the shell tool.
    pub shell_hint: bool,
    /// Per-stage cap on the wall-clock time (in seconds) one inference for this
    /// stage may run (the whole call including retries). Sourced from
    /// `[stages.<name>.model] request_timeout_secs`. When `Some`, it overrides the
    /// default inference job timeout at dispatch; when `None`, the default applies.
    pub request_timeout_secs: Option<u64>,
}

/// Per-entity tool result routing configuration.
///
/// When present on an entity, tool results are routed to the specified region(s)
/// instead of the default "conversation" region.
#[derive(Component, Debug, Clone)]
pub struct ToolResultRoutingComponent {
    /// The routing configuration.
    pub routing: leviath_core::ToolResultRouting,
}

/// Result of assembling a context window into system blocks and conversation messages.
///
/// Produced by [`ContextWindow::assemble()`]. System-bound regions (Pinned,
/// CompactHistory, etc.) become `system_blocks`; the messages region
/// (SlidingWindow) becomes typed `messages`.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    /// System prompt blocks (from Pinned, CompactHistory, etc. regions).
    pub system_blocks: Vec<leviath_providers::SystemBlock>,
    /// Conversation messages with proper role typing.
    pub messages: Vec<leviath_providers::Message>,
    /// Per-block digests of the system prefix this assembly produced.
    ///
    /// Handed back so the caller can pass them as
    /// [`crate::custom_region::AssembleMeta::previous_block_hashes`] next time,
    /// which is what lets the breakpoint decision name only blocks that held
    /// still.
    pub block_hashes: Vec<u64>,
    /// Hash of the system prefix this assembly produced.
    ///
    /// Handed back so the caller can pass it as
    /// [`crate::custom_region::AssembleMeta::previous_system_hash`] next time
    /// and let the breakpoint decision below be made on evidence rather than
    /// hope.
    pub system_hash: u64,
}

/// Context window component storing the agent's memory regions.
#[derive(Component, Debug, Clone)]
pub struct ContextWindow {
    /// All regions in this context window
    pub regions: Vec<Region>,

    /// Current total token usage
    pub current_tokens: usize,

    /// Maximum token budget
    pub max_tokens: usize,

    /// Compiled custom-region scripts, keyed by the script path each
    /// `RegionKind::Custom` carries. Populated once at spawn by the CLI
    /// (which resolves blueprint-dir-relative paths and compile-checks the
    /// files); a stage-layout swap rebuilds `regions` but leaves this table
    /// untouched, so per-stage custom regions keep working. Empty when no
    /// custom regions exist - every hook lookup then misses and the region
    /// renders its fallback shape.
    pub region_scripts: std::collections::HashMap<
        String,
        std::sync::Arc<leviath_scripting::region_hook::RegionScript>,
    >,

    /// Regions the current stage does not attend to.
    ///
    /// Held, not deleted. A stage layout that omits a region used to have it
    /// dropped from the window entirely, so re-declaring it in a later stage
    /// brought it back empty - which made the feature unusable for the thing it
    /// looks designed for, narrowing what one stage sees in a pipeline whose
    /// later stages still need the data. Omission now means "not assembled for
    /// this stage" and nothing else.
    ///
    /// Reset on every stage entry by [`crate::context_setup::apply_layout`], so
    /// it describes the stage in front of it rather than accumulating.
    pub hidden: std::collections::HashSet<String>,
}

/// Put a region's own name above its contents, and its description under that
/// when it has one.
///
/// The name is the part that earns its tokens. An agent writes to a region *by
/// name* - `context_write { region: "sources_index", .. }` - but until now the
/// prompt showed it the contents of every region with nothing saying which was
/// which. It could read `sources_index` and it could write to `sources_index`,
/// and it had no way to know they were the same place. Three tokens of heading
/// closes that.
///
/// The description is opt-in and usually absent, because most regions are named
/// well enough that a sentence would only cost tokens. It is for the ones whose
/// contents do not explain themselves - a bibliography with a required format,
/// a scratch area with a convention.
pub(super) fn labelled(region: &Region, body: &str) -> String {
    match region
        .description
        .as_deref()
        .filter(|_| region.describe_in_prompt)
    {
        Some(description) => format!("## {}\n{}\n\n{}", region.name, description, body),
        None => format!("## {}\n{}", region.name, body),
    }
}

impl ContextWindow {
    /// Create a new context window with the specified budget.
    pub fn new(max_tokens: usize) -> Self {
        Self {
            regions: Vec::new(),
            hidden: std::collections::HashSet::new(),
            current_tokens: 0,
            max_tokens,
            region_scripts: std::collections::HashMap::new(),
        }
    }

    /// The compiled script backing `region_name`, when it is a custom region
    /// whose script path has an entry in [`Self::region_scripts`].
    fn custom_script_for(
        &self,
        region_name: &str,
    ) -> Option<std::sync::Arc<leviath_scripting::region_hook::RegionScript>> {
        let region = self.get_region(region_name)?;
        let leviath_core::RegionKind::Custom { script, .. } = &region.kind else {
            return None;
        };
        self.region_scripts.get(script).cloned()
    }

    /// Run a custom region's `on_write` hook (when defined) for an incoming
    /// entry. `None` means the script dropped the entry - the write reports
    /// success without storing anything. Non-custom regions, missing scripts,
    /// and hook failures all accept the entry unchanged.
    ///
    /// Deliberately NOT invoked by the layout-swap carry or restore overlay:
    /// those re-add entries the hook already accepted once.
    fn on_write_outcome(
        &self,
        region_name: &str,
        content: String,
        tokens: usize,
        kind: &leviath_core::EntryKind,
    ) -> Option<(String, usize)> {
        let Some(script) = self.custom_script_for(region_name) else {
            return Some((content, tokens));
        };
        if !script.has_on_write() {
            return Some((content, tokens));
        }
        // The region exists - custom_script_for resolved through it.
        let region = self
            .get_region(region_name)
            .expect("custom_script_for resolved through this region");
        match crate::custom_region::apply_on_write(&script, region, content, tokens, kind) {
            crate::custom_region::OnWriteOutcome::Accept(content, tokens) => {
                Some((content, tokens))
            }
            crate::custom_region::OnWriteOutcome::Drop => None,
        }
    }

    /// Retry hook for a custom-region write that hit `TokenBudgetExceeded`:
    /// let the script's `on_overflow` free room, then report whether a single
    /// retry is worthwhile. Non-custom regions and hook failures leave the
    /// original error standing (the callers' existing truncation ladders
    /// apply).
    fn try_custom_overflow(&mut self, region_name: &str, incoming_tokens: usize) -> bool {
        let Some(script) = self.custom_script_for(region_name) else {
            return false;
        };
        if !script.has_on_overflow() {
            return false;
        }
        let region = self
            .get_region_mut(region_name)
            .expect("custom_script_for resolved through this region");
        let needed = (region.current_tokens + incoming_tokens).saturating_sub(region.max_tokens);
        let freed = crate::custom_region::apply_overflow(&script, region, needed);
        self.current_tokens = self.calculate_tokens();
        freed >= needed && needed > 0
    }

    /// Get a region by name.
    pub fn get_region(&self, name: &str) -> Option<&Region> {
        self.regions.iter().find(|r| r.name == name)
    }

    /// Get a mutable reference to a region by name.
    pub fn get_region_mut(&mut self, name: &str) -> Option<&mut Region> {
        self.regions.iter_mut().find(|r| r.name == name)
    }

    /// Add a region to this context window.
    pub fn add_region(&mut self, region: Region) {
        self.regions.push(region);
        self.current_tokens = self.calculate_tokens();
    }

    /// Add content to a specific region.
    pub fn add_to_region(
        &mut self,
        region_name: &str,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.add_to_region_keyed(region_name, None, content, tokens)
    }

    /// Add an entry that may carry a key, so the agent can name it again to
    /// release it.
    ///
    /// Routed through the same private `write_to_region` tail
    /// as the unkeyed path, so a keyed write still passes the region's
    /// `on_write` hook and still gets `on_overflow` a chance to make room.
    /// Writing keys through a shortcut instead is how they came to be honoured
    /// on one region kind and dropped on the rest.
    pub fn add_to_region_keyed(
        &mut self,
        region_name: &str,
        key: Option<&str>,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let Some((content, tokens)) =
            self.on_write_outcome(region_name, content, tokens, &leviath_core::EntryKind::Text)
        else {
            return Ok(()); // the region's script dropped the entry
        };
        self.write_to_region(region_name, tokens, &mut |region, tokens| match key {
            Some(k) => region.add_keyed_entry(k, content.clone(), tokens),
            None => region.add_entry(content.clone(), tokens),
        })
    }

    /// Replace a region's entire content with a single entry (clear, then add).
    /// Returns `false` (no-op) if the region does not exist. Used to keep an
    /// authoritative document region (e.g. the plan) holding only its current
    /// version, so revisions build on it instead of accumulating stale copies.
    pub fn replace_region(&mut self, region_name: &str, content: String, tokens: usize) -> bool {
        // The replacement passes through on_write like any incoming entry - a
        // custom region's script sees (and may transform or refuse) it.
        let Some((content, tokens)) =
            self.on_write_outcome(region_name, content, tokens, &leviath_core::EntryKind::Text)
        else {
            // Dropped by the script: the region keeps its current content.
            return self.get_region(region_name).is_some();
        };
        if let Some(region) = self.get_region_mut(region_name) {
            region.clear();
            let _ = region.add_entry(content, tokens);
            self.current_tokens = self.calculate_tokens();
            true
        } else {
            false
        }
    }

    /// Add a typed entry to a specific region.
    ///
    /// Like [`add_to_region`](Self::add_to_region) but the entry carries an
    /// `EntryKind` so message roles are determined by type, not text-prefix
    /// parsing.
    pub fn add_typed_entry(
        &mut self,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let Some((content, tokens)) = self.on_write_outcome(region_name, content, tokens, &kind)
        else {
            return Ok(());
        };
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            region.add_typed_entry(content.clone(), tokens, kind.clone())
        })
    }

    /// Shared tail of every region write: run the insert, give a custom
    /// region's `on_overflow` one shot at freeing room when the budget
    /// rejects it, and recount the window. A `&mut dyn FnMut` (not generic)
    /// keeps one instantiation for the coverage gate.
    fn write_to_region(
        &mut self,
        region_name: &str,
        tokens: usize,
        insert: &mut dyn FnMut(&mut Region, usize) -> leviath_core::Result<()>,
    ) -> leviath_core::Result<()> {
        if self.get_region(region_name).is_none() {
            return Err(leviath_core::Error::RegionNotFound(region_name.to_string()));
        }
        let first = {
            let region = self.get_region_mut(region_name).expect("checked above");
            insert(region, tokens)
        };
        match first {
            Ok(()) => {
                self.current_tokens = self.calculate_tokens();
                Ok(())
            }
            Err(leviath_core::Error::TokenBudgetExceeded { .. })
                if self.try_custom_overflow(region_name, tokens) =>
            {
                let region = self.get_region_mut(region_name).expect("checked above");
                let retried = insert(region, tokens);
                self.current_tokens = self.calculate_tokens();
                retried
            }
            Err(e) => Err(e),
        }
    }

    /// Calculate current token usage across all regions.
    pub fn calculate_tokens(&self) -> usize {
        self.regions.iter().map(|r| r.current_tokens).sum()
    }

    /// Check if the context window needs eviction.
    pub fn needs_eviction(&self, threshold: f32) -> bool {
        let usage_ratio = self.current_tokens as f32 / self.max_tokens as f32;
        usage_ratio >= threshold
    }

    /// Execute eviction cascade to free up space.
    ///
    /// Returns an `EvictionResult` with tokens freed and any regions that need
    /// LLM-based compaction. The caller is responsible for performing compaction
    /// on the listed regions (since it requires async LLM access).
    pub fn try_evict(&mut self, target_free_tokens: usize) -> leviath_core::Result<EvictionResult> {
        use leviath_core::RegionKind;

        let initial_tokens = self.current_tokens;

        // A region under `admission = "reject"` is exempt from every phase
        // below. Refusing writes to protect what a region holds would mean
        // nothing if the window-level cascade could take the same entries a
        // moment later - `reject` would only change which code did the silent
        // dropping. The agent releases from these, or nothing does.
        let evictable = |r: &Region| {
            r.admission != leviath_core::region::Admission::Reject
                && matches!(
                    r.kind,
                    RegionKind::Clearable
                        | RegionKind::Temporary
                        | RegionKind::Custom {
                            persistent: false,
                            ..
                        }
                )
        };

        // Check if we have any evictable regions
        let has_evictable = self.regions.iter().any(evictable);

        if !has_evictable {
            tracing::warn!(
                "Context window has no Clearable or Temporary regions. \
                 This may be intentional, but usually indicates a configuration error."
            );
        }

        // Phase 1: Clear Clearable regions (all-or-nothing)
        for region in &mut self.regions {
            if matches!(region.kind, RegionKind::Clearable)
                && region.admission != leviath_core::region::Admission::Reject
                && !region.content.is_empty()
            {
                let freed = region.current_tokens;
                region.clear();
                self.current_tokens -= freed;
                tracing::debug!(
                    region = %region.name,
                    tokens_freed = freed,
                    "Cleared Clearable region (all-or-nothing)"
                );

                if self.max_tokens.saturating_sub(self.current_tokens) >= target_free_tokens {
                    return Ok(EvictionResult {
                        tokens_freed: initial_tokens - self.current_tokens,
                        needs_compaction: Vec::new(),
                    });
                }
            }
        }

        // Phase 1.5: Give each non-persistent custom region's on_overflow
        // hook first say over what IT loses, before the indiscriminate
        // oldest-first cascade below. A script that keeps errors and drops
        // successes only works if it runs before oldest-first does. Hook
        // absent/failing/insufficient → phase 2 makes the guaranteed
        // progress.
        let mut custom_freed = 0usize;
        for i in 0..self.regions.len() {
            let needed = target_free_tokens
                .saturating_sub(self.max_tokens.saturating_sub(self.current_tokens));
            if needed == 0 {
                break;
            }
            let region = &self.regions[i];
            if !matches!(
                region.kind,
                RegionKind::Custom {
                    persistent: false,
                    ..
                }
            ) || region.admission == leviath_core::region::Admission::Reject
                || region.content.is_empty()
            {
                continue;
            }
            let Some(script) = self.custom_script_for(&region.name.clone()) else {
                continue;
            };
            if !script.has_on_overflow() {
                continue;
            }
            let freed = crate::custom_region::apply_overflow(&script, &mut self.regions[i], needed);
            self.current_tokens = self.current_tokens.saturating_sub(freed);
            custom_freed += freed;
            if freed > 0 {
                tracing::debug!(
                    region = %self.regions[i].name,
                    tokens_freed = freed,
                    "custom region's on_overflow chose its own evictions"
                );
            }
        }
        // Return early ONLY when a script's own drops satisfied the target -
        // otherwise phase 2 would immediately evict one more entry (it checks
        // the target *after* each eviction), overriding the script's
        // retention choice. Windows with no custom drops (custom_freed == 0)
        // fall through with phase 2's pre-existing behavior, byte-identical.
        if custom_freed > 0
            && self.max_tokens.saturating_sub(self.current_tokens) >= target_free_tokens
        {
            return Ok(EvictionResult {
                tokens_freed: initial_tokens - self.current_tokens,
                needs_compaction: Vec::new(),
            });
        }

        // Phase 2: Evict from Temporary regions (oldest first, one at a time).
        // Non-persistent Custom regions join this phase: their script's
        // on_overflow hook (when present) has already had its say in phase
        // 1.5; oldest-first is the guaranteed-progress fallback.
        loop {
            let mut evicted_any = false;

            for region in &mut self.regions {
                if matches!(
                    region.kind,
                    RegionKind::Temporary
                        | RegionKind::Custom {
                            persistent: false,
                            ..
                        }
                ) && region.admission != leviath_core::region::Admission::Reject
                    && let Some(entry) = region.remove_oldest()
                {
                    let freed = entry.tokens;
                    self.current_tokens -= freed;
                    evicted_any = true;

                    tracing::debug!(
                        region = %region.name,
                        tokens_freed = freed,
                        "Evicted temporary region entry (oldest first)"
                    );

                    if self.max_tokens.saturating_sub(self.current_tokens) >= target_free_tokens {
                        return Ok(EvictionResult {
                            tokens_freed: initial_tokens - self.current_tokens,
                            needs_compaction: Vec::new(),
                        });
                    }
                }
            }

            if !evicted_any {
                break;
            }
        }

        // Phase 3: If still need space, identify Compacting regions that need compaction
        let mut needs_compaction = Vec::new();
        if self.max_tokens.saturating_sub(self.current_tokens) < target_free_tokens {
            for region in &self.regions {
                if region.needs_compaction() {
                    needs_compaction.push(region.name.clone());
                }
            }
        }

        // Phase 4: SlidingWindow regions are NEVER reduced
        // Phase 5: Pinned and CompactHistory regions are NEVER touched

        // Check for pinned regions over budget
        let pinned_tokens: usize = self
            .regions
            .iter()
            .filter(|r| {
                matches!(
                    r.kind,
                    RegionKind::Pinned
                        | RegionKind::CompactHistory { .. }
                        | RegionKind::Custom {
                            persistent: true,
                            ..
                        }
                )
            })
            .map(|r| r.current_tokens)
            .sum();

        if pinned_tokens > self.max_tokens {
            return Err(leviath_core::Error::PinnedRegionsOverBudget {
                pinned_tokens,
                total_budget: self.max_tokens,
            });
        }

        Ok(EvictionResult {
            tokens_freed: initial_tokens - self.current_tokens,
            needs_compaction,
        })
    }

    /// Result of assembling the context window into system blocks + messages.
    ///
    /// System-bound regions become `system_blocks`; the messages region
    /// becomes `messages` with proper typed entries (no text-prefix parsing).
    ///
    /// Thin wrapper over [`assemble_with_meta`](Self::assemble_with_meta) with
    /// no stage metadata - custom-region scripts see empty stage fields.
    pub fn assemble(&self) -> AssembledContext {
        self.assemble_with_meta(&crate::custom_region::AssembleMeta::default())
    }

    /// [`assemble`](Self::assemble) with stage metadata for custom-region
    /// `render(ctx)` hooks (stage name, per-stage iteration count, model).
    /// The inference path (`build_request`) threads real values; other
    /// callers use the default.
    pub fn assemble_with_meta(
        &self,
        meta: &crate::custom_region::AssembleMeta,
    ) -> AssembledContext {
        use leviath_core::{CacheHint, EntryKind};

        let mut system_blocks = Vec::new();
        let mut messages: Vec<leviath_providers::Message> = Vec::new();
        // One entry per `UntilChanged` system block, in assembly order, holding
        // the newest entry timestamp of the region that produced it. Feeds the
        // cache-breakpoint split after the sort.
        let mut volatile_recency: Vec<i64> = Vec::new();

        for region in &self.regions {
            // A region this stage does not attend to is held but not shown.
            // Skipped here rather than dropped from the window, so a later
            // stage that declares it again gets its contents back.
            if self.hidden.contains(&region.name) {
                continue;
            }
            // Custom regions render even when empty - a script may emit
            // static scaffolding. Every other kind skips an empty region.
            let is_custom = matches!(region.kind, leviath_core::RegionKind::Custom { .. });
            if region.content.is_empty() && !is_custom {
                continue;
            }

            // Where this region's system blocks begin, so the recency mapping
            // below covers exactly the blocks this region adds.
            let first_new_block = system_blocks.len();

            match &region.kind {
                // System-level content → system blocks
                leviath_core::RegionKind::Pinned => {
                    let body = region
                        .content
                        .iter()
                        .map(|e| e.content.clone())
                        .collect::<Vec<_>>();
                    push_chunked(&mut system_blocks, region, &body, CacheHint::Always);
                }
                // A checklist renders as instruction rather than history: one
                // stable block, open items first, so what is left to do is at
                // the top of what the model reads every turn. The whole value
                // of the state being real is that this block is derived from
                // it rather than from whatever prose the model last wrote.
                leviath_core::RegionKind::Checklist => {
                    let body = region.render_checklist();
                    if !body.is_empty() {
                        system_blocks.push(leviath_providers::SystemBlock {
                            text: labelled(region, &body),
                            cache_hint: CacheHint::UntilChanged,
                            breakpoint_eligible: true,
                        });
                    }
                }
                leviath_core::RegionKind::CompactHistory { .. } => {
                    let body = region
                        .content
                        .iter()
                        .map(|e| e.content.clone())
                        .collect::<Vec<_>>();
                    // Not `Always`, though nothing ever evicts from here.
                    // `Always` is the most-stable tier and sorts ahead of
                    // everything else, and this region gains an entry every
                    // time compaction fires - so tagging it stable put a
                    // *changing* block in front of genuinely immutable content
                    // and invalidated the whole prefix behind it. Which content
                    // survived depended on the order the regions happened to be
                    // declared in: measured with the history region declared
                    // first, one compaction took the cacheable prefix from
                    // 2,502 tokens to zero, pinned instructions included, and
                    // declared second it kept them (issue #474).
                    //
                    // `UntilChanged` says what is true of it - held still until
                    // compaction moves it - and sorts it behind the pinned
                    // content it must not poison.
                    push_chunked(&mut system_blocks, region, &body, CacheHint::UntilChanged);
                }

                // Messages region → Vec<Message> with proper typed entries.
                // Consecutive ToolResult entries are merged into a single user
                // message with multiple tool_result content blocks (required by
                // Anthropic: one assistant tool_use msg → one user tool_result msg).
                leviath_core::RegionKind::SlidingWindow { .. } => {
                    let mut pending_tool_results: Vec<leviath_providers::ContentBlock> = Vec::new();

                    for entry in &region.content {
                        // Flush any pending tool results when we hit a non-ToolResult entry
                        if !matches!(entry.kind, EntryKind::ToolResult { .. })
                            && !pending_tool_results.is_empty()
                        {
                            messages.push(leviath_providers::Message {
                                role: "user".to_string(),
                                content: leviath_providers::MessageContent::Blocks(std::mem::take(
                                    &mut pending_tool_results,
                                )),
                                cache_breakpoint: false,
                            });
                        }

                        match &entry.kind {
                            EntryKind::UserMessage => {
                                messages.push(leviath_providers::Message {
                                    role: "user".to_string(),
                                    content: entry.content.clone().into(),
                                    cache_breakpoint: false,
                                });
                            }
                            EntryKind::AssistantTurn { tool_calls } => {
                                if tool_calls.is_empty() {
                                    messages.push(leviath_providers::Message {
                                        role: "assistant".to_string(),
                                        content: entry.content.clone().into(),
                                        cache_breakpoint: false,
                                    });
                                } else {
                                    let mut blocks = Vec::new();
                                    if !entry.content.is_empty() {
                                        blocks.push(leviath_providers::ContentBlock::Text {
                                            text: entry.content.clone(),
                                        });
                                    }
                                    for tc in tool_calls {
                                        blocks.push(leviath_providers::ContentBlock::ToolUse {
                                            id: tc.id.clone(),
                                            name: tc.name.clone(),
                                            input: tc.arguments.clone(),
                                            thought_signature: tc.thought_signature.clone(),
                                        });
                                    }
                                    messages.push(leviath_providers::Message {
                                        role: "assistant".to_string(),
                                        content: leviath_providers::MessageContent::Blocks(blocks),
                                        cache_breakpoint: false,
                                    });
                                }
                            }
                            EntryKind::ToolResult {
                                tool_call_id,
                                is_error,
                                ..
                            } => {
                                // Accumulate - will be flushed on next non-ToolResult or end
                                pending_tool_results.push(
                                    leviath_providers::ContentBlock::ToolResult {
                                        tool_use_id: tool_call_id.clone(),
                                        content: entry.content.clone(),
                                        is_error: *is_error,
                                    },
                                );
                            }
                            EntryKind::Text => {
                                let trimmed = entry.content.trim();
                                if let Some(rest) = trimmed.strip_prefix("Assistant: ") {
                                    messages.push(leviath_providers::Message {
                                        role: "assistant".to_string(),
                                        content: rest.to_string().into(),
                                        cache_breakpoint: false,
                                    });
                                } else if let Some(rest) = trimmed.strip_prefix("User: ") {
                                    messages.push(leviath_providers::Message {
                                        role: "user".to_string(),
                                        content: rest.to_string().into(),
                                        cache_breakpoint: false,
                                    });
                                } else {
                                    messages.push(leviath_providers::Message {
                                        role: "user".to_string(),
                                        content: entry.content.clone().into(),
                                        cache_breakpoint: false,
                                    });
                                }
                            }
                        }
                    }

                    // Flush any remaining tool results at the end of the region
                    if !pending_tool_results.is_empty() {
                        messages.push(leviath_providers::Message {
                            role: "user".to_string(),
                            content: leviath_providers::MessageContent::Blocks(std::mem::take(
                                &mut pending_tool_results,
                            )),
                            cache_breakpoint: false,
                        });
                    }
                }

                // Compacting / Temporary / Clearable → system blocks
                leviath_core::RegionKind::Compacting { .. } => {
                    let entries = region
                        .content
                        .iter()
                        .map(|e| e.content.clone())
                        .collect::<Vec<_>>();
                    let chunks = append_only_chunks(&entries, CACHE_CHUNK_TOKENS);
                    for (index, chunk) in chunks.iter().enumerate() {
                        let text = match index {
                            0 => format!("[{}]:\n{}", region.name, chunk),
                            _ => format!("[{} continued]:\n{}", region.name, chunk),
                        };
                        system_blocks.push(leviath_providers::SystemBlock {
                            text,
                            cache_hint: CacheHint::UntilChanged,
                            breakpoint_eligible: true,
                        });
                    }
                }
                leviath_core::RegionKind::Temporary => {
                    let text = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    system_blocks.push(leviath_providers::SystemBlock {
                        text: format!("[{}]:\n{}", region.name, text),
                        cache_hint: CacheHint::Never,
                        breakpoint_eligible: true,
                    });
                }
                leviath_core::RegionKind::Clearable => {
                    let text = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    system_blocks.push(leviath_providers::SystemBlock {
                        text: format!("[{}]:\n{}", region.name, text),
                        cache_hint: CacheHint::Never,
                        breakpoint_eligible: true,
                    });
                }

                // Custom (script-backed) regions render through their Rhai
                // hook; a missing script or any hook failure falls back to
                // the Temporary-style block inside `render_custom_region`,
                // so a custom region is never silently dropped.
                leviath_core::RegionKind::Custom { script, persistent } => {
                    crate::custom_region::render_custom_region(
                        crate::custom_region::RegionRender {
                            region,
                            script: self.region_scripts.get(script),
                            persistent: *persistent,
                            meta,
                            window_current: self.current_tokens,
                            window_max: self.max_tokens,
                        },
                        crate::custom_region::RenderSink {
                            system_blocks: &mut system_blocks,
                            messages: &mut messages,
                        },
                    );
                }

                // HashMap regions → system blocks with key headers
                leviath_core::RegionKind::HashMap { .. } => {
                    let text = region
                        .content
                        .iter()
                        .map(|e| {
                            if let Some(key) = &e.key {
                                format!("### [{}]\n{}", key, e.content)
                            } else {
                                e.content.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    system_blocks.push(leviath_providers::SystemBlock {
                        text: format!("[{}]:\n{}", region.name, text),
                        cache_hint: CacheHint::UntilChanged,
                        breakpoint_eligible: true,
                    });
                }
            }

            // The newest entry timestamp stands in for "when did this region
            // last change". Regions are append-mostly and timestamps only move
            // forward, so the block holding the newest entry is the one that
            // mutated most recently.
            let mut newest = i64::MIN;
            for entry in &region.content {
                if entry.timestamp > newest {
                    newest = entry.timestamp;
                }
            }
            for block in &system_blocks[first_new_block..] {
                if block.cache_hint == CacheHint::UntilChanged {
                    volatile_recency.push(newest);
                }
            }
        }

        // ── Sort system blocks for optimal prefix caching ────────────────
        //
        // Anthropic caches system content based on prefix matching.
        // Stable blocks (Pinned, CompactHistory) should come first so
        // they form the cacheable prefix, with volatile blocks
        // (Compacting, Temporary, Clearable) after.
        system_blocks.sort_by_key(|block| cache_hint_sort_priority(block.cache_hint));
        // After the sort, because the order is part of what Anthropic matches.
        let system_hash = system_prefix_hash(&system_blocks);
        let block_hashes =
            mark_breakpoint_eligibility(&mut system_blocks, &meta.previous_block_hashes);

        // ── Spend a cache breakpoint on the volatile boundary ────────────
        //
        // Runs strictly after the sort, so it can only change breakpoint
        // metadata: the block order and the block text are already final.
        mark_recently_changed_run(&mut system_blocks, &volatile_recency);

        // ── Sanitize orphaned tool_use / tool_result blocks ──────────────
        //
        // Collect all tool_use IDs from assistant messages and all tool_result
        // IDs from user messages. Strip any that don't have a matching pair.
        let mut tool_use_ids = std::collections::HashSet::new();
        let mut tool_result_ids = std::collections::HashSet::new();

        for msg in &messages {
            if let leviath_providers::MessageContent::Blocks(blocks) = &msg.content {
                for block in blocks {
                    match block {
                        leviath_providers::ContentBlock::ToolUse { id, .. } => {
                            tool_use_ids.insert(id.clone());
                        }
                        leviath_providers::ContentBlock::ToolResult { tool_use_id, .. } => {
                            tool_result_ids.insert(tool_use_id.clone());
                        }
                        _ => {}
                    }
                }
            }
        }

        let orphaned_tool_uses: std::collections::HashSet<_> =
            tool_use_ids.difference(&tool_result_ids).cloned().collect();
        let orphaned_tool_results: std::collections::HashSet<_> =
            tool_result_ids.difference(&tool_use_ids).cloned().collect();

        if !orphaned_tool_uses.is_empty() || !orphaned_tool_results.is_empty() {
            tracing::warn!(
                orphaned_tool_uses = orphaned_tool_uses.len(),
                orphaned_tool_results = orphaned_tool_results.len(),
                "Stripping orphaned tool_use/tool_result blocks from assembled context"
            );

            messages = messages
                .into_iter()
                .filter_map(|msg| {
                    if let leviath_providers::MessageContent::Blocks(blocks) = &msg.content {
                        let filtered: Vec<_> = blocks
                            .iter()
                            .filter(|block| match block {
                                leviath_providers::ContentBlock::ToolUse { id, .. } => {
                                    !orphaned_tool_uses.contains(id)
                                }
                                leviath_providers::ContentBlock::ToolResult {
                                    tool_use_id, ..
                                } => !orphaned_tool_results.contains(tool_use_id),
                                _ => true,
                            })
                            .cloned()
                            .collect();

                        if filtered.is_empty() {
                            // No content left - drop this message entirely
                            None
                        } else {
                            Some(leviath_providers::Message {
                                role: msg.role.clone(),
                                content: leviath_providers::MessageContent::Blocks(filtered),
                                cache_breakpoint: msg.cache_breakpoint,
                            })
                        }
                    } else {
                        Some(msg)
                    }
                })
                .collect();
        }

        // ── Set cache breakpoints on stable message prefix ──────────────
        //
        // In an iterative inference loop, only the last few messages change
        // each iteration (new assistant turn + tool results). Everything
        // before is stable across iterations and benefits from Anthropic's
        // prompt caching. We place a cache breakpoint near the end of the
        // stable prefix to maximize cache hits.
        //
        // Anthropic allows up to 4 breakpoints. Exactly 1 lands on the
        // messages; the system blocks get theirs from their cache hints, and
        // [`MAX_SYSTEM_CACHE_RUNS`] caps those at 3 so this one always fits.
        // Place it on the 4th-from-last message to give a buffer for the
        // new messages added each iteration (typically 2-3).
        //
        // Skipped entirely when the system prefix moved since the last request.
        // Anthropic caches by prefix, so this breakpoint's entry covers every
        // system block as well as the messages before it - and a prefix that
        // changed has already invalidated that entry before it could be read.
        // The 1.25x write is still charged. Measured on a run whose bulk region
        // grew every turn: 3.3M cache-write tokens against 267k reads, a 0.074
        // hit rate, for a prefix that was never going to match. Sending the
        // churn at the base rate instead is the same request for less money.
        //
        // Re-armed the moment the prefix settles, so a steady-state run keeps
        // the caching it was always getting.
        let prefix_moved = meta
            .previous_system_hash
            .is_some_and(|previous| previous != system_hash);
        if prefix_moved {
            // Nothing to place: the entry could not be read back.
        } else if messages.len() >= 5 {
            let bp_idx = messages.len() - 4;
            messages[bp_idx].cache_breakpoint = true;
        } else if messages.len() >= 2 {
            // Small conversation - cache at least the first message
            messages[0].cache_breakpoint = true;
        }

        // Ensure there's at least one user message
        if !messages.iter().any(|m| m.role == "user") {
            messages.push(leviath_providers::Message {
                role: "user".to_string(),
                content: "Begin.".into(),
                cache_breakpoint: false,
            });
        }

        // The conversation must END with a user message: providers reject a
        // request that ends on an assistant turn as an (unsupported) prefill
        // ("This model does not support assistant message prefill"). After a
        // stage transition that carries the conversation, the last message is
        // the previous stage's final assistant turn - hand the turn back to the
        // model with a minimal nudge so it acts on the new stage's instructions.
        if messages.last().map(|m| m.role.as_str()) == Some("assistant") {
            messages.push(leviath_providers::Message {
                role: "user".to_string(),
                content: "Continue.".into(),
                cache_breakpoint: false,
            });
        }

        AssembledContext {
            block_hashes,
            system_blocks,
            messages,
            system_hash,
        }
    }

    /// Enable taint tracking on all regions in this context window.
    pub fn enable_taint_tracking(&mut self) {
        for region in &mut self.regions {
            region.enable_taint_tracking();
        }
    }

    /// Add tainted content to a specific region.
    pub fn add_tainted_to_region(
        &mut self,
        region_name: &str,
        content: String,
        tokens: usize,
        taint_level: leviath_core::TaintLevel,
    ) -> leviath_core::Result<()> {
        let Some((content, tokens)) =
            self.on_write_outcome(region_name, content, tokens, &leviath_core::EntryKind::Text)
        else {
            return Ok(());
        };
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            region.add_tainted_entry(content.clone(), tokens, taint_level)
        })
    }

    /// Add a typed entry to a region with a specific taint level.
    ///
    /// The typed+tainted counterpart of [`add_typed_entry`](Self::add_typed_entry)
    /// and [`add_tainted_to_region`](Self::add_tainted_to_region): the entry keeps
    /// its `EntryKind` (so turn-group eviction stays intact) while contributing
    /// the given taint level (so the taint gate sees sensitive tool output).
    pub fn add_typed_tainted_to_region(
        &mut self,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
        taint_level: leviath_core::TaintLevel,
    ) -> leviath_core::Result<()> {
        let Some((content, tokens)) = self.on_write_outcome(region_name, content, tokens, &kind)
        else {
            return Ok(());
        };
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            region.add_typed_tainted_entry(content.clone(), tokens, kind.clone(), taint_level)
        })
    }

    /// Get the overall taint level (max across all regions).
    /// Returns None if no region has taint tracking enabled.
    pub fn overall_taint(&self) -> Option<leviath_core::TaintLevel> {
        let mut max_taint = None;
        for region in &self.regions {
            if let Some(level) = region.taint_level() {
                max_taint = Some(match max_taint {
                    Some(current) => level.max(current),
                    None => level,
                });
            }
        }
        max_taint
    }

    /// Get a summary of taint levels across all regions (for dashboard/audit).
    pub fn taint_summary(&self) -> Vec<(String, leviath_core::TaintLevel)> {
        self.regions
            .iter()
            .filter_map(|r| r.taint_level().map(|t| (r.name.clone(), t)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::{CacheHint, Region, RegionKind};
    use leviath_providers::SystemBlock;

    /// A system block carrying only the hint under test.
    fn block(hint: CacheHint) -> SystemBlock {
        SystemBlock {
            text: "x".to_string(),
            cache_hint: hint,
            breakpoint_eligible: true,
        }
    }

    /// The number of cache breakpoints the Anthropic provider would place on
    /// these system blocks: one per contiguous run of same-hint cacheable
    /// blocks. Mirrors `system_cache_breakpoints` so the ceiling can be
    /// asserted from this side of the crate boundary.
    fn provider_system_breakpoints(blocks: &[SystemBlock]) -> usize {
        let mut runs = 0;
        for (index, block) in blocks.iter().enumerate() {
            let hint = block.cache_hint;
            if hint != CacheHint::Never && blocks.get(index + 1).map(|b| b.cache_hint) != Some(hint)
            {
                runs += 1;
            }
        }
        runs
    }

    /// A region holding one entry stamped at `timestamp`.
    fn stamped_region(name: &str, kind: RegionKind, timestamp: i64) -> Region {
        let mut region = Region::new(name.to_string(), kind, 10_000);
        region.add_entry(format!("{name} contents"), 10).unwrap();
        region.content[0].timestamp = timestamp;
        region
    }

    fn hashmap_region(name: &str, timestamp: i64) -> Region {
        stamped_region(
            name,
            RegionKind::HashMap {
                max_entries: Some(16),
            },
            timestamp,
        )
    }

    #[test]
    fn recently_changed_sorts_with_until_changed() {
        assert_eq!(
            cache_hint_sort_priority(CacheHint::RecentlyChanged),
            cache_hint_sort_priority(CacheHint::UntilChanged)
        );
        assert_eq!(cache_hint_sort_priority(CacheHint::RecentlyChanged), 2);
    }

    #[test]
    fn mark_recently_changed_run_splits_at_the_newest_block() {
        // Always, three volatile blocks, and a trailing uncacheable block. The
        // third volatile block is the one that just changed.
        let mut blocks = vec![
            block(CacheHint::Always),
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
            block(CacheHint::Never),
        ];
        mark_recently_changed_run(&mut blocks, &[10, 20, 90]);

        let hints: Vec<CacheHint> = blocks.iter().map(|b| b.cache_hint).collect();
        assert_eq!(
            hints,
            vec![
                CacheHint::Always,
                CacheHint::UntilChanged,
                CacheHint::UntilChanged,
                CacheHint::RecentlyChanged,
                CacheHint::Never,
            ]
        );
        // Always run, stable volatile head, changed volatile tail. The
        // uncacheable trailing block claims nothing.
        assert_eq!(provider_system_breakpoints(&blocks), 3);
    }

    #[test]
    fn mark_recently_changed_run_retags_every_block_after_the_boundary() {
        let mut blocks = vec![
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
        ];
        mark_recently_changed_run(&mut blocks, &[1, 7, 5]);

        let hints: Vec<CacheHint> = blocks.iter().map(|b| b.cache_hint).collect();
        assert_eq!(
            hints,
            vec![
                CacheHint::UntilChanged,
                CacheHint::RecentlyChanged,
                CacheHint::RecentlyChanged,
            ]
        );
    }

    #[test]
    fn mark_recently_changed_run_ties_resolve_to_the_earliest_block() {
        // Two regions written in the same second both changed, so the boundary
        // belongs ahead of the earlier one.
        let mut blocks = vec![
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
        ];
        mark_recently_changed_run(&mut blocks, &[1, 9, 9]);

        assert_eq!(blocks[0].cache_hint, CacheHint::UntilChanged);
        assert_eq!(blocks[1].cache_hint, CacheHint::RecentlyChanged);
        assert_eq!(blocks[2].cache_hint, CacheHint::RecentlyChanged);
    }

    #[test]
    fn mark_recently_changed_run_leaves_a_headless_tier_alone() {
        // The newest block is already first, so there is no stable head to put
        // behind a breakpoint.
        let mut blocks = vec![
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
        ];
        mark_recently_changed_run(&mut blocks, &[42, 1]);
        assert!(
            blocks
                .iter()
                .all(|b| b.cache_hint == CacheHint::UntilChanged)
        );
    }

    #[test]
    fn mark_recently_changed_run_leaves_a_tierless_prompt_alone() {
        // No volatile blocks at all means an empty recency list.
        let mut blocks = vec![block(CacheHint::Always), block(CacheHint::Never)];
        mark_recently_changed_run(&mut blocks, &[]);
        assert_eq!(blocks[0].cache_hint, CacheHint::Always);
        assert_eq!(blocks[1].cache_hint, CacheHint::Never);
    }

    #[test]
    fn mark_recently_changed_run_refuses_when_the_run_budget_is_full() {
        // Always, SlidingPrefix and UntilChanged already claim three
        // breakpoints. Splitting further would take the messages' one.
        let mut blocks = vec![
            block(CacheHint::Always),
            block(CacheHint::SlidingPrefix {
                stable_fraction: 0.75,
            }),
            block(CacheHint::UntilChanged),
            block(CacheHint::UntilChanged),
        ];
        assert_eq!(provider_system_breakpoints(&blocks), 3);

        mark_recently_changed_run(&mut blocks, &[1, 99]);

        assert_eq!(blocks[2].cache_hint, CacheHint::UntilChanged);
        assert_eq!(blocks[3].cache_hint, CacheHint::UntilChanged);
        assert_eq!(provider_system_breakpoints(&blocks), 3);
    }

    #[test]
    fn assemble_marks_the_volatile_tail_without_moving_any_block() {
        let mut window = ContextWindow::new(100_000);
        window.add_region(stamped_region("brief", RegionKind::Pinned, 1));
        window.add_region(hashmap_region("spec", 100));
        window.add_region(hashmap_region("data_preview", 200));
        window.add_region(hashmap_region("results", 300));
        window.add_region(stamped_region("scratch", RegionKind::Temporary, 400));

        let assembled = window.assemble();

        // Declaration order inside each tier is exactly what it was: the split
        // only rewrites cache hints.
        let texts: Vec<&str> = assembled
            .system_blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        assert!(texts[0].contains("brief contents"));
        assert!(texts[1].starts_with("[spec]:"));
        assert!(texts[2].starts_with("[data_preview]:"));
        assert!(texts[3].starts_with("[results]:"));
        assert!(texts[4].starts_with("[scratch]:"));

        let hints: Vec<CacheHint> = assembled
            .system_blocks
            .iter()
            .map(|b| b.cache_hint)
            .collect();
        assert_eq!(
            hints,
            vec![
                CacheHint::Always,
                CacheHint::UntilChanged,
                CacheHint::UntilChanged,
                CacheHint::RecentlyChanged,
                CacheHint::Never,
            ]
        );
    }

    #[test]
    fn assemble_leaves_a_flat_window_untouched() {
        // One pinned block and one working region: the flat shape that already
        // caches well keeps a single volatile run and a single breakpoint.
        let mut window = ContextWindow::new(100_000);
        window.add_region(stamped_region("brief", RegionKind::Pinned, 1));
        window.add_region(hashmap_region("results", 300));

        let assembled = window.assemble();
        let hints: Vec<CacheHint> = assembled
            .system_blocks
            .iter()
            .map(|b| b.cache_hint)
            .collect();
        assert_eq!(hints, vec![CacheHint::Always, CacheHint::UntilChanged]);
        assert_eq!(provider_system_breakpoints(&assembled.system_blocks), 2);
    }

    #[test]
    fn assemble_stays_within_four_cache_breakpoints() {
        let mut window = ContextWindow::new(1_000_000);
        window.add_region(stamped_region("brief", RegionKind::Pinned, 1));
        window.add_region(stamped_region(
            "history",
            RegionKind::CompactHistory {
                source_region: "conversation".to_string(),
            },
            2,
        ));
        for (index, name) in ["spec", "data_preview", "scripts", "results"]
            .iter()
            .enumerate()
        {
            window.add_region(hashmap_region(name, 100 + index as i64));
        }
        window.add_region(stamped_region("scratch", RegionKind::Clearable, 500));

        let mut conversation = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            100_000,
        );
        for turn in 0..8 {
            conversation
                .add_entry(format!("User: turn {turn}"), 10)
                .unwrap();
        }
        window.add_region(conversation);

        let assembled = window.assemble();
        let system = provider_system_breakpoints(&assembled.system_blocks);
        let message = assembled
            .messages
            .iter()
            .filter(|m| m.cache_breakpoint)
            .count();

        assert!(
            system <= super::block_cache::MAX_SYSTEM_CACHE_RUNS,
            "system runs: {system}"
        );
        assert_eq!(message, 1);
        let total = system + message;
        assert!(total <= 4, "total breakpoints: {total}");
    }

    /// Whether a compaction destroys the cacheable prefix must not depend on
    /// the order regions happen to be declared in.
    ///
    /// A compact-history region gains an entry every time compaction fires. It
    /// used to be tagged `Always` - the most-stable tier - so it sorted ahead of
    /// genuinely immutable pinned content, and a compaction invalidated
    /// everything behind it. Declared before the pinned region that cost the
    /// whole prefix; declared after, it cost nothing. Same content, same run.
    #[test]
    fn a_compaction_does_not_depend_on_region_declaration_order() {
        fn cacheable_after_a_compaction(history_first: bool) -> usize {
            let mut window = ContextWindow::new(1_000_000);
            let history = || {
                Region::new(
                    "notes_history".to_string(),
                    RegionKind::CompactHistory {
                        source_region: "notes".to_string(),
                    },
                    100_000,
                )
            };
            if history_first {
                window.add_region(history());
            }
            let mut task = Region::new("task".to_string(), RegionKind::Pinned, 40_000);
            task.add_entry("stable instructions ".repeat(300), 1500)
                .expect("fits");
            window.add_region(task);
            if !history_first {
                window.add_region(history());
            }

            // One settled request, then a compaction: the history gains a
            // summary, which is what moves the prefix.
            let first = window.assemble();
            window
                .add_to_region("notes_history", "a summary".to_string(), 5)
                .expect("fits");
            let second = window.assemble_with_meta(&crate::custom_region::AssembleMeta {
                stage_name: "work".to_string(),
                stage_iterations: 1,
                model: "m".to_string(),
                previous_system_hash: Some(first.system_hash),
                previous_block_hashes: first.block_hashes.clone(),
            });
            second
                .system_blocks
                .iter()
                .filter(|b| b.breakpoint_eligible)
                .map(|b| leviath_core::estimate_tokens(&b.text))
                .sum()
        }

        let declared_first = cacheable_after_a_compaction(true);
        let declared_second = cacheable_after_a_compaction(false);
        assert_eq!(
            declared_first, declared_second,
            "declaration order changed what survives a compaction"
        );
        assert!(
            declared_first > 1000,
            "the pinned instructions should survive a compaction, got {declared_first}"
        );
    }

    /// A compacting region large enough to span chunks says which region each
    /// continuation belongs to, and keeps every entry.
    #[test]
    fn a_compacting_region_labels_its_continuations() {
        let mut window = ContextWindow::new(2_000_000);
        window.add_region(Region::new(
            "history".to_string(),
            RegionKind::Compacting {
                threshold_tokens: usize::MAX,
            },
            1_000_000,
        ));
        for i in 0..20 {
            window
                .add_to_region("history", format!("{i}:{}", "word ".repeat(200)), 250)
                .expect("fits");
        }

        let blocks = window.assemble().system_blocks;
        assert!(blocks.len() > 1, "the fixture is meant to span chunks");
        assert!(blocks[0].text.starts_with("[history]:"));
        for block in &blocks[1..] {
            assert!(
                block.text.starts_with("[history continued]:"),
                "{:.40}",
                block.text
            );
        }
        let whole = blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        for i in 0..20 {
            assert!(whole.contains(&format!("{i}:")), "entry {i} went missing");
        }
    }

    #[test]
    fn a_pinned_region_is_labelled_with_its_own_name() {
        let mut window = ContextWindow::new(10_000);
        let mut region = Region::new("sources_index".to_string(), RegionKind::Pinned, 1000);
        region
            .add_entry("[1] RFC 9110 - https://example".to_string(), 10)
            .expect("fits");
        window.add_region(region);

        let assembled = window.assemble();
        assert_eq!(
            assembled.system_blocks[0].text, "## sources_index\n[1] RFC 9110 - https://example",
            "the name the agent would pass to context_write"
        );
    }

    /// A description is opt-in, and absent costs nothing - which is the point.
    /// Most regions are named well enough that a sentence would only spend
    /// tokens.
    #[test]
    fn a_description_reaches_the_model_only_when_the_blueprint_opts_in() {
        let mut window = ContextWindow::new(10_000);

        let mut shown = Region::new("scratch".to_string(), RegionKind::Pinned, 1000);
        shown.description = Some("Working notes; cleared between stages.".to_string());
        shown.describe_in_prompt = true;
        shown.add_entry("half an idea".to_string(), 5).unwrap();
        window.add_region(shown);

        // Described for whoever maintains the blueprint, not for the model. The
        // sentence is still on the region - `lev dash` and the blueprint API
        // read it - and it costs nothing per turn.
        let mut documented = Region::new("sources".to_string(), RegionKind::Pinned, 1000);
        documented.description = Some("One line per source actually used.".to_string());
        documented.add_entry("[1] RFC 9110".to_string(), 5).unwrap();
        window.add_region(documented);

        let mut plain = Region::new("task".to_string(), RegionKind::Pinned, 1000);
        plain.add_entry("do the thing".to_string(), 5).unwrap();
        window.add_region(plain);

        let assembled = window.assemble();
        let texts: Vec<&str> = assembled
            .system_blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "## scratch\nWorking notes; cleared between stages.\n\nhalf an idea",
                "## sources\n[1] RFC 9110",
                "## task\ndo the thing",
            ]
        );
    }

    /// The opt-in with nothing to show is the same as no opt-in: a region that
    /// asks for its description in the prompt and has none must not render a
    /// blank line where the sentence would go.
    #[test]
    fn opting_in_with_no_description_changes_nothing() {
        let mut window = ContextWindow::new(10_000);
        let mut region = Region::new("task".to_string(), RegionKind::Pinned, 1000);
        region.describe_in_prompt = true;
        region.add_entry("do the thing".to_string(), 5).unwrap();
        window.add_region(region);

        assert_eq!(
            window.assemble().system_blocks[0].text,
            "## task\ndo the thing"
        );
    }

    /// What the labelling actually costs, held to a number rather than a
    /// feeling: a heading is a handful of tokens against a region's contents,
    /// and it is charged once per region rather than per entry.
    #[test]
    fn labelling_costs_a_few_tokens_per_region_not_per_entry() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new("findings".to_string(), RegionKind::Pinned, 50_000);
        for i in 0..20 {
            region
                .add_entry(format!("finding number {i}, with some substance to it"), 12)
                .expect("fits");
        }
        window.add_region(region);

        let assembled = window.assemble();
        let text = &assembled.system_blocks[0].text;
        let header = "## findings\n";
        assert!(text.starts_with(header), "{text:.60}");
        assert_eq!(
            leviath_core::estimate_tokens(header),
            3,
            "one short heading for the whole region, however many entries it holds"
        );
    }

    /// A blueprint declares its pinned regions up front and most are empty for
    /// most of a run - deep-researcher has eight, of which four were empty at
    /// the point it failed against ollama. They contribute no system block,
    /// which is worth pinning: it bounds how many system messages a provider
    /// that counts them actually receives, and how many headings the labelled
    /// prompt carries.
    #[test]
    fn an_empty_pinned_region_assembles_into_no_system_block() {
        let mut window = ContextWindow::new(10_000);
        let mut filled = Region::new("task".to_string(), RegionKind::Pinned, 1000);
        filled
            .add_entry("do the thing".to_string(), 5)
            .expect("fits");
        window.add_region(filled);
        // Declared, never written to - the common case mid-run.
        window.add_region(Region::new("scope".to_string(), RegionKind::Pinned, 1000));
        window.add_region(Region::new("notes".to_string(), RegionKind::Pinned, 1000));

        let assembled = window.assemble();
        let texts: Vec<&str> = assembled
            .system_blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec!["## task\ndo the thing"],
            "only the region with content, and it names itself"
        );
    }
}
