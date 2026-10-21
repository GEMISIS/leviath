//! The runtime's half of making assembled system blocks cacheable.
//!
//! Chunking, ordering, hashing and breakpoint placement are provider-neutral
//! and live in [`leviath_core::context::cache`], where a program that lays out
//! its own context can use them. What is left here needs the runtime: telling
//! the author, through the log, that a region declared `stable` keeps moving.

use leviath_core::context::block_hash;

/// Warn about a region that declared itself `stable` and then moved.
///
/// A wrong declaration is worse than no declaration: `stable` sorts a region to
/// the *front* of the prefix, so churn declared stable lands in the most
/// destructive position there is, and lands there precisely because we believed
/// the label. That is not hypothetical - the bug this whole mechanism replaces
/// was a region tagged as the most stable kind of content while gaining an entry
/// on every compaction.
///
/// The blocks are hashed every request anyway, so catching it is nearly free:
/// one change can be setup settling, two is a pattern. Reported once per region
/// per run, because a run that does this does it every turn and the point is to
/// tell the author, not to fill the log.
///
/// A warning rather than a correction. Re-sorting mid-run would move the prefix
/// itself, which is the very thing being paid for here, and the author can fix
/// the manifest in less time than the run takes.
pub(super) fn warn_on_unstable_declaration(
    blocks: &[leviath_providers::SystemBlock],
    previous: &[u64],
    changes: &mut std::collections::HashMap<String, usize>,
) {
    if previous.is_empty() {
        return; // nothing to compare against yet
    }
    for (index, block) in blocks.iter().enumerate() {
        if block.volatility != leviath_core::Volatility::Stable {
            continue;
        }
        let Some(before) = previous.get(index) else {
            continue; // a block that did not exist last time is not a change
        };
        if block_hash(&block.text) == *before {
            continue;
        }
        let seen = changes.entry(block.region.clone()).or_default();
        *seen += 1;
        if *seen == 2 {
            tracing::warn!(
                region = %block.region,
                "this region declares volatility = \"stable\" and its contents keep \
                 changing, so it is sorted to the front of the prompt where every \
                 change invalidates the cache for everything behind it. Declare it \
                 \"grows\" if entries are appended, or \"rewritten\" if they change \
                 in place"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::CacheHint;

    // ─── declaration verification ───────────────────────────────────────────

    fn stable_block(region: &str, text: &str) -> leviath_providers::SystemBlock {
        leviath_providers::SystemBlock {
            text: text.to_string(),
            cache_hint: CacheHint::Always,
            volatility: leviath_core::Volatility::Stable,
            region: region.to_string(),
        }
    }

    /// A region that declares itself stable and then keeps changing is a wrong
    /// declaration, and a wrong one is worse than none: `stable` sorts it to the
    /// front, where every change invalidates everything behind it.
    ///
    /// Counted rather than reported on sight - one change can be setup settling,
    /// two is a pattern - and counted once per region, because a run that does
    /// this does it every turn.
    #[test]
    fn a_stable_region_that_keeps_changing_is_counted_once() {
        let mut seen = std::collections::HashMap::new();
        let mut previous = vec![block_hash("first")];

        for turn in 0..5 {
            let blocks = vec![stable_block("notes", &format!("turn {turn}"))];
            warn_on_unstable_declaration(&blocks, &previous, &mut seen);
            previous = vec![block_hash(&blocks[0].text)];
        }
        assert_eq!(seen.get("notes"), Some(&5));
    }

    /// A region that holds still is never reported, however often it is checked.
    #[test]
    fn a_stable_region_that_holds_still_is_never_reported() {
        let mut seen = std::collections::HashMap::new();
        let blocks = vec![stable_block("task", "unchanging")];
        let previous = vec![block_hash("unchanging")];
        for _ in 0..3 {
            warn_on_unstable_declaration(&blocks, &previous, &mut seen);
        }
        assert!(seen.is_empty());
    }

    /// Only a `stable` declaration can be wrong in this way. A region that says
    /// it grows or is rewritten is expected to change.
    #[test]
    fn a_region_that_never_claimed_to_be_stable_is_not_reported() {
        let mut seen = std::collections::HashMap::new();
        let mut block = stable_block("history", "changed");
        block.volatility = leviath_core::Volatility::Grows;
        warn_on_unstable_declaration(&[block], &[block_hash("before")], &mut seen);
        assert!(seen.is_empty());
    }

    /// The first request has nothing to compare against, and a block that did
    /// not exist last time has not changed.
    #[test]
    fn nothing_is_reported_without_a_previous_request_to_compare() {
        let mut seen = std::collections::HashMap::new();
        let blocks = vec![stable_block("task", "a"), stable_block("notes", "b")];
        warn_on_unstable_declaration(&blocks, &[], &mut seen);
        assert!(seen.is_empty());
        // One previous hash, two blocks now: the second is new, not changed.
        warn_on_unstable_declaration(&blocks, &[block_hash("a")], &mut seen);
        assert!(seen.is_empty());
    }
}
