//! ECS components for agent state and execution.

use bevy_ecs::prelude::*;
use leviath_core::Region;
use serde::{Deserialize, Serialize};

/// Agent execution state component.
///
/// Tracks the current state of an agent's execution, including which stage
/// it's in and iteration counts.
#[derive(Component, Debug, Clone)]
pub struct AgentState {
    /// Unique identifier for this agent instance
    pub agent_id: String,

    /// Current execution stage
    pub current_stage: String,

    /// Number of iterations in current stage
    pub iteration: usize,

    /// Agent status
    pub status: AgentStatus,

    /// IDs of child agents spawned by this agent
    pub spawned_children_ids: Vec<String>,

    /// If set, this agent is blocked waiting for the named child to complete
    pub pending_wait: Option<String>,

    /// Whether the current stage accepts mid-run user messages.
    /// When false, messages stay in the inbox until a stage that accepts them.
    pub accepts_messages: bool,
}

/// Reference to a parent agent, making this agent a sub-agent.
#[derive(Component, Debug, Clone)]
pub struct ParentRef {
    /// Entity of the parent agent
    pub parent_entity: Entity,

    /// Agent ID of the parent
    pub parent_agent_id: String,

    /// Depth in the agent tree (root = 0)
    pub depth: usize,
}

/// Tracks child agents spawned by this agent.
#[derive(Component, Debug, Clone)]
pub struct SubAgentChildren {
    /// Child agent entities
    pub children: Vec<Entity>,

    /// Maximum allowed sub-agent tree depth
    pub max_child_depth: usize,
}

/// Marker: this agent is blocked on an open user interaction (a tool-approval
/// prompt, an `ask_user_*` question, or a plan-approval review).
///
/// Inserted by [`reflect_interaction_status`](crate::pipeline::reflect_interaction_status)
/// when the shared [`InteractionHub`](crate::interaction_hub::InteractionHub)
/// reports a pending request for the agent, and removed when that request
/// clears. It records that the agent's `Waiting` status is interaction-driven,
/// so the reflection is distinct from fan-out waiting
/// ([`FanOutWaiting`](crate::fanout::FanOutWaiting)).
#[derive(Component, Debug, Clone)]
pub struct AwaitingInteraction;

/// Marker: auto-approve this agent's taint-gate blocks instead of raising a
/// gate prompt.
///
/// Set when an agent is launched with `--yolo` (approve everything, run
/// unattended). The taint gate raises a `MultipleChoice` interaction that the
/// tool-policy `--yolo` wildcard does not cover, so without this a headless run -
/// e.g. one driven over the Agent Client Protocol, where no human can answer -
/// would block forever on a gate no one resolves. When present,
/// [`dispatch_tools`](crate::pipeline::dispatch_tools) still evaluates the gate
/// (so an over-cleared call is recorded in the audit trail as
/// [`YoloAutoApprove`](leviath_core::taint::GateDecisionSource::YoloAutoApprove))
/// but auto-approves the call instead of raising a prompt - enforcement is
/// waived, accountability is kept.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct GateAutoApprove;

/// `--yolo`'s counterpart for blueprint-declared interaction points: approve
/// them without opening a prompt.
///
/// A stage-boundary checkpoint (`plan_approval` and friends) blocks on the
/// interaction hub exactly like a tool approval does, so an unattended run
/// would park at the first one forever - the same dead end a blocking tool
/// approval poses for a headless run, reached a different way. When present,
/// [`dispatch_interaction_point`](crate::interaction_points::dispatch_interaction_point)
/// still publishes the document to its region (so the decision is inspectable
/// afterwards) but resolves the point as approved.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct InteractionAutoApprove;

/// Status of an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentStatus {
    /// Agent is idle, ready for tasks
    Idle,

    /// Agent is actively working on a task
    Active,

    /// Agent is waiting for input or external event
    Waiting,

    /// Agent has completed its task
    Complete,

    /// Agent encountered an error
    Error { message: String },

    /// Agent was cancelled by the user or system
    Cancelled,
}

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
fn cache_hint_sort_priority(hint: leviath_core::CacheHint) -> u8 {
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

    /// World-scoped interner handle (cloned from [`crate::ContentInternerRes`]
    /// at spawn). Write paths intern through this handle; domain `Region`
    /// methods never see it.
    interner: leviath_core::ContentInterner,
}

impl ContextWindow {
    /// Create a new context window with the specified budget.
    pub fn new(max_tokens: usize) -> Self {
        Self::with_interner(max_tokens, leviath_core::ContentInterner::new())
    }

    /// Build a window that shares `interner` with other agents in the same World.
    pub fn with_interner(max_tokens: usize, interner: leviath_core::ContentInterner) -> Self {
        Self {
            regions: Vec::new(),
            current_tokens: 0,
            max_tokens,
            region_scripts: std::collections::HashMap::new(),
            interner,
        }
    }

    /// The interner handle this window uses for entry text.
    pub fn interner(&self) -> &leviath_core::ContentInterner {
        &self.interner
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
        let interner = self.interner.clone();
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            region.insert_entry(leviath_core::RegionEntry::interned(
                &interner,
                content.as_str(),
                tokens,
            ))
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
        let interner = self.interner.clone();
        if let Some(region) = self.get_region_mut(region_name) {
            region.clear();
            let _ = region.insert_entry(leviath_core::RegionEntry::interned(
                &interner,
                content.as_str(),
                tokens,
            ));
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
        let interner = self.interner.clone();
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            let mut entry =
                leviath_core::RegionEntry::interned(&interner, content.as_str(), tokens);
            entry.kind = kind.clone();
            region.insert_entry(entry)
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
                        .map(|e| e.content())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    system_blocks.push(leviath_providers::SystemBlock {
                        text,
                        cache_hint: CacheHint::Always,
                    });
                }
                leviath_core::RegionKind::CompactHistory { .. } => {
                    let text = region
                        .content
                        .iter()
                        .map(|e| e.content())
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
                                    content: entry.content().into(),
                                    cache_breakpoint: false,
                                });
                            }
                            EntryKind::AssistantTurn { tool_calls } => {
                                if tool_calls.is_empty() {
                                    messages.push(leviath_providers::Message {
                                        role: "assistant".to_string(),
                                        content: entry.content().into(),
                                        cache_breakpoint: false,
                                    });
                                } else {
                                    let mut blocks = Vec::new();
                                    if !entry.content().is_empty() {
                                        blocks.push(leviath_providers::ContentBlock::Text {
                                            text: entry.content_owned(),
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
                                        content: entry.content_owned(),
                                        is_error: *is_error,
                                    },
                                );
                            }
                            EntryKind::Text => {
                                let trimmed = entry.content().trim();
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
                                        content: entry.content().into(),
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
                        .map(|e| e.content())
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
                        .map(|e| e.content())
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
                        .map(|e| e.content())
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
                        region,
                        self.region_scripts.get(script),
                        *persistent,
                        meta,
                        self.current_tokens,
                        self.max_tokens,
                        &mut system_blocks,
                        &mut messages,
                    );
                }

                // HashMap regions → system blocks with key headers
                leviath_core::RegionKind::HashMap { .. } => {
                    let text = region
                        .content
                        .iter()
                        .map(|e| {
                            if let Some(key) = &e.key {
                                format!("### [{}]\n{}", key, e.content())
                            } else {
                                e.content_owned()
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
        let interner = self.interner.clone();
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            region.insert_tainted_entry(
                leviath_core::RegionEntry::interned(&interner, content.as_str(), tokens),
                taint_level,
            )
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
        let interner = self.interner.clone();
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            let mut entry =
                leviath_core::RegionEntry::interned(&interner, content.as_str(), tokens);
            entry.kind = kind.clone();
            region.insert_tainted_entry(entry, taint_level)
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

/// Inference result component.
///
/// Stores the result of an LLM inference call, including the response
/// and any tool calls that need to be executed.
#[derive(Component, Debug, Clone)]
pub struct InferenceResult {
    /// The model's response text
    pub response: String,

    /// Tool calls requested by the model
    pub tool_calls: Vec<ToolCall>,

    /// Tokens used in this inference
    pub tokens_used: usize,

    /// Timestamp of this inference
    pub timestamp: i64,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool identifier
    pub tool_id: String,

    /// Tool name
    pub name: String,

    /// Tool arguments
    pub arguments: serde_json::Value,
    /// Opaque provider token echoed back with this call on the next request
    /// (Gemini's `thought_signature`); `None` when the provider has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// A message that can be sent to a running agent.
#[derive(Debug, Clone)]
pub struct AgentMessage {
    /// Target agent ID
    pub agent_id: String,
    /// Message content
    pub content: String,
    /// Which region to add the message to (default: "conversation")
    pub target_region: Option<String>,
    /// Priority (higher = processed sooner)
    pub priority: i32,
}

/// Inbox component for receiving messages sent to a running agent.
#[derive(Component, Debug, Clone)]
pub struct MessageInbox {
    /// Pending messages waiting to be processed
    pub messages: Vec<AgentMessage>,
}

impl MessageInbox {
    /// Create a new empty inbox.
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// Add a message to the inbox.
    pub fn push(&mut self, msg: AgentMessage) {
        self.messages.push(msg);
        // Sort by priority descending so highest priority is first
        self.messages.sort_by_key(|m| std::cmp::Reverse(m.priority));
    }

    /// Drain all messages from the inbox.
    pub fn drain_all(&mut self) -> Vec<AgentMessage> {
        std::mem::take(&mut self.messages)
    }
}

impl Default for MessageInbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_core::{EvictionStrategy, Region, RegionKind};

    #[test]
    fn test_context_window_creation() {
        let window = ContextWindow::new(10000);
        assert_eq!(window.max_tokens, 10000);
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn test_needs_eviction() {
        let mut window = ContextWindow::new(10000);
        window.current_tokens = 9500;
        assert!(window.needs_eviction(0.9));

        window.current_tokens = 5000;
        assert!(!window.needs_eviction(0.9));
    }

    #[test]
    fn test_add_region() {
        let mut window = ContextWindow::new(10000);
        let region = Region::new("test".to_string(), RegionKind::Pinned, 1000);
        window.add_region(region);
        assert_eq!(window.regions.len(), 1);
    }

    #[test]
    fn replace_region_overwrites_existing_and_reports_missing() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("plan".to_string(), RegionKind::Pinned, 6000);
        region.add_entry("old plan", 3).unwrap();
        window.add_region(region);

        // Replacing an existing region overwrites its content wholesale.
        assert!(window.replace_region("plan", "new plan".to_string(), 3));
        let plan = window.get_region("plan").unwrap();
        assert_eq!(plan.content.len(), 1);
        assert_eq!(plan.content[0].content(), "new plan");

        // A missing region is a no-op that reports false.
        assert!(!window.replace_region("nope", "x".to_string(), 1));
    }

    #[test]
    fn test_clearable_eviction() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("scratch".to_string(), RegionKind::Clearable, 5000);
        region.add_entry("test content 1", 1000).unwrap();
        region.add_entry("test content 2", 1000).unwrap();
        window.add_region(region);

        assert_eq!(window.current_tokens, 2000);

        // Evict should clear the entire Clearable region
        let result = with_tracing(|| window.try_evict(1000)).unwrap();
        assert_eq!(result.tokens_freed, 2000);
        assert!(result.needs_compaction.is_empty());
        assert_eq!(window.current_tokens, 0);
    }

    #[test]
    fn test_temporary_eviction_oldest_first() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("temp".to_string(), RegionKind::Temporary, 5000);
        region.add_entry("old content", 1000).unwrap();
        region.add_entry("middle content", 1000).unwrap();
        region.add_entry("new content", 1000).unwrap();
        window.add_region(region);

        assert_eq!(window.current_tokens, 3000);

        // Evict should remove oldest first
        let result = with_tracing(|| window.try_evict(500)).unwrap();
        assert!(result.tokens_freed >= 1000); // Should free at least one entry
        assert!(result.needs_compaction.is_empty());

        // Check that oldest was removed
        let region = window.get_region("temp").unwrap();
        assert_eq!(region.content.len(), 2);
        assert_eq!(region.content[0].content(), "middle content");
    }

    fn assert_sliding_window_unreduced(initial_count: usize, after_count: usize) {
        assert_eq!(
            initial_count, after_count,
            "SlidingWindow should never be reduced during eviction"
        );
    }

    #[test]
    fn test_sliding_window_never_reduced() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 5,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        );
        region.add_entry("msg 1", 1000).unwrap();
        region.add_entry("msg 2", 1000).unwrap();
        region.add_entry("msg 3", 1000).unwrap();
        window.add_region(region);

        let initial_count = window.get_region("conversation").unwrap().content.len();

        // Try to evict - should not touch SlidingWindow
        window.try_evict(1000).ok();

        let after_count = window.get_region("conversation").unwrap().content.len();
        assert_sliding_window_unreduced(initial_count, after_count);
    }

    #[test]
    #[should_panic(expected = "SlidingWindow should never be reduced during eviction")]
    fn test_sliding_window_never_reduced_panics_on_mismatch() {
        assert_sliding_window_unreduced(3, 2);
    }

    fn assert_pinned_unevicted(initial_tokens: usize, after_tokens: usize) {
        assert_eq!(
            initial_tokens, after_tokens,
            "Pinned region should never be evicted"
        );
    }

    #[test]
    fn test_pinned_never_touched() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new("architecture".to_string(), RegionKind::Pinned, 3000);
        region.add_entry("architecture diagram", 2000).unwrap();
        window.add_region(region);

        let initial_tokens = window.get_region("architecture").unwrap().current_tokens;

        // Try to evict - should not touch Pinned
        window.try_evict(1000).ok();

        let after_tokens = window.get_region("architecture").unwrap().current_tokens;
        assert_pinned_unevicted(initial_tokens, after_tokens);
    }

    #[test]
    #[should_panic(expected = "Pinned region should never be evicted")]
    fn test_pinned_never_touched_panics_on_mismatch() {
        assert_pinned_unevicted(2000, 1000);
    }

    #[test]
    fn test_eviction_cascade_order() {
        let mut window = ContextWindow::new(10000);

        // Add Clearable region
        let mut clearable = Region::new("scratch".to_string(), RegionKind::Clearable, 2000);
        clearable.add_entry("scratch data", 1000).unwrap();
        window.add_region(clearable);

        // Add Temporary region
        let mut temporary = Region::new("temp".to_string(), RegionKind::Temporary, 3000);
        temporary.add_entry("temp data 1", 1000).unwrap();
        temporary.add_entry("temp data 2", 1000).unwrap();
        window.add_region(temporary);

        assert_eq!(window.current_tokens, 3000);

        // Evict with small target - should clear Clearable first
        window.try_evict(500).unwrap();

        // Clearable should be empty
        assert_eq!(window.get_region("scratch").unwrap().current_tokens, 0);

        // Temporary should still have content
        assert!(window.get_region("temp").unwrap().current_tokens > 0);
    }

    #[test]
    fn test_message_inbox() {
        let mut inbox = MessageInbox::new();
        assert!(inbox.messages.is_empty());

        inbox.push(AgentMessage {
            agent_id: "agent-1".to_string(),
            content: "hello".to_string(),
            target_region: None,
            priority: 0,
        });
        assert_eq!(inbox.messages.len(), 1);

        let drained = inbox.drain_all();
        assert_eq!(drained.len(), 1);
        assert!(inbox.messages.is_empty());
    }

    #[test]
    fn test_message_inbox_priority_ordering() {
        let mut inbox = MessageInbox::new();

        inbox.push(AgentMessage {
            agent_id: "a".to_string(),
            content: "low".to_string(),
            target_region: None,
            priority: 1,
        });
        inbox.push(AgentMessage {
            agent_id: "a".to_string(),
            content: "high".to_string(),
            target_region: None,
            priority: 10,
        });
        inbox.push(AgentMessage {
            agent_id: "a".to_string(),
            content: "medium".to_string(),
            target_region: None,
            priority: 5,
        });

        let msgs = inbox.drain_all();
        assert_eq!(msgs[0].content, "high");
        assert_eq!(msgs[1].content, "medium");
        assert_eq!(msgs[2].content, "low");
    }

    #[test]
    fn test_eviction_result_identifies_compaction_regions() {
        // Small window so compacting region fills most of it
        let mut window = ContextWindow::new(1000);
        // Add a compacting region that's over threshold
        let mut compacting = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 500,
            },
            900,
        );
        compacting.add_entry("lots of content", 600).unwrap();
        window.add_region(compacting);

        assert_eq!(window.current_tokens, 600);

        // Request 500 free tokens - only 400 free, can't free clearable/temporary, so compacting should be identified
        let result = window.try_evict(500).unwrap();
        assert_eq!(result.tokens_freed, 0);
        assert_eq!(result.needs_compaction, vec!["impl".to_string()]);
    }

    #[test]
    fn test_try_evict_returns_needs_compaction_when_full() {
        let mut window = ContextWindow::new(1200);

        // Fill with compacting region content above threshold
        let mut compacting = Region::new(
            "analysis".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 800,
            },
            1100,
        );
        compacting.add_entry("data 1", 500).unwrap();
        compacting.add_entry("data 2", 500).unwrap();
        window.add_region(compacting);

        // 200 free tokens, request 500 → needs compaction
        let result = window.try_evict(500).unwrap();
        assert_eq!(result.tokens_freed, 0);
        assert!(result.needs_compaction.contains(&"analysis".to_string()));
    }

    #[test]
    fn test_try_evict_errors_when_pinned_regions_exceed_budget() {
        // Pinned/CompactHistory regions are never evicted - if their combined
        // token usage alone exceeds max_tokens, try_evict must report this as
        // a configuration error instead of silently doing nothing useful.
        let mut window = ContextWindow::new(1000);
        let mut pinned = Region::new("architecture".to_string(), RegionKind::Pinned, 2000);
        pinned.add_entry("huge pinned doc", 1500).unwrap();
        window.add_region(pinned);

        let result = window.try_evict(100);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("Pinned regions"));
    }

    #[test]
    fn test_clearable_eviction_continues_past_insufficient_first_region() {
        // Phase 1 clears Clearable regions one at a time and returns early as
        // soon as enough space has been freed. If clearing the *first*
        // Clearable region alone isn't enough, the loop must fall through and
        // keep clearing subsequent Clearable regions rather than stopping.
        let mut window = ContextWindow::new(2000);

        let mut region_a = Region::new("a".to_string(), RegionKind::Clearable, 1000);
        region_a.add_entry("small", 500).unwrap();
        window.add_region(region_a);

        let mut region_b = Region::new("b".to_string(), RegionKind::Clearable, 1000);
        region_b.add_entry("large", 1000).unwrap();
        window.add_region(region_b);

        assert_eq!(window.current_tokens, 1500);

        // After clearing only "a" (frees 500), 2000 - 1000 = 1000 free tokens,
        // which is still below the 1400 target, so the loop must continue on
        // to clear "b" as well before it can satisfy the request.
        let result = with_tracing(|| window.try_evict(1400)).unwrap();
        assert_eq!(result.tokens_freed, 1500);
        assert_eq!(window.current_tokens, 0);
        assert_eq!(window.get_region("a").unwrap().current_tokens, 0);
        assert_eq!(window.get_region("b").unwrap().current_tokens, 0);
    }

    #[test]
    fn test_agent_status_cancelled() {
        assert_eq!(AgentStatus::Cancelled, AgentStatus::Cancelled);
    }

    #[test]
    fn test_parent_ref_component() {
        let parent_ref = super::ParentRef {
            parent_entity: Entity::from_raw_u32(42)
                .expect("a small literal index is always a valid entity id"),
            parent_agent_id: "coder-01".to_string(),
            depth: 1,
        };
        assert_eq!(parent_ref.parent_agent_id, "coder-01");
        assert_eq!(parent_ref.depth, 1);
    }

    #[test]
    fn test_children_component() {
        let children = super::SubAgentChildren {
            children: vec![
                Entity::from_raw_u32(1).expect("a small literal index is always a valid entity id"),
                Entity::from_raw_u32(2).expect("a small literal index is always a valid entity id"),
            ],
            max_child_depth: 3,
        };
        assert_eq!(children.children.len(), 2);
        assert_eq!(children.max_child_depth, 3);
    }

    #[test]
    fn test_agent_state_with_children_fields() {
        let state = AgentState {
            agent_id: "test-01".to_string(),
            current_stage: "analyze".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec!["child-01".to_string(), "child-02".to_string()],
            pending_wait: Some("child-01".to_string()),
            accepts_messages: true,
        };
        assert_eq!(state.spawned_children_ids.len(), 2);
        assert_eq!(state.pending_wait, Some("child-01".to_string()));
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_context_window_get_region() {
        let mut window = ContextWindow::new(10000);
        let region = Region::new("test".to_string(), RegionKind::Pinned, 1000);
        window.add_region(region);

        assert!(window.get_region("test").is_some());
        assert!(window.get_region("nonexistent").is_none());
    }

    #[test]
    fn test_context_window_get_region_mut() {
        let mut window = ContextWindow::new(10000);
        let region = Region::new("test".to_string(), RegionKind::Temporary, 1000);
        window.add_region(region);

        let region = window.get_region_mut("test").unwrap();
        region.add_entry("new content", 50).unwrap();
        assert_eq!(region.content.len(), 1);

        assert!(window.get_region_mut("nonexistent").is_none());
    }

    #[test]
    fn test_context_window_add_to_region_success() {
        let mut window = ContextWindow::new(10000);
        let region = Region::new("conv".to_string(), RegionKind::Temporary, 5000);
        window.add_region(region);

        let result = window.add_to_region("conv", "Hello".to_string(), 10);
        assert!(result.is_ok());
        assert_eq!(window.current_tokens, 10);
    }

    #[test]
    fn test_context_window_add_to_region_not_found() {
        let mut window = ContextWindow::new(10000);
        let result = window.add_to_region("nonexistent", "Hello".to_string(), 10);
        assert!(result.is_err());
    }

    #[test]
    fn test_context_window_calculate_tokens() {
        let mut window = ContextWindow::new(10000);
        let mut r1 = Region::new("a".to_string(), RegionKind::Pinned, 5000);
        r1.add_entry("x", 100).unwrap();
        let mut r2 = Region::new("b".to_string(), RegionKind::Temporary, 5000);
        r2.add_entry("y", 200).unwrap();
        window.add_region(r1);
        window.add_region(r2);

        assert_eq!(window.calculate_tokens(), 300);
    }

    #[test]
    fn test_context_window_needs_eviction_boundary() {
        let mut window = ContextWindow::new(100);
        // Exactly 90% → should trigger at 0.9 threshold
        window.current_tokens = 90;
        assert!(window.needs_eviction(0.9));

        // Just below 90%
        window.current_tokens = 89;
        assert!(!window.needs_eviction(0.9));
    }

    #[test]
    fn test_eviction_result_default_fields() {
        let result = EvictionResult {
            tokens_freed: 0,
            needs_compaction: Vec::new(),
        };
        assert_eq!(result.tokens_freed, 0);
        assert!(result.needs_compaction.is_empty());
    }

    #[test]
    fn test_message_inbox_default() {
        let inbox = MessageInbox::default();
        assert!(inbox.messages.is_empty());
    }

    #[test]
    fn test_message_inbox_drain_all_empties() {
        let mut inbox = MessageInbox::new();
        inbox.push(AgentMessage {
            agent_id: "a".to_string(),
            content: "msg".to_string(),
            target_region: None,
            priority: 0,
        });
        let _ = inbox.drain_all();
        assert!(inbox.messages.is_empty());
        // Drain again should return empty vec
        let result = inbox.drain_all();
        assert!(result.is_empty());
    }

    #[test]
    fn test_agent_message_clone() {
        let msg = AgentMessage {
            agent_id: "agent-1".to_string(),
            content: "hello".to_string(),
            target_region: Some("conv".to_string()),
            priority: 5,
        };
        let cloned = msg.clone();
        assert_eq!(cloned.agent_id, "agent-1");
        assert_eq!(cloned.content, "hello");
        assert_eq!(cloned.target_region, Some("conv".to_string()));
        assert_eq!(cloned.priority, 5);
    }

    #[test]
    fn test_agent_status_serialization() {
        let status = AgentStatus::Active;
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("Active"));

        let error_status = AgentStatus::Error {
            message: "boom".to_string(),
        };
        let json = serde_json::to_string(&error_status).unwrap();
        assert!(json.contains("boom"));
    }

    #[test]
    fn test_tool_call_serialization() {
        let tc = ToolCall {
            tool_id: "tool-1".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"query": "rust"}),
            thought_signature: None,
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert!(json.contains("search"));
        assert!(json.contains("rust"));
    }

    #[test]
    fn test_eviction_with_only_pinned_region_frees_nothing() {
        // When the only region is Pinned (within budget), eviction frees nothing.
        let mut window = ContextWindow::new(10000);
        let mut pinned = Region::new("pinned".to_string(), RegionKind::Pinned, 5000);
        pinned.add_entry("important data", 2000).unwrap();
        window.add_region(pinned);

        let result = with_tracing(|| window.try_evict(500)).unwrap();
        assert_eq!(result.tokens_freed, 0);
        assert!(result.needs_compaction.is_empty());
    }

    #[test]
    fn test_inference_result_fields() {
        let ir = InferenceResult {
            response: "Hello".to_string(),
            tool_calls: vec![ToolCall {
                tool_id: "t1".to_string(),
                name: "search".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            }],
            tokens_used: 100,
            timestamp: 99999,
        };
        assert_eq!(ir.response, "Hello");
        assert_eq!(ir.tool_calls.len(), 1);
        assert_eq!(ir.tokens_used, 100);
    }

    #[test]
    fn test_sub_agent_children_clone() {
        let children = SubAgentChildren {
            children: vec![
                Entity::from_raw_u32(1).expect("a small literal index is always a valid entity id"),
            ],
            max_child_depth: 2,
        };
        let cloned = children.clone();
        assert_eq!(cloned.children.len(), 1);
        assert_eq!(cloned.max_child_depth, 2);
    }

    // ─── try_evict: FALSE path after each single-entry removal ────────────
    // Covers 235:25 (false path of the early-return check) and 242:13 (break).
    //
    // Setup: max=1000, current=950, target=200.
    // Two Temporary entries of 50 tokens each.
    //
    // Pass 1: remove entry1 (50 tokens) → current=900, available=100 < 200
    //   → condition FALSE → line 235 covered → outer loop continues
    // Pass 2: remove entry2 (50 tokens) → current=850, available=150 < 200
    //   → condition FALSE → line 235 covered again
    // Pass 3: no more entries → evicted_any=false → break → line 242 covered

    #[test]
    fn try_evict_continues_loop_when_each_entry_removal_is_insufficient() {
        let mut window = ContextWindow::new(1000);
        let mut temp = Region::new("cache".to_string(), RegionKind::Temporary, 800);
        temp.add_entry("entry1", 50).unwrap();
        temp.add_entry("entry2", 50).unwrap();
        window.add_region(temp);
        window.current_tokens = 950; // 95% full

        // Target=200: removing 50 at a time is insufficient each pass
        let result = window.try_evict(200).unwrap();
        assert_eq!(result.tokens_freed, 100); // freed 50+50, but not enough for target
    }

    // ─── Context window taint tracking ──────────────────────────────────────

    #[test]
    fn test_enable_taint_tracking_on_context_window() {
        let mut window = ContextWindow::new(10000);
        window.add_region(Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        ));
        window.add_region(Region::new(
            "tools".to_string(),
            RegionKind::Temporary,
            3000,
        ));

        assert!(window.overall_taint().is_none());
        window.enable_taint_tracking();
        assert_eq!(
            window.overall_taint(),
            Some(leviath_core::TaintLevel::Public)
        );
    }

    #[test]
    fn test_add_tainted_to_region() {
        let mut window = ContextWindow::new(10000);
        let region =
            Region::new("tools".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        window.add_region(region);

        window
            .add_tainted_to_region(
                "tools",
                "secret data".to_string(),
                10,
                leviath_core::TaintLevel::Private,
            )
            .unwrap();

        assert_eq!(
            window.get_region("tools").and_then(|r| r.taint_level()),
            Some(leviath_core::TaintLevel::Private)
        );
        assert_eq!(
            window.overall_taint(),
            Some(leviath_core::TaintLevel::Private)
        );
    }

    #[test]
    fn test_add_tainted_to_nonexistent_region() {
        let mut window = ContextWindow::new(10000);
        let result = window.add_tainted_to_region(
            "nope",
            "data".to_string(),
            10,
            leviath_core::TaintLevel::Public,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_overall_taint_is_max_across_regions() {
        let mut window = ContextWindow::new(10000);
        let r1 = Region::new("a".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        let r2 = Region::new("b".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        window.add_region(r1);
        window.add_region(r2);

        window
            .add_tainted_to_region("a", "x".to_string(), 5, leviath_core::TaintLevel::Internal)
            .unwrap();
        window
            .add_tainted_to_region("b", "y".to_string(), 5, leviath_core::TaintLevel::Public)
            .unwrap();

        assert_eq!(
            window.overall_taint(),
            Some(leviath_core::TaintLevel::Internal)
        );
    }

    #[test]
    fn test_taint_summary() {
        let mut window = ContextWindow::new(10000);
        let r1 = Region::new("conv".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        let r2 =
            Region::new("tools".to_string(), RegionKind::Temporary, 5000).with_taint_tracking();
        window.add_region(r1);
        window.add_region(r2);

        window
            .add_tainted_to_region(
                "conv",
                "x".to_string(),
                5,
                leviath_core::TaintLevel::Private,
            )
            .unwrap();

        let summary = window.taint_summary();
        assert_eq!(summary.len(), 2);
        assert!(
            summary
                .iter()
                .any(|(name, level)| name == "conv" && *level == leviath_core::TaintLevel::Private)
        );
        assert!(
            summary
                .iter()
                .any(|(name, level)| name == "tools" && *level == leviath_core::TaintLevel::Public)
        );
    }

    #[test]
    fn test_taint_recovery_through_eviction() {
        with_tracing(|| {});
        let mut window = ContextWindow::new(100);
        let r = Region::new("temp".to_string(), RegionKind::Temporary, 100).with_taint_tracking();
        window.add_region(r);

        window
            .add_tainted_to_region(
                "temp",
                "private".to_string(),
                30,
                leviath_core::TaintLevel::Private,
            )
            .unwrap();
        window
            .add_tainted_to_region(
                "temp",
                "public".to_string(),
                30,
                leviath_core::TaintLevel::Public,
            )
            .unwrap();

        assert_eq!(
            window.get_region("temp").and_then(|r| r.taint_level()),
            Some(leviath_core::TaintLevel::Private)
        );

        // Eviction should trigger and remove oldest (private) entry
        window.current_tokens = 96; // Push over 0.95 threshold
        let result = window.try_evict(10).unwrap();
        assert!(result.tokens_freed > 0);

        // After evicting the private entry, taint should recover
        assert_eq!(
            window.get_region("temp").and_then(|r| r.taint_level()),
            Some(leviath_core::TaintLevel::Public)
        );
    }

    // ─── Tool-use/tool-result pairing sanitization tests ────────────────

    #[test]
    fn test_assemble_appends_user_nudge_when_conversation_ends_with_assistant() {
        // After a stage transition the carried conversation ends with the prior
        // stage's assistant turn; assemble must append a trailing user message so
        // the request doesn't end on an assistant turn (rejected as prefill).
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        ));
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "do the task".to_string(),
                10,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                "All done with stage one.".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();
        assert_eq!(
            assembled.messages.last().map(|m| m.role.as_str()),
            Some("user"),
            "the assembled conversation must end with a user message"
        );
    }

    #[test]
    fn test_assemble_strips_orphaned_tool_use() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // Add an assistant turn with a tool_use but no matching tool_result
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "tc_orphan".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                        thought_signature: None,
                    }],
                },
                "Let me read that file.".to_string(),
                50,
            )
            .unwrap();

        let assembled = with_tracing(|| window.assemble());

        // The orphaned tool_use should be stripped; text should remain
        for msg in &assembled.messages {
            if let leviath_providers::MessageContent::Blocks(blocks) = &msg.content {
                for block in blocks {
                    assert!(
                        !matches!(block, leviath_providers::ContentBlock::ToolUse { .. }),
                        "Orphaned tool_use should have been stripped"
                    );
                }
            }
        }
        // The assistant text should still be present
        assert!(
            assembled
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.content.as_text().contains("read that file"))
        );
    }

    #[test]
    fn test_assemble_strips_orphaned_tool_result() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // Add a user message first
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "Hello".to_string(),
                10,
            )
            .unwrap();

        // Add a tool_result with no preceding tool_use
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_missing".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "file contents here".to_string(),
                20,
            )
            .unwrap();

        let assembled = with_tracing(|| window.assemble());

        // The orphaned tool_result message is stripped to empty and dropped;
        // only the plain user message survives (as Text, carrying no blocks).
        assert_eq!(assembled.messages.len(), 1);
        assert_eq!(assembled.messages[0].role, "user");
        assert_eq!(
            assembled.messages[0].content,
            leviath_providers::MessageContent::Text("Hello".to_string())
        );
    }

    #[test]
    fn test_assemble_paired_tool_use_result_passes_through() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // User message
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "Fix the bug".to_string(),
                10,
            )
            .unwrap();

        // Assistant with tool_use
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "tc_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "main.rs"}),
                        thought_signature: None,
                    }],
                },
                "".to_string(),
                10,
            )
            .unwrap();

        // Matching tool_result
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "fn main() {}".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();

        // Both tool_use and tool_result should be present
        let has_tool_use = assembled.messages.iter().any(|m| {
            if let leviath_providers::MessageContent::Blocks(blocks) = &m.content {
                blocks
                    .iter()
                    .any(|b| matches!(b, leviath_providers::ContentBlock::ToolUse { id, .. } if id == "tc_1"))
            } else {
                false
            }
        });
        let has_tool_result = assembled.messages.iter().any(|m| {
            if let leviath_providers::MessageContent::Blocks(blocks) = &m.content {
                blocks
                    .iter()
                    .any(|b| matches!(b, leviath_providers::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tc_1"))
            } else {
                false
            }
        });
        assert!(has_tool_use, "Paired tool_use should remain");
        assert!(has_tool_result, "Paired tool_result should remain");
    }

    #[test]
    fn test_assemble_removes_empty_assistant_after_stripping() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // User message
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "Do something".to_string(),
                10,
            )
            .unwrap();

        // Assistant with ONLY a tool_use (no text), and no matching result
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "tc_gone".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::json!({"command": "ls"}),
                        thought_signature: None,
                    }],
                },
                "".to_string(),
                10,
            )
            .unwrap();

        let assembled = with_tracing(|| window.assemble());

        // The assistant message should be entirely removed (empty after stripping)
        let assistant_msgs: Vec<_> = assembled
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .collect();
        assert!(
            assistant_msgs.is_empty(),
            "Assistant message with only orphaned tool_use should be removed entirely"
        );
    }

    #[test]
    fn test_assemble_strips_multiple_orphaned_tool_uses_in_one_message() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // User message first
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "Do two things".to_string(),
                10,
            )
            .unwrap();

        // Assistant with TWO orphaned tool_uses (no matching results for either)
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![
                        leviath_core::SerializedToolCall {
                            id: "tc_orphan_1".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({"path": "a.rs"}),
                            thought_signature: None,
                        },
                        leviath_core::SerializedToolCall {
                            id: "tc_orphan_2".to_string(),
                            name: "bash".to_string(),
                            arguments: serde_json::json!({"cmd": "ls"}),
                            thought_signature: None,
                        },
                    ],
                },
                "Let me do both.".to_string(),
                50,
            )
            .unwrap();

        let assembled = with_tracing(|| window.assemble());

        // Both orphaned tool_uses should be stripped
        for msg in &assembled.messages {
            if let leviath_providers::MessageContent::Blocks(blocks) = &msg.content {
                for block in blocks {
                    assert!(
                        !matches!(block, leviath_providers::ContentBlock::ToolUse { .. }),
                        "All orphaned tool_uses should have been stripped"
                    );
                }
            }
        }
        // The assistant text should still be present
        assert!(
            assembled
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.content.as_text().contains("do both"))
        );
    }

    #[test]
    fn test_assemble_mixed_valid_and_orphaned_in_same_message() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // User message
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "Do stuff".to_string(),
                10,
            )
            .unwrap();

        // Assistant with one valid tool_use (tc_valid) and one orphaned (tc_orphan)
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![
                        leviath_core::SerializedToolCall {
                            id: "tc_valid".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({"path": "main.rs"}),
                            thought_signature: None,
                        },
                        leviath_core::SerializedToolCall {
                            id: "tc_orphan".to_string(),
                            name: "bash".to_string(),
                            arguments: serde_json::json!({"cmd": "ls"}),
                            thought_signature: None,
                        },
                    ],
                },
                "".to_string(),
                10,
            )
            .unwrap();

        // Only provide tool_result for tc_valid
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_valid".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "fn main() {}".to_string(),
                10,
            )
            .unwrap();

        let assembled = with_tracing(|| window.assemble());

        // Collect the tool_use ids that survived assembly.
        let tool_use_ids: Vec<&str> = assembled
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                leviath_providers::MessageContent::Blocks(blocks) => Some(blocks),
                _ => None,
            })
            .flatten()
            .filter_map(|b| match b {
                leviath_providers::ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        // tc_valid's tool_use remains; the orphaned tc_orphan is stripped.
        assert!(
            tool_use_ids.contains(&"tc_valid"),
            "Valid tool_use should remain"
        );
        assert!(
            !tool_use_ids.contains(&"tc_orphan"),
            "Orphaned tool_use should be stripped"
        );

        // tc_valid tool_result should remain
        let has_result = assembled.messages.iter().any(|m| {
            if let leviath_providers::MessageContent::Blocks(blocks) = &m.content {
                blocks.iter().any(|b| {
                    matches!(b, leviath_providers::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tc_valid")
                })
            } else {
                false
            }
        });
        assert!(has_result, "Valid tool_result should remain");
    }

    // ─── assemble() region kind coverage ──────────────────────────────────

    #[test]
    fn test_assemble_compact_history_region_produces_system_block_always() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new(
            "history".to_string(),
            RegionKind::CompactHistory {
                source_region: "conv".to_string(),
            },
            10_000,
        );
        region
            .add_entry("summary of earlier conversation", 50)
            .unwrap();
        window.add_region(region);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(
            assembled.system_blocks[0].text,
            "summary of earlier conversation"
        );
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            leviath_core::CacheHint::Always
        );
    }

    #[test]
    fn test_assemble_compacting_region_produces_system_block_until_changed() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 500,
            },
            10_000,
        );
        region.add_entry("implementation details", 50).unwrap();
        window.add_region(region);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(
            assembled.system_blocks[0].text,
            "[impl]:\nimplementation details"
        );
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            leviath_core::CacheHint::UntilChanged
        );
    }

    #[test]
    fn test_assemble_temporary_region_produces_system_block_never() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new("scratch".to_string(), RegionKind::Temporary, 10_000);
        region.add_entry("temp data", 20).unwrap();
        window.add_region(region);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(assembled.system_blocks[0].text, "[scratch]:\ntemp data");
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            leviath_core::CacheHint::Never
        );
    }

    #[test]
    fn test_assemble_clearable_region_produces_system_block_never() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new("cache".to_string(), RegionKind::Clearable, 10_000);
        region.add_entry("clearable data", 20).unwrap();
        window.add_region(region);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(assembled.system_blocks[0].text, "[cache]:\nclearable data");
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            leviath_core::CacheHint::Never
        );
    }

    fn custom_kind(script: &str, persistent: bool) -> RegionKind {
        RegionKind::Custom {
            script: script.to_string(),
            persistent,
        }
    }

    #[test]
    fn test_assemble_custom_region_falls_back_to_temporary_style_block() {
        // Plain `assemble()` has no compiled script available, so a custom
        // region renders as the hook-less fallback: a Temporary-style block -
        // never silently dropped.
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new("brain".to_string(), custom_kind("b.rhai", false), 10_000);
        region.add_entry("thought one", 10).unwrap();
        region.add_entry("thought two", 10).unwrap();
        window.add_region(region);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(
            assembled.system_blocks[0].text,
            "[brain]:\nthought one\n\nthought two"
        );
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            leviath_core::CacheHint::Never
        );
    }

    #[test]
    fn try_evict_evicts_non_persistent_custom_regions_oldest_first() {
        let mut window = ContextWindow::new(100);
        let mut region = Region::new("brain".to_string(), custom_kind("b.rhai", false), 100);
        region.add_entry("old", 40).unwrap();
        region.add_entry("new", 40).unwrap();
        window.add_region(region);
        window.current_tokens = 80;

        let result = with_tracing(|| window.try_evict(30).unwrap());
        assert!(result.tokens_freed >= 40);
        let brain = window.get_region("brain").unwrap();
        assert_eq!(brain.content.len(), 1);
        assert_eq!(brain.content[0].content(), "new");
    }

    #[test]
    fn try_evict_never_touches_persistent_custom_and_counts_it_as_pinned() {
        // Persistent custom content survives eviction, and when it alone
        // exceeds the whole window budget the pinned over-budget guard fires.
        let mut window = ContextWindow::new(50);
        let mut vault = Region::new("vault".to_string(), custom_kind("v.rhai", true), 100);
        vault.add_entry("precious", 60).unwrap();
        window.add_region(vault);
        window.current_tokens = 60;

        let err = with_tracing(|| window.try_evict(10).unwrap_err());
        assert_eq!(
            err.to_string(),
            "Pinned regions (60) exceed total budget (50)"
        );
        assert_eq!(window.get_region("vault").unwrap().content.len(), 1);
    }

    /// A window with one custom region (`brain`, budget 100) backed by `src`,
    /// compiled and installed in the script table under "s.rhai".
    fn custom_window(src: &str, persistent: bool) -> ContextWindow {
        let mut window = ContextWindow::new(10_000);
        window.add_region(Region::new(
            "brain".to_string(),
            RegionKind::Custom {
                script: "s.rhai".to_string(),
                persistent,
            },
            100,
        ));
        window.region_scripts.insert(
            "s.rhai".to_string(),
            std::sync::Arc::new(leviath_scripting::region_hook::compile("s.rhai", src).unwrap()),
        );
        window
    }

    #[test]
    fn custom_region_on_write_fires_across_all_write_methods() {
        let src = r#"
            fn render(ctx) { "" }
            fn on_write(ctx) { `${ctx.entry.kind}:${ctx.entry.content}` }
        "#;
        let mut window = custom_window(src, false);

        window.add_to_region("brain", "a".to_string(), 1).unwrap();
        window
            .add_typed_entry(
                "brain",
                leviath_core::EntryKind::UserMessage,
                "b".to_string(),
                1,
            )
            .unwrap();
        window
            .add_tainted_to_region(
                "brain",
                "c".to_string(),
                1,
                leviath_core::TaintLevel::Public,
            )
            .unwrap();
        window
            .add_typed_tainted_to_region(
                "brain",
                leviath_core::EntryKind::UserMessage,
                "d".to_string(),
                1,
                leviath_core::TaintLevel::Public,
            )
            .unwrap();

        let contents: Vec<_> = window
            .get_region("brain")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content())
            .collect();
        assert_eq!(
            contents,
            vec!["text:a", "user_message:b", "text:c", "user_message:d"],
            "every write method passes through on_write with the entry kind visible"
        );
        // Token counts were re-estimated for the replacements.
        assert_eq!(window.current_tokens, window.calculate_tokens());

        assert!(window.replace_region("brain", "e".to_string(), 1));
        let region = window.get_region("brain").unwrap();
        assert_eq!(region.content.len(), 1);
        assert_eq!(region.content[0].content(), "text:e");
    }

    #[test]
    fn custom_region_on_write_drop_reports_success_without_storing() {
        let src = r#"
            fn render(ctx) { "" }
            fn on_write(ctx) { false }
        "#;
        let mut window = custom_window(src, false);
        window
            .add_to_region("brain", "spam".to_string(), 1)
            .unwrap();
        assert!(window.get_region("brain").unwrap().content.is_empty());

        // A dropped replacement leaves existing content in place.
        assert!(window.replace_region("brain", "more spam".to_string(), 1));
        assert!(window.get_region("brain").unwrap().content.is_empty());
    }

    #[test]
    fn custom_region_on_write_drop_covers_typed_and_tainted_methods() {
        // Every write method's drop arm, not just add_to_region's.
        let src = r#"
            fn render(ctx) { "" }
            fn on_write(ctx) { false }
        "#;
        let mut window = custom_window(src, false);
        window
            .add_typed_entry(
                "brain",
                leviath_core::EntryKind::UserMessage,
                "a".to_string(),
                1,
            )
            .unwrap();
        window
            .add_tainted_to_region(
                "brain",
                "b".to_string(),
                1,
                leviath_core::TaintLevel::Public,
            )
            .unwrap();
        window
            .add_typed_tainted_to_region(
                "brain",
                leviath_core::EntryKind::UserMessage,
                "c".to_string(),
                1,
                leviath_core::TaintLevel::Public,
            )
            .unwrap();
        assert!(window.get_region("brain").unwrap().content.is_empty());
    }

    #[test]
    fn try_evict_skips_custom_region_whose_script_has_no_on_overflow() {
        // Phase 1.5 leaves the choice to phase 2 (oldest-first) when the
        // script defines no on_overflow.
        let mut window = ContextWindow::new(100);
        window.add_region(Region::new(
            "brain".to_string(),
            RegionKind::Custom {
                script: "s.rhai".to_string(),
                persistent: false,
            },
            100,
        ));
        window.region_scripts.insert(
            "s.rhai".to_string(),
            std::sync::Arc::new(
                leviath_scripting::region_hook::compile("s.rhai", "fn render(ctx) { \"\" }")
                    .unwrap(),
            ),
        );
        window
            .add_to_region("brain", "old".to_string(), 40)
            .unwrap();
        window
            .add_to_region("brain", "new".to_string(), 40)
            .unwrap();

        let result = with_tracing(|| window.try_evict(30).unwrap());
        assert!(result.tokens_freed >= 40);
        let brain = window.get_region("brain").unwrap();
        assert_eq!(brain.content.len(), 1);
        assert_eq!(
            brain.content[0].content(),
            "new",
            "oldest-first fallback ran"
        );
    }

    #[test]
    fn try_evict_falls_to_oldest_first_when_script_frees_nothing() {
        // on_overflow returns [] under pressure: phase 1.5 frees 0 and phase 2
        // makes the progress.
        let src = r#"
            fn render(ctx) { "" }
            fn on_overflow(ctx) { [] }
        "#;
        let mut window = ContextWindow::new(100);
        window.add_region(Region::new(
            "brain".to_string(),
            RegionKind::Custom {
                script: "s.rhai".to_string(),
                persistent: false,
            },
            100,
        ));
        window.region_scripts.insert(
            "s.rhai".to_string(),
            std::sync::Arc::new(leviath_scripting::region_hook::compile("s.rhai", src).unwrap()),
        );
        window
            .add_to_region("brain", "old".to_string(), 40)
            .unwrap();
        window
            .add_to_region("brain", "new".to_string(), 40)
            .unwrap();

        let result = with_tracing(|| window.try_evict(30).unwrap());
        assert!(result.tokens_freed >= 40);
        assert_eq!(window.get_region("brain").unwrap().content.len(), 1);
    }

    #[test]
    fn non_custom_regions_bypass_the_on_write_seam() {
        // A script table entry exists, but the region is plain Temporary - the
        // hook must not fire for it.
        let mut window = custom_window(
            "fn render(ctx) { \"\" }\nfn on_write(ctx) { \"MANGLED\" }",
            false,
        );
        window.add_region(Region::new("plain".to_string(), RegionKind::Temporary, 100));
        window
            .add_to_region("plain", "untouched".to_string(), 2)
            .unwrap();
        assert_eq!(
            window.get_region("plain").unwrap().content[0].content(),
            "untouched"
        );
    }

    #[test]
    fn write_to_missing_region_still_errors() {
        let mut window = custom_window("fn render(ctx) { \"\" }", false);
        let err = window
            .add_to_region("ghost", "x".to_string(), 1)
            .unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn custom_region_add_time_overflow_retries_after_script_drops() {
        // Region budget 100: fill with 90, then add 20 - over budget. The
        // script drops entry 0 (90 tokens), freeing room; the retry succeeds.
        let src = r#"
            fn render(ctx) { "" }
            fn on_overflow(ctx) { [0] }
        "#;
        let mut window = custom_window(src, false);
        window
            .add_to_region("brain", "big".to_string(), 90)
            .unwrap();
        window
            .add_to_region("brain", "next".to_string(), 20)
            .unwrap();

        let region = window.get_region("brain").unwrap();
        assert_eq!(region.content.len(), 1);
        assert_eq!(region.content[0].content(), "next");
        assert_eq!(window.current_tokens, 20);
    }

    #[test]
    fn custom_region_add_time_overflow_propagates_when_still_too_big() {
        // The script frees nothing, so the retry path never runs and the
        // original budget error propagates to the caller's ladders.
        let src = r#"
            fn render(ctx) { "" }
            fn on_overflow(ctx) { [] }
        "#;
        let mut window = custom_window(src, false);
        window
            .add_to_region("brain", "big".to_string(), 90)
            .unwrap();
        let err =
            with_tracing(|| window.add_to_region("brain", "too much".to_string(), 50)).unwrap_err();
        assert_eq!(err.to_string(), "Content exceeds token budget: 140 > 100");
        assert_eq!(window.get_region("brain").unwrap().content.len(), 1);
    }

    #[test]
    fn custom_region_without_on_overflow_gets_no_retry() {
        let mut window = custom_window("fn render(ctx) { \"\" }", false);
        window
            .add_to_region("brain", "big".to_string(), 90)
            .unwrap();
        let err = window
            .add_to_region("brain", "too much".to_string(), 50)
            .unwrap_err();
        assert_eq!(err.to_string(), "Content exceeds token budget: 140 > 100");
    }

    #[test]
    fn try_evict_lets_custom_script_choose_what_to_drop() {
        // The script keeps errors, drops successes - the retention choice the
        // oldest-first cascade could never make. Window is small so eviction
        // has real pressure.
        let src = r#"
            fn render(ctx) { "" }
            fn on_overflow(ctx) {
                let drops = [];
                for (entry, i) in ctx.entries {
                    if !entry.content.contains("ERROR") { drops.push(i); }
                }
                drops
            }
        "#;
        let mut window = ContextWindow::new(100);
        window.add_region(Region::new(
            "brain".to_string(),
            RegionKind::Custom {
                script: "s.rhai".to_string(),
                persistent: false,
            },
            100,
        ));
        window.region_scripts.insert(
            "s.rhai".to_string(),
            std::sync::Arc::new(leviath_scripting::region_hook::compile("s.rhai", src).unwrap()),
        );
        window
            .add_to_region("brain", "ok one".to_string(), 30)
            .unwrap();
        window
            .add_to_region("brain", "ERROR two".to_string(), 30)
            .unwrap();
        window
            .add_to_region("brain", "ok three".to_string(), 30)
            .unwrap();

        let result = with_tracing(|| window.try_evict(40)).unwrap();
        assert!(result.tokens_freed >= 40);
        let contents: Vec<_> = window
            .get_region("brain")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content())
            .collect();
        assert_eq!(
            contents,
            vec!["ERROR two"],
            "script retention choice honored"
        );
    }

    // ─── assemble(): custom regions ──────────────────────────────────────

    #[test]
    fn assemble_custom_region_renders_through_script() {
        let src = r#"fn render(ctx) { `<brain iter=${ctx.stage_iterations}>` }"#;
        let mut window = custom_window(src, false);
        window
            .add_to_region("brain", "note".to_string(), 2)
            .unwrap();

        // Default meta via plain assemble().
        let assembled = window.assemble();
        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(assembled.system_blocks[0].text, "<brain iter=0>");
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            leviath_core::CacheHint::UntilChanged
        );

        // Real meta via assemble_with_meta.
        let assembled = window.assemble_with_meta(&crate::custom_region::AssembleMeta {
            stage_name: "plan".to_string(),
            stage_iterations: 7,
            model: "m".to_string(),
        });
        assert_eq!(assembled.system_blocks[0].text, "<brain iter=7>");
    }

    #[test]
    fn assemble_custom_region_renders_even_when_empty() {
        // Static scaffolding: the script emits structure with no entries.
        let src = r#"fn render(ctx) { `<empty count=${ctx.entries.len()}>` }"#;
        let window = custom_window(src, false);
        let assembled = window.assemble();
        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(assembled.system_blocks[0].text, "<empty count=0>");
    }

    #[test]
    fn assemble_custom_conversation_takeover_renders_single_user_message() {
        // The 12-factor case: a custom region NAMED conversation holds the
        // typed history and renders it as one XML user message. No sliding
        // window exists; the request's only message is the script's.
        let src = r#"
            fn render(ctx) {
                let xml = "<context>";
                for entry in ctx.entries {
                    xml += `<event kind="${entry.kind}">${entry.content}</event>`;
                }
                xml += "</context>";
                #{ messages: [ #{ role: "user", content: xml } ] }
            }
        "#;
        let mut window = ContextWindow::new(10_000);
        window.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Custom {
                script: "conv.rhai".to_string(),
                persistent: false,
            },
            5_000,
        ));
        window.region_scripts.insert(
            "conv.rhai".to_string(),
            std::sync::Arc::new(leviath_scripting::region_hook::compile("conv.rhai", src).unwrap()),
        );
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                "do the task".to_string(),
                4,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "c1".to_string(),
                    tool_name: "shell".to_string(),
                    is_error: false,
                },
                "output".to_string(),
                2,
            )
            .unwrap();

        let assembled = window.assemble();
        assert!(assembled.system_blocks.is_empty());
        assert_eq!(assembled.messages.len(), 1);
        assert_eq!(assembled.messages[0].role, "user");
        assert_eq!(
            assembled.messages[0].content.as_text(),
            "<context><event kind=\"user_message\">do the task</event>\
             <event kind=\"tool_result\">output</event></context>"
        );
    }

    #[test]
    fn assemble_custom_script_emitting_nothing_gets_begin_fallback() {
        // A script that emits no messages leaves the request message-less;
        // the shared finalization injects the "Begin." user message.
        let window = custom_window("fn render(ctx) { \"\" }", false);
        let assembled = window.assemble();
        assert!(assembled.system_blocks.is_empty());
        assert_eq!(assembled.messages.len(), 1);
        assert_eq!(assembled.messages[0].content.as_text(), "Begin.");
    }

    #[test]
    fn assemble_custom_unpaired_tool_blocks_are_sanitized() {
        // A buggy script emits a tool_result with no matching tool_use; the
        // orphan sanitizer strips it instead of sending a provider-invalid
        // request.
        let src = r#"
            fn render(ctx) {
                #{ messages: [
                    #{ role: "user", content: "hello" },
                    #{ role: "user", tool_results: [
                        #{ tool_call_id: "ghost", content: "orphan" },
                    ] },
                ] }
            }
        "#;
        let window = custom_window(src, false);
        let assembled = window.assemble();
        assert_eq!(assembled.messages.len(), 1, "orphan tool_result stripped");
        assert_eq!(assembled.messages[0].content.as_text(), "hello");
    }

    // ─── assemble() EntryKind::Text prefix parsing ────────────────────────

    #[test]
    fn test_assemble_text_entry_with_assistant_prefix() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::Text,
                "Assistant: I can help with that.".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();

        let assistant_msgs: Vec<_> = assembled
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .collect();
        assert_eq!(assistant_msgs.len(), 1);
        assert_eq!(assistant_msgs[0].content.as_text(), "I can help with that.");
    }

    #[test]
    fn test_assemble_text_entry_with_user_prefix() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::Text,
                "User: What is Rust?".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();

        let user_msgs: Vec<_> = assembled
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content.as_text(), "What is Rust?");
    }

    #[test]
    fn test_assemble_text_entry_without_prefix_defaults_to_user() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::Text,
                "some plain text".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();

        let user_msgs: Vec<_> = assembled
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content.as_text(), "some plain text");
    }

    // ─── assemble() AssistantTurn variants ────────────────────────────────

    #[test]
    fn test_assemble_assistant_turn_with_text_and_tool_calls() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // User message first
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "Read my file".to_string(),
                10,
            )
            .unwrap();

        // Assistant with text + tool_calls
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "tc_a".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                        thought_signature: None,
                    }],
                },
                "Sure, let me read it.".to_string(),
                20,
            )
            .unwrap();

        // Matching tool result
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_a".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "fn main() {}".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();

        // Find the assistant message with blocks
        let assistant_msg = assembled
            .messages
            .iter()
            .find(|m| m.role == "assistant")
            .expect("should have assistant message");

        // Assistant turn with text + a tool call assembles to a Text block
        // followed by the ToolUse block.
        assert_eq!(
            assistant_msg.content,
            leviath_providers::MessageContent::Blocks(vec![
                leviath_providers::ContentBlock::Text {
                    text: "Sure, let me read it.".to_string(),
                },
                leviath_providers::ContentBlock::ToolUse {
                    id: "tc_a".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "foo.rs"}),
                    thought_signature: None,
                },
            ])
        );
    }

    #[test]
    fn test_assemble_assistant_turn_no_text_only_tool_calls() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // User message
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "Do it".to_string(),
                10,
            )
            .unwrap();

        // Assistant with empty text + tool_calls
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "tc_b".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::json!({"cmd": "ls"}),
                        thought_signature: None,
                    }],
                },
                "".to_string(),
                10,
            )
            .unwrap();

        // Matching tool result
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_b".to_string(),
                    tool_name: "bash".to_string(),
                    is_error: false,
                },
                "file1.rs\nfile2.rs".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();

        let assistant_msg = assembled
            .messages
            .iter()
            .find(|m| m.role == "assistant")
            .expect("should have assistant message");

        // Empty assistant text produces a single ToolUse block, no Text block.
        assert_eq!(
            assistant_msg.content,
            leviath_providers::MessageContent::Blocks(vec![
                leviath_providers::ContentBlock::ToolUse {
                    id: "tc_b".to_string(),
                    name: "bash".to_string(),
                    input: serde_json::json!({"cmd": "ls"}),
                    thought_signature: None,
                },
            ])
        );
    }

    // ─── assemble() consecutive ToolResults flushed ───────────────────────

    #[test]
    fn test_assemble_consecutive_tool_results_flushed_on_non_tool_result() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // User message
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "Run two tools".to_string(),
                10,
            )
            .unwrap();

        // Assistant with two tool calls
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![
                        leviath_core::SerializedToolCall {
                            id: "tc_1".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({"path": "a.rs"}),
                            thought_signature: None,
                        },
                        leviath_core::SerializedToolCall {
                            id: "tc_2".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({"path": "b.rs"}),
                            thought_signature: None,
                        },
                    ],
                },
                "".to_string(),
                10,
            )
            .unwrap();

        // Two consecutive ToolResults
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "content of a.rs".to_string(),
                10,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_2".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "content of b.rs".to_string(),
                10,
            )
            .unwrap();

        // Then a UserMessage (should flush the pending tool results first)
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "Now fix the bug".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();

        // Messages should be: user("Run two tools"), assistant(tool_uses),
        // user(tool_result x2), user("Now fix the bug")
        assert_eq!(assembled.messages.len(), 4);

        // The third message should be a user message with two ToolResult blocks
        let tool_result_msg = &assembled.messages[2];
        assert_eq!(tool_result_msg.role, "user");
        // The two consecutive tool results merge into one user message with two
        // ToolResult blocks, in order.
        assert_eq!(
            tool_result_msg.content,
            leviath_providers::MessageContent::Blocks(vec![
                leviath_providers::ContentBlock::ToolResult {
                    tool_use_id: "tc_1".to_string(),
                    content: "content of a.rs".to_string(),
                    is_error: false,
                },
                leviath_providers::ContentBlock::ToolResult {
                    tool_use_id: "tc_2".to_string(),
                    content: "content of b.rs".to_string(),
                    is_error: false,
                },
            ])
        );

        // The fourth message should be the user follow-up
        assert_eq!(assembled.messages[3].role, "user");
        assert_eq!(assembled.messages[3].content.as_text(), "Now fix the bug");
    }

    // ─── assemble() "Begin." fallback ─────────────────────────────────────

    #[test]
    fn test_assemble_injects_begin_when_no_user_messages() {
        let mut window = ContextWindow::new(100_000);
        // Only a Pinned region, no SlidingWindow with user messages
        let mut pinned = Region::new("system".to_string(), RegionKind::Pinned, 10_000);
        pinned
            .add_entry("You are a helpful assistant.", 20)
            .unwrap();
        window.add_region(pinned);

        let assembled = window.assemble();

        // Should have injected a "Begin." fallback user message
        assert_eq!(assembled.messages.len(), 1);
        assert_eq!(assembled.messages[0].role, "user");
        assert_eq!(assembled.messages[0].content.as_text(), "Begin.");
    }

    // ─── add_typed_entry / add_typed_tainted_to_region error paths ────────

    #[test]
    fn test_add_typed_entry_to_nonexistent_region() {
        let mut window = ContextWindow::new(10000);
        let result = window.add_typed_entry(
            "nonexistent",
            leviath_core::EntryKind::UserMessage,
            "hello".to_string(),
            10,
        );
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("nonexistent"),
            "Error should mention the missing region name"
        );
    }

    #[test]
    fn test_add_typed_tainted_to_nonexistent_region() {
        let mut window = ContextWindow::new(10000);
        let result = window.add_typed_tainted_to_region(
            "ghost",
            leviath_core::EntryKind::UserMessage,
            "data".to_string(),
            10,
            leviath_core::TaintLevel::Public,
        );
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("ghost"),
            "Error should mention the missing region name"
        );
    }

    #[test]
    fn test_assemble_tool_result_before_any_tool_use() {
        // Edge case: tool_result appears in context but no tool_use exists at all
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // A tool_result with no tool_use anywhere
        window
            .add_typed_entry(
                "conversation",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_nowhere".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "orphan result".to_string(),
                10,
            )
            .unwrap();

        let assembled = with_tracing(|| window.assemble());

        // The orphaned tool_result message is stripped to empty and dropped,
        // leaving no messages - so the "Begin." user fallback is synthesized.
        assert_eq!(assembled.messages.len(), 1);
        assert_eq!(assembled.messages[0].role, "user");
        assert_eq!(
            assembled.messages[0].content,
            leviath_providers::MessageContent::Text("Begin.".to_string())
        );
    }

    // ─── Prompt caching tests ────────────────────────────────────────────

    #[test]
    fn test_assemble_sets_cache_breakpoint_on_stable_prefix() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // Add 10 alternating user/assistant messages
        for i in 0..10 {
            let kind = if i % 2 == 0 {
                leviath_core::EntryKind::UserMessage
            } else {
                leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] }
            };
            window
                .add_typed_entry("conv", kind, format!("message {i}"), 10)
                .unwrap();
        }

        let assembled = window.assemble();
        // 10 alternating messages end on an assistant turn, so assemble appends a
        // trailing "Continue." user nudge → 11 messages.
        assert_eq!(assembled.messages.len(), 11);
        assert_eq!(assembled.messages.last().unwrap().role, "user");

        // The breakpoint is placed at the 4th-from-last of the pre-nudge run
        // (index 6 of the original 10); the nudge is appended after.
        let bp_idx = 6;
        for (i, msg) in assembled.messages.iter().enumerate() {
            if i == bp_idx {
                assert!(
                    msg.cache_breakpoint,
                    "Message at index {i} should have cache_breakpoint = true"
                );
            } else {
                assert!(
                    !msg.cache_breakpoint,
                    "Message at index {i} should have cache_breakpoint = false"
                );
            }
        }
    }

    #[test]
    fn test_assemble_cache_breakpoint_small_conversation() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // Add 3 messages (user, assistant, user)
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "Hello".to_string(),
                10,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                "Hi there".to_string(),
                10,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "How are you?".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();
        assert_eq!(assembled.messages.len(), 3);

        // With < 5 messages but >= 2, first message gets the breakpoint
        assert!(
            assembled.messages[0].cache_breakpoint,
            "First message should have cache_breakpoint in small conversation"
        );
        assert!(!assembled.messages[1].cache_breakpoint);
        assert!(!assembled.messages[2].cache_breakpoint);
    }

    #[test]
    fn test_assemble_cache_breakpoint_too_few_messages() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // Add only 1 message
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "Solo message".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();
        assert_eq!(assembled.messages.len(), 1);

        // With only 1 message, no breakpoints should be set
        assert!(
            !assembled.messages[0].cache_breakpoint,
            "Single message should not get a cache breakpoint"
        );
    }

    #[test]
    fn test_assemble_system_blocks_sorted_by_cache_stability() {
        use leviath_core::CacheHint;

        let mut window = ContextWindow::new(100_000);

        // Add regions in "wrong" order: volatile first, stable last
        let mut clearable = Region::new("scratch".to_string(), RegionKind::Clearable, 10_000);
        clearable.add_entry("clearable data", 20).unwrap();
        window.add_region(clearable);

        let mut temporary = Region::new("temp".to_string(), RegionKind::Temporary, 10_000);
        temporary.add_entry("temporary data", 20).unwrap();
        window.add_region(temporary);

        let mut compacting = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 500,
            },
            10_000,
        );
        compacting.add_entry("compacting data", 20).unwrap();
        window.add_region(compacting);

        let mut pinned = Region::new("system".to_string(), RegionKind::Pinned, 10_000);
        pinned.add_entry("pinned system prompt", 20).unwrap();
        window.add_region(pinned);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 4);

        // Verify ordering: Always (Pinned) first, UntilChanged (Compacting) second,
        // Never (Temporary, Clearable) last
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            CacheHint::Always,
            "First system block should be Always (Pinned)"
        );
        assert_eq!(
            assembled.system_blocks[1].cache_hint,
            CacheHint::UntilChanged,
            "Second system block should be UntilChanged (Compacting)"
        );
        assert_eq!(
            assembled.system_blocks[2].cache_hint,
            CacheHint::Never,
            "Third system block should be Never"
        );
        assert_eq!(
            assembled.system_blocks[3].cache_hint,
            CacheHint::Never,
            "Fourth system block should be Never"
        );
    }

    // ─── Coverage for ContextWindow typed+tainted methods ─────────────────

    #[test]
    fn test_add_typed_tainted_to_region_success() {
        let mut window = ContextWindow::new(10000);
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5000,
        );
        region.enable_taint_tracking();
        window.add_region(region);

        window
            .add_typed_tainted_to_region(
                "conv",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "secret data".to_string(),
                100,
                leviath_core::TaintLevel::Private,
            )
            .unwrap();

        assert_eq!(window.current_tokens, 100);
        assert_eq!(
            window.get_region("conv").and_then(|r| r.taint_level()),
            Some(leviath_core::TaintLevel::Private)
        );
    }

    #[test]
    fn test_add_typed_tainted_to_region_not_found() {
        let mut window = ContextWindow::new(10000);
        let result = window.add_typed_tainted_to_region(
            "nonexistent",
            leviath_core::EntryKind::Text,
            "data".to_string(),
            10,
            leviath_core::TaintLevel::Public,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_assemble_consecutive_tool_results_flushed_at_end() {
        // Tool results at the END of the region (not followed by a non-ToolResult)
        // should still be flushed into a user message.
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        window.add_region(region);

        // Add user message, then assistant with tool calls, then tool results at end
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::UserMessage,
                "do something".to_string(),
                10,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "tc_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                        thought_signature: None,
                    }],
                },
                "Let me read that".to_string(),
                10,
            )
            .unwrap();
        window
            .add_typed_entry(
                "conv",
                leviath_core::EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
                "fn main() {}".to_string(),
                10,
            )
            .unwrap();

        let assembled = window.assemble();
        // user msg + assistant (with tool_use blocks) + user (with tool_result blocks)
        assert_eq!(assembled.messages.len(), 3);
        assert_eq!(assembled.messages[2].role, "user");
        // The last message is a Blocks message carrying the single ToolResult.
        assert_eq!(
            assembled.messages[2].content,
            leviath_providers::MessageContent::Blocks(vec![
                leviath_providers::ContentBlock::ToolResult {
                    tool_use_id: "tc_1".to_string(),
                    content: "fn main() {}".to_string(),
                    is_error: false,
                },
            ])
        );
    }

    #[test]
    fn test_assemble_compact_history_with_sliding_prefix_sorting() {
        // CompactHistory should sort before Compacting/Temporary in system blocks
        use leviath_core::CacheHint;

        let mut window = ContextWindow::new(100_000);

        let mut temp = Region::new("temp".to_string(), RegionKind::Temporary, 10_000);
        temp.add_entry("temp data", 10).unwrap();
        window.add_region(temp);

        let mut history = Region::new(
            "history".to_string(),
            RegionKind::CompactHistory {
                source_region: "impl".to_string(),
            },
            10_000,
        );
        history.add_entry("summary data", 10).unwrap();
        window.add_region(history);

        let assembled = window.assemble();
        assert_eq!(assembled.system_blocks.len(), 2);
        // CompactHistory (Always) should come before Temporary (Never)
        assert_eq!(assembled.system_blocks[0].cache_hint, CacheHint::Always);
        assert_eq!(assembled.system_blocks[1].cache_hint, CacheHint::Never);
    }

    #[test]
    fn cache_hint_sort_priority_orders_by_stability() {
        use leviath_core::CacheHint;
        // Most stable first (lowest priority), volatile last.
        assert_eq!(cache_hint_sort_priority(CacheHint::Always), 0);
        assert_eq!(
            cache_hint_sort_priority(CacheHint::SlidingPrefix {
                stable_fraction: 0.75
            }),
            1
        );
        assert_eq!(cache_hint_sort_priority(CacheHint::UntilChanged), 2);
        assert_eq!(cache_hint_sort_priority(CacheHint::Never), 3);
        // The four priorities are strictly increasing by volatility.
        assert!(
            cache_hint_sort_priority(CacheHint::Always)
                < cache_hint_sort_priority(CacheHint::SlidingPrefix {
                    stable_fraction: 0.5
                })
        );
    }

    #[test]
    fn test_assemble_empty_regions_skipped() {
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(
            "system".to_string(),
            RegionKind::Pinned,
            10_000,
        ));
        // Empty pinned region should be skipped
        let assembled = window.assemble();
        assert!(assembled.system_blocks.is_empty());
    }

    #[test]
    fn test_assemble_hashmap_region_with_keys() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10_000,
        );
        region
            .upsert_by_key("src/main.rs", "fn main() {}", 10)
            .unwrap();
        region
            .upsert_by_key("src/lib.rs", "pub mod foo;", 8)
            .unwrap();
        window.add_region(region);

        let assembled = window.assemble();
        assert_eq!(assembled.system_blocks.len(), 1);
        let block_text = &assembled.system_blocks[0].text;
        assert!(block_text.contains("[files]:"));
        assert!(block_text.contains("### [src/main.rs]"));
        assert!(block_text.contains("fn main() {}"));
        assert!(block_text.contains("### [src/lib.rs]"));
        assert!(block_text.contains("pub mod foo;"));
    }

    #[test]
    fn test_assemble_hashmap_region_cache_hint() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10_000,
        );
        region.upsert_by_key("a.rs", "content", 5).unwrap();
        window.add_region(region);

        let assembled = window.assemble();
        assert_eq!(assembled.system_blocks.len(), 1);
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            leviath_core::CacheHint::UntilChanged
        );
    }

    // ─── HashMap region assembly tests ──────────────────────────────────

    #[test]
    fn test_assemble_hashmap_single_keyed_entry() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new(
            "context".to_string(),
            RegionKind::HashMap { max_entries: None },
            10_000,
        );
        region
            .upsert_by_key("config.toml", "key = \"value\"", 10)
            .unwrap();
        window.add_region(region);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 1);
        let block_text = &assembled.system_blocks[0].text;
        assert!(
            block_text.starts_with("[context]:"),
            "System block should start with [region_name]: prefix"
        );
        assert!(
            block_text.contains("### [config.toml]"),
            "Entry should have ### [key] header"
        );
        assert!(
            block_text.contains("key = \"value\""),
            "Entry content should be present"
        );
    }

    #[test]
    fn test_assemble_hashmap_multiple_keyed_entries() {
        let mut window = ContextWindow::new(100_000);
        let mut region = Region::new(
            "tracked_files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10_000,
        );
        region
            .upsert_by_key("alpha.rs", "fn alpha() {}", 10)
            .unwrap();
        region.upsert_by_key("beta.rs", "fn beta() {}", 10).unwrap();
        region
            .upsert_by_key("gamma.rs", "fn gamma() {}", 10)
            .unwrap();
        window.add_region(region);

        let assembled = window.assemble();

        assert_eq!(assembled.system_blocks.len(), 1);
        let block_text = &assembled.system_blocks[0].text;
        assert!(block_text.starts_with("[tracked_files]:"));
        assert!(block_text.contains("### [alpha.rs]"));
        assert!(block_text.contains("fn alpha() {}"));
        assert!(block_text.contains("### [beta.rs]"));
        assert!(block_text.contains("fn beta() {}"));
        assert!(block_text.contains("### [gamma.rs]"));
        assert!(block_text.contains("fn gamma() {}"));
    }

    #[test]
    fn test_assemble_hashmap_empty_region_skipped() {
        let mut window = ContextWindow::new(100_000);
        let region = Region::new(
            "empty_map".to_string(),
            RegionKind::HashMap { max_entries: None },
            10_000,
        );
        // No entries added
        window.add_region(region);

        let assembled = window.assemble();

        assert!(
            assembled.system_blocks.is_empty(),
            "Empty HashMap region should not produce a system block"
        );
    }

    #[test]
    fn test_assemble_mixed_pinned_hashmap_sliding_window() {
        use leviath_core::CacheHint;

        let mut window = ContextWindow::new(100_000);

        // Pinned region
        let mut pinned = Region::new("system".to_string(), RegionKind::Pinned, 10_000);
        pinned
            .add_entry("You are a helpful assistant.", 20)
            .unwrap();
        window.add_region(pinned);

        // HashMap region
        let mut hashmap = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10_000,
        );
        hashmap
            .upsert_by_key("main.rs", "fn main() {}", 10)
            .unwrap();
        window.add_region(hashmap);

        // SlidingWindow region with user messages
        let mut sliding = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50_000,
        );
        sliding
            .add_typed_entry("Hello there", 10, leviath_core::EntryKind::UserMessage)
            .unwrap();
        window.add_region(sliding);

        let assembled = window.assemble();

        // Pinned and HashMap should produce system blocks (2 total)
        assert_eq!(assembled.system_blocks.len(), 2);

        // System blocks sorted by cache hint: Pinned (Always) first, HashMap (UntilChanged) second
        assert_eq!(
            assembled.system_blocks[0].cache_hint,
            CacheHint::Always,
            "Pinned region should sort first (Always cache hint)"
        );
        assert!(
            assembled.system_blocks[0]
                .text
                .contains("You are a helpful assistant."),
            "First system block should be the pinned content"
        );

        assert_eq!(
            assembled.system_blocks[1].cache_hint,
            CacheHint::UntilChanged,
            "HashMap region should sort second (UntilChanged cache hint)"
        );
        assert!(
            assembled.system_blocks[1].text.starts_with("[files]:"),
            "HashMap system block should have [region_name]: prefix"
        );
        assert!(
            assembled.system_blocks[1].text.contains("### [main.rs]"),
            "HashMap system block should contain ### [key] header"
        );

        // SlidingWindow should produce messages, not system blocks
        assert!(
            assembled
                .messages
                .iter()
                .any(|m| m.role == "user" && m.content.as_text().contains("Hello there")),
            "SlidingWindow entries should appear as messages"
        );
    }

    #[test]
    fn test_add_tainted_to_region_propagates_budget_error() {
        // Region is found, but the entry exceeds its token budget, so the
        // inner `add_tainted_entry` error must propagate through the `?`.
        let mut window = ContextWindow::new(10_000);
        let mut region = Region::new("conv".to_string(), RegionKind::Temporary, 10);
        region.enable_taint_tracking();
        window.add_region(region);

        let result = window.add_tainted_to_region(
            "conv",
            "far too many tokens".to_string(),
            100,
            leviath_core::TaintLevel::Private,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_add_typed_tainted_to_region_propagates_budget_error() {
        // Region is found, but the entry exceeds its token budget, so the
        // inner `add_typed_tainted_entry` error must propagate through the `?`.
        let mut window = ContextWindow::new(10_000);
        let region = Region::new("conv".to_string(), RegionKind::Temporary, 10);
        window.add_region(region);

        let result = window.add_typed_tainted_to_region(
            "conv",
            leviath_core::EntryKind::Text,
            "far too many tokens".to_string(),
            100,
            leviath_core::TaintLevel::Public,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_assemble_hashmap_region_entry_without_key() {
        // A HashMap-region entry with no key falls back to its raw content
        // (rather than a "### [key]" header) when assembled.
        let mut window = ContextWindow::new(10_000);
        let region = Region::new(
            "kv".to_string(),
            RegionKind::HashMap { max_entries: None },
            5000,
        );
        window.add_region(region);
        // add_to_region stores the entry with key: None.
        window
            .add_to_region("kv", "keyless content".to_string(), 10)
            .unwrap();

        let assembled = window.assemble();
        assert!(
            assembled
                .system_blocks
                .iter()
                .any(|b| b.text.contains("keyless content")),
            "keyless HashMap entry should appear verbatim in a system block"
        );
    }
    #[test]
    fn shared_interner_deduplicates_pinned_text_across_windows() {
        let interner = leviath_core::ContentInterner::new();
        let mut a = ContextWindow::with_interner(10_000, interner.clone());
        let mut b = ContextWindow::with_interner(10_000, interner);
        assert!(a.interner().ptr_eq(b.interner()));
        a.add_region(Region::new("pin".into(), RegionKind::Pinned, 5_000));
        b.add_region(Region::new("pin".into(), RegionKind::Pinned, 5_000));
        let text = "shared architecture block";
        a.add_to_region("pin", text.into(), 10).unwrap();
        b.add_to_region("pin", text.into(), 10).unwrap();
        assert!(
            a.get_region("pin").unwrap().content[0]
                .shares_content_with(&b.get_region("pin").unwrap().content[0])
        );
    }
}
