//! Cache hint types for prompt caching across providers.
//!
//! Context regions are assembled in order of volatility (most stable first).
//! Cache breakpoints are inserted at region boundaries. Providers translate
//! these breakpoints into their native caching APIs.

use serde::{Deserialize, Serialize};

/// Cache hint for a region or message boundary.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CacheHint {
    /// Always cache -- content never changes (pinned, system, tools, compact history).
    Always,
    /// Cache until content hash changes (compacting regions between compaction events).
    UntilChanged,
    /// Cache the stable prefix of a sliding window.
    /// `stable_fraction` is 0.0..1.0 (default 0.75 = oldest 75% of messages are stable).
    SlidingPrefix { stable_fraction: f32 },
    /// Never cache (temporary, clearable, new messages).
    Never,
}

/// A cache breakpoint in the assembled message sequence.
#[derive(Debug, Clone)]
pub struct CacheBreakpoint {
    /// Index in the message array AFTER which to insert the cache marker.
    /// (i.e., messages[0..=index] should be cached)
    pub after_message_index: usize,
    /// The cache hint for this breakpoint.
    pub hint: CacheHint,
}
