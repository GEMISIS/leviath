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

    #[test]
    fn cache_breakpoint_construction() {
        let bp = CacheBreakpoint {
            after_message_index: 5,
            hint: CacheHint::Always,
        };
        assert_eq!(bp.after_message_index, 5);
        assert_eq!(bp.hint, CacheHint::Always);
    }

    #[test]
    fn cache_breakpoint_clone() {
        let bp = CacheBreakpoint {
            after_message_index: 3,
            hint: CacheHint::UntilChanged,
        };
        let cloned = bp.clone();
        assert_eq!(cloned.after_message_index, 3);
        assert_eq!(cloned.hint, CacheHint::UntilChanged);
    }

    #[test]
    fn cache_breakpoint_debug() {
        let bp = CacheBreakpoint {
            after_message_index: 10,
            hint: CacheHint::Never,
        };
        let dbg = format!("{:?}", bp);
        assert!(dbg.contains("10"));
        assert!(dbg.contains("Never"));
    }
}
