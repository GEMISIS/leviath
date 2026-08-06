//! Allocator tuning for the leviath binary.
//!
//! This crate holds exactly one capability: telling mimalloc to purge freed
//! memory at free time instead of deferring it. It exists as a separate
//! crate because the call is an `unsafe` C FFI invocation and the rest of
//! the workspace compiles under `unsafe_code = "forbid"`; the policy is that
//! OS-level unsafe lives in small audited places, and this crate is one.
//!
//! ## Why purge at free time
//!
//! mimalloc's default purge delay (10ms) is not a timer. It is a minimum age
//! checked only when the owning thread next touches the allocator. A daemon
//! worker thread that goes idle right after a burst therefore never returns
//! its freed pages to the OS: the process sits tens to hundreds of MB above
//! its idle baseline holding pages that contain nothing. Measured on the
//! daemon: byte-for-byte flat across 10 idle minutes, then released by the
//! next moment of allocator activity.
//!
//! With a delay of zero the `free` that empties a span performs the OS
//! handoff itself, so nothing ever waits on future activity. The cost is a
//! syscall per emptied span (paid at run-reap, off every latency path) and
//! re-faulting pages when a later burst reallocates (milliseconds per burst
//! ramp, measured indistinguishable end to end on the benchmark suite). The
//! win is a daemon whose resident memory actually returns to its baseline
//! when work finishes.
//!
//! A user who exports `MIMALLOC_PURGE_DELAY` themselves has made a choice,
//! and [`use_purge_at_free_unless_overridden`] leaves it alone.
//!
//! ## Audit note (the single unsafe call)
//!
//! `mi_option_set(option, value)` writes a `long` into mimalloc's static
//! options table. It allocates nothing, frees nothing, holds no lock, and is
//! documented callable at any time; subsequent purge decisions read the
//! stored value. The option index for `mi_option_purge_delay` is `15` in
//! the mimalloc v2 header vendored by `libmimalloc-sys` 0.1.49, anchored on
//! both sides by constants that crate does bind: `mi_option_eager_commit_delay
//! = 14` and `mi_option_use_numa_nodes = 16` (the crate predates the
//! `reset_delay` to `purge_delay` rename and simply does not name index 15).
//! The test below asserts the round trip through `mi_option_get`, so a
//! vendored-header reordering would fail loudly rather than silently tune
//! the wrong knob.

/// The environment variable mimalloc itself reads for this option; a user
/// who set it keeps their value.
pub const PURGE_DELAY_VAR: &str = "MIMALLOC_PURGE_DELAY";

/// `mi_option_purge_delay` in the vendored mimalloc v2 option enum. See the
/// module-level audit note for how this index is pinned.
#[cfg(feature = "mimalloc")]
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

/// Configure mimalloc to purge freed memory at free time, unless the user
/// chose their own delay via [`PURGE_DELAY_VAR`].
///
/// Returns whether the option was applied, so the decision is observable in
/// tests. Call once, early in `main`; pages freed before the call simply
/// purge on the pre-existing schedule.
#[cfg(feature = "mimalloc")]
pub fn use_purge_at_free_unless_overridden() -> bool {
    if std::env::var_os(PURGE_DELAY_VAR).is_some() {
        return false;
    }
    // SAFETY: writes a long into mimalloc's in-process options table; no
    // memory is allocated, freed, or aliased. See the module audit note.
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
    true
}

/// Without the mimalloc feature there is nothing to tune: builds on the
/// system allocator keep their platform defaults.
#[cfg(not(feature = "mimalloc"))]
pub fn use_purge_at_free_unless_overridden() -> bool {
    false
}

#[cfg(all(test, feature = "mimalloc"))]
mod tests {
    use super::*;

    /// The applied value must be readable back through mimalloc itself: this
    /// is the guard against the option index drifting in a future vendored
    /// header (the failure mode would be silently tuning the wrong knob).
    #[test]
    fn purge_at_free_is_applied_and_round_trips_through_mimalloc() {
        temp_env::with_var_unset(PURGE_DELAY_VAR, || {
            assert!(use_purge_at_free_unless_overridden());
            // SAFETY: reads a long from the options table just written above.
            let value = unsafe { libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY) };
            assert_eq!(value, 0);
        });
    }

    /// An exported MIMALLOC_PURGE_DELAY is the user's decision, whatever the
    /// value - even an explicit "0" is theirs to own, not ours to rewrite.
    #[test]
    fn a_user_exported_delay_is_left_alone() {
        temp_env::with_var(PURGE_DELAY_VAR, Some("25"), || {
            assert!(!use_purge_at_free_unless_overridden());
        });
        temp_env::with_var(PURGE_DELAY_VAR, Some("0"), || {
            assert!(!use_purge_at_free_unless_overridden());
        });
    }
}
