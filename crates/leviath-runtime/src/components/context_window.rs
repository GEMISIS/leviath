//! The agent's context window: what it remembers, and what it forgets first.
//!
//! Every region an agent holds lives here, along with the assembly that turns
//! them into a request and the eviction that keeps them inside a budget. Split
//! out of `components` because it was two thirds of that file on its own, and
//! because "what an agent *is*" and "what an agent *remembers*" are different
//! questions to arrive with.

use super::block_cache::{
    block_hash, block_sort_priority, lifecycle_cache_hint, mark_recently_changed_run,
    push_bracketed, push_chunked, system_prefix_hash, warn_on_unstable_declaration,
};
use super::*;

mod eviction;

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
pub(crate) struct InferenceConfig {
    /// Temperature override. If None, uses 0.7 (or 0.0 if model doesn't support it).
    pub temperature: Option<f32>,
    /// The stage's output cap (`parameters.max_output_tokens`), resolved
    /// against the model and the window when each request is built. `None`
    /// caps at the model's `max_output_tokens` capability.
    pub max_output_tokens: Option<leviath_core::blueprint::OutputCap>,
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
pub(crate) struct ToolResultRoutingComponent {
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

/// Whose write a region write is.
///
/// A custom region's `on_write` hook may reject a write, and what a rejection
/// does depends on whether anyone can be told about it. An agent-origin write
/// (a `context_write`/`context_append` call, a routed tool result) has a tool
/// result to carry the refusal, so it becomes an error naming the reason. A
/// system-origin write (an assistant turn, a delivered message, a framework
/// record) has no such channel, and a script must not be able to silently
/// delete it - the rejection is downgraded to accept-unchanged plus a warning.
///
/// When classifying a new write path, `System` is the safe default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteOrigin {
    /// A write the model itself asked for; a rejection is reported back to it.
    Agent,
    /// A framework record; a rejection is stored unchanged with a warning.
    System,
}

/// What a custom region's `on_write` hook decided about one write, with the
/// original content carried through the `Refused` arm so a system-origin
/// caller can store it unchanged.
enum HookDecision {
    /// Store this content (possibly replaced), with this token count and,
    /// when the hook chose one, this key instead of the one the write named.
    Store(String, usize, Option<String>),
    /// The hook refused the write; the original entry rides along.
    Refused {
        /// The content exactly as the write carried it.
        content: String,
        /// Its original token count.
        tokens: usize,
        /// The hook's reason, when it gave one.
        reason: Option<String>,
    },
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

    /// How many times each region declared `stable` has been seen to change,
    /// so the warning about a wrong declaration is said once rather than every
    /// turn. Shared across clones because a cloned window is the same logical
    /// window and should not start warning again.
    pub unstable_declarations:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,

    /// Regions the current stage does not attend to.
    ///
    /// Held, not deleted. Dropping an omitted region from the window entirely
    /// brings it back empty when a later stage re-declares it, which makes the
    /// feature unusable for the thing it looks designed for: narrowing what one
    /// stage sees in a pipeline whose later stages still need the data.
    /// Omission means "not assembled for this stage" and nothing else.
    ///
    /// Reset on every stage entry by `crate::context_setup::apply_layout`, so
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
            unstable_declarations: Default::default(),
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
    /// entry. Non-custom regions, missing scripts, and hook failures all
    /// store the entry unchanged; what a `Refused` decision does is the
    /// origin adapters' business ([`Self::on_write_agent`],
    /// [`Self::on_write_system`]).
    ///
    /// Deliberately NOT invoked by the layout-swap carry or restore overlay:
    /// those re-add entries the hook already accepted once.
    fn on_write_decision(
        &self,
        region_name: &str,
        content: String,
        tokens: usize,
        kind: &leviath_core::EntryKind,
        key: Option<&str>,
    ) -> HookDecision {
        let Some(script) = self.custom_script_for(region_name) else {
            return HookDecision::Store(content, tokens, None);
        };
        if !script.has_on_write() {
            return HookDecision::Store(content, tokens, None);
        }
        // The region exists - custom_script_for resolved through it.
        let region = self
            .get_region(region_name)
            .expect("custom_script_for resolved through this region");
        let incoming = crate::custom_region::IncomingEntry {
            content: &content,
            tokens,
            kind,
            key,
        };
        match crate::custom_region::apply_on_write(&script, region, incoming) {
            crate::custom_region::OnWriteOutcome::Accept {
                content,
                tokens,
                key_override,
            } => HookDecision::Store(content, tokens, key_override),
            crate::custom_region::OnWriteOutcome::Reject(reason) => HookDecision::Refused {
                content,
                tokens,
                reason,
            },
        }
    }

    /// [`Self::on_write_decision`] for an agent-origin write: a refusal
    /// becomes an error carrying the hook's reason, which the tool result
    /// reports back to the model.
    fn on_write_agent(
        &self,
        region_name: &str,
        content: String,
        tokens: usize,
        kind: &leviath_core::EntryKind,
        key: Option<&str>,
    ) -> leviath_core::Result<(String, usize, Option<String>)> {
        match self.on_write_decision(region_name, content, tokens, kind, key) {
            HookDecision::Store(content, tokens, key_override) => {
                Ok((content, tokens, key_override))
            }
            HookDecision::Refused { reason, .. } => Err(leviath_core::Error::RegionRefusedWrite {
                region: region_name.to_string(),
                reason: reason
                    .unwrap_or_else(|| "the region's on_write hook declined it".to_string()),
            }),
        }
    }

    /// [`Self::on_write_decision`] for a system-origin write: a refusal is
    /// downgraded to store-unchanged plus a warning, because these writes are
    /// framework records (assistant turns, delivered messages, nudges) that a
    /// script must never be able to silently delete.
    fn on_write_system(
        &self,
        region_name: &str,
        content: String,
        tokens: usize,
        kind: &leviath_core::EntryKind,
        key: Option<&str>,
    ) -> (String, usize, Option<String>) {
        match self.on_write_decision(region_name, content, tokens, kind, key) {
            HookDecision::Store(content, tokens, key_override) => (content, tokens, key_override),
            HookDecision::Refused {
                content,
                tokens,
                reason,
            } => {
                tracing::warn!(
                    region = %region_name,
                    reason = reason.as_deref().unwrap_or("none given"),
                    "on_write rejected a system-origin write; storing unchanged \
                     (a script cannot drop framework records)"
                );
                (content, tokens, None)
            }
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
        self.add_to_region_keyed(WriteOrigin::System, region_name, None, content, tokens)
    }

    /// Add an entry that may carry a key, so the agent can name it again to
    /// release it.
    ///
    /// Routed through the same private `write_to_region` tail
    /// as the unkeyed path, so a keyed write still passes the region's
    /// `on_write` hook and still gets `on_overflow` a chance to make room.
    /// Writing keys through a shortcut instead is how they came to be honoured
    /// on one region kind and dropped on the rest.
    pub(crate) fn add_to_region_keyed(
        &mut self,
        origin: WriteOrigin,
        region_name: &str,
        key: Option<&str>,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let (content, tokens, key_override) = match origin {
            WriteOrigin::Agent => self.on_write_agent(
                region_name,
                content,
                tokens,
                &leviath_core::EntryKind::Text,
                key,
            )?,
            WriteOrigin::System => self.on_write_system(
                region_name,
                content,
                tokens,
                &leviath_core::EntryKind::Text,
                key,
            ),
        };
        let key = key_override.as_deref().or(key);
        self.write_to_region(region_name, tokens, &mut |region, tokens| match key {
            Some(k) => region.add_keyed_entry(k, content.clone(), tokens),
            None => region.add_entry(content.clone(), tokens),
        })
    }

    /// Replace a region's content with a single (possibly keyed) entry on the
    /// agent's own behalf: `context_write`'s non-hashmap arm.
    ///
    /// The `on_write` hook runs BEFORE anything is cleared, so a rejection
    /// leaves the region exactly as it was - refusing the replacement and
    /// clearing anyway would be a second way to lose content.
    pub(crate) fn agent_replace_region(
        &mut self,
        region_name: &str,
        key: Option<&str>,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let (content, tokens, key_override) = self.on_write_agent(
            region_name,
            content,
            tokens,
            &leviath_core::EntryKind::Text,
            key,
        )?;
        let Some(region) = self.get_region_mut(region_name) else {
            return Err(leviath_core::Error::RegionNotFound(region_name.to_string()));
        };
        region.clear();
        self.current_tokens = self.calculate_tokens();
        let key = key_override.as_deref().or(key);
        self.write_to_region(region_name, tokens, &mut |region, tokens| match key {
            Some(k) => region.add_keyed_entry(k, content.clone(), tokens),
            None => region.add_entry(content.clone(), tokens),
        })
    }

    /// Replace a region's entire content with a single entry (clear, then add).
    /// Returns `false` (no-op) if the region does not exist. Used to keep an
    /// authoritative document region (e.g. the plan) holding only its current
    /// version, so revisions build on it instead of accumulating stale copies.
    pub(crate) fn replace_region(
        &mut self,
        region_name: &str,
        content: String,
        tokens: usize,
    ) -> bool {
        // The replacement passes through on_write like any incoming entry - a
        // custom region's script sees (and may transform) it. These callers
        // are all framework lanes (stage seeds, transforms, interaction
        // answers), so a hook rejection is downgraded inside the adapter to
        // store-unchanged plus a warning: a script that could veto the
        // replacement could silently delete an interaction answer.
        let (content, tokens, key_override) = self.on_write_system(
            region_name,
            content,
            tokens,
            &leviath_core::EntryKind::Text,
            None,
        );
        if let Some(region) = self.get_region_mut(region_name) {
            region.clear();
            let _ = match key_override.as_deref() {
                Some(k) => region.add_keyed_entry(k, content, tokens),
                None => region.add_entry(content, tokens),
            };
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
    pub(crate) fn add_typed_entry(
        &mut self,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.add_assistant_turn(region_name, kind, content, tokens, None)
    }

    /// [`add_typed_entry`](Self::add_typed_entry) for a turn that carries an
    /// opaque provider token to replay.
    ///
    /// A separate method rather than a parameter on the shared one: only the
    /// two writers that record an assistant turn have such a token, and the
    /// other twenty callers would carry a `None` that means nothing to them.
    pub(crate) fn add_assistant_turn(
        &mut self,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
        reasoning: Option<String>,
    ) -> leviath_core::Result<()> {
        self.typed_write(
            WriteOrigin::System,
            region_name,
            kind,
            content,
            tokens,
            None,
        )?;
        // On success the entry just written is the last one: the region may
        // have evicted to make room, but it appends what it accepted. Same
        // after-the-push attachment the core's reasoning-carrying add uses.
        if reasoning.is_some()
            && let Some(region) = self.get_region_mut(region_name)
            && let Some(entry) = region.content.last_mut()
        {
            entry.reasoning = reasoning;
        }
        Ok(())
    }

    /// Shared core of the typed write methods: run the `on_write` seam with
    /// the caller's origin, then insert the entry with its kind and (when
    /// given) taint level, honouring a key override from the hook.
    pub(crate) fn typed_write(
        &mut self,
        origin: WriteOrigin,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: String,
        tokens: usize,
        taint: Option<leviath_core::TaintLevel>,
    ) -> leviath_core::Result<()> {
        let (content, tokens, key_override) = match origin {
            WriteOrigin::Agent => self.on_write_agent(region_name, content, tokens, &kind, None)?,
            WriteOrigin::System => self.on_write_system(region_name, content, tokens, &kind, None),
        };
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            match taint {
                Some(level) => {
                    region.add_typed_tainted_entry(content.clone(), tokens, kind.clone(), level)?;
                }
                None => region.add_typed_entry(content.clone(), tokens, kind.clone())?,
            }
            // A key override from the hook names the entry just pushed.
            if let Some(key) = key_override.as_deref()
                && let Some(entry) = region.content.last_mut()
            {
                entry.key = Some(key.to_string());
            }
            Ok(())
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
    pub(crate) fn calculate_tokens(&self) -> usize {
        self.regions.iter().map(|r| r.current_tokens).sum()
    }

    /// Check if the context window needs eviction.
    #[cfg(test)]
    pub(crate) fn needs_eviction(&self, threshold: f32) -> bool {
        let usage_ratio = self.current_tokens as f32 / self.max_tokens as f32;
        usage_ratio >= threshold
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
        // Messages a custom region rendered. Held apart from the conversation
        // and spliced in front of it after the loop.
        let mut preamble: Vec<leviath_providers::Message> = Vec::new();
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
                            volatility: region.volatility,
                            region: region.name.clone(),
                        });
                    }
                }
                leviath_core::RegionKind::CompactHistory { .. } => {
                    let body = region
                        .content
                        .iter()
                        .map(|e| e.content.clone())
                        .collect::<Vec<_>>();
                    push_chunked(&mut system_blocks, region, &body, CacheHint::Always);
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
                                reasoning: None,
                            });
                        }

                        match &entry.kind {
                            EntryKind::UserMessage => {
                                messages.push(leviath_providers::Message {
                                    role: "user".to_string(),
                                    content: entry.content.clone().into(),
                                    cache_breakpoint: false,
                                    reasoning: None,
                                });
                            }
                            EntryKind::AssistantTurn { tool_calls } => {
                                if tool_calls.is_empty() {
                                    messages.push(leviath_providers::Message {
                                        role: "assistant".to_string(),
                                        content: entry.content.clone().into(),
                                        cache_breakpoint: false,
                                        reasoning: entry.reasoning.clone(),
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
                                        reasoning: entry.reasoning.clone(),
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
                                        reasoning: None,
                                    });
                                } else if let Some(rest) = trimmed.strip_prefix("User: ") {
                                    messages.push(leviath_providers::Message {
                                        role: "user".to_string(),
                                        content: rest.to_string().into(),
                                        cache_breakpoint: false,
                                        reasoning: None,
                                    });
                                } else {
                                    messages.push(leviath_providers::Message {
                                        role: "user".to_string(),
                                        content: entry.content.clone().into(),
                                        cache_breakpoint: false,
                                        reasoning: None,
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
                            reasoning: None,
                        });
                    }
                }

                // Compacting / Temporary / Clearable → system blocks
                leviath_core::RegionKind::Compacting { .. } => {
                    push_bracketed(&mut system_blocks, region, CacheHint::UntilChanged);
                }
                // Both say when the region is thrown away, not how it moves
                // in between, so the hint comes from the declaration - see
                // `lifecycle_cache_hint`.
                leviath_core::RegionKind::Temporary | leviath_core::RegionKind::Clearable => {
                    let hint = lifecycle_cache_hint(region.volatility);
                    push_bracketed(&mut system_blocks, region, hint);
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
                            // Not into the conversation directly: wherever this
                            // region sits in the manifest, what it renders comes
                            // before the dialogue. See the splice below.
                            messages: &mut preamble,
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
                        volatility: region.volatility,
                        region: region.name.clone(),
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

        // ── The conversation ends the request ────────────────────────────
        //
        // A model treats the end of its input as "now", and for an agent the
        // thing that is now is the dialogue it is having. Every other region is
        // reference material, however recently it was written.
        //
        // Only a custom region can render into the conversation at all, and
        // rendering it wherever its author happened to declare it puts a region
        // declared after the conversation *behind* the last user turn. On a
        // small region that is untidy; on one holding a document corpus it
        // replaces the dialogue in the position the model weighs most heavily,
        // and the agent stops behaving like it is in a conversation.
        //
        // Splicing rather than sorting: a region declared before the
        // conversation already rendered in front of it and stays exactly where
        // it was, so this changes nothing for a blueprint that was already
        // ordered sensibly.
        if !preamble.is_empty() {
            let conversation = std::mem::replace(&mut messages, preamble);
            messages.extend(conversation);
        }

        // ── Sort system blocks for optimal prefix caching ────────────────
        //
        // A provider caches by prefix, so a block that changes invalidates
        // every block behind it: the arrangement that pays is stable content
        // first and churn last. That is what the region declared, not what its
        // kind implies - see [`leviath_core::Volatility`].
        system_blocks.sort_by_key(block_sort_priority);
        // After the sort, because the order is part of what Anthropic matches.
        let system_hash = system_prefix_hash(&system_blocks);
        let block_hashes: Vec<u64> = system_blocks.iter().map(|b| block_hash(&b.text)).collect();
        // A declaration is a hint we can falsify, not a promise: `stable` sorts a
        // region to the front, so one that is really churning does the most
        // damage possible and does it because we believed the label.
        warn_on_unstable_declaration(
            &system_blocks,
            &meta.previous_block_hashes,
            &mut leviath_core::sync::lock(&self.unstable_declarations),
        );

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
                                // Carried, not dropped. Stripping an orphaned
                                // tool block does not make the turn's opaque
                                // reasoning token any less the turn's.
                                reasoning: msg.reasoning.clone(),
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

        // ── Drop any message with nothing in it ─────────────────────────
        //
        // Every provider rejects a zero-length turn (`messages.0: user messages
        // must have non-empty content`), so one reaching the wire is a 400 the
        // runtime built for itself - and a 400 does not retry away.
        //
        // Here rather than at each writer because there are many writers and
        // one request. A block-shaped message that emptied out is already
        // dropped by the tool-pair sanitizer above; this catches the text-shaped
        // ones, and it runs before the two guards below so that a conversation
        // left empty by the drop still gets its fallback turn.
        messages.retain(|m| match &m.content {
            leviath_providers::MessageContent::Text(text) => !text.trim().is_empty(),
            leviath_providers::MessageContent::Blocks(blocks) => !blocks.is_empty(),
        });

        // Ensure there's at least one user message
        if !messages.iter().any(|m| m.role == "user") {
            messages.push(leviath_providers::Message {
                role: "user".to_string(),
                content: "Begin.".into(),
                cache_breakpoint: false,
                reasoning: None,
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
                reasoning: None,
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
        self.typed_write(
            WriteOrigin::System,
            region_name,
            leviath_core::EntryKind::Text,
            content,
            tokens,
            Some(taint_level),
        )
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
    pub(crate) fn taint_summary(&self) -> Vec<(String, leviath_core::TaintLevel)> {
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
            volatility: leviath_core::Volatility::default(),
            region: String::new(),
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
            super::block_cache::cache_hint_sort_priority(CacheHint::RecentlyChanged),
            super::block_cache::cache_hint_sort_priority(CacheHint::UntilChanged)
        );
        assert_eq!(
            super::block_cache::cache_hint_sort_priority(CacheHint::RecentlyChanged),
            2
        );
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
    fn an_assistant_turns_reasoning_blob_survives_into_the_assembled_message() {
        // A stateless backend keeps no server-side thread, so the chain of
        // thought lives only in this blob. If assembly drops it the run still
        // works and quietly pays to re-derive its reasoning every turn.
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::default(),
            },
            10_000,
        ));
        window
            .add_assistant_turn(
                "conversation",
                leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                "the answer".to_string(),
                10,
                Some("sealed-blob".to_string()),
            )
            .expect("the write fits");

        let assembled = window.assemble();
        let turn = assembled
            .messages
            .iter()
            .find(|m| m.role == "assistant")
            .expect("the assistant turn");
        assert_eq!(turn.reasoning.as_deref(), Some("sealed-blob"));
    }

    #[test]
    fn a_turn_with_tool_calls_carries_its_reasoning_blob_too() {
        // The other arm of the assembly match: a turn that called tools takes
        // the block-content path and must not lose the blob on the way.
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::default(),
            },
            10_000,
        ));
        window
            .add_assistant_turn(
                "conversation",
                leviath_core::EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    }],
                },
                "looking".to_string(),
                10,
                Some("sealed-blob".to_string()),
            )
            .expect("the write fits");

        let assembled = window.assemble();
        let turn = assembled
            .messages
            .iter()
            .find(|m| m.role == "assistant")
            .expect("the assistant turn");
        assert_eq!(turn.reasoning.as_deref(), Some("sealed-blob"));
    }

    #[test]
    fn a_turn_from_a_provider_with_no_reasoning_carries_none() {
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::default(),
            },
            10_000,
        ));
        window
            .add_assistant_turn(
                "conversation",
                leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                "plain".to_string(),
                10,
                None,
            )
            .expect("the write fits");
        assert!(
            window
                .assemble()
                .messages
                .iter()
                .all(|m| m.reasoning.is_none())
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

    /// The property the whole mechanism exists for: churn must not sit in front
    /// of stable content, whatever order the regions were declared in.
    ///
    /// A provider caches by prefix, so a block that changes invalidates every
    /// block behind it. Declared worst-first - a rewritten region, then a
    /// growing one, then the immutable task - the prompt must still come out
    /// stable-first. Measured before this existed: 0 cacheable tokens of 3,021.
    #[test]
    fn a_declared_prompt_orders_itself_stable_first() {
        let mut window = ContextWindow::new(1_000_000);

        let mut scratch = Region::new("scratch".to_string(), RegionKind::Pinned, 100_000);
        scratch.volatility = leviath_core::Volatility::Rewritten;
        scratch.add_entry("state".to_string(), 5).expect("fits");
        window.add_region(scratch);

        let mut history = Region::new("history".to_string(), RegionKind::Pinned, 100_000);
        history.volatility = leviath_core::Volatility::Grows;
        history.add_entry("a finding".to_string(), 5).expect("fits");
        window.add_region(history);

        let mut task = Region::new("task".to_string(), RegionKind::Pinned, 100_000);
        task.volatility = leviath_core::Volatility::Stable;
        task.add_entry("do the thing".to_string(), 5).expect("fits");
        window.add_region(task);

        let order: Vec<String> = window
            .assemble()
            .system_blocks
            .iter()
            .map(|b| b.region.clone())
            .collect();
        assert_eq!(order, vec!["task", "history", "scratch"]);
    }

    /// A region that changes must not cost the stable content in front of it.
    #[test]
    fn churn_behind_stable_content_leaves_it_cacheable() {
        let mut window = ContextWindow::new(1_000_000);
        let mut task = Region::new("task".to_string(), RegionKind::Pinned, 100_000);
        task.volatility = leviath_core::Volatility::Stable;
        task.add_entry("instructions ".repeat(200), 700)
            .expect("fits");
        window.add_region(task);
        let mut scratch = Region::new("scratch".to_string(), RegionKind::Pinned, 100_000);
        scratch.volatility = leviath_core::Volatility::Rewritten;
        // Seeded, because an empty region contributes no block at all and the
        // comparison below is between two requests that both have one.
        scratch
            .add_entry("first state".to_string(), 5)
            .expect("fits");
        window.add_region(scratch);

        let first = window.assemble();
        // The scratch region is rebuilt, as such a region is every turn.
        window
            .regions
            .iter_mut()
            .find(|r| r.name == "scratch")
            .expect("declared above")
            .clear();
        window
            .add_to_region("scratch", "new state".to_string(), 5)
            .expect("fits");

        let second = window.assemble_with_meta(&crate::custom_region::AssembleMeta {
            stage_name: "work".to_string(),
            stage_iterations: 1,
            model: "m".to_string(),
            previous_system_hash: Some(first.system_hash),
            previous_block_hashes: first.block_hashes.clone(),
        });
        // The property that matters to a prefix cache: the stable content is
        // still first and still byte-identical, so everything a provider stores
        // up to and including it can be read back. The churn is behind it, where
        // it invalidates only itself.
        assert_eq!(second.system_blocks[0].region, "task");
        assert_eq!(second.block_hashes[0], first.block_hashes[0]);
        assert_ne!(
            second.block_hashes[1], first.block_hashes[1],
            "the fixture is meant to churn the second region"
        );
    }

    /// Chunking is what a growing region gets. A stable one is already a single
    /// boundary, and splitting a rewritten one buys nothing because no boundary
    /// inside it survives.
    #[test]
    fn only_a_growing_region_is_split_into_chunks() {
        let entries = || {
            (0..20)
                .map(|i| format!("{i}: {}", "word ".repeat(200)))
                .collect::<Vec<_>>()
        };
        let blocks_for = |volatility| {
            let mut window = ContextWindow::new(2_000_000);
            let mut region = Region::new("notes".to_string(), RegionKind::Pinned, 1_000_000);
            region.volatility = volatility;
            for entry in entries() {
                region.add_entry(entry, 250).expect("fits");
            }
            window.add_region(region);
            window.assemble().system_blocks.len()
        };

        assert!(blocks_for(leviath_core::Volatility::Grows) > 1);
        assert_eq!(blocks_for(leviath_core::Volatility::Stable), 1);
        assert_eq!(blocks_for(leviath_core::Volatility::Rewritten), 1);
    }

    /// A compacting region large enough to span chunks says which region each
    /// continuation belongs to, and keeps every entry.
    #[test]
    fn a_compacting_region_labels_its_continuations() {
        let mut window = ContextWindow::new(2_000_000);
        let mut history = Region::new(
            "history".to_string(),
            RegionKind::Compacting {
                threshold_tokens: usize::MAX,
            },
            1_000_000,
        );
        // Only a region that says it grows is split into chunks.
        history.volatility = leviath_core::Volatility::Grows;
        window.add_region(history);
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
