//! How assembled system blocks are made cacheable.
//!
//! Split out of `context_window` because "what an agent remembers" and "which
//! of those bytes a provider can read back" are different questions, and this
//! is the half that has to hold still while the region kinds grow.
//!
//! A provider caches by prefix and matches only at a block boundary. Everything
//! here exists to make sure the boundaries offered are ones that survive into
//! the next request, and that a breakpoint is only ever placed at one of them.

use super::*;

/// How much text one cache chunk of an append-only region holds, in tokens.
///
/// The chunk is the unit of what can be cached, so it wants to be at least a
/// provider's minimum cacheable prefix - Anthropic's is 1024 tokens on Sonnet -
/// and small enough that the uncached tail stays cheap. It also bounds how many
/// blocks a large region becomes: 200k tokens of history is a hundred blocks,
/// which is fine to send and costs nothing to skip.
pub(super) const CACHE_CHUNK_TOKENS: usize = 2048;

/// Split an append-only region's entries into blocks at boundaries that survive
/// into the next request.
///
/// A provider matches a cached prefix only at a *block boundary*. A region
/// rendered as one block therefore offers exactly one boundary, and it sits
/// after the region's newest content - so the moment the region grows, the entry
/// named there is unreadable and every call rewrites the lot at the write
/// premium. Measured before this: 456,860 cache-write tokens across twelve
/// calls with zero reads (issue #474).
///
/// Chunking gives the region interior boundaries. Entries are packed greedily
/// and a full chunk is never repacked, so a boundary that existed last turn
/// exists this turn with the same bytes in front of it - which is precisely the
/// condition [`mark_breakpoint_eligibility`] looks for. The frozen head of the
/// region then caches and only the tail is re-sent, which is how the message
/// path has always behaved.
///
/// Only for kinds whose entries append. A region that rewrites entries in place
/// has no stable interior boundary to offer, and gets its protection from
/// eligibility alone.
pub(super) fn append_only_chunks(entries: &[String], budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut tokens = 0usize;
    for entry in entries {
        current.push(entry.as_str());
        tokens = tokens.saturating_add(leviath_core::estimate_tokens(entry));
        if tokens >= budget {
            chunks.push(current.join("\n\n"));
            current.clear();
            tokens = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current.join("\n\n"));
    }
    chunks
}

/// Push a region's entries as chunked system blocks.
///
/// Every chunk after the first says it continues the same region. Splitting for
/// the cache's benefit must not cost the model what the heading buys it:
/// without this the second half of a region arrives as unlabelled prose and
/// reads as content from nowhere, which is the confusion naming regions was
/// meant to remove. The continuation line is a constant, so a frozen chunk
/// stays frozen and the split still caches.
pub(super) fn push_chunked(
    blocks: &mut Vec<leviath_providers::SystemBlock>,
    region: &Region,
    entries: &[String],
    hint: leviath_core::CacheHint,
) {
    let chunks = append_only_chunks(entries, CACHE_CHUNK_TOKENS);
    for (index, chunk) in chunks.iter().enumerate() {
        let text = match index {
            0 => super::context_window::labelled(region, chunk),
            _ => format!("## {} (continued)\n{}", region.name, chunk),
        };
        blocks.push(leviath_providers::SystemBlock {
            text,
            cache_hint: hint,
            breakpoint_eligible: true,
        });
    }
}

/// A digest of one system block's text, for deciding what held still.
pub(super) fn block_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Mark every block a cache breakpoint could actually be read back at.
///
/// A provider caches by prefix, so the entry a breakpoint names is readable
/// next time only if every byte before it is unchanged. That makes the rule
/// exact rather than a heuristic: a block is eligible when it, and every block
/// ahead of it, is byte-identical to the previous request. The eligible set is
/// therefore the longest common prefix of this request's block hashes and the
/// last one's.
///
/// Before the first request there is nothing to compare against, so everything
/// is eligible - the first call is a write whatever we do, and marking it
/// ineligible would forfeit the entry the second call wants to read.
///
/// This is what stops a growing region being paid for over and over. A region
/// that gained content is not in the common prefix, so no breakpoint lands at
/// or after it, and the stable head in front of it keeps the entry it always
/// had (issue #474: 456,860 cache-write tokens across twelve calls, zero reads,
/// because the only breakpoint sat after content that changed every call).
pub(super) fn mark_breakpoint_eligibility(
    blocks: &mut [leviath_providers::SystemBlock],
    previous: &[u64],
) -> Vec<u64> {
    let hashes: Vec<u64> = blocks.iter().map(|b| block_hash(&b.text)).collect();
    if previous.is_empty() {
        return hashes;
    }
    let stable = hashes
        .iter()
        .zip(previous)
        .take_while(|(now, before)| now == before)
        .count();
    for (index, block) in blocks.iter_mut().enumerate() {
        block.breakpoint_eligible = index < stable;
    }
    hashes
}

/// A stable digest of the assembled system prefix.
///
/// Over the block texts in final order, which is exactly the byte sequence
/// Anthropic prefix-matches against. Hints are excluded deliberately: they
/// decide ordering, and ordering is already reflected in the sequence.
pub(super) fn system_prefix_hash(blocks: &[leviath_providers::SystemBlock]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for block in blocks {
        block.text.hash(&mut hasher);
    }
    hasher.finish()
}

/// Sort priority for a system block's cache hint.
///
/// Anthropic caches system content by prefix matching, so the most stable
/// blocks must sort first to form the cacheable prefix. Lower value = earlier.
pub(super) fn cache_hint_sort_priority(hint: leviath_core::CacheHint) -> u8 {
    use leviath_core::CacheHint;
    match hint {
        CacheHint::Always => 0,               // Pinned, CompactHistory - most stable
        CacheHint::SlidingPrefix { .. } => 1, // Partially stable
        CacheHint::UntilChanged => 2,         // Compacting - changes on compaction
        // Same tier as UntilChanged on purpose: the hint marks where a cache
        // breakpoint belongs, never where a block belongs in the prompt.
        CacheHint::RecentlyChanged => 2,
        CacheHint::Never => 3, // Temporary, Clearable - changes every iteration
    }
}

/// The most cache breakpoints assembly will let the system blocks claim.
///
/// Anthropic allows four `cache_control` blocks across the whole request and
/// the provider hands the system blocks first claim on that budget, so leaving
/// one run unclaimed is what keeps a breakpoint available for the messages.
pub(super) const MAX_SYSTEM_CACHE_RUNS: usize = 3;

/// Split the volatile tier of the system prompt at its most recently changed
/// block, by retagging that block and every block after it as
/// [`leviath_core::CacheHint::RecentlyChanged`].
///
/// Providers place one cache breakpoint per run of same-hint blocks, so with a
/// single `UntilChanged` run the only breakpoint sits at the end of the tier.
/// A block mutating in the middle of that run therefore invalidates the whole
/// run, and every block after the mutation is re-sent as a cache write. Adding
/// a boundary just before the changed block gives the unchanged head of the
/// tier a cache entry of its own. Only the breakpoint metadata moves: block
/// order and block text are both left exactly as the sort left them, and this
/// runs after the sort so it cannot influence ordering at all. The effect on
/// cache-write volume is not measurable inside this repository.
///
/// `recency` carries the newest entry timestamp of the region behind each
/// `UntilChanged` block, in the order those blocks were assembled. The sort is
/// stable and every block in the tier shares one sort priority, so that order
/// is also the order the blocks appear in now.
///
/// Nothing is retagged when the newest block is already the first one (there is
/// no stable head to protect), or when the blocks already fill the run budget.
pub(super) fn mark_recently_changed_run(
    blocks: &mut [leviath_providers::SystemBlock],
    recency: &[i64],
) {
    use leviath_core::CacheHint;

    let mut volatile: Vec<usize> = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if block.cache_hint == CacheHint::UntilChanged {
            volatile.push(index);
        }
    }

    // The first block carrying the newest timestamp. Ties resolve to the
    // earliest block, which is the conservative choice: when two regions were
    // written in the same second, both of them changed, and the boundary
    // belongs ahead of the earlier one.
    let mut boundary = 0usize;
    let mut newest = i64::MIN;
    for (position, &stamp) in recency.iter().enumerate() {
        if stamp > newest {
            newest = stamp;
            boundary = position;
        }
    }
    if boundary == 0 {
        return;
    }

    // Count the breakpoints the provider would place today. Splitting the
    // volatile tier adds exactly one, so refusing at the limit is what keeps
    // the messages breakpoint from being squeezed out.
    let mut runs = 0usize;
    for index in 0..blocks.len() {
        let hint = blocks[index].cache_hint;
        if hint != CacheHint::Never && blocks.get(index + 1).map(|b| b.cache_hint) != Some(hint) {
            runs += 1;
        }
    }
    if runs >= MAX_SYSTEM_CACHE_RUNS {
        return;
    }

    for &index in volatile.iter().skip(boundary) {
        blocks[index].cache_hint = CacheHint::RecentlyChanged;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::{CacheHint, Region, RegionKind};

    fn entries(n: usize, each: &str) -> Vec<String> {
        (0..n).map(|i| format!("{i}:{each}")).collect()
    }

    fn block(text: &str, hint: CacheHint, eligible: bool) -> leviath_providers::SystemBlock {
        leviath_providers::SystemBlock {
            text: text.to_string(),
            cache_hint: hint,
            breakpoint_eligible: eligible,
        }
    }

    // ─── chunking ───────────────────────────────────────────────────────────

    /// The property the whole fix rests on: a boundary that existed last turn
    /// exists this turn, with the same bytes in front of it. Without it there is
    /// nothing a provider can match.
    #[test]
    fn appending_never_repacks_an_earlier_chunk() {
        let big = "word ".repeat(400);
        let mut before: Vec<String> = Vec::new();
        for turn in 1..12 {
            let now = append_only_chunks(&entries(turn, &big), 2048);
            // Every *full* chunk the previous turn had, this turn still has,
            // verbatim. The last chunk is the live tail and is meant to grow -
            // it is the only part a provider has to re-send.
            let frozen = before.len().saturating_sub(1);
            for (old, new) in before.iter().take(frozen).zip(&now) {
                assert_eq!(old, new, "a frozen chunk was repacked at turn {turn}");
            }
            assert!(now.len() >= before.len(), "chunks never merge back");
            before = now;
        }
    }

    /// Nothing is lost or reordered by splitting: the chunks rejoin into exactly
    /// the text a single block used to carry.
    #[test]
    fn chunking_preserves_the_content_exactly() {
        let all = entries(25, &"word ".repeat(120));
        let rejoined = append_only_chunks(&all, 2048).join("\n\n");
        assert_eq!(rejoined, all.join("\n\n"));
    }

    #[test]
    fn a_region_with_nothing_in_it_produces_no_chunks() {
        assert!(append_only_chunks(&[], 2048).is_empty());
    }

    /// One entry larger than the budget is still one chunk: the entry is the
    /// smallest thing there is to split on.
    #[test]
    fn an_entry_larger_than_the_budget_is_its_own_chunk() {
        let huge = "word ".repeat(5000);
        let chunks = append_only_chunks(std::slice::from_ref(&huge), 2048);
        assert_eq!(chunks, vec![huge.clone()]);
    }

    /// Splitting for the cache must not cost the model the heading. Every chunk
    /// after the first says which region it continues.
    #[test]
    fn every_chunk_after_the_first_names_the_region_it_continues() {
        let mut region = Region::new("sources".to_string(), RegionKind::Pinned, 1_000_000);
        for entry in entries(20, &"word ".repeat(200)) {
            region.add_entry(entry, 250).expect("fits");
        }
        let contents: Vec<String> = region.content.iter().map(|e| e.content.clone()).collect();

        let mut blocks = Vec::new();
        push_chunked(&mut blocks, &region, &contents, CacheHint::Always);

        assert!(blocks.len() > 1, "the fixture is meant to span chunks");
        assert!(blocks[0].text.starts_with("## sources"));
        for block in &blocks[1..] {
            assert!(
                block.text.starts_with("## sources (continued)"),
                "a continuation arrived unlabelled: {:.40}",
                block.text
            );
        }
        // And every entry is still present, in order.
        let whole = blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut last = 0;
        for (i, _) in contents.iter().enumerate() {
            let needle = format!("{i}:");
            let at = whole.find(&needle).expect("entry present");
            assert!(at >= last, "entries came out of order");
            last = at;
        }
    }

    // ─── eligibility ────────────────────────────────────────────────────────

    /// Nothing to compare against, so nothing is held back: the first request
    /// is a write whatever we do, and refusing it would forfeit the entry the
    /// second request wants to read.
    #[test]
    fn the_first_request_leaves_every_block_eligible() {
        let mut blocks = vec![
            block("a", CacheHint::Always, true),
            block("b", CacheHint::Always, true),
        ];
        let hashes = mark_breakpoint_eligibility(&mut blocks, &[]);
        assert_eq!(hashes.len(), 2);
        assert!(blocks.iter().all(|b| b.breakpoint_eligible));
    }

    /// The rule, stated as the provider needs it: a block is eligible only if it
    /// and everything ahead of it is byte-identical to last time.
    #[test]
    fn eligibility_stops_at_the_first_block_that_changed() {
        let mut first = vec![
            block("head", CacheHint::Always, true),
            block("middle", CacheHint::Always, true),
            block("tail", CacheHint::Always, true),
        ];
        let previous = mark_breakpoint_eligibility(&mut first, &[]);

        let mut second = vec![
            block("head", CacheHint::Always, true),
            block("middle grew", CacheHint::Always, true),
            block("tail", CacheHint::Always, true),
        ];
        mark_breakpoint_eligibility(&mut second, &previous);

        assert!(second[0].breakpoint_eligible, "the head held still");
        assert!(!second[1].breakpoint_eligible, "this is what changed");
        assert!(
            !second[2].breakpoint_eligible,
            "and everything after it is behind changed bytes, however stable its own text"
        );
    }

    /// A prefix that held still entirely stays entirely eligible.
    #[test]
    fn an_unchanged_prefix_stays_eligible() {
        let mut first = vec![
            block("head", CacheHint::Always, true),
            block("body", CacheHint::Always, true),
        ];
        let previous = mark_breakpoint_eligibility(&mut first, &[]);
        let mut second = vec![
            block("head", CacheHint::Always, true),
            block("body", CacheHint::Always, true),
            block("new tail", CacheHint::Always, true),
        ];
        mark_breakpoint_eligibility(&mut second, &previous);
        assert!(second[0].breakpoint_eligible);
        assert!(second[1].breakpoint_eligible);
        assert!(
            !second[2].breakpoint_eligible,
            "the new block is not proven yet"
        );
    }
}
