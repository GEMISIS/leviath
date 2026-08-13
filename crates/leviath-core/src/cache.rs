//! Cache hint types for prompt caching across providers.
//!
//! Context regions are assembled in order of volatility (most stable first).
//! Cache breakpoints are inserted at region boundaries. Providers translate
//! these breakpoints into their native caching APIs.

use serde::{Deserialize, Serialize};

/// Cache hint for a region or message boundary.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum CacheHint {
    /// Always cache - content never changes (pinned, system, tools, compact history).
    Always,
    /// Cache until content hash changes (compacting regions between compaction events).
    UntilChanged,
    /// Same cacheability as [`CacheHint::UntilChanged`], and additionally marks
    /// the start of the most recently changed stretch of that tier.
    ///
    /// A provider that spends one cache breakpoint per run of same-hint blocks
    /// sees the change of hint as a run boundary, so the blocks ahead of the
    /// mutation end up in a cache entry of their own instead of sharing one
    /// with the block that just changed. Nothing else about the block differs:
    /// assembly sorts it to the same position as `UntilChanged`, and its text
    /// is untouched.
    RecentlyChanged,
    /// Cache the stable prefix of a sliding window.
    /// `stable_fraction` is 0.0..1.0 (default 0.75 = oldest 75% of messages are stable).
    SlidingPrefix {
        /// How much of the window counts as stable, in `0.0..1.0`. The oldest
        /// that fraction is cached; the newest tail is not, because it is what
        /// changes every turn and would invalidate the whole prefix with it.
        stable_fraction: f32,
    },
    /// Never cache (temporary, clearable, new messages).
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hint_always_equality() {
        assert_eq!(CacheHint::Always, CacheHint::Always);
        assert_ne!(CacheHint::Always, CacheHint::Never);
    }

    #[test]
    fn cache_hint_never_equality() {
        assert_eq!(CacheHint::Never, CacheHint::Never);
        assert_ne!(CacheHint::Never, CacheHint::UntilChanged);
    }

    #[test]
    fn cache_hint_until_changed_equality() {
        assert_eq!(CacheHint::UntilChanged, CacheHint::UntilChanged);
    }

    #[test]
    fn cache_hint_recently_changed_is_distinct_from_until_changed() {
        assert_eq!(CacheHint::RecentlyChanged, CacheHint::RecentlyChanged);
        assert_ne!(CacheHint::RecentlyChanged, CacheHint::UntilChanged);
        let dbg = format!("{:?}", CacheHint::RecentlyChanged);
        assert!(dbg.contains("RecentlyChanged"));
    }

    #[test]
    fn cache_hint_sliding_prefix_equality() {
        let a = CacheHint::SlidingPrefix {
            stable_fraction: 0.75,
        };
        let b = CacheHint::SlidingPrefix {
            stable_fraction: 0.75,
        };
        assert_eq!(a, b);

        let c = CacheHint::SlidingPrefix {
            stable_fraction: 0.5,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn cache_hint_clone() {
        let hint = CacheHint::SlidingPrefix {
            stable_fraction: 0.8,
        };
        let cloned = hint;
        assert_eq!(
            cloned,
            CacheHint::SlidingPrefix {
                stable_fraction: 0.8
            }
        );
    }

    #[test]
    fn cache_hint_debug() {
        let hint = CacheHint::Always;
        let dbg = format!("{:?}", hint);
        assert!(dbg.contains("Always"));
    }

    #[test]
    fn cache_hint_serde_roundtrip() {
        let hints = vec![
            CacheHint::Always,
            CacheHint::UntilChanged,
            CacheHint::RecentlyChanged,
            CacheHint::SlidingPrefix {
                stable_fraction: 0.75,
            },
            CacheHint::Never,
        ];
        for hint in hints {
            let json = serde_json::to_string(&hint).unwrap();
            let parsed: CacheHint = serde_json::from_str(&json).unwrap();
            assert_eq!(hint, parsed);
        }
    }
}
