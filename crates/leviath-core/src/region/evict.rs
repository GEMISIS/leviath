//! Making room in a [`Region`]: what leaves when a write does not fit, and
//! how a sliding window keeps to its item cap. Split out of `mod.rs` for
//! size; a child module so it stays on the struct's private fields.

use super::*;

impl Region {
    /// Roll off the oldest entries until `tokens` more would fit, and report
    /// how many were dropped.
    ///
    /// This is what [`Admission::Evict`] has always claimed to do. Stops as
    /// soon as the write fits, and does not start at all when it never can:
    /// an entry larger than `max_tokens` does not fit an *empty* region either,
    /// so evicting for it would destroy everything held and still fail. The
    /// caller truncates instead, which is the right answer and needs the region
    /// intact to have anywhere to put the truncation.
    ///
    /// Goes through [`remove_oldest`](Self::remove_oldest) rather than touching
    /// `content` directly because that method is turn-group aware: an
    /// `AssistantTurn` carrying tool calls leaves together with the
    /// `ToolResult` entries that answer it, so eviction never strands a
    /// `tool_use` that a provider would then reject.
    pub fn make_room(&mut self, tokens: usize) -> usize {
        if tokens > self.max_tokens {
            return 0;
        }
        let mut evicted = 0;
        while self.current_tokens + tokens > self.max_tokens && self.remove_oldest().is_some() {
            evicted += 1;
        }
        evicted
    }

    /// Whether one more entry would push a sliding window past its item cap.
    ///
    /// Only sliding windows roll off by count; every other kind is bounded by
    /// tokens alone and is answered by the budget check above.
    pub(super) fn would_roll_off(&self) -> bool {
        match &self.kind {
            RegionKind::SlidingWindow { max_items, .. } => self.content.len() + 1 > *max_items,
            _ => false,
        }
    }

    /// Drop the `n` oldest entries, returning how many were actually removed.
    ///
    /// Fewer than `n` when the region holds fewer, which is not an error: the
    /// agent asked for room and got as much as there was.
    pub fn release_oldest(&mut self, n: usize) -> usize {
        let count = n.min(self.content.len());
        for _ in 0..count {
            self.remove_oldest();
        }
        count
    }

    /// Evict the least-recently-updated entry (LRU) for HashMap regions.
    pub(super) fn evict_lru_entry(&mut self) {
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
    pub(super) fn enforce_sliding_window(&mut self) {
        if let RegionKind::SlidingWindow {
            max_items,
            eviction_strategy,
        } = &self.kind
        {
            let max = *max_items;
            match *eviction_strategy {
                EvictionStrategy::PerItem => {
                    // `remove_oldest` only returns None when empty, which the
                    // `len > max >= 0` guard already precludes; folding it into
                    // the condition keeps the guard without a dead break arm.
                    while self.content.len() > max && self.remove_oldest().is_some() {}
                }
                EvictionStrategy::Bulk { overflow } => {
                    if self.content.len() > max.saturating_add(overflow) {
                        while self.content.len() > max && self.remove_oldest().is_some() {}
                    }
                }
                EvictionStrategy::Compact { compact_count } => {
                    if self.content.len() > max.saturating_add(compact_count.saturating_mul(2)) {
                        // Fallback: runtime hasn't compacted, bulk-evict to prevent
                        // unbounded growth.
                        while self.content.len() > max && self.remove_oldest().is_some() {}
                        self.needs_message_compaction = false;
                    } else if self.content.len() > max.saturating_add(compact_count) {
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
    pub(super) fn turn_group_size_at(&self, idx: usize) -> usize {
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
}
