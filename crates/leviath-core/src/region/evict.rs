//! Making room in a [`Region`]: what leaves when a write does not fit, and
//! how a sliding window keeps to its item cap. A child module of `mod.rs`, so
//! it stays on the struct's private fields.

use super::*;

impl Region {
    /// Whether a full context window may take entries from this region: a
    /// clearable, temporary or unpinned custom region, unless it is under
    /// [`Admission::Reject`], whose entries only the agent releases.
    pub fn evictable(&self) -> bool {
        self.admission != Admission::Reject
            && matches!(
                self.kind,
                RegionKind::Clearable
                    | RegionKind::Temporary
                    | RegionKind::Custom { pinned: false, .. }
            )
    }

    /// Roll off the oldest entries until `tokens` more would fit under the
    /// [low-water mark](Self::low_water), and report how many were dropped.
    ///
    /// This is what [`Admission::Evict`] has always claimed to do. It rolls
    /// off in a chunk rather than stopping the moment the write fits: a region
    /// that frees exactly one write's worth is full again on the next write
    /// and rewrites its prefix every time, which is what the prompt cache
    /// cannot keep. Freed down to [`evict_to`](Self::evict_to) of the budget,
    /// the region takes the writes after this one without moving anything.
    ///
    /// Does not start at all when the write never fits: an entry larger than
    /// `max_tokens` does not fit an *empty* region either, so evicting for it
    /// would destroy everything held and still fail. The caller truncates
    /// instead, which is the right answer and needs the region intact to have
    /// anywhere to put the truncation.
    ///
    /// Goes through [`remove_oldest`](Self::remove_oldest) rather than touching
    /// `content` directly because that method is turn-group aware: an
    /// `AssistantTurn` carrying tool calls leaves together with the
    /// `ToolResult` entries that answer it, so eviction never strands a
    /// `tool_use` that a provider would then reject.
    pub fn make_room(&mut self, tokens: usize) -> usize {
        // Sticky entries stay whatever happens, so their tokens are as good
        // as part of the write when asking whether it can ever fit.
        if tokens + self.sticky_tokens() > self.max_tokens {
            return 0;
        }
        let floor = self.low_water();
        let mut evicted = 0;
        while self.current_tokens + tokens > floor && self.remove_oldest().is_some() {
            evicted += 1;
        }
        evicted
    }

    /// The tokens the region holds after it makes room for a write:
    /// [`evict_to`](Self::evict_to) of its budget, to the nearest token.
    ///
    /// A fraction outside 0 to 1 is read as the nearer end, so a region built
    /// by hand with a stray value frees something sensible rather than
    /// nothing or a negative amount.
    pub fn low_water(&self) -> usize {
        (self.max_tokens as f64 * self.evict_to.clamp(0.0, 1.0)).round() as usize
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

    /// Evict the least recently written entry that is neither sticky nor
    /// under `except`, and report whether there was one.
    ///
    /// Least recent is the lowest [`RegionEntry::seq`], then the earliest
    /// timestamp, then the first position: an entry built by hand with no
    /// sequence is older than anything the region stamped.
    pub(super) fn evict_lru_entry(&mut self, except: Option<&str>) -> bool {
        let oldest = self
            .content
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.sticky && (except.is_none() || e.key.as_deref() != except))
            .min_by_key(|(_, e)| (e.seq, e.timestamp))
            .map(|(i, _)| i);
        let Some(index) = oldest else {
            return false;
        };
        let tokens = self.content[index].tokens;
        self.content.remove(index);
        self.current_tokens -= tokens;
        if let Some(taint) = &mut self.taint {
            taint.remove_at(index);
        }
        true
    }

    /// Evict every entry that is not [`sticky`](RegionEntry::sticky), and
    /// report the tokens freed.
    ///
    /// What wiping a region wholesale means under pressure: a clearable
    /// region dumped to make room keeps the entries its writer marked to
    /// keep. [`clear`](Self::clear) is the explicit wipe, and takes those too.
    pub fn evict_all(&mut self) -> usize {
        let before = self.current_tokens;
        let mut index = 0;
        while index < self.content.len() {
            if self.content[index].sticky {
                index += 1;
                continue;
            }
            let entry = self.content.remove(index);
            self.current_tokens -= entry.tokens;
            if let Some(taint) = &mut self.taint {
                taint.remove_at(index);
            }
        }
        before - self.current_tokens
    }

    /// Tokens held by [`sticky`](RegionEntry::sticky) entries, which no
    /// eviction frees.
    pub fn sticky_tokens(&self) -> usize {
        self.content
            .iter()
            .filter(|e| e.sticky)
            .map(|e| e.tokens)
            .sum()
    }

    /// Enforce the SlidingWindow max_items limit by removing oldest entries.
    ///
    /// Behaviour depends on the configured [`EvictionStrategy`]:
    /// - **PerItem** - evict one turn group at a time; the default.
    /// - **Bulk** - only evict when `len > max_items + overflow`, then evict
    ///   down to `max_items`. Between bulk evictions the prefix is stable,
    ///   which preserves Anthropic prompt-cache keys.
    /// - **Compact** - set `needs_message_compaction` when `len > max_items + compact_count`.
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

    /// Remove the oldest entry that is not [`sticky`](RegionEntry::sticky),
    /// with the rest of its turn group.
    ///
    /// `None` when every entry is sticky, or there are none.
    pub fn remove_oldest(&mut self) -> Option<RegionEntry> {
        // Respect turn groups: an AssistantTurn with tool_calls must be
        // evicted together with its following ToolResult entries to avoid
        // orphaned tool_use/tool_result blocks that providers reject. A group
        // with a sticky entry in it stays whole, for the same reason.
        let mut start = 0;
        let mut group_size = self.turn_group_size_at(0);
        while group_size > 0
            && self.content[start..start + group_size]
                .iter()
                .any(|e| e.sticky)
        {
            start += group_size;
            group_size = self.turn_group_size_at(start);
        }
        let mut removed = self.content.drain(start..start + group_size);
        let mut first = removed.next()?;
        // Embed the rest of the group's tokens in the returned entry so
        // callers that use `entry.tokens` to adjust their own totals account
        // for the full group.
        let extra_tokens: usize = removed.map(|e| e.tokens).sum();
        first.tokens += extra_tokens;
        self.current_tokens -= first.tokens;
        if let Some(taint) = &mut self.taint {
            for _ in 0..group_size {
                taint.remove_at(start);
            }
        }
        Some(first)
    }
}
