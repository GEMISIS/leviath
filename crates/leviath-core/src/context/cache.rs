//! How assembled system blocks are made cacheable.
//!
//! Apart from the rest of [`crate::context`] because "what an agent
//! remembers" and "which of those bytes a provider can read back" are
//! different questions, and this is the half that has to hold still while the
//! region kinds grow.
//!
//! A provider caches by prefix and matches only at a block boundary. Everything
//! here exists to make sure the boundaries offered are ones that survive into
//! the next request, and that a breakpoint is only ever placed at one of them.

use super::{SystemBlock, labelled};
use crate::{CacheHint, Region, Volatility};

/// How much text one cache chunk of an append-only region holds, in tokens.
///
/// The chunk is the unit of what can be cached, so it wants to be at least a
/// provider's minimum cacheable prefix - Anthropic's is 1024 tokens on Sonnet -
/// and small enough that the uncached tail stays cheap. It also bounds how many
/// blocks a large region becomes: 200k tokens of history is a hundred blocks,
/// which is fine to send and costs nothing to skip.
pub const CACHE_CHUNK_TOKENS: usize = 2048;

/// Split an append-only region's entries into blocks at boundaries that survive
/// into the next request.
///
/// A provider matches a cached prefix only at a *block boundary*. A region
/// rendered as one block therefore offers exactly one boundary, and it sits
/// after the region's newest content - so the moment the region grows, the entry
/// named there is unreadable and every call rewrites the lot at the write
/// premium. Measured before this: 456,860 cache-write tokens across twelve
/// calls with zero reads.
///
/// Chunking gives the region interior boundaries. Entries are packed greedily
/// and a full chunk is never repacked, so a boundary that existed last turn
/// exists this turn with the same bytes in front of it - which is precisely the
/// condition a cache breakpoint needs. The frozen head of the region then
/// caches and only the tail is re-sent, as on the message path.
///
/// Only for kinds whose entries append. A region that rewrites entries in place
/// has no stable interior boundary to offer, and gets its protection from
/// eligibility alone.
pub fn append_only_chunks(entries: &[String], budget: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut tokens = 0usize;
    for entry in entries {
        current.push(entry.as_str());
        tokens = tokens.saturating_add(crate::estimate_tokens(entry));
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
pub fn push_chunked(
    blocks: &mut Vec<SystemBlock>,
    region: &Region,
    entries: &[String],
    hint: CacheHint,
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
        Volatility::Grows => append_only_chunks(entries, CACHE_CHUNK_TOKENS),
        _ => vec![entries.join("\n\n")],
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let text = match index {
            0 => labelled(region, chunk),
            _ => format!("## {} (continued)\n{}", region.name, chunk),
        };
        blocks.push(SystemBlock {
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
/// The bracket label rather than [`labelled`]'s heading, because that is the
/// shape these regions have always rendered with and the prompt is not the
/// place to make a cosmetic change.
pub fn push_bracketed(blocks: &mut Vec<SystemBlock>, region: &Region, hint: CacheHint) {
    let entries: Vec<String> = region
        .content
        .iter()
        .map(|e| e.content.to_string())
        .collect();
    let chunks = match region.volatility {
        Volatility::Grows => append_only_chunks(&entries, CACHE_CHUNK_TOKENS),
        _ => vec![entries.join("\n\n")],
    };
    for (index, chunk) in chunks.iter().enumerate() {
        let text = match index {
            0 => format!("[{}]:\n{}", region.name, chunk),
            _ => format!("[{} continued]:\n{}", region.name, chunk),
        };
        blocks.push(SystemBlock {
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
/// exit, the other on demand. Tagging them `Never` reads that lifecycle as
/// "this content never holds still" - right at the boundary, since caching
/// across a wholesale drop buys nothing, and wrong between them. A stage that
/// reads a corpus into a `temporary` region and then works through it for forty
/// calls has an append-mostly region that changes at the tail, which is the
/// shape chunking exists for; `Never` re-sends the whole corpus at full rate on
/// every one of those calls, measured once at 5.36M tokens across 46 calls.
///
/// The kind cannot answer this and the author can, so it is read from the
/// declaration. `Rewritten` is the default, so a region that says nothing keeps
/// exactly the behaviour it had.
pub fn lifecycle_cache_hint(volatility: Volatility) -> CacheHint {
    match volatility {
        Volatility::Rewritten => CacheHint::Never,
        Volatility::Stable | Volatility::Grows => CacheHint::UntilChanged,
    }
}

/// A digest of one system block's text, for deciding what held still.
pub fn block_hash(text: &str) -> u64 {
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
pub fn system_prefix_hash(blocks: &[SystemBlock]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for block in blocks {
        block.text.hash(&mut hasher);
    }
    hasher.finish()
}

/// Where a block belongs in the prompt, most-stable first. Lower value =
/// earlier.
///
/// Volatility leads, because it is the thing prefix caching actually responds
/// to: a block that changes invalidates every block behind it, so the ordering
/// that pays is stable content first and churn last, whatever those blocks are
/// otherwise made of.
///
/// The cache hint breaks ties within a tier rather than leading. It is derived
/// from the region's *kind*, and a pinned region sounds immutable while being
/// written constantly, so leading with it puts churn at the front of the prefix
/// and invalidates everything behind it.
pub fn block_sort_priority(block: &SystemBlock) -> (u8, u8) {
    let volatility = match block.volatility {
        Volatility::Stable => 0,
        Volatility::Grows => 1,
        Volatility::Rewritten => 2,
    };
    (volatility, cache_hint_sort_priority(block.cache_hint))
}

/// Sort priority for a system block's cache hint.
///
/// Anthropic caches system content by prefix matching, so the most stable
/// blocks must sort first to form the cacheable prefix. Lower value = earlier.
pub fn cache_hint_sort_priority(hint: CacheHint) -> u8 {
    match hint {
        CacheHint::Always => 0,        // Pinned, CompactHistory - most stable
        CacheHint::SlidingPrefix => 1, // Partially stable
        CacheHint::UntilChanged => 2,  // Compacting - changes on compaction
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
pub const MAX_SYSTEM_CACHE_RUNS: usize = 3;

/// Split the volatile tier of the system prompt at its most recently changed
/// block, by retagging that block and every block after it as
/// [`CacheHint::RecentlyChanged`].
///
/// Providers place one cache breakpoint per run of same-hint blocks, so with a
/// single `UntilChanged` run the only breakpoint sits at the end of the tier.
/// A block mutating in the middle of that run therefore invalidates the whole
/// run, and every block after the mutation is re-sent as a cache write. Adding
/// a boundary just before the changed block gives the unchanged head of the
/// tier a cache entry of its own. Only the breakpoint metadata moves: block
/// order and block text are both left exactly as the sort left them, and this
/// runs after the sort so it cannot influence ordering at all.
///
/// `recency` carries the newest entry timestamp of the region behind each
/// `UntilChanged` block, in the order those blocks were assembled. The sort is
/// stable and every block in the tier shares one sort priority, so that order
/// is also the order the blocks appear in now.
///
/// Nothing is retagged when the newest block is already the first one (there is
/// no stable head to protect), or when the blocks already fill the run budget.
pub fn mark_recently_changed_run(blocks: &mut [SystemBlock], recency: &[i64]) {
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

/// Which message, if any, carries the request's one message cache breakpoint.
///
/// In an iterative inference loop only the last few messages change each
/// iteration (a new assistant turn and its tool results), so everything before
/// them is a stable prefix worth caching. Anthropic allows four breakpoints;
/// the system blocks take at most [`MAX_SYSTEM_CACHE_RUNS`] of them, so this
/// one always fits. It lands on the 4th-from-last message, leaving a buffer for
/// the two or three added each iteration, or on the first message of a short
/// conversation.
///
/// `None` when the system prefix moved since the last request: a provider
/// caches by prefix, so this breakpoint's entry covers every system block as
/// well as the messages before it, and a prefix that changed has already
/// invalidated it. The write premium would still be charged for an entry that
/// could never be read. Measured on a run whose bulk region grew every turn:
/// 3.3M cache-write tokens against 267k reads.
pub fn message_breakpoint(messages: usize, prefix_moved: bool) -> Option<usize> {
    match messages {
        _ if prefix_moved => None,
        n if n >= 5 => Some(n - 4),
        n if n >= 2 => Some(0),
        _ => None,
    }
}
