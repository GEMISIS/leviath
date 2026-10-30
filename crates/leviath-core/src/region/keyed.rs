//! Keyed writes: upserts, keyed appends, recency and sticky entries. A child
//! module of `mod.rs`, so it stays on the struct's private helpers.
//!
//! A keyed region keeps what was written most recently and lets the rest go
//! when it is full. Two things decide what "the rest" is:
//!
//! - **Recency** is [`RegionEntry::seq`], a per-region counter stamped on
//!   every write. Two writes in the same second still have an order, so the
//!   least recently written entry is always one entry, not a tie.
//! - **Stickiness** is [`RegionEntry::sticky`]. A sticky entry is never
//!   evicted to make room, only removed by name, so a write that would need
//!   one gone fails instead.

use super::*;

/// What [`Region::replace_keyed_entry`] did with a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyedWrite {
    /// Nothing held the key: the entry went in at the end.
    Added,
    /// The entry under the key left and this one went in at the end, so the
    /// key's content is what was written last and is the newest the region
    /// holds.
    Replaced,
    /// The entry under the key already held exactly this: nothing moved, so
    /// the prompt is byte for byte what it was.
    Unchanged,
}

impl Region {
    /// Write `content` under `key` in a region that keeps its entries in
    /// write order, for a result the same call produces again.
    ///
    /// A tool result routed into a region is keyed by the call that produced
    /// it, so a second `read_file` of one path does not stack a second copy on
    /// the first. What happens depends on what the key already holds: an
    /// identical entry stays exactly where it is, which keeps the prompt's
    /// cached prefix intact, and a different one is dropped before this one
    /// goes in at the end as the region's newest. A new key is an ordinary
    /// [`add_typed_tainted_entry`](Self::add_typed_tainted_entry).
    ///
    /// Distinct from [`upsert_by_key`](Self::upsert_by_key), which replaces an
    /// entry in place and evicts by recency: that is a keyed region's
    /// discipline, and a region that rolls its oldest entries off wants a
    /// re-read to count as recent. Content the region does not take is
    /// refused with the earlier entry still there; a replacement that fits
    /// nowhere, even with the earlier one gone, leaves the key empty for the
    /// caller's truncated copy, which is what a fresh write would get.
    pub fn replace_keyed_entry(
        &mut self,
        key: &str,
        content: EntryContent,
        tokens: usize,
        kind: EntryKind,
        taint_level: crate::taint::TaintLevel,
    ) -> crate::error::Result<KeyedWrite> {
        self.refuse_unaccepted(&content)?;
        let outcome = match self.position_of(key) {
            Some(pos) if self.content[pos].content == content && self.content[pos].kind == kind => {
                // Still the most recently written, so a keyed region's LRU
                // sees the re-read; the entry's bytes and place do not move.
                let seq = self.next_seq();
                let entry = &mut self.content[pos];
                entry.seq = seq;
                entry.timestamp = chrono::Utc::now().timestamp();
                return Ok(KeyedWrite::Unchanged);
            }
            Some(_) => {
                self.remove_by_key(key);
                KeyedWrite::Replaced
            }
            None => KeyedWrite::Added,
        };
        self.push_entry(content, tokens, None, kind, taint_level, Some(key))?;
        Ok(outcome)
    }

    /// Add an entry under `key`, so the agent can name it again to release it.
    ///
    /// Distinct from [`upsert_by_key`](Self::upsert_by_key), which replaces:
    /// appending two sources under one key should keep both halves, the way an
    /// unkeyed append keeps everything appended before it. A keyed region's
    /// `max_entries` still holds: a full one lets its least recently written
    /// entry go, or refuses under [`Admission::Reject`].
    pub fn add_keyed_entry(
        &mut self,
        key: &str,
        content: impl Into<EntryContent>,
        tokens: usize,
    ) -> crate::error::Result<()> {
        self.push_entry(
            content.into(),
            tokens,
            None,
            EntryKind::default(),
            crate::taint::TaintLevel::Public,
            Some(key),
        )
    }

    /// Write `content` under `key`, replacing the entry already there.
    ///
    /// A replacement keeps the entry's place in the region and whether it is
    /// [`sticky`](RegionEntry::sticky); a new key goes at the end. Either way
    /// the entry becomes the most recently written. To make room, the least
    /// recently written entries other than this one are evicted, sticky ones
    /// never. Whether the write can fit is decided before anything is
    /// evicted, so a write that fails leaves the region exactly as it was.
    ///
    /// ```
    /// use leviath_core::{Region, RegionKind};
    ///
    /// let mut tasks = Region::new("tasks".into(), RegionKind::HashMap { max_entries: None }, 30);
    /// tasks.upsert_by_key("a", "first", 10).unwrap();
    /// tasks.upsert_by_key("b", "second", 10).unwrap();
    /// // Too big for the region even empty: refused, nothing evicted.
    /// assert!(tasks.upsert_by_key("c", "huge", 31).is_err());
    /// assert_eq!(tasks.keys(), ["a", "b"]);
    /// // Growing `b` past the budget evicts `a`, the least recently written.
    /// tasks.upsert_by_key("b", "second, longer", 25).unwrap();
    /// assert_eq!(tasks.keys(), ["b"]);
    /// ```
    pub fn upsert_by_key(
        &mut self,
        key: &str,
        content: impl Into<EntryContent>,
        tokens: usize,
    ) -> Result<(), String> {
        self.upsert_by_key_content(key, content.into(), tokens, None)
    }

    /// [`upsert_by_key`](Self::upsert_by_key), also setting whether the
    /// entry is [`sticky`](RegionEntry::sticky).
    ///
    /// A sticky entry is never evicted. The write fails, untouched, when the
    /// sticky entries would then take more than the region's budget between
    /// them.
    ///
    /// ```
    /// use leviath_core::{Region, RegionKind};
    ///
    /// let mut tasks = Region::new("tasks".into(), RegionKind::HashMap { max_entries: None }, 20);
    /// tasks.upsert_by_key_sticky("running", "build the parser", 10, true).unwrap();
    /// tasks.upsert_by_key("done", "write the lexer", 10).unwrap();
    /// // Room is made from the non-sticky entry, however old `running` is.
    /// tasks.upsert_by_key("next", "add tests", 10).unwrap();
    /// assert_eq!(tasks.keys(), ["running", "next"]);
    /// ```
    pub fn upsert_by_key_sticky(
        &mut self,
        key: &str,
        content: impl Into<EntryContent>,
        tokens: usize,
        sticky: bool,
    ) -> Result<(), String> {
        self.upsert_by_key_content(key, content.into(), tokens, Some(sticky))
    }

    /// Mark the entry under `key` sticky or not. `false` when no entry has
    /// that key.
    ///
    /// Marking needs no room, since the entry is already held. Unmarking
    /// evicts nothing either: the entry simply becomes something a later
    /// write may let go.
    pub fn set_sticky(&mut self, key: &str, sticky: bool) -> bool {
        match self.position_of(key) {
            Some(pos) => {
                self.content[pos].sticky = sticky;
                true
            }
            None => false,
        }
    }

    /// The logic behind both upserts, once. `sticky` is `None` to keep a
    /// replaced entry's flag (and leave a new one unsticky).
    fn upsert_by_key_content(
        &mut self,
        key: &str,
        content: EntryContent,
        tokens: usize,
        sticky: Option<bool>,
    ) -> Result<(), String> {
        let existing = self.position_of(key);
        self.check_upsert_fits(existing, tokens)?;

        // A new key in a region at its entry cap needs that many others gone.
        // `check_upsert_fits` has confirmed enough of them are evictable.
        if existing.is_none()
            && let Some(max) = self.max_entries()
        {
            let excess = (self.content.len() + 1).saturating_sub(max);
            for _ in 0..excess {
                self.evict_lru_entry(Some(key));
            }
        }
        let old_tokens = existing.map_or(0, |pos| self.content[pos].tokens);
        // Terminates: the sticky entries and this one fit together, so there
        // is always an evictable entry while the region is over.
        while self.current_tokens - old_tokens + tokens > self.max_tokens
            && self.evict_lru_entry(Some(key))
        {}

        let seq = self.next_seq();
        let timestamp = chrono::Utc::now().timestamp();
        // Looked up again: evicting the entries in front of it moved it.
        match self.position_of(key) {
            Some(pos) => {
                let entry = &mut self.content[pos];
                self.current_tokens = self.current_tokens - entry.tokens + tokens;
                entry.content = content;
                entry.tokens = tokens;
                entry.timestamp = timestamp;
                entry.seq = seq;
                entry.sticky = sticky.unwrap_or(entry.sticky);
            }
            None => {
                self.content.push(RegionEntry {
                    content,
                    tokens,
                    timestamp,
                    metadata: None,
                    kind: EntryKind::default(),
                    key: Some(key.to_string()),
                    reasoning: None,
                    seq,
                    sticky: sticky.unwrap_or(false),
                });
                self.current_tokens += tokens;
            }
        }
        Ok(())
    }

    /// Whether an upsert of `tokens` (replacing the entry at `existing`, if
    /// any) can be made to fit by evicting only what may be evicted.
    fn check_upsert_fits(&self, existing: Option<usize>, tokens: usize) -> Result<(), String> {
        if tokens > self.max_tokens {
            return Err(format!(
                "Entry ({} tokens) exceeds region budget ({} max)",
                tokens, self.max_tokens
            ));
        }
        let others = || {
            self.content
                .iter()
                .enumerate()
                .filter(move |(i, _)| Some(*i) != existing)
                .map(|(_, e)| e)
        };
        let held = others()
            .filter(|e| e.sticky)
            .map(|e| e.tokens)
            .sum::<usize>();
        if held + tokens > self.max_tokens {
            return Err(format!(
                "Entry ({tokens} tokens) does not fit region '{}' ({} max): sticky entries \
                 hold {held} tokens and are never evicted",
                self.name, self.max_tokens
            ));
        }
        if existing.is_none()
            && let Some(max) = self.max_entries()
        {
            let excess = (self.content.len() + 1).saturating_sub(max);
            let evictable = others().filter(|e| !e.sticky).count();
            if excess > evictable {
                return Err(format!(
                    "Region '{}' is at its {max}-entry limit and its entries are sticky, \
                     which are never evicted",
                    self.name
                ));
            }
        }
        Ok(())
    }

    /// The region's entry cap, when it is a keyed region that has one.
    pub(super) fn max_entries(&self) -> Option<usize> {
        match self.kind {
            RegionKind::HashMap { max_entries } => max_entries,
            _ => None,
        }
    }

    /// Where the entry under `key` is, the first when several share it.
    fn position_of(&self, key: &str) -> Option<usize> {
        self.content
            .iter()
            .position(|e| e.key.as_deref() == Some(key))
    }

    /// The [`RegionEntry::seq`] the next write takes: one past the newest.
    pub(super) fn next_seq(&self) -> u64 {
        self.content.iter().map(|e| e.seq).max().unwrap_or(0) + 1
    }

    /// Stamp every entry's [`RegionEntry::seq`] from its timestamp, then its
    /// position, so the oldest write gets 1.
    ///
    /// For entries loaded from somewhere that does not keep a sequence. The
    /// order it gives is the one recency had before the sequence existed, so
    /// a region restored this way evicts exactly as it would have then, and
    /// every write after it is ordered exactly.
    pub fn restamp_recency(&mut self) {
        let mut order: Vec<usize> = (0..self.content.len()).collect();
        // A stable sort, so equal timestamps keep their positions.
        order.sort_by_key(|&i| self.content[i].timestamp);
        for (seq, i) in (1..).zip(order) {
            self.content[i].seq = seq;
        }
    }

    /// Get entry by key.
    pub fn get_by_key(&self, key: &str) -> Option<&RegionEntry> {
        self.content.iter().find(|e| e.key.as_deref() == Some(key))
    }

    /// Remove entry by key. Removes a sticky entry too: naming it is how one
    /// is released.
    pub fn remove_by_key(&mut self, key: &str) -> bool {
        match self.position_of(key) {
            Some(pos) => {
                let tokens = self.content[pos].tokens;
                self.content.remove(pos);
                self.current_tokens -= tokens;
                if let Some(taint) = &mut self.taint {
                    taint.remove_at(pos);
                }
                true
            }
            None => false,
        }
    }

    /// List all keys in this region.
    pub fn keys(&self) -> Vec<&str> {
        self.content
            .iter()
            .filter_map(|e| e.key.as_deref())
            .collect()
    }
}
