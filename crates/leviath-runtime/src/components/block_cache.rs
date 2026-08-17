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
    // Only a growing region is worth splitting. Chunking exists to give the
    // settled head of a region a boundary that survives into the next request,
    // and that only means anything where entries are appended and left alone:
    //
    // - `Stable` content does not move, so its single block is already a
    //   boundary and splitting it just spends blocks.
    // - `Rewritten` content changes in place, so no boundary inside it survives
    //   and splitting buys nothing at all.
    //
    // Which is why this is keyed on what the region declared rather than on its
    // kind: a pinned region may be either, and only the blueprint knows.
    let chunks = match region.volatility {
        leviath_core::Volatility::Grows => append_only_chunks(entries, CACHE_CHUNK_TOKENS),
        _ => vec![entries.join("\n\n")],
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let text = match index {
            0 => super::context_window::labelled(region, chunk),
            _ => format!("## {} (continued)\n{}", region.name, chunk),
        };
        blocks.push(leviath_providers::SystemBlock {
            text,
            cache_hint: hint,
            volatility: region.volatility,
            region: region.name.clone(),
        });
    }
}

/// Push a region's entries as `[name]:` system blocks, split when the region
/// declared that it grows.
///
/// The bracket label rather than [`super::context_window::labelled`]'s heading,
/// because that is the shape these regions have always rendered with and the
/// prompt is not the place to make a cosmetic change.
pub(super) fn push_bracketed(
    blocks: &mut Vec<leviath_providers::SystemBlock>,
    region: &Region,
    hint: leviath_core::CacheHint,
) {
    let entries: Vec<String> = region.content.iter().map(|e| e.content.clone()).collect();
    let chunks = match region.volatility {
        leviath_core::Volatility::Grows => append_only_chunks(&entries, CACHE_CHUNK_TOKENS),
        _ => vec![entries.join("\n\n")],
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let text = match index {
            0 => format!("[{}]:\n{}", region.name, chunk),
            _ => format!("[{} continued]:\n{}", region.name, chunk),
        };
        blocks.push(leviath_providers::SystemBlock {
            text,
            cache_hint: hint,
            volatility: region.volatility,
            region: region.name.clone(),
        });
    }
}

/// The cache hint for a region whose kind describes when it is *thrown away*
/// rather than how it changes.
///
/// `temporary` and `clearable` are lifecycle kinds: one is dropped at stage
/// exit, the other on demand. Both used to be tagged `Never`, which reads that
/// lifecycle as "this content never holds still" - and for the boundary it is
/// right, since caching across a wholesale drop would buy nothing. Between
/// those boundaries it is wrong. A stage that reads a corpus into a `temporary`
/// region and then works through it for forty calls has an append-mostly region
/// that changes at the tail, which is the shape chunking exists for; tagging it
/// `Never` re-sent the whole corpus at full rate on every one of those calls
/// (issue #490: 5.36M tokens across 46 calls, the largest cost line in the run).
///
/// The kind cannot answer this and the author can, so it is read from the
/// declaration. `Rewritten` is the default, so a region that says nothing keeps
/// exactly the behaviour it had.
pub(super) fn lifecycle_cache_hint(
    volatility: leviath_core::Volatility,
) -> leviath_core::CacheHint {
    match volatility {
        leviath_core::Volatility::Rewritten => leviath_core::CacheHint::Never,
        leviath_core::Volatility::Stable | leviath_core::Volatility::Grows => {
            leviath_core::CacheHint::UntilChanged
        }
    }
}

/// A digest of one system block's text, for deciding what held still.
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

pub(super) fn block_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
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
/// Where a block belongs in the prompt, most-stable first.
///
/// Volatility leads, because it is the thing prefix caching actually responds
/// to: a block that changes invalidates every block behind it, so the ordering
/// that pays is stable content first and churn last, whatever those blocks are
/// otherwise made of.
///
/// The cache hint breaks ties within a tier. It used to lead, and it is derived
/// from the region's *kind*, which is why this needed changing: a pinned region
/// sounds immutable and is written constantly, so ordering by kind put churn at
/// the front of the prefix and invalidated everything behind it (issue #474).
pub(super) fn block_sort_priority(block: &leviath_providers::SystemBlock) -> (u8, u8) {
    let volatility = match block.volatility {
        leviath_core::Volatility::Stable => 0,
        leviath_core::Volatility::Grows => 1,
        leviath_core::Volatility::Rewritten => 2,
    };
    (volatility, cache_hint_sort_priority(block.cache_hint))
}

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

    // ─── lifecycle kinds (issue #490) ───────────────────────────────────────

    fn lifecycle_region(
        kind: RegionKind,
        volatility: leviath_core::Volatility,
        entry_count: usize,
    ) -> Region {
        let mut region = Region::new("corpus".to_string(), kind, 1_000_000);
        region.volatility = volatility;
        for i in 0..entry_count {
            let body = format!("excerpt {i} {}", "word ".repeat(600));
            region.add_entry(body, 750).unwrap();
        }
        region
    }

    /// A region that says nothing about how it moves keeps exactly the
    /// behaviour it had: one block, uncacheable. This is what makes the change
    /// safe to ship on by default - nobody who has not opted in is affected.
    #[test]
    fn an_undeclared_lifecycle_region_is_unchanged() {
        for kind in [RegionKind::Temporary, RegionKind::Clearable] {
            let region = lifecycle_region(kind, leviath_core::Volatility::Rewritten, 8);
            let mut blocks = Vec::new();
            push_bracketed(
                &mut blocks,
                &region,
                lifecycle_cache_hint(region.volatility),
            );

            assert_eq!(blocks.len(), 1, "one block, as before");
            assert_eq!(blocks[0].cache_hint, CacheHint::Never);
            assert!(blocks[0].text.starts_with("[corpus]:\n"));
        }
    }

    /// The fix. A corpus the author says accumulates gets interior boundaries
    /// and a hint a marker can land on, so the settled head caches and only the
    /// tail is re-sent.
    #[test]
    fn a_lifecycle_region_declared_grows_is_chunk_cacheable() {
        let region = lifecycle_region(RegionKind::Temporary, leviath_core::Volatility::Grows, 12);
        let mut blocks = Vec::new();
        push_bracketed(
            &mut blocks,
            &region,
            lifecycle_cache_hint(region.volatility),
        );

        let count = blocks.len();
        assert!(
            count > 1,
            "a 12-entry corpus past the chunk budget splits: {count} blocks"
        );
        assert!(
            blocks
                .iter()
                .all(|b| b.cache_hint == CacheHint::UntilChanged)
        );
        assert!(blocks[0].text.starts_with("[corpus]:\n"));
        assert!(
            blocks[1].text.starts_with("[corpus continued]:\n"),
            "a split region still says which region it is"
        );
    }

    /// Chunking must not change a single byte of what the model reads, headings
    /// aside - the whole point is a cheaper way to send the same corpus.
    #[test]
    fn chunking_a_lifecycle_region_preserves_its_content() {
        let grows = lifecycle_region(RegionKind::Temporary, leviath_core::Volatility::Grows, 9);
        let whole = lifecycle_region(
            RegionKind::Temporary,
            leviath_core::Volatility::Rewritten,
            9,
        );
        let mut split = Vec::new();
        push_bracketed(&mut split, &grows, CacheHint::UntilChanged);
        let mut one = Vec::new();
        push_bracketed(&mut one, &whole, CacheHint::Never);

        let strip = |text: &str| {
            text.replace("[corpus continued]:\n", "")
                .replace("[corpus]:\n", "")
        };
        let rejoined: String = split
            .iter()
            .map(|b| strip(&b.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(rejoined, strip(&one[0].text));
    }

    /// The declaration is what decides, because the kind cannot: `temporary`
    /// says when the region is thrown away, not whether it holds still between
    /// those moments.
    #[test]
    fn the_hint_follows_the_declaration_not_the_kind() {
        use leviath_core::Volatility;
        assert_eq!(
            lifecycle_cache_hint(Volatility::Rewritten),
            CacheHint::Never
        );
        assert_eq!(
            lifecycle_cache_hint(Volatility::Grows),
            CacheHint::UntilChanged
        );
        assert_eq!(
            lifecycle_cache_hint(Volatility::Stable),
            CacheHint::UntilChanged
        );
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
        // Chunking is what a *growing* region gets; a region that does not say
        // so is left as one block, so the declaration is part of the fixture.
        region.volatility = leviath_core::Volatility::Grows;
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
