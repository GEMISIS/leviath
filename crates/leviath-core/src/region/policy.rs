//! What a region does under pressure, and how that shows up in the prompt.
//!
//! Three policies that travel together because they answer one question between
//! them - a region that is full has to either drop something or refuse, and
//! which it does decides whether the agent ever finds out. None of them touch
//! `Region`'s state, which is why they are not in with it.

use serde::{Deserialize, Serialize};

use super::RegionKind;

/// Eviction strategy for `SlidingWindow` regions.
///
/// Controls how entries are removed when the window exceeds its `max_items` limit.
/// The choice of strategy affects prompt caching effectiveness: PerItem eviction
/// shifts the message prefix every iteration (breaking cache), while Bulk and
/// Compact keep the prefix stable between eviction events.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
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

impl RegionKind {
    /// Whether a write that does not fit may roll the oldest entries off to
    /// make room, given the region is under [`Admission::Evict`].
    ///
    /// Working regions say yes: they hold material whose oldest end is its
    /// least useful, and refusing the newest write to protect it is backwards.
    ///
    /// The rest say no, each for its own reason. [`Pinned`](Self::Pinned) and a
    /// persistent [`Custom`](Self::Custom) region are meant to survive the run,
    /// so dropping the task to admit a tool result would be a strange trade.
    /// [`HashMap`](Self::HashMap) already evicts by LRU on its own keyed path,
    /// and oldest-first would fight it. [`CompactHistory`](Self::CompactHistory)
    /// is the record of what was summarized away, and losing its front end
    /// loses the only copy. A non-persistent [`Custom`](Self::Custom) region is
    /// excluded too: its script's `on_overflow` hook IS the author's eviction
    /// policy, and rolling off behind it would override the thing the region
    /// exists to express.
    pub fn rolls_off_oldest(&self) -> bool {
        matches!(
            self,
            RegionKind::Temporary
                | RegionKind::Clearable
                | RegionKind::SlidingWindow { .. }
                | RegionKind::Compacting { .. }
        )
    }
}

/// What a region does when a write does not fit.
///
/// The default is what every region did before this existed: make room. That
/// is the right behaviour for a transcript, where the oldest turn is the least
/// useful thing present and losing it costs nothing anyone will notice.
///
/// It is the wrong behaviour for a region holding material the agent chose to
/// keep. There, silently dropping the oldest entry is a decision about what
/// matters, taken by whichever write happened to arrive when the region was
/// full - and the agent never learns it happened. [`Admission::Reject`] hands
/// that decision back: the write fails, the agent is told the region is full,
/// and it releases what it is finished with before adding more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// Make room for the write - roll off the oldest entry, or let the
    /// window-level cascade reclaim the region.
    #[default]
    Evict,
    /// Refuse the write and say so. Nothing already in the region is lost to a
    /// write the agent did not know would displace it.
    Reject,
}

/// How much a region's contents move between requests.
///
/// This exists because a provider caches by *prefix*: an entry is readable next
/// time only when every byte in front of it is unchanged, so a block that moves
/// invalidates everything behind it however stable that later content is. The
/// arrangement that pays is stable content first and churn last.
///
/// A region's [`RegionKind`] cannot answer this. A pinned region sounds
/// immutable and is written constantly - `context_write` into a findings region
/// is an ordinary move, and tool routing sends read results straight into one.
/// A compact-history region sounds settled and gains an entry every time
/// compaction fires. Inferring stability from the kind put churn at the front of
/// the prefix and cost a measured 456,860 cache-write tokens against zero reads
/// (issue #474).
///
/// So the blueprint says. The author knows whether a region is set once at spawn
/// or written every turn, and nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Volatility {
    /// Written rarely or never after setup: a task, a system prompt, a
    /// convention the run does not revise. Sorted to the front, where it forms
    /// the prefix everything else caches behind.
    Stable,
    /// Appended to, with existing entries left alone: a findings list, a
    /// bibliography, a transcript of what has been read. Sorted after the stable
    /// content, and split into chunks so the settled head of it can be cached
    /// while only the tail is re-sent.
    Grows,
    /// Existing content changes in place: a scratchpad, a key-value store whose
    /// keys are overwritten, anything rebuilt each turn. Sorted last, where it
    /// invalidates nothing but itself.
    ///
    /// The default, and deliberately the pessimistic one: a blueprint that
    /// declares nothing must never be made *worse* by this, and an optimistic
    /// default is exactly how inferring stability from the kind went wrong.
    #[default]
    Rewritten,
}
