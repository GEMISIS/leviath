//! The agent's context window: what it remembers, and what it forgets first.
//!
//! Every region an agent holds lives here, along with the assembly that turns
//! them into a request and the eviction that keeps them inside a budget. Split
//! out of `components` because it was two thirds of that file on its own, and
//! because "what an agent *is*" and "what an agent *remembers*" are different
//! questions to arrive with.

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
}

/// Sort priority for a system block's cache hint.
///
/// Anthropic caches system content by prefix matching, so the most stable
/// blocks must sort first to form the cacheable prefix. Lower value = earlier.
pub(super) fn cache_hint_sort_priority(hint: leviath_core::CacheHint) -> u8 {
    use leviath_core::CacheHint;
    match hint {
        CacheHint::Always => 0,               // Pinned, CompactHistory - most stable
        CacheHint::SlidingPrefix { .. } => 1, // Partially stable
        CacheHint::UntilChanged => 2,         // Compacting - changes on compaction
        CacheHint::Never => 3,                // Temporary, Clearable - changes every iteration
    }
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
        let Some((content, tokens)) =
            self.on_write_outcome(region_name, content, tokens, &leviath_core::EntryKind::Text)
        else {
            return Ok(()); // the region's script dropped the entry
        };
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            region.add_entry(content.clone(), tokens)
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

        // Check if we have any evictable regions
        let has_evictable = self.regions.iter().any(|r| {
            matches!(
                r.kind,
                RegionKind::Clearable
                    | RegionKind::Temporary
                    | RegionKind::Custom {
                        persistent: false,
                        ..
                    }
            )
        });

        if !has_evictable {
            tracing::warn!(
                "Context window has no Clearable or Temporary regions. \
                 This may be intentional, but usually indicates a configuration error."
            );
        }

        // Phase 1: Clear Clearable regions (all-or-nothing)
        for region in &mut self.regions {
            if matches!(region.kind, RegionKind::Clearable) && !region.content.is_empty() {
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
            ) || region.content.is_empty()
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
                ) && let Some(entry) = region.remove_oldest()
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

            match &region.kind {
                // System-level content → system blocks
                leviath_core::RegionKind::Pinned => {
                    let text = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    system_blocks.push(leviath_providers::SystemBlock {
                        text,
                        cache_hint: CacheHint::Always,
                    });
                }
                // A checklist renders as instruction rather than history: one
                // stable block, open items first, so what is left to do is at
                // the top of what the model reads every turn. The whole value
                // of the state being real is that this block is derived from
                // it rather than from whatever prose the model last wrote.
                leviath_core::RegionKind::Checklist => {
                    let text = region.render_checklist();
                    if !text.is_empty() {
                        system_blocks.push(leviath_providers::SystemBlock {
                            text,
                            cache_hint: CacheHint::UntilChanged,
                        });
                    }
                }
                leviath_core::RegionKind::CompactHistory { .. } => {
                    let text = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    system_blocks.push(leviath_providers::SystemBlock {
                        text,
                        cache_hint: CacheHint::Always,
                    });
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
                    let text = region
                        .content
                        .iter()
                        .map(|e| e.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    system_blocks.push(leviath_providers::SystemBlock {
                        text: format!("[{}]:\n{}", region.name, text),
                        cache_hint: CacheHint::UntilChanged,
                    });
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
                    });
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
        // Anthropic allows up to 4 breakpoints. We use 1 on messages
        // (system blocks already have cache_control via CacheHint).
        // Place it on the 4th-from-last message to give a buffer for the
        // new messages added each iteration (typically 2-3).
        if messages.len() >= 5 {
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
            system_blocks,
            messages,
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
