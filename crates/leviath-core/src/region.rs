//! Memory region types and validation schemas.
//!
//! Regions are typed sections of an agent's context window with different lifecycle
//! policies. This module defines the region kinds, content storage, and validation
//! schemas that enforce content format requirements.

use serde::{Deserialize, Serialize};

/// The kind of content stored in a region entry.
///
/// Entries carry typed metadata instead of relying on text-prefix parsing
/// (e.g., "Assistant: " / "User: ") to determine message roles. This
/// eliminates the bug where tool results stored outside the conversation
/// region all become "user" role messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum EntryKind {
    /// Plain text (system content, summaries, scratch).
    #[default]
    Text,
    /// User message in conversation.
    UserMessage,
    /// Assistant response with optional tool calls.
    AssistantTurn { tool_calls: Vec<SerializedToolCall> },
    /// Tool execution result, paired with a tool_call_id.
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },
}

/// A serialized tool call stored within an `AssistantTurn` entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerializedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Opaque provider token that must be replayed with this call
    /// (Gemini's `thought_signature`). Persisted so it survives a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Eviction strategy for `SlidingWindow` regions.
///
/// Controls how entries are removed when the window exceeds its `max_items` limit.
/// The choice of strategy affects prompt caching effectiveness: PerItem eviction
/// shifts the message prefix every iteration (breaking cache), while Bulk and
/// Compact keep the prefix stable between eviction events.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum EvictionStrategy {
    /// Evict one turn group at a time (current behavior). Default.
    #[default]
    PerItem,
    /// Evict in bulk when items exceed max + overflow.
    /// Between bulk evictions, the prefix stays stable for caching.
    Bulk {
        /// How many items over max_items before triggering a bulk eviction.
        /// When triggered, evicts items back down to max_items.
        overflow: usize,
    },
    /// Summarize oldest entries when threshold is hit (requires external LLM call).
    /// The region stores a `pending_compaction` flag; the runtime checks this
    /// and performs compaction externally.
    Compact {
        /// Number of oldest entries to compact into a summary when triggered.
        compact_count: usize,
    },
}

/// A typed memory region within an agent's context window.
///
/// Regions have different lifecycle policies controlling how they behave
/// when the context window fills up. This is inspired by hardware memory
/// architectures like SNES VRAM, where different memory regions serve
/// distinct purposes with their own access patterns and constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RegionKind {
    /// Never evicted or compacted. Architecture diagrams, constraints, identity.
    ///
    /// Like SNES OAM (Object Attribute Memory) - fixed format, always present.
    /// Use for content that defines the agent's core identity, constraints,
    /// and architectural understanding. This content persists for the entire
    /// agent lifecycle.
    Pinned,

    /// Maintains the last N items, oldest rolls off. Conversation history.
    ///
    /// Like a ring buffer with configurable size. When the buffer is full,
    /// the oldest item is removed to make room for new content. Use for
    /// conversation history or any sequential data where recent items
    /// are most relevant.
    SlidingWindow {
        /// Maximum number of items to retain in the window
        max_items: usize,
        /// Strategy used to evict entries when the window is full
        eviction_strategy: EvictionStrategy,
    },

    /// First to be evicted when space is needed. Tool outputs, intermediate results.
    ///
    /// Cheapest to regenerate, lowest priority to keep. Use for content that
    /// can be easily regenerated or has low value after immediate use, such as
    /// tool execution results or temporary computations.
    Temporary,

    /// Compacts (summarizes) when threshold is hit, then cleared.
    ///
    /// When token count exceeds the threshold, the region's content is summarized
    /// and moved to a paired CompactHistory region, then the original Compacting
    /// region is completely cleared, giving fresh capacity.
    Compacting {
        /// Token count that triggers compaction
        threshold_tokens: usize,
    },

    /// Wiped entirely in one shot when space is needed. All-or-nothing eviction.
    ///
    /// Unlike Temporary (which evicts oldest entries one at a time), Clearable
    /// regions are dumped completely and immediately when eviction is needed.
    /// Use for scratch space or temporary working data where partial results
    /// are useless.
    Clearable,

    /// Receives summaries from paired Compacting regions, never evicted.
    ///
    /// When a Compacting region hits its threshold and summarizes, the summary
    /// moves here. CompactHistory regions hold compressed knowledge indefinitely
    /// and are never evicted. Can also support sliding window behavior (oldest
    /// summaries drop off) and re-compaction (combine multiple summaries).
    CompactHistory {
        /// Name of the source Compacting region
        source_region: String,
    },

    /// Key-value region where entries are indexed by string key.
    /// Writing with an existing key replaces that entry (upsert semantics).
    /// When over token budget, evicts least-recently-updated entries (LRU).
    HashMap {
        /// Optional maximum number of keys
        max_entries: Option<usize>,
    },
}

impl PartialEq for RegionKind {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Pinned, Self::Pinned)
            | (Self::Temporary, Self::Temporary)
            | (Self::Clearable, Self::Clearable) => true,
            (
                Self::SlidingWindow {
                    max_items: a,
                    eviction_strategy: sa,
                },
                Self::SlidingWindow {
                    max_items: b,
                    eviction_strategy: sb,
                },
            ) => a == b && sa == sb,
            (
                Self::Compacting {
                    threshold_tokens: a,
                },
                Self::Compacting {
                    threshold_tokens: b,
                },
            ) => a == b,
            (
                Self::CompactHistory { source_region: a },
                Self::CompactHistory { source_region: b },
            ) => a == b,
            (Self::HashMap { max_entries: a }, Self::HashMap { max_entries: b }) => a == b,
            _ => false,
        }
    }
}
impl Eq for RegionKind {}

impl RegionKind {
    /// Return the cache hint appropriate for this region kind.
    pub fn cache_hint(&self) -> crate::cache::CacheHint {
        match self {
            RegionKind::Pinned | RegionKind::CompactHistory { .. } => {
                crate::cache::CacheHint::Always
            }
            RegionKind::Compacting { .. } => crate::cache::CacheHint::UntilChanged,
            RegionKind::SlidingWindow { .. } => crate::cache::CacheHint::SlidingPrefix {
                stable_fraction: 0.75,
            },
            RegionKind::HashMap { .. } => crate::cache::CacheHint::UntilChanged,
            RegionKind::Temporary | RegionKind::Clearable => crate::cache::CacheHint::Never,
        }
    }
}

/// A single region in the context window with its content and metadata.
///
/// Each region tracks its own token budget, current usage, and optional
/// validation schema to enforce content format requirements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    /// Unique name identifying this region
    pub name: String,

    /// Lifecycle policy for this region
    pub kind: RegionKind,

    /// Content entries stored in this region
    pub content: Vec<RegionEntry>,

    /// Maximum tokens allowed in this region
    pub max_tokens: usize,

    /// Current token count
    pub current_tokens: usize,

    /// Optional validation schema enforcing content format
    pub schema: Option<RegionSchema>,

    /// Taint tracking state. Present when taint tracking is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<crate::taint::RegionTaint>,

    /// When true, the Compact eviction strategy has determined that oldest
    /// entries should be summarized. The runtime checks this flag and
    /// performs the compaction externally (requires an LLM call).
    #[serde(default)]
    pub needs_message_compaction: bool,
}

impl Region {
    /// Create a new region with the specified configuration.
    pub fn new(name: String, kind: RegionKind, max_tokens: usize) -> Self {
        Self {
            name,
            kind,
            content: Vec::new(),
            max_tokens,
            current_tokens: 0,
            schema: None,
            taint: None,
            needs_message_compaction: false,
        }
    }

    /// Enable taint tracking for this region.
    pub fn with_taint_tracking(mut self) -> Self {
        self.taint = Some(crate::taint::RegionTaint::new());
        self
    }

    /// Enable taint tracking on this region (mutable).
    pub fn enable_taint_tracking(&mut self) {
        if self.taint.is_none() {
            self.taint = Some(crate::taint::RegionTaint::new());
        }
    }

    /// Get the current taint level of this region, if taint tracking is enabled.
    pub fn taint_level(&self) -> Option<crate::taint::TaintLevel> {
        self.taint.as_ref().map(|t| t.level())
    }

    /// Add an entry with a taint level. Used when taint tracking is enabled.
    pub fn add_tainted_entry(
        &mut self,
        content: String,
        tokens: usize,
        taint_level: crate::taint::TaintLevel,
    ) -> crate::error::Result<()> {
        // Validate against schema if present
        if let Some(schema) = &self.schema {
            schema.validate(&content)?;
        }

        // Check token budget
        if self.current_tokens + tokens > self.max_tokens {
            return Err(crate::error::Error::TokenBudgetExceeded {
                used: self.current_tokens + tokens,
                max: self.max_tokens,
            });
        }

        // Add entry
        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: None,
            kind: EntryKind::default(),
            key: None,
        });
        self.current_tokens += tokens;

        // Update taint tracking
        if let Some(taint) = &mut self.taint {
            taint.add_entry(taint_level);
        }

        // Enforce SlidingWindow max_items limit
        self.enforce_sliding_window();

        Ok(())
    }

    /// Add a typed entry with a taint level.
    ///
    /// Combines [`add_typed_entry`](Self::add_typed_entry) (the entry carries a
    /// typed [`EntryKind`] so eviction can group turns) with
    /// [`add_tainted_entry`](Self::add_tainted_entry) (the entry contributes a
    /// specific taint level rather than defaulting to `Public`). Used for tool
    /// results when taint tracking is enabled, so a sensitive tool's output
    /// both keeps its `ToolResult` kind and raises the region's taint level.
    pub fn add_typed_tainted_entry(
        &mut self,
        content: String,
        tokens: usize,
        kind: EntryKind,
        taint_level: crate::taint::TaintLevel,
    ) -> crate::error::Result<()> {
        // Validate against schema if present
        if let Some(schema) = &self.schema {
            schema.validate(&content)?;
        }

        // Check token budget
        if self.current_tokens + tokens > self.max_tokens {
            return Err(crate::error::Error::TokenBudgetExceeded {
                used: self.current_tokens + tokens,
                max: self.max_tokens,
            });
        }

        // Add entry
        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: None,
            kind,
            key: None,
        });
        self.current_tokens += tokens;

        // Update taint tracking with the supplied level
        if let Some(taint) = &mut self.taint {
            taint.add_entry(taint_level);
        }

        // Enforce SlidingWindow max_items limit
        self.enforce_sliding_window();

        Ok(())
    }

    /// Add a validation schema to this region.
    pub fn with_schema(mut self, schema: RegionSchema) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Add an entry to this region.
    ///
    /// Validates content against schema if present, checks token budget,
    /// and adds the entry to the region.
    pub fn add_entry(&mut self, content: String, tokens: usize) -> crate::error::Result<()> {
        // Validate against schema if present
        if let Some(schema) = &self.schema {
            schema.validate(&content)?;
        }

        // Check token budget
        if self.current_tokens + tokens > self.max_tokens {
            return Err(crate::error::Error::TokenBudgetExceeded {
                used: self.current_tokens + tokens,
                max: self.max_tokens,
            });
        }

        // Add entry
        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: None,
            kind: EntryKind::default(),
            key: None,
        });
        self.current_tokens += tokens;

        // Track taint as Public for untagged entries
        if let Some(taint) = &mut self.taint {
            taint.add_entry(crate::taint::TaintLevel::Public);
        }

        // Enforce SlidingWindow max_items limit
        self.enforce_sliding_window();

        Ok(())
    }

    /// Add an entry with metadata.
    pub fn add_entry_with_metadata(
        &mut self,
        content: String,
        tokens: usize,
        metadata: serde_json::Value,
    ) -> crate::error::Result<()> {
        // Validate against schema if present
        if let Some(schema) = &self.schema {
            schema.validate(&content)?;
        }

        // Check token budget
        if self.current_tokens + tokens > self.max_tokens {
            return Err(crate::error::Error::TokenBudgetExceeded {
                used: self.current_tokens + tokens,
                max: self.max_tokens,
            });
        }

        // Add entry
        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: Some(metadata),
            kind: EntryKind::default(),
            key: None,
        });
        self.current_tokens += tokens;

        // Track taint as Public for untagged entries
        if let Some(taint) = &mut self.taint {
            taint.add_entry(crate::taint::TaintLevel::Public);
        }

        // Enforce SlidingWindow max_items limit
        self.enforce_sliding_window();

        Ok(())
    }

    /// Add an entry with a specific [`EntryKind`] to this region.
    ///
    /// Like [`add_entry`](Self::add_entry), but the caller supplies the entry
    /// kind so the entry carries typed metadata rather than relying on
    /// text-prefix parsing.
    pub fn add_typed_entry(
        &mut self,
        content: String,
        tokens: usize,
        kind: EntryKind,
    ) -> crate::error::Result<()> {
        // Validate against schema if present
        if let Some(schema) = &self.schema {
            schema.validate(&content)?;
        }

        // Check token budget
        if self.current_tokens + tokens > self.max_tokens {
            return Err(crate::error::Error::TokenBudgetExceeded {
                used: self.current_tokens + tokens,
                max: self.max_tokens,
            });
        }

        // Add entry
        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: None,
            kind,
            key: None,
        });
        self.current_tokens += tokens;

        // Track taint as Public for untagged entries
        if let Some(taint) = &mut self.taint {
            taint.add_entry(crate::taint::TaintLevel::Public);
        }

        // Enforce SlidingWindow max_items limit
        self.enforce_sliding_window();

        Ok(())
    }

    /// Upsert an entry by key. If key exists, replace content and update timestamp/tokens.
    /// If key doesn't exist, add new entry. Enforces max_tokens and max_entries via LRU eviction.
    pub fn upsert_by_key(
        &mut self,
        key: &str,
        content: String,
        tokens: usize,
    ) -> Result<(), String> {
        // If key exists, update in place
        if let Some(pos) = self
            .content
            .iter()
            .position(|e| e.key.as_deref() == Some(key))
        {
            let old_tokens = self.content[pos].tokens;
            self.current_tokens -= old_tokens;
            self.content[pos].content = content;
            self.content[pos].tokens = tokens;
            self.content[pos].timestamp = chrono::Utc::now().timestamp();
            self.current_tokens += tokens;
            return Ok(());
        }

        // Enforce max_entries via LRU eviction
        let max_entries = if let RegionKind::HashMap {
            max_entries: Some(max),
        } = &self.kind
        {
            Some(*max)
        } else {
            None
        };
        if let Some(max) = max_entries {
            while self.content.len() >= max {
                self.evict_lru_entry();
            }
        }

        // Enforce max_tokens via LRU eviction
        while self.current_tokens + tokens > self.max_tokens && !self.content.is_empty() {
            self.evict_lru_entry();
        }

        if self.current_tokens + tokens > self.max_tokens {
            return Err(format!(
                "Entry ({} tokens) exceeds region budget ({} max)",
                tokens, self.max_tokens
            ));
        }

        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: None,
            kind: EntryKind::default(),
            key: Some(key.to_string()),
        });
        self.current_tokens += tokens;
        Ok(())
    }

    /// Get entry by key.
    pub fn get_by_key(&self, key: &str) -> Option<&RegionEntry> {
        self.content.iter().find(|e| e.key.as_deref() == Some(key))
    }

    /// Remove entry by key.
    pub fn remove_by_key(&mut self, key: &str) -> bool {
        if let Some(pos) = self
            .content
            .iter()
            .position(|e| e.key.as_deref() == Some(key))
        {
            let tokens = self.content[pos].tokens;
            self.content.remove(pos);
            self.current_tokens -= tokens;
            if let Some(taint) = &mut self.taint {
                taint.remove_at(pos);
            }
            true
        } else {
            false
        }
    }

    /// List all keys in this region.
    pub fn keys(&self) -> Vec<&str> {
        self.content
            .iter()
            .filter_map(|e| e.key.as_deref())
            .collect()
    }

    /// Evict the least-recently-updated entry (LRU) for HashMap regions.
    fn evict_lru_entry(&mut self) {
        if self.content.is_empty() {
            return;
        }
        let oldest_idx = self
            .content
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.timestamp)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let tokens = self.content[oldest_idx].tokens;
        self.content.remove(oldest_idx);
        self.current_tokens -= tokens;
        if let Some(taint) = &mut self.taint {
            taint.remove_at(oldest_idx);
        }
    }

    /// Enforce the SlidingWindow max_items limit by removing oldest entries.
    ///
    /// Behaviour depends on the configured [`EvictionStrategy`]:
    /// - **PerItem** – evict one turn group at a time (original behaviour).
    /// - **Bulk** – only evict when `len > max_items + overflow`, then evict
    ///   down to `max_items`. Between bulk evictions the prefix is stable,
    ///   which preserves Anthropic prompt-cache keys.
    /// - **Compact** – set `needs_message_compaction` when `len > max_items + compact_count`.
    ///   If the runtime hasn't compacted and `len > max_items + compact_count * 2`,
    ///   fall back to bulk eviction to prevent unbounded growth.
    fn enforce_sliding_window(&mut self) {
        if let RegionKind::SlidingWindow {
            max_items,
            eviction_strategy,
        } = &self.kind
        {
            let max = *max_items;
            match eviction_strategy.clone() {
                EvictionStrategy::PerItem => {
                    // `remove_oldest` only returns None when empty, which the
                    // `len > max >= 0` guard already precludes; folding it into
                    // the condition keeps the guard without a dead break arm.
                    while self.content.len() > max && self.remove_oldest().is_some() {}
                }
                EvictionStrategy::Bulk { overflow } => {
                    if self.content.len() > max + overflow {
                        while self.content.len() > max && self.remove_oldest().is_some() {}
                    }
                }
                EvictionStrategy::Compact { compact_count } => {
                    if self.content.len() > max + compact_count * 2 {
                        // Fallback: runtime hasn't compacted, bulk-evict to prevent
                        // unbounded growth.
                        while self.content.len() > max && self.remove_oldest().is_some() {}
                        self.needs_message_compaction = false;
                    } else if self.content.len() > max + compact_count {
                        self.needs_message_compaction = true;
                    }
                }
            }
        }
    }

    /// Returns the number of entries in the turn group starting at `idx`.
    ///
    /// A turn group is:
    /// - A single Text or UserMessage entry (group size = 1)
    /// - An AssistantTurn followed by consecutive ToolResult entries
    ///   (group size = 1 + number of following ToolResults)
    /// - A lone ToolResult (shouldn't happen, but size = 1 for safety)
    fn turn_group_size_at(&self, idx: usize) -> usize {
        if idx >= self.content.len() {
            return 0;
        }
        match &self.content[idx].kind {
            EntryKind::AssistantTurn { .. } => {
                let mut size = 1;
                while idx + size < self.content.len() {
                    if matches!(self.content[idx + size].kind, EntryKind::ToolResult { .. }) {
                        size += 1;
                    } else {
                        break;
                    }
                }
                size
            }
            _ => 1,
        }
    }

    /// Clear all content from this region.
    pub fn clear(&mut self) {
        self.content.clear();
        self.current_tokens = 0;
        if let Some(taint) = &mut self.taint {
            taint.clear();
        }
    }

    /// Remove the oldest entry (for Temporary regions).
    pub fn remove_oldest(&mut self) -> Option<RegionEntry> {
        if self.content.is_empty() {
            return None;
        }
        // Respect turn groups: an AssistantTurn with tool_calls must be
        // evicted together with its following ToolResult entries to avoid
        // orphaned tool_use/tool_result blocks that providers reject.
        let group_size = self.turn_group_size_at(0);
        let mut first = None;
        let mut extra_tokens = 0usize;
        // `group_size <= content.len()`, so the window never empties mid-group;
        // the `!is_empty()` guard lives in the loop condition (no dead break arm).
        let mut i = 0;
        while i < group_size && !self.content.is_empty() {
            let entry_tokens = self.content[0].tokens;
            self.current_tokens -= entry_tokens;
            let removed = self.content.remove(0);
            if let Some(taint) = &mut self.taint {
                taint.remove_oldest();
            }
            if i == 0 {
                first = Some(removed);
            } else {
                extra_tokens += entry_tokens;
            }
            i += 1;
        }
        // Embed extra group tokens in the returned entry so callers that use
        // `entry.tokens` to adjust their own totals account for the full group.
        // `first` is `Some` whenever we removed anything (guaranteed by the
        // non-empty early return), so `map` always runs; `extra_tokens` is 0
        // for a single-entry group, making the add a no-op there.
        first.map(|mut entry| {
            entry.tokens += extra_tokens;
            entry
        })
    }

    /// Remove all entries whose content starts with the given prefix.
    ///
    /// Used to clear tagged entries (e.g. stage instructions) before injecting
    /// replacements, so stale instructions don't accumulate across stage
    /// transitions.
    pub fn remove_entries_by_prefix(&mut self, prefix: &str) {
        let mut i = 0;
        while i < self.content.len() {
            if self.content[i].content.starts_with(prefix) {
                let tokens = self.content[i].tokens;
                self.content.remove(i);
                self.current_tokens -= tokens;
                if let Some(taint) = &mut self.taint {
                    taint.remove_at(i);
                }
            } else {
                i += 1;
            }
        }
    }

    /// Get the number of entries in this region.
    pub fn entry_count(&self) -> usize {
        self.content.len()
    }

    /// Check if region needs compaction (for Compacting regions).
    pub fn needs_compaction(&self) -> bool {
        if let RegionKind::Compacting { threshold_tokens } = self.kind {
            self.current_tokens > threshold_tokens
        } else {
            false
        }
    }
}

/// A single entry within a region.
///
/// Each entry has content and metadata tracking its token usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionEntry {
    /// The actual content of this entry
    pub content: String,

    /// Token count for this entry
    pub tokens: usize,

    /// Timestamp when this entry was added
    pub timestamp: i64,

    /// Optional metadata about this entry
    pub metadata: Option<serde_json::Value>,

    /// The kind of content stored in this entry.
    /// Defaults to `EntryKind::Text` for backward compatibility with
    /// serialized data that predates the typed-entry system.
    #[serde(default)]
    pub kind: EntryKind,

    /// Optional key for HashMap regions. When set, upsert semantics apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// Validation schema for a region's content.
///
/// Enforces that content matches expected format (e.g., mermaid diagrams only,
/// JSON only, code only). Schemas can include multiple validators that are
/// checked when content is added to a region.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegionSchema {
    /// Expected content format
    pub format: ContentFormat,

    /// Optional custom validation script (Rhai)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_script: Option<String>,
}

impl Clone for RegionSchema {
    fn clone(&self) -> Self {
        Self {
            format: self.format.clone(),
            custom_script: self.custom_script.clone(),
        }
    }
}

impl RegionSchema {
    /// Create a new schema with the specified format.
    pub fn new(format: ContentFormat) -> Self {
        Self {
            format,
            custom_script: None,
        }
    }

    /// Add a custom validation script.
    pub fn with_custom_script(mut self, script: String) -> Self {
        self.custom_script = Some(script);
        self
    }

    /// Validate content against this schema.
    pub fn validate(&self, content: &str) -> crate::error::Result<()> {
        match &self.format {
            ContentFormat::Json => {
                serde_json::from_str::<serde_json::Value>(content).map_err(|e| {
                    crate::error::Error::ValidationFailed(format!("Invalid JSON: {}", e))
                })?;
            }
            ContentFormat::Mermaid => {
                // Basic mermaid syntax validation
                if !content.contains("graph")
                    && !content.contains("sequenceDiagram")
                    && !content.contains("classDiagram")
                    && !content.contains("stateDiagram")
                    && !content.contains("erDiagram")
                    && !content.contains("journey")
                    && !content.contains("gantt")
                    && !content.contains("pie")
                    && !content.contains("flowchart")
                {
                    return Err(crate::error::Error::ValidationFailed(
                        "Mermaid diagrams must contain a valid diagram type (graph, sequenceDiagram, etc.)".to_string()
                    ));
                }
            }
            ContentFormat::Code { .. } => {
                // Basic code validation - just check it's not empty
                if content.trim().is_empty() {
                    return Err(crate::error::Error::ValidationFailed(
                        "Code cannot be empty".to_string(),
                    ));
                }
            }
            ContentFormat::Markdown => {
                // Markdown is very permissive, just check it's not empty
                if content.trim().is_empty() {
                    return Err(crate::error::Error::ValidationFailed(
                        "Markdown content cannot be empty".to_string(),
                    ));
                }
            }
            ContentFormat::Text | ContentFormat::Custom { .. } => {
                // Text has no restrictions, Custom is handled by scripting layer
            }
        }

        Ok(())
    }
}

/// Content format types that can be enforced via schemas.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContentFormat {
    /// Plain text, no formatting requirements
    Text,

    /// Valid JSON
    Json,

    /// Mermaid diagram syntax
    Mermaid,

    /// Source code in a specific language
    Code { language: String },

    /// Markdown formatted text
    Markdown,

    /// Custom format with user-defined validation
    Custom { format_name: String },
}

/// Trait for content validators.
///
/// Validators check whether content meets specific requirements before
/// it's added to a region. This enables enforcing architectural constraints
/// like "only mermaid diagrams in the architecture region".
pub trait Validator: Send + Sync {
    /// Validate content and return an error message if invalid.
    fn validate(&self, content: &str) -> std::result::Result<(), crate::error::ValidationError>;

    /// Get a description of what this validator checks.
    fn description(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_region_creation() {
        let region = Region::new("test".to_string(), RegionKind::Pinned, 1000);
        assert_eq!(region.name, "test");
        assert_eq!(region.max_tokens, 1000);
        assert_eq!(region.current_tokens, 0);
    }

    #[test]
    fn test_sliding_window_config() {
        let kind = RegionKind::SlidingWindow {
            max_items: 10,
            eviction_strategy: EvictionStrategy::PerItem,
        };
        let region = Region::new("history".to_string(), kind.clone(), 5000);
        assert_eq!(region.kind, kind);
    }

    #[test]
    fn test_region_kind_equality() {
        assert_eq!(RegionKind::Clearable, RegionKind::Clearable);
        assert_eq!(
            RegionKind::Compacting {
                threshold_tokens: 500
            },
            RegionKind::Compacting {
                threshold_tokens: 500
            }
        );
        assert_eq!(
            RegionKind::CompactHistory {
                source_region: "conv".to_string()
            },
            RegionKind::CompactHistory {
                source_region: "conv".to_string()
            }
        );
        assert_ne!(RegionKind::Pinned, RegionKind::Temporary);
    }

    #[test]
    fn test_sliding_window_enforces_max_items() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 3,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50000,
        );

        region.add_entry("msg1".to_string(), 10).unwrap();
        region.add_entry("msg2".to_string(), 20).unwrap();
        region.add_entry("msg3".to_string(), 30).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.current_tokens, 60);

        // Adding a 4th entry should evict the oldest
        region.add_entry("msg4".to_string(), 40).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.content[0].content, "msg2");
        assert_eq!(region.content[2].content, "msg4");
        assert_eq!(region.current_tokens, 90); // 20 + 30 + 40

        // Adding a 5th entry should evict again
        region.add_entry("msg5".to_string(), 50).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.content[0].content, "msg3");
        assert_eq!(region.current_tokens, 120); // 30 + 40 + 50
    }

    #[test]
    fn test_sliding_window_enforces_max_items_with_metadata() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 2,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50000,
        );

        region
            .add_entry_with_metadata("a".to_string(), 10, serde_json::json!({"idx": 1}))
            .unwrap();
        region
            .add_entry_with_metadata("b".to_string(), 20, serde_json::json!({"idx": 2}))
            .unwrap();
        region
            .add_entry_with_metadata("c".to_string(), 30, serde_json::json!({"idx": 3}))
            .unwrap();

        assert_eq!(region.entry_count(), 2);
        assert_eq!(region.content[0].content, "b");
        assert_eq!(region.content[1].content, "c");
        assert_eq!(region.current_tokens, 50);
    }

    #[test]
    fn test_cache_hint_pinned() {
        let kind = RegionKind::Pinned;
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::Always);
    }

    #[test]
    fn test_cache_hint_compact_history() {
        let kind = RegionKind::CompactHistory {
            source_region: "conv".to_string(),
        };
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::Always);
    }

    #[test]
    fn test_cache_hint_compacting() {
        let kind = RegionKind::Compacting {
            threshold_tokens: 1000,
        };
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::UntilChanged);
    }

    #[test]
    fn test_cache_hint_sliding_window() {
        let kind = RegionKind::SlidingWindow {
            max_items: 10,
            eviction_strategy: EvictionStrategy::PerItem,
        };
        assert_eq!(
            kind.cache_hint(),
            crate::cache::CacheHint::SlidingPrefix {
                stable_fraction: 0.75
            }
        );
    }

    #[test]
    fn test_cache_hint_temporary() {
        assert_eq!(
            RegionKind::Temporary.cache_hint(),
            crate::cache::CacheHint::Never
        );
    }

    #[test]
    fn test_cache_hint_clearable() {
        assert_eq!(
            RegionKind::Clearable.cache_hint(),
            crate::cache::CacheHint::Never
        );
    }

    // ─── Region::with_schema / add_entry schema + budget checks ────────────

    #[test]
    fn test_with_schema_attaches_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        assert!(region.schema.is_some());
    }

    #[test]
    fn test_add_entry_rejects_content_failing_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        let result = region.add_entry("not json".to_string(), 10);
        assert!(result.is_err());
        assert_eq!(region.entry_count(), 0);
    }

    #[test]
    fn test_add_entry_accepts_content_passing_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        let result = region.add_entry("{\"a\":1}".to_string(), 10);
        assert!(result.is_ok());
        assert_eq!(region.entry_count(), 1);
    }

    #[test]
    fn test_add_entry_rejects_over_budget() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 10);
        let result = region.add_entry("too much".to_string(), 20);
        assert_eq!(
            result.unwrap_err().to_string(),
            "Content exceeds token budget: 20 > 10"
        );
        assert_eq!(region.entry_count(), 0);
    }

    #[test]
    fn test_add_entry_with_metadata_rejects_content_failing_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        let result =
            region.add_entry_with_metadata("not json".to_string(), 10, serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_add_entry_with_metadata_rejects_over_budget() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 10);
        let result =
            region.add_entry_with_metadata("too much".to_string(), 20, serde_json::json!({}));
        assert_eq!(
            result.unwrap_err().to_string(),
            "Content exceeds token budget: 20 > 10"
        );
    }

    #[test]
    fn test_add_entry_with_metadata_stores_metadata() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        region
            .add_entry_with_metadata("hello".to_string(), 5, serde_json::json!({"k": "v"}))
            .unwrap();
        assert_eq!(
            region.content[0].metadata,
            Some(serde_json::json!({"k": "v"}))
        );
    }

    // ─── clear / remove_oldest / needs_compaction ──────────────────────────

    #[test]
    fn test_clear_removes_all_content_and_resets_tokens() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        region.add_entry("a".to_string(), 10).unwrap();
        region.add_entry("b".to_string(), 20).unwrap();
        assert_eq!(region.entry_count(), 2);

        region.clear();
        assert_eq!(region.entry_count(), 0);
        assert_eq!(region.current_tokens, 0);
    }

    #[test]
    fn test_remove_oldest_returns_and_removes_first_entry() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        region.add_entry("first".to_string(), 10).unwrap();
        region.add_entry("second".to_string(), 20).unwrap();

        let removed = region.remove_oldest().unwrap();
        assert_eq!(removed.content, "first");
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.current_tokens, 20);
    }

    #[test]
    fn test_remove_oldest_returns_none_when_empty() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        assert!(region.remove_oldest().is_none());
    }

    #[test]
    fn test_needs_compaction_true_when_over_threshold() {
        let mut region = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 10,
            },
            1000,
        );
        region.add_entry("x".to_string(), 20).unwrap();
        assert!(region.needs_compaction());
    }

    #[test]
    fn test_needs_compaction_false_when_under_threshold() {
        let mut region = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 100,
            },
            1000,
        );
        region.add_entry("x".to_string(), 20).unwrap();
        assert!(!region.needs_compaction());
    }

    #[test]
    fn test_needs_compaction_false_for_non_compacting_kind() {
        let region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        assert!(!region.needs_compaction());
    }

    // ─── RegionSchema::with_custom_script ──────────────────────────────────

    #[test]
    fn test_region_schema_with_custom_script() {
        let schema = RegionSchema::new(ContentFormat::Custom {
            format_name: "special".to_string(),
        })
        .with_custom_script("validate_special()".to_string());
        assert_eq!(schema.custom_script.as_deref(), Some("validate_special()"));
    }

    // ─── RegionSchema::validate - every ContentFormat branch ───────────────

    #[test]
    fn test_validate_json_valid() {
        let schema = RegionSchema::new(ContentFormat::Json);
        assert!(schema.validate("{\"a\": 1}").is_ok());
    }

    #[test]
    fn test_validate_json_invalid() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let err = schema.validate("not json").unwrap_err();
        assert!(err.to_string().starts_with("Region validation failed:"));
    }

    #[test]
    fn test_validate_mermaid_valid() {
        let schema = RegionSchema::new(ContentFormat::Mermaid);
        assert!(schema.validate("graph TD\nA-->B").is_ok());
    }

    #[test]
    fn test_validate_mermaid_all_recognized_diagram_types() {
        let schema = RegionSchema::new(ContentFormat::Mermaid);
        for kind in [
            "graph",
            "sequenceDiagram",
            "classDiagram",
            "stateDiagram",
            "erDiagram",
            "journey",
            "gantt",
            "pie",
            "flowchart",
        ] {
            assert!(schema.validate(&format!("{} content", kind)).is_ok());
        }
    }

    #[test]
    fn test_validate_mermaid_invalid() {
        let schema = RegionSchema::new(ContentFormat::Mermaid);
        let err = schema.validate("just some text").unwrap_err();
        assert!(err.to_string().starts_with("Region validation failed:"));
    }

    #[test]
    fn test_validate_code_non_empty_is_ok() {
        let schema = RegionSchema::new(ContentFormat::Code {
            language: "rust".to_string(),
        });
        assert!(schema.validate("fn main() {}").is_ok());
    }

    #[test]
    fn test_validate_code_empty_is_error() {
        let schema = RegionSchema::new(ContentFormat::Code {
            language: "rust".to_string(),
        });
        let err = schema.validate("   ").unwrap_err();
        assert!(err.to_string().starts_with("Region validation failed:"));
    }

    #[test]
    fn test_validate_markdown_non_empty_is_ok() {
        let schema = RegionSchema::new(ContentFormat::Markdown);
        assert!(schema.validate("# Heading").is_ok());
    }

    #[test]
    fn test_validate_markdown_empty_is_error() {
        let schema = RegionSchema::new(ContentFormat::Markdown);
        let err = schema.validate("").unwrap_err();
        assert!(err.to_string().starts_with("Region validation failed:"));
    }

    #[test]
    fn test_validate_text_has_no_restrictions() {
        let schema = RegionSchema::new(ContentFormat::Text);
        assert!(schema.validate("").is_ok());
        assert!(schema.validate("anything at all").is_ok());
    }

    #[test]
    fn test_validate_custom_has_no_restrictions_here() {
        let schema = RegionSchema::new(ContentFormat::Custom {
            format_name: "special".to_string(),
        });
        // Custom format validation is deferred to the scripting layer -
        // this schema's own validate() is a no-op for it.
        assert!(schema.validate("").is_ok());
        assert!(schema.validate("whatever").is_ok());
    }

    // ─── RegionSchema Clone impl ────────────────────────────────────────────

    #[test]
    fn test_region_schema_clone_preserves_fields() {
        let schema = RegionSchema::new(ContentFormat::Text).with_custom_script("s".to_string());
        let cloned = schema.clone();
        assert_eq!(cloned.custom_script.as_deref(), Some("s"));
        assert_eq!(cloned.format, ContentFormat::Text);
    }

    // ─── Region taint tracking ──────────────────────────────────────────────

    #[test]
    fn test_region_with_taint_tracking() {
        let region =
            Region::new("test".to_string(), RegionKind::Temporary, 1000).with_taint_tracking();
        assert!(region.taint.is_some());
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
    }

    #[test]
    fn test_region_without_taint_tracking() {
        let region = Region::new("test".to_string(), RegionKind::Temporary, 1000);
        assert!(region.taint.is_none());
        assert_eq!(region.taint_level(), None);
    }

    #[test]
    fn test_enable_taint_tracking() {
        let mut region = Region::new("test".to_string(), RegionKind::Temporary, 1000);
        assert!(region.taint.is_none());
        region.enable_taint_tracking();
        assert!(region.taint.is_some());
        // Calling again is a no-op
        region.enable_taint_tracking();
        assert!(region.taint.is_some());
    }

    #[test]
    fn test_add_tainted_entry() {
        let mut region =
            Region::new("test".to_string(), RegionKind::Temporary, 1000).with_taint_tracking();
        region
            .add_tainted_entry(
                "secret data".to_string(),
                10,
                crate::taint::TaintLevel::Private,
            )
            .unwrap();
        assert_eq!(
            region.taint_level(),
            Some(crate::taint::TaintLevel::Private)
        );
        assert_eq!(region.entry_count(), 1);
    }

    #[test]
    fn test_add_tainted_entry_validates_schema() {
        let mut region = Region::new("test".to_string(), RegionKind::Temporary, 1000)
            .with_taint_tracking()
            .with_schema(RegionSchema::new(ContentFormat::Json));
        let result = region.add_tainted_entry(
            "not json".to_string(),
            10,
            crate::taint::TaintLevel::Internal,
        );
        assert!(result.is_err());
        assert_eq!(region.entry_count(), 0);
    }

    #[test]
    fn test_add_tainted_entry_checks_budget() {
        let mut region =
            Region::new("test".to_string(), RegionKind::Temporary, 10).with_taint_tracking();
        let result = region.add_tainted_entry(
            "too much".to_string(),
            20,
            crate::taint::TaintLevel::Internal,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_add_entry_tracks_taint_as_public() {
        let mut region =
            Region::new("test".to_string(), RegionKind::Temporary, 1000).with_taint_tracking();
        region.add_entry("public data".to_string(), 10).unwrap();
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
    }

    #[test]
    fn test_taint_recovery_on_remove_oldest() {
        let mut region =
            Region::new("test".to_string(), RegionKind::Temporary, 1000).with_taint_tracking();
        region
            .add_tainted_entry("private".to_string(), 10, crate::taint::TaintLevel::Private)
            .unwrap();
        region
            .add_tainted_entry("public".to_string(), 10, crate::taint::TaintLevel::Public)
            .unwrap();
        assert_eq!(
            region.taint_level(),
            Some(crate::taint::TaintLevel::Private)
        );

        region.remove_oldest(); // removes private entry
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
    }

    #[test]
    fn test_taint_recovery_on_clear() {
        let mut region =
            Region::new("test".to_string(), RegionKind::Temporary, 1000).with_taint_tracking();
        region
            .add_tainted_entry("private".to_string(), 10, crate::taint::TaintLevel::Private)
            .unwrap();
        region.clear();
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
    }

    #[test]
    fn test_taint_recovery_on_sliding_window_eviction() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 2,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50000,
        )
        .with_taint_tracking();

        region
            .add_tainted_entry("private".to_string(), 10, crate::taint::TaintLevel::Private)
            .unwrap();
        region
            .add_tainted_entry("public1".to_string(), 10, crate::taint::TaintLevel::Public)
            .unwrap();
        assert_eq!(
            region.taint_level(),
            Some(crate::taint::TaintLevel::Private)
        );

        // Third entry evicts the private one
        region
            .add_tainted_entry("public2".to_string(), 10, crate::taint::TaintLevel::Public)
            .unwrap();
        assert_eq!(region.entry_count(), 2);
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
    }

    #[test]
    fn test_taint_field_not_serialized_when_none() {
        let region = Region::new("test".to_string(), RegionKind::Temporary, 1000);
        let json = serde_json::to_string(&region).unwrap();
        assert!(!json.contains("taint"));
    }

    #[test]
    fn test_taint_field_deserialized_as_none_when_missing() {
        let json = r#"{"name":"test","kind":"Temporary","content":[],"max_tokens":1000,"current_tokens":0,"schema":null}"#;
        let region: Region = serde_json::from_str(json).unwrap();
        assert!(region.taint.is_none());
    }

    #[test]
    fn test_add_typed_tainted_entry() {
        let mut region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            1000,
        )
        .with_taint_tracking();

        region
            .add_typed_tainted_entry(
                "secret data".to_string(),
                10,
                EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "calendar".to_string(),
                    is_error: false,
                },
                crate::taint::TaintLevel::Private,
            )
            .unwrap();

        assert_eq!(region.content.len(), 1);
        assert_eq!(
            region.content[0].kind,
            EntryKind::ToolResult {
                tool_call_id: "tc_1".to_string(),
                tool_name: "calendar".to_string(),
                is_error: false,
            }
        );
        assert_eq!(
            region.taint_level(),
            Some(crate::taint::TaintLevel::Private)
        );
    }

    /// The replay token survives persistence, and archives written before the
    /// field existed still load (`#[serde(default)]`) - a restart must not
    /// strand a Gemini run on a missing signature or fail on an old run dir.
    #[test]
    fn serialized_tool_call_round_trips_thought_signature_and_reads_old_json() {
        let with = SerializedToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "ls"}),
            thought_signature: Some("sig".into()),
        };
        let json = serde_json::to_string(&with).unwrap();
        let back: SerializedToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back.thought_signature.as_deref(), Some("sig"));

        // Pre-field JSON (what every existing run dir contains).
        let old = r#"{"id":"c2","name":"shell","arguments":{}}"#;
        let back: SerializedToolCall = serde_json::from_str(old).unwrap();
        assert_eq!(back.thought_signature, None);

        // And a `None` signature serializes to the old shape, so new writes
        // stay readable by anything parsing the documented format.
        let without = SerializedToolCall {
            id: "c3".into(),
            name: "shell".into(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        };
        assert!(
            !serde_json::to_string(&without)
                .unwrap()
                .contains("thought_signature")
        );
    }

    #[test]
    fn test_add_typed_tainted_entry_checks_budget() {
        let mut region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            5,
        )
        .with_taint_tracking();

        let result = region.add_typed_tainted_entry(
            "too large".to_string(),
            100,
            EntryKind::ToolResult {
                tool_call_id: "tc_1".to_string(),
                tool_name: "tool".to_string(),
                is_error: false,
            },
            crate::taint::TaintLevel::Internal,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_add_typed_tainted_entry_validates_schema() {
        let mut region = Region::new("test".to_string(), RegionKind::Pinned, 1000)
            .with_taint_tracking()
            .with_schema(RegionSchema::new(ContentFormat::Json));

        // Non-JSON content should fail validation
        let result = region.add_typed_tainted_entry(
            "not json".to_string(),
            5,
            EntryKind::Text,
            crate::taint::TaintLevel::Public,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_add_typed_tainted_entry_without_taint_tracking() {
        // When taint tracking is NOT enabled, add_typed_tainted_entry still works
        // but the taint level is not tracked
        let mut region = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 100,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            1000,
        );
        // No .with_taint_tracking()

        region
            .add_typed_tainted_entry(
                "data".to_string(),
                10,
                EntryKind::Text,
                crate::taint::TaintLevel::Private,
            )
            .unwrap();

        assert_eq!(region.content.len(), 1);
        assert_eq!(region.taint_level(), None); // no tracking
    }

    // ─── turn_group_size_at ────────────────────────────────────────────────

    #[test]
    fn test_turn_group_size_at_assistant_with_tool_results() {
        let mut region = Region::new("conv".to_string(), RegionKind::Temporary, 50000);
        region
            .add_typed_entry(
                "assistant response".to_string(),
                10,
                EntryKind::AssistantTurn {
                    tool_calls: vec![
                        SerializedToolCall {
                            id: "tc_1".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        },
                        SerializedToolCall {
                            id: "tc_2".to_string(),
                            name: "write_file".to_string(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        },
                    ],
                },
            )
            .unwrap();
        region
            .add_typed_entry(
                "result 1".to_string(),
                5,
                EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
            )
            .unwrap();
        region
            .add_typed_entry(
                "result 2".to_string(),
                5,
                EntryKind::ToolResult {
                    tool_call_id: "tc_2".to_string(),
                    tool_name: "write_file".to_string(),
                    is_error: false,
                },
            )
            .unwrap();

        assert_eq!(region.turn_group_size_at(0), 3);
    }

    #[test]
    fn test_turn_group_size_at_assistant_at_end() {
        let mut region = Region::new("conv".to_string(), RegionKind::Temporary, 50000);
        region
            .add_typed_entry(
                "assistant with no tools".to_string(),
                10,
                EntryKind::AssistantTurn { tool_calls: vec![] },
            )
            .unwrap();

        assert_eq!(region.turn_group_size_at(0), 1);
    }

    #[test]
    fn test_turn_group_size_at_out_of_bounds() {
        let region = Region::new("conv".to_string(), RegionKind::Temporary, 50000);
        assert_eq!(region.turn_group_size_at(0), 0);
        assert_eq!(region.turn_group_size_at(99), 0);
    }

    #[test]
    fn test_turn_group_size_at_non_assistant_entries() {
        let mut region = Region::new("conv".to_string(), RegionKind::Temporary, 50000);
        region
            .add_typed_entry("hello".to_string(), 5, EntryKind::Text)
            .unwrap();
        region
            .add_typed_entry("hi".to_string(), 5, EntryKind::UserMessage)
            .unwrap();
        region
            .add_typed_entry(
                "orphan result".to_string(),
                5,
                EntryKind::ToolResult {
                    tool_call_id: "tc_x".to_string(),
                    tool_name: "tool".to_string(),
                    is_error: false,
                },
            )
            .unwrap();

        assert_eq!(region.turn_group_size_at(0), 1); // Text
        assert_eq!(region.turn_group_size_at(1), 1); // UserMessage
        assert_eq!(region.turn_group_size_at(2), 1); // ToolResult (orphan)
    }

    // ─── remove_oldest with turn group eviction ────────────────────────────

    #[test]
    fn test_remove_oldest_evicts_entire_turn_group() {
        let mut region = Region::new("conv".to_string(), RegionKind::Temporary, 50000);
        // AssistantTurn with 2 tool calls
        region
            .add_typed_entry(
                "assistant".to_string(),
                100,
                EntryKind::AssistantTurn {
                    tool_calls: vec![
                        SerializedToolCall {
                            id: "tc_1".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        },
                        SerializedToolCall {
                            id: "tc_2".to_string(),
                            name: "list_dir".to_string(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        },
                    ],
                },
            )
            .unwrap();
        region
            .add_typed_entry(
                "result 1".to_string(),
                30,
                EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "read_file".to_string(),
                    is_error: false,
                },
            )
            .unwrap();
        region
            .add_typed_entry(
                "result 2".to_string(),
                20,
                EntryKind::ToolResult {
                    tool_call_id: "tc_2".to_string(),
                    tool_name: "list_dir".to_string(),
                    is_error: false,
                },
            )
            .unwrap();
        // A trailing user message that should survive
        region
            .add_typed_entry("user msg".to_string(), 10, EntryKind::UserMessage)
            .unwrap();

        assert_eq!(region.entry_count(), 4);
        assert_eq!(region.current_tokens, 160);

        let removed = region.remove_oldest().unwrap();
        // The returned entry is the AssistantTurn, with tokens adjusted to
        // include the extra tokens from the 2 ToolResult entries.
        assert_eq!(removed.content, "assistant");
        assert_eq!(removed.tokens, 100 + 30 + 20); // 150
        // Only the user message remains
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.content[0].content, "user msg");
        assert_eq!(region.current_tokens, 10);
    }

    // ─── remove_oldest with taint tracking and turn group ──────────────────

    #[test]
    fn test_remove_oldest_turn_group_calls_taint_remove_for_each_entry() {
        let mut region =
            Region::new("conv".to_string(), RegionKind::Temporary, 50000).with_taint_tracking();

        // AssistantTurn (Private) + 1 ToolResult (Internal) + 1 trailing Public entry
        region
            .add_typed_tainted_entry(
                "assistant".to_string(),
                10,
                EntryKind::AssistantTurn {
                    tool_calls: vec![SerializedToolCall {
                        id: "tc_1".to_string(),
                        name: "tool".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    }],
                },
                crate::taint::TaintLevel::Private,
            )
            .unwrap();
        region
            .add_typed_tainted_entry(
                "result".to_string(),
                5,
                EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "tool".to_string(),
                    is_error: false,
                },
                crate::taint::TaintLevel::Internal,
            )
            .unwrap();
        region
            .add_tainted_entry(
                "public stuff".to_string(),
                5,
                crate::taint::TaintLevel::Public,
            )
            .unwrap();

        assert_eq!(
            region.taint_level(),
            Some(crate::taint::TaintLevel::Private)
        );
        assert_eq!(region.taint.as_ref().unwrap().entry_count(), 3);

        // Evict the turn group (AssistantTurn + ToolResult)
        let removed = region.remove_oldest().unwrap();
        assert_eq!(removed.content, "assistant");
        assert_eq!(region.entry_count(), 1);
        // Taint should have called remove_oldest twice (once per group member),
        // leaving only the Public entry's taint.
        assert_eq!(region.taint.as_ref().unwrap().entry_count(), 1);
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
    }

    // ─── enforce_sliding_window with turn group ────────────────────────────

    #[test]
    fn test_sliding_window_evicts_entire_turn_group() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 3,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50000,
        );

        // Add an AssistantTurn + 2 ToolResults = 3 entries (fills the window)
        region
            .add_typed_entry(
                "assistant".to_string(),
                10,
                EntryKind::AssistantTurn {
                    tool_calls: vec![
                        SerializedToolCall {
                            id: "tc_1".to_string(),
                            name: "t1".to_string(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        },
                        SerializedToolCall {
                            id: "tc_2".to_string(),
                            name: "t2".to_string(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        },
                    ],
                },
            )
            .unwrap();
        region
            .add_typed_entry(
                "r1".to_string(),
                5,
                EntryKind::ToolResult {
                    tool_call_id: "tc_1".to_string(),
                    tool_name: "t1".to_string(),
                    is_error: false,
                },
            )
            .unwrap();
        region
            .add_typed_entry(
                "r2".to_string(),
                5,
                EntryKind::ToolResult {
                    tool_call_id: "tc_2".to_string(),
                    tool_name: "t2".to_string(),
                    is_error: false,
                },
            )
            .unwrap();

        assert_eq!(region.entry_count(), 3);

        // Adding a 4th entry should evict the entire turn group (3 entries)
        // because the group at index 0 is an AssistantTurn with 2 ToolResults.
        region
            .add_typed_entry("user msg".to_string(), 15, EntryKind::UserMessage)
            .unwrap();

        // After eviction: only the new user message remains
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.content[0].content, "user msg");
        assert_eq!(region.current_tokens, 15);
    }

    // ─── add_entry_with_metadata with taint tracking ───────────────────────

    #[test]
    fn test_add_entry_with_metadata_tracks_taint_as_public() {
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_taint_tracking();

        region
            .add_entry_with_metadata("content".to_string(), 10, serde_json::json!({"key": "val"}))
            .unwrap();

        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
        assert_eq!(region.taint.as_ref().unwrap().entry_count(), 1);
        assert_eq!(
            region.taint.as_ref().unwrap().entry_taint(0),
            Some(crate::taint::TaintLevel::Public)
        );
    }

    // ─── add_typed_entry with taint tracking ───────────────────────────────

    #[test]
    fn test_add_typed_entry_tracks_taint_as_public() {
        let mut region =
            Region::new("conv".to_string(), RegionKind::Temporary, 1000).with_taint_tracking();

        region
            .add_typed_entry(
                "assistant response".to_string(),
                10,
                EntryKind::AssistantTurn { tool_calls: vec![] },
            )
            .unwrap();

        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
        assert_eq!(region.taint.as_ref().unwrap().entry_count(), 1);
        assert_eq!(
            region.taint.as_ref().unwrap().entry_taint(0),
            Some(crate::taint::TaintLevel::Public)
        );
    }

    // ─── EvictionStrategy tests ───────────────────────────────────────────

    #[test]
    fn test_per_item_strategy_evicts_one_at_a_time() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 3,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            50000,
        );
        for i in 0..5 {
            region.add_entry(format!("msg{}", i), 10).unwrap();
        }
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.content[0].content, "msg2");
        assert_eq!(region.content[1].content, "msg3");
        assert_eq!(region.content[2].content, "msg4");
    }

    #[test]
    fn test_bulk_eviction_triggers_on_overflow() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 5,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 3 },
            },
            50000,
        );
        // Add 8 entries: 5 (max) + 3 (overflow) = 8, which does NOT trigger
        // because the check is > not >=.
        for i in 0..8 {
            region.add_entry(format!("msg{}", i), 10).unwrap();
        }
        assert_eq!(region.entry_count(), 8);

        // Adding one more (9 total > 5+3=8) triggers bulk eviction → down to 5
        region.add_entry("msg8".to_string(), 10).unwrap();
        assert_eq!(region.entry_count(), 5);
        assert_eq!(region.content[0].content, "msg4");
    }

    #[test]
    fn test_bulk_eviction_respects_turn_groups() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 3,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 2 },
            },
            50000,
        );
        // Add AssistantTurn + ToolResult (turn group of 2)
        region
            .add_typed_entry(
                "assistant".to_string(),
                10,
                EntryKind::AssistantTurn {
                    tool_calls: vec![SerializedToolCall {
                        id: "tc1".to_string(),
                        name: "tool".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    }],
                },
            )
            .unwrap();
        region
            .add_typed_entry(
                "result".to_string(),
                5,
                EntryKind::ToolResult {
                    tool_call_id: "tc1".to_string(),
                    tool_name: "tool".to_string(),
                    is_error: false,
                },
            )
            .unwrap();
        // Add more entries to exceed overflow
        region.add_entry("msg2".to_string(), 10).unwrap();
        region.add_entry("msg3".to_string(), 10).unwrap();
        region.add_entry("msg4".to_string(), 10).unwrap();
        // 5 entries, under overflow (5 < 3+2=5 is not >), no eviction yet
        assert_eq!(region.entry_count(), 5);

        // Adding 6th entry: 6 > 5 triggers bulk eviction
        region.add_entry("msg5".to_string(), 10).unwrap();
        // Turn group (assistant+result=2) evicted together, then msg2 evicted
        // to get down to max_items=3
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.content[0].content, "msg3");
    }

    #[test]
    fn test_bulk_eviction_under_overflow_no_eviction() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 5,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 3 },
            },
            50000,
        );
        // Add exactly max_items + overflow - 1 = 7 entries
        for i in 0..7 {
            region.add_entry(format!("msg{}", i), 10).unwrap();
        }
        // 7 <= 8 (5+3), so no eviction
        assert_eq!(region.entry_count(), 7);
    }

    #[test]
    fn test_compact_sets_needs_message_compaction_flag() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 5,
                eviction_strategy: EvictionStrategy::Compact { compact_count: 3 },
            },
            50000,
        );
        assert!(!region.needs_message_compaction);

        // Add 9 entries: > max_items(5) + compact_count(3) = 8
        for i in 0..9 {
            region.add_entry(format!("msg{}", i), 10).unwrap();
        }
        assert!(region.needs_message_compaction);
        // No entries were evicted - compaction flag is set for the runtime
        assert_eq!(region.entry_count(), 9);
    }

    #[test]
    fn test_compact_fallback_to_bulk_eviction() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 5,
                eviction_strategy: EvictionStrategy::Compact { compact_count: 3 },
            },
            50000,
        );
        // Add enough entries to exceed 2x threshold:
        // > max_items(5) + compact_count(3) * 2 = 11
        for i in 0..12 {
            region.add_entry(format!("msg{}", i), 10).unwrap();
        }
        // Should have bulk-evicted down to max_items=5
        assert_eq!(region.entry_count(), 5);
        assert_eq!(region.content[0].content, "msg7");
        // Compaction flag should be cleared after fallback
        assert!(!region.needs_message_compaction);
    }

    #[test]
    fn test_eviction_strategy_default_is_per_item() {
        assert_eq!(EvictionStrategy::default(), EvictionStrategy::PerItem);
    }

    #[test]
    fn test_remove_entries_by_prefix() {
        let mut region = Region::new("system".to_string(), RegionKind::Pinned, 50000);
        region
            .add_entry("[Stage instructions: Be terse.]".to_string(), 10)
            .unwrap();
        region
            .add_entry("Core identity block".to_string(), 20)
            .unwrap();
        region
            .add_entry("[Stage instructions: Be verbose.]".to_string(), 15)
            .unwrap();

        assert_eq!(region.entry_count(), 3);
        region.remove_entries_by_prefix("[Stage instructions:");
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.content[0].content, "Core identity block");
        assert_eq!(region.current_tokens, 20);
    }

    #[test]
    fn test_remove_entries_by_prefix_with_taint_tracking() {
        let mut region =
            Region::new("system".to_string(), RegionKind::Pinned, 50000).with_taint_tracking();
        region
            .add_tainted_entry(
                "[Stage instructions: Be terse.]".to_string(),
                10,
                crate::taint::TaintLevel::Private,
            )
            .unwrap();
        region
            .add_tainted_entry(
                "Core identity block".to_string(),
                20,
                crate::taint::TaintLevel::Public,
            )
            .unwrap();
        region
            .add_tainted_entry(
                "[Stage instructions: Be verbose.]".to_string(),
                15,
                crate::taint::TaintLevel::Internal,
            )
            .unwrap();

        assert_eq!(region.entry_count(), 3);
        assert_eq!(
            region.taint_level(),
            Some(crate::taint::TaintLevel::Private)
        );

        region.remove_entries_by_prefix("[Stage instructions:");
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.content[0].content, "Core identity block");
        assert_eq!(region.current_tokens, 20);
        // After removing Private and Internal entries, only Public remains
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
        assert_eq!(region.taint.as_ref().unwrap().entry_count(), 1);
    }

    #[test]
    fn test_compact_below_threshold_no_flag() {
        // When entries are <= max_items + compact_count, no flag should be set
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 5,
                eviction_strategy: EvictionStrategy::Compact { compact_count: 3 },
            },
            50000,
        );
        for i in 0..8 {
            region.add_entry(format!("msg{}", i), 10).unwrap();
        }
        // 8 == max_items(5) + compact_count(3), not >, so no flag
        assert!(!region.needs_message_compaction);
        assert_eq!(region.entry_count(), 8);
    }

    #[test]
    fn test_bulk_eviction_with_taint_tracking() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow {
                max_items: 3,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 2 },
            },
            50000,
        )
        .with_taint_tracking();

        // Add 5 entries (3+2): at threshold, no eviction
        region
            .add_tainted_entry("private".to_string(), 10, crate::taint::TaintLevel::Private)
            .unwrap();
        for i in 1..5 {
            region
                .add_tainted_entry(format!("pub{}", i), 10, crate::taint::TaintLevel::Public)
                .unwrap();
        }
        assert_eq!(region.entry_count(), 5);

        // 6th entry triggers bulk eviction to max_items=3
        region
            .add_tainted_entry("pub5".to_string(), 10, crate::taint::TaintLevel::Public)
            .unwrap();
        assert_eq!(region.entry_count(), 3);
        // Private entry was evicted, only public remain
        assert_eq!(region.taint_level(), Some(crate::taint::TaintLevel::Public));
    }

    #[test]
    fn test_eviction_strategy_serde_roundtrip() {
        let bulk = EvictionStrategy::Bulk { overflow: 5 };
        let json = serde_json::to_string(&bulk).unwrap();
        let parsed: EvictionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, bulk);

        let compact = EvictionStrategy::Compact { compact_count: 10 };
        let json = serde_json::to_string(&compact).unwrap();
        let parsed: EvictionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, compact);

        let per_item = EvictionStrategy::PerItem;
        let json = serde_json::to_string(&per_item).unwrap();
        let parsed: EvictionStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, per_item);
    }

    #[test]
    fn test_sliding_window_kind_equality_with_eviction_strategy() {
        assert_eq!(
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 3 },
            },
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 3 },
            }
        );
        assert_ne!(
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 3 },
            }
        );
    }

    #[test]
    fn test_needs_message_compaction_default_false() {
        let region = Region::new("conv".to_string(), RegionKind::Temporary, 1000);
        assert!(!region.needs_message_compaction);
    }

    // ─── add_typed_entry schema + budget edge cases ───────────────────────

    #[test]
    fn test_add_typed_entry_validates_schema() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000)
            .with_schema(RegionSchema::new(ContentFormat::Json));
        let result = region.add_typed_entry("not json".to_string(), 5, EntryKind::Text);
        assert!(result.is_err());
        assert_eq!(region.entry_count(), 0);
    }

    #[test]
    fn test_add_typed_entry_checks_budget() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 10);
        let result = region.add_typed_entry("too big".to_string(), 20, EntryKind::UserMessage);
        assert!(result.is_err());
        assert_eq!(region.entry_count(), 0);
    }

    #[test]
    fn test_add_tainted_entry_without_taint_tracking() {
        // When taint tracking is NOT enabled, the taint level is silently ignored.
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        region
            .add_tainted_entry("data".to_string(), 10, crate::taint::TaintLevel::Private)
            .unwrap();
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.taint_level(), None);
    }

    #[test]
    fn test_remove_entries_by_prefix_no_match() {
        let mut region = Region::new("system".to_string(), RegionKind::Pinned, 50000);
        region.add_entry("Keep this".to_string(), 10).unwrap();
        region.add_entry("And this".to_string(), 20).unwrap();
        region.remove_entries_by_prefix("[Stage instructions:");
        assert_eq!(region.entry_count(), 2);
        assert_eq!(region.current_tokens, 30);
    }

    // ─── HashMap region tests ──────────────────────────────────────────────

    #[test]
    fn test_hashmap_region_upsert_and_get() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10000,
        );
        region
            .upsert_by_key("src/main.rs", "fn main() {}".to_string(), 10)
            .unwrap();
        region
            .upsert_by_key("src/lib.rs", "pub mod foo;".to_string(), 8)
            .unwrap();

        assert_eq!(region.entry_count(), 2);
        assert_eq!(region.current_tokens, 18);

        let entry = region.get_by_key("src/main.rs").unwrap();
        assert_eq!(entry.content, "fn main() {}");
        assert_eq!(entry.key.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn test_hashmap_region_upsert_replaces_existing() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10000,
        );
        region
            .upsert_by_key("file.rs", "version 1".to_string(), 10)
            .unwrap();
        assert_eq!(region.current_tokens, 10);

        region
            .upsert_by_key("file.rs", "version 2".to_string(), 15)
            .unwrap();
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.current_tokens, 15);
        assert_eq!(region.get_by_key("file.rs").unwrap().content, "version 2");
    }

    #[test]
    fn test_hashmap_region_remove_by_key() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10000,
        );
        region.upsert_by_key("a.rs", "aaa".to_string(), 10).unwrap();
        region.upsert_by_key("b.rs", "bbb".to_string(), 20).unwrap();

        assert!(region.remove_by_key("a.rs"));
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.current_tokens, 20);
        assert!(region.get_by_key("a.rs").is_none());
        assert!(!region.remove_by_key("nonexistent"));
    }

    #[test]
    fn test_hashmap_region_keys() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            10000,
        );
        region.upsert_by_key("x.rs", "x".to_string(), 5).unwrap();
        region.upsert_by_key("y.rs", "y".to_string(), 5).unwrap();

        let keys = region.keys();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"x.rs"));
        assert!(keys.contains(&"y.rs"));
    }

    #[test]
    fn test_hashmap_region_lru_eviction_on_max_tokens() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            30, // tight budget
        );
        region.upsert_by_key("a.rs", "aaa".to_string(), 10).unwrap();
        // Make 'a' older by manually adjusting timestamp
        region.content[0].timestamp -= 100;
        region.upsert_by_key("b.rs", "bbb".to_string(), 10).unwrap();
        region.upsert_by_key("c.rs", "ccc".to_string(), 10).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.current_tokens, 30);

        // Adding d.rs should evict a.rs (oldest timestamp)
        region.upsert_by_key("d.rs", "ddd".to_string(), 10).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert!(region.get_by_key("a.rs").is_none());
        assert!(region.get_by_key("d.rs").is_some());
    }

    #[test]
    fn test_hashmap_region_max_entries_eviction() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap {
                max_entries: Some(2),
            },
            10000,
        );
        region.upsert_by_key("a.rs", "aaa".to_string(), 10).unwrap();
        region.content[0].timestamp -= 100; // make oldest
        region.upsert_by_key("b.rs", "bbb".to_string(), 10).unwrap();
        assert_eq!(region.entry_count(), 2);

        // Adding c.rs should evict a.rs (oldest, max_entries=2)
        region.upsert_by_key("c.rs", "ccc".to_string(), 10).unwrap();
        assert_eq!(region.entry_count(), 2);
        assert!(region.get_by_key("a.rs").is_none());
        assert!(region.get_by_key("c.rs").is_some());
    }

    #[test]
    fn test_hashmap_region_upsert_too_large_for_budget() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            5, // very small
        );
        let result = region.upsert_by_key("big.rs", "huge content".to_string(), 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_hashmap_region_kind_equality() {
        assert_eq!(
            RegionKind::HashMap {
                max_entries: Some(10)
            },
            RegionKind::HashMap {
                max_entries: Some(10)
            }
        );
        assert_ne!(
            RegionKind::HashMap {
                max_entries: Some(10)
            },
            RegionKind::HashMap {
                max_entries: Some(20)
            }
        );
        assert_ne!(
            RegionKind::HashMap { max_entries: None },
            RegionKind::Pinned
        );
    }

    #[test]
    fn test_hashmap_cache_hint() {
        let kind = RegionKind::HashMap { max_entries: None };
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::UntilChanged);
    }

    #[test]
    fn test_region_entry_key_default_none() {
        let mut region = Region::new("test".to_string(), RegionKind::Temporary, 1000);
        region.add_entry("content".to_string(), 10).unwrap();
        assert!(region.content[0].key.is_none());
    }

    #[test]
    fn test_region_entry_key_serde_skip_when_none() {
        let entry = RegionEntry {
            content: "test".to_string(),
            tokens: 5,
            timestamp: 0,
            metadata: None,
            kind: EntryKind::default(),
            key: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("key"));
    }

    #[test]
    fn test_region_entry_key_serde_roundtrip() {
        let entry = RegionEntry {
            content: "test".to_string(),
            tokens: 5,
            timestamp: 0,
            metadata: None,
            kind: EntryKind::default(),
            key: Some("mykey".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("mykey"));
        let back: RegionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.key.as_deref(), Some("mykey"));
    }

    // ─── Additional HashMap region tests ──────────────────────────────────

    #[test]
    fn test_hashmap_region_creation_and_basic_properties() {
        let region = Region::new(
            "lookup".to_string(),
            RegionKind::HashMap {
                max_entries: Some(5),
            },
            2000,
        );
        assert_eq!(region.name, "lookup");
        assert_eq!(
            region.kind,
            RegionKind::HashMap {
                max_entries: Some(5)
            }
        );
        assert_eq!(region.max_tokens, 2000);
        assert_eq!(region.current_tokens, 0);
        assert_eq!(region.entry_count(), 0);
        assert!(region.content.is_empty());
    }

    #[test]
    fn test_hashmap_upsert_insert_new_entry() {
        let mut region = Region::new(
            "store".to_string(),
            RegionKind::HashMap {
                max_entries: Some(5),
            },
            5000,
        );
        region
            .upsert_by_key("config.toml", "[package]\nname = \"foo\"".to_string(), 12)
            .unwrap();

        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.current_tokens, 12);

        let entry = region.get_by_key("config.toml").unwrap();
        assert_eq!(entry.content, "[package]\nname = \"foo\"");
        assert_eq!(entry.tokens, 12);
        assert_eq!(entry.key.as_deref(), Some("config.toml"));
    }

    #[test]
    fn test_hashmap_upsert_update_existing_entry() {
        let mut region = Region::new(
            "store".to_string(),
            RegionKind::HashMap { max_entries: None },
            5000,
        );
        region
            .upsert_by_key("readme.md", "# Old".to_string(), 20)
            .unwrap();
        assert_eq!(region.current_tokens, 20);

        region
            .upsert_by_key("readme.md", "# New and improved".to_string(), 35)
            .unwrap();
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.current_tokens, 35);

        let entry = region.get_by_key("readme.md").unwrap();
        assert_eq!(entry.content, "# New and improved");
        assert_eq!(entry.tokens, 35);
    }

    #[test]
    fn test_hashmap_upsert_lru_eviction_on_max_tokens() {
        let mut region = Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            100, // small token budget
        );

        // Insert entries that together fill the budget
        region
            .upsert_by_key("first.rs", "first content".to_string(), 40)
            .unwrap();
        region.content[0].timestamp -= 200; // oldest

        region
            .upsert_by_key("second.rs", "second content".to_string(), 40)
            .unwrap();
        region.content[1].timestamp -= 100; // middle age

        region
            .upsert_by_key("third.rs", "third content".to_string(), 20)
            .unwrap();
        // total = 100, at budget

        // Inserting another entry that exceeds budget should evict oldest
        region
            .upsert_by_key("fourth.rs", "fourth content".to_string(), 30)
            .unwrap();

        // first.rs (oldest timestamp) should have been evicted
        assert!(region.get_by_key("first.rs").is_none());
        assert!(region.get_by_key("fourth.rs").is_some());
        // total tokens should be within budget
        assert!(region.current_tokens <= 100);
    }

    #[test]
    fn test_hashmap_upsert_max_entries_enforcement() {
        let mut region = Region::new(
            "cache".to_string(),
            RegionKind::HashMap {
                max_entries: Some(2),
            },
            50000,
        );

        region
            .upsert_by_key("alpha", "aaa".to_string(), 10)
            .unwrap();
        region.content[0].timestamp -= 200; // make oldest

        region.upsert_by_key("beta", "bbb".to_string(), 10).unwrap();
        region.content[1].timestamp -= 100;

        region
            .upsert_by_key("gamma", "ccc".to_string(), 10)
            .unwrap();

        // Only 2 entries should remain, oldest evicted
        assert_eq!(region.entry_count(), 2);
        assert!(region.get_by_key("alpha").is_none());
        assert!(region.get_by_key("beta").is_some());
        assert!(region.get_by_key("gamma").is_some());
    }

    #[test]
    fn test_hashmap_get_by_key_found_and_not_found() {
        let mut region = Region::new(
            "data".to_string(),
            RegionKind::HashMap { max_entries: None },
            5000,
        );
        region
            .upsert_by_key("exists", "hello".to_string(), 5)
            .unwrap();

        // Found
        let found = region.get_by_key("exists");
        assert!(found.is_some());
        assert_eq!(found.unwrap().content, "hello");

        // Not found
        let missing = region.get_by_key("does_not_exist");
        assert!(missing.is_none());
    }

    #[test]
    fn test_hashmap_remove_by_key_exists() {
        let mut region = Region::new(
            "data".to_string(),
            RegionKind::HashMap { max_entries: None },
            5000,
        );
        region
            .upsert_by_key("target", "remove me".to_string(), 25)
            .unwrap();
        assert_eq!(region.current_tokens, 25);

        let removed = region.remove_by_key("target");
        assert!(removed);
        assert_eq!(region.entry_count(), 0);
        assert_eq!(region.current_tokens, 0);
        assert!(region.get_by_key("target").is_none());
    }

    #[test]
    fn test_hashmap_remove_by_key_does_not_exist() {
        let mut region = Region::new(
            "data".to_string(),
            RegionKind::HashMap { max_entries: None },
            5000,
        );
        let removed = region.remove_by_key("ghost");
        assert!(!removed);
    }

    #[test]
    fn test_hashmap_keys_empty_populated_after_removal() {
        let mut region = Region::new(
            "data".to_string(),
            RegionKind::HashMap { max_entries: None },
            5000,
        );

        // Empty
        assert!(region.keys().is_empty());

        // Populated
        region.upsert_by_key("one", "1".to_string(), 5).unwrap();
        region.upsert_by_key("two", "2".to_string(), 5).unwrap();
        region.upsert_by_key("three", "3".to_string(), 5).unwrap();

        let keys = region.keys();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"one"));
        assert!(keys.contains(&"two"));
        assert!(keys.contains(&"three"));

        // After removal
        region.remove_by_key("two");
        let keys = region.keys();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"one"));
        assert!(!keys.contains(&"two"));
        assert!(keys.contains(&"three"));
    }

    #[test]
    fn test_region_entry_serialization_with_key_field() {
        // Entry with key
        let entry_with_key = RegionEntry {
            content: "some data".to_string(),
            tokens: 10,
            timestamp: 1234567890,
            metadata: None,
            kind: EntryKind::default(),
            key: Some("mykey".to_string()),
        };
        let json = serde_json::to_string(&entry_with_key).unwrap();
        let deserialized: RegionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.key.as_deref(), Some("mykey"));
        assert_eq!(deserialized.content, "some data");
        assert_eq!(deserialized.tokens, 10);

        // Entry without key
        let entry_no_key = RegionEntry {
            content: "no key data".to_string(),
            tokens: 7,
            timestamp: 1234567890,
            metadata: None,
            kind: EntryKind::default(),
            key: None,
        };
        let json = serde_json::to_string(&entry_no_key).unwrap();
        assert!(!json.contains("\"key\""));
        let deserialized: RegionEntry = serde_json::from_str(&json).unwrap();
        assert!(deserialized.key.is_none());
        assert_eq!(deserialized.content, "no key data");
    }

    #[test]
    fn test_hashmap_partial_eq() {
        let a = RegionKind::HashMap {
            max_entries: Some(5),
        };
        let b = RegionKind::HashMap {
            max_entries: Some(5),
        };
        let c = RegionKind::HashMap {
            max_entries: Some(10),
        };
        let d = RegionKind::HashMap { max_entries: None };

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_ne!(c, d);
        assert_ne!(a, RegionKind::Pinned);
        assert_ne!(a, RegionKind::Temporary);
    }

    #[test]
    fn test_hashmap_cache_hint_returns_until_changed() {
        let kind = RegionKind::HashMap { max_entries: None };
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::UntilChanged);

        let kind_with_max = RegionKind::HashMap {
            max_entries: Some(10),
        };
        assert_eq!(
            kind_with_max.cache_hint(),
            crate::cache::CacheHint::UntilChanged
        );
    }

    // ─── taint-vector fixups on keyed removal / LRU eviction ───────────────

    #[test]
    fn test_remove_by_key_recomputes_taint_when_tracking_enabled() {
        // A taint-tracked region: remove_by_key must run its taint-vector
        // fixup branch (`taint.remove_at`) without panicking.
        let mut region = Region::new(
            "kv".to_string(),
            RegionKind::HashMap { max_entries: None },
            10_000,
        )
        .with_taint_tracking();
        region
            .upsert_by_key("k1", "value one".to_string(), 10)
            .unwrap();
        region
            .upsert_by_key("k2", "value two".to_string(), 10)
            .unwrap();

        assert!(region.remove_by_key("k1"));
        assert!(!region.remove_by_key("missing"));
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.current_tokens, 10);
    }

    #[test]
    fn test_evict_lru_entry_runs_taint_fixup() {
        // A taint-tracked HashMap region with a max_entries cap: inserting past
        // the cap triggers evict_lru_entry, which must run its taint-vector
        // fixup branch.
        let mut region = Region::new(
            "kv".to_string(),
            RegionKind::HashMap {
                max_entries: Some(1),
            },
            10_000,
        )
        .with_taint_tracking();
        region
            .upsert_by_key("first", "aaa".to_string(), 10)
            .unwrap();
        region
            .upsert_by_key("second", "bbb".to_string(), 10)
            .unwrap();

        // Only the most-recently-inserted key survives after LRU eviction.
        assert_eq!(region.entry_count(), 1);
        assert!(region.get_by_key("second").is_some());
        assert!(region.get_by_key("first").is_none());
    }

    #[test]
    fn test_evict_lru_entry_on_empty_region_is_noop() {
        // Directly exercise the early-return guard in `evict_lru_entry` when
        // there is nothing to evict - a defensive branch not reachable through
        // the public upsert path (which only evicts non-empty regions).
        let mut region = Region::new(
            "kv".to_string(),
            RegionKind::HashMap {
                max_entries: Some(4),
            },
            1000,
        );
        assert_eq!(region.entry_count(), 0);
        region.evict_lru_entry();
        assert_eq!(region.entry_count(), 0);
        assert_eq!(region.current_tokens, 0);
    }
}
