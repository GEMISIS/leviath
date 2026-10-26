//! A context window's regions as the system prompt and the parts beside it.
//!
//! The provider-neutral half of assembly: which regions render, in what
//! order, under what label, split at which boundaries, and with which cache
//! hints. It reads [`Region`]s and writes [`SystemBlock`]s and [`PartBlock`]s,
//! so a program that calls a model API itself can lay out its context the way
//! a Leviath run does without taking the runtime or the providers with it.
//!
//! The conversation is laid out here too, as provider-neutral turns: a
//! `sliding_window` region, and the `conversation` region whatever its kind
//! ([`renders_as_turns`]), becomes [`conversation::ConversationMessage`]s,
//! with tool results paired and merged the way providers require.
//! [`assemble_context`] returns both halves; [`assemble_system`] hands the
//! conversation back to its caller instead, for one that renders it itself.
//!
//! What stays out, and where it lives:
//!
//! - **Request policy.** The opening user turn a request with none gets, the
//!   `Continue.` after a closing assistant turn, stripping tool calls left
//!   without a result, and the message cache breakpoint are the runtime's,
//!   as is turning a [`conversation::ConversationMessage`] into a vendor's
//!   message (`leviath-providers` converts from it).
//! - **Custom regions.** A `custom` region renders through its Rhai script,
//!   which `leviath-runtime` compiles and runs; it is handed back the same
//!   way.
//! - **Logging.** The warning about a region that declared itself `stable`
//!   and kept changing is the runtime's to say; this crate has no logger.
//!
//! Stored parts never reach a system block, which is text. Each one is lifted
//! into a [`PartBlock`] list that the caller sends as one user message ahead
//! of the conversation, with a pointer naming the region so the model can tie
//! the bytes to the stand-in it reads in the system prompt. What a part then
//! becomes on the wire is [`crate::mime::hydrate`]'s decision.

use serde::{Deserialize, Serialize};

use crate::mime::{Part, PartBody};
use crate::region::EntryContent;
use crate::{CacheHint, Region, RegionKind, Volatility};

pub mod cache;
pub mod conversation;

pub use cache::{
    CACHE_CHUNK_TOKENS, MAX_SYSTEM_CACHE_RUNS, append_only_chunks, block_hash, block_sort_priority,
    cache_hint_sort_priority, lifecycle_cache_hint, mark_recently_changed_run, message_breakpoint,
    push_bracketed, push_chunked, system_prefix_hash,
};

/// A system prompt block, separated from conversation messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    /// The text content of this system block.
    pub text: String,
    /// Cache hint for this system block.
    pub cache_hint: CacheHint,
    /// The region this block was rendered from, for diagnostics.
    ///
    /// Empty for a block that is not a region - a hint, a tool preamble - which
    /// is exactly the set of blocks no volatility warning could be about.
    ///
    /// Defaulted on the wire so a request serialized before these fields
    /// existed still deserializes - a script provider that round-trips one, or
    /// a dumped body replayed later, must not fail on a field it predates.
    #[serde(default)]
    pub region: String,
    /// How much the region this block came from moves between requests.
    ///
    /// Carried so assembly can order blocks by it: a provider caches by prefix,
    /// so a block that moves invalidates everything behind it, and the
    /// arrangement that pays is stable content first and churn last. The
    /// region's *kind* cannot answer this - a pinned region is written
    /// constantly - so the blueprint says and this carries the answer.
    #[serde(default)]
    pub volatility: Volatility,
}

impl SystemBlock {
    /// Whether this is a framework hint: no region, and cached always.
    ///
    /// A hint is the same on every request, so it belongs at the head of a
    /// prompt a provider caches by prefix. A block with no region that is not
    /// cached always (the note about tools the history calls and the stage
    /// does not offer) moves with the stage, and is not one.
    pub fn is_hint(&self) -> bool {
        self.region.is_empty() && self.cache_hint == CacheHint::Always
    }
}

/// One piece of a message as assembly lays it out, before any provider shape.
///
/// A stored part is always preceded by a `Text` piece carrying its stand-in or
/// a pointer naming it, so a request that drops the part still says it was
/// there.
#[derive(Debug, Clone, PartialEq)]
pub enum PartBlock<'a> {
    /// Text, sent as it is.
    Text(String),
    /// A stored part, sent as its bytes, as text or as its stand-in once
    /// [`crate::mime::hydrate`] has decided which.
    Stored(&'a Part),
}

/// Put a region's own name above its contents, and its description under that
/// when it has one.
///
/// The name is the part that earns its tokens. An agent writes to a region *by
/// name* - `context_write { region: "sources_index", .. }` - so the contents it
/// reads must say which region they are, or it cannot tell that what it reads
/// as `sources_index` is the place it writes to by that name. Three tokens of
/// heading say it.
///
/// The description is opt-in and usually absent, because most regions are named
/// well enough that a sentence would only cost tokens. It is for the ones whose
/// contents do not explain themselves - a bibliography with a required format,
/// a scratch area with a convention.
pub fn labelled(region: &Region, body: &str) -> String {
    match region
        .description
        .as_deref()
        .filter(|_| region.describe_in_prompt)
    {
        Some(description) => format!("## {}\n{}\n\n{}", region.name, description, body),
        None => format!("## {}\n{}", region.name, body),
    }
}

/// Open a region rendered as `[name]:` with its heading, and with its
/// description on the next line when it shows one to the model.
///
/// The bracketed form of [`labelled`], for the kinds that render that way
/// (`keyed`, `temporary`, `clearable`, `compacting`). The region's body
/// follows it. Without a shown description the heading is exactly
/// `[name]:` and a newline.
///
/// ```
/// use leviath_core::{Region, RegionKind};
/// use leviath_core::context::bracketed_heading;
///
/// let mut tasks = Region::new("tasks".into(), RegionKind::HashMap { max_entries: None }, 100);
/// assert_eq!(bracketed_heading(&tasks), "[tasks]:\n");
/// tasks.description = Some("One line per open task.".into());
/// tasks.describe_in_prompt = true;
/// assert_eq!(bracketed_heading(&tasks), "[tasks]:\nOne line per open task.\n\n");
/// ```
pub fn bracketed_heading(region: &Region) -> String {
    match region
        .description
        .as_deref()
        .filter(|_| region.describe_in_prompt)
    {
        Some(description) => format!("[{}]:\n{}\n\n", region.name, description),
        None => format!("[{}]:\n", region.name),
    }
}

/// Every part of `content` in order: inline text as itself (an empty one
/// left out), a stored part as its stand-in followed by the part.
pub fn content_blocks(content: &EntryContent) -> Vec<PartBlock<'_>> {
    let mut blocks = Vec::new();
    for part in content.parts() {
        match &part.body {
            PartBody::Inline(text) => {
                if !text.is_empty() {
                    blocks.push(PartBlock::Text(text.clone()));
                }
            }
            PartBody::Stored(blob) => {
                blocks.push(PartBlock::Text(blob.stand_in.clone()));
                blocks.push(PartBlock::Stored(part));
            }
        }
    }
    blocks
}

/// The stored parts of a region that renders into the system prompt, each
/// after a pointer naming the region, the key and the part.
///
/// A system block is text, so a region's stored parts cannot travel inside
/// it. They are lifted into a user message placed before the conversation,
/// and the pointer is what lets the model tie the bytes to the stand-in it
/// reads in the system prompt.
pub fn lifted_blocks(region: &Region) -> Vec<PartBlock<'_>> {
    let mut blocks = Vec::new();
    for entry in &region.content {
        for part in entry.content.parts() {
            let PartBody::Stored(blob) = &part.body else {
                continue;
            };
            let pointer = match &entry.key {
                Some(key) => format!("[{} / {key}] {}", region.name, blob.stand_in),
                None => format!("[{}] {}", region.name, blob.stand_in),
            };
            blocks.push(PartBlock::Text(pointer));
            blocks.push(PartBlock::Stored(part));
        }
    }
    blocks
}

/// Render `region` as system blocks, for every kind that renders into the
/// system prompt without a script. A `sliding_window` or `custom` region adds
/// nothing: see [`assemble_system`].
pub fn push_region_blocks(blocks: &mut Vec<SystemBlock>, region: &Region) {
    let entries = || -> Vec<String> {
        region
            .content
            .iter()
            .map(|e| e.content.to_string())
            .collect()
    };
    match &region.kind {
        RegionKind::Pinned | RegionKind::CompactHistory { .. } => {
            push_chunked(blocks, region, &entries(), CacheHint::Always);
        }
        // A checklist renders as instruction rather than history: one
        // stable block, open items first, so what is left to do is at
        // the top of what the model reads every turn. The whole value
        // of the state being real is that this block is derived from
        // it rather than from whatever prose the model last wrote.
        RegionKind::Checklist => {
            let body = region.render_checklist();
            if !body.is_empty() {
                blocks.push(SystemBlock {
                    text: labelled(region, &body),
                    cache_hint: CacheHint::UntilChanged,
                    volatility: region.volatility,
                    region: region.name.clone(),
                });
            }
        }
        RegionKind::Compacting { .. } => {
            push_bracketed(blocks, region, CacheHint::UntilChanged);
        }
        // Both say when the region is thrown away, not how it moves
        // in between, so the hint comes from the declaration - see
        // `lifecycle_cache_hint`.
        RegionKind::Temporary | RegionKind::Clearable => {
            push_bracketed(blocks, region, lifecycle_cache_hint(region.volatility));
        }
        // Key headers, so an entry reads as the slot it was written to.
        RegionKind::HashMap { .. } => {
            let text = region
                .content
                .iter()
                .map(|e| match &e.key {
                    Some(key) => format!("### [{}]\n{}", key, e.content),
                    None => e.content.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            blocks.push(SystemBlock {
                text: format!("{}{}", bracketed_heading(region), text),
                cache_hint: CacheHint::UntilChanged,
                volatility: region.volatility,
                region: region.name.clone(),
            });
        }
        RegionKind::SlidingWindow { .. } | RegionKind::Custom { .. } => {}
    }
}

/// The system side of an assembled request.
#[derive(Debug, Clone)]
pub struct SystemAssembly<'a> {
    /// The system blocks, most stable first, cache hints final.
    pub blocks: Vec<SystemBlock>,
    /// The stored parts of every region that rendered into the system
    /// prompt, for one user message ahead of the conversation. Empty when
    /// there are none, and then no such message is sent.
    pub lifted: Vec<PartBlock<'a>>,
    /// [`block_hash`] of each block, in final order: what the next assembly
    /// compares against to tell which blocks held still.
    pub block_hashes: Vec<u64>,
    /// [`system_prefix_hash`] of the blocks: when it differs from the
    /// previous request's, the prefix moved and [`message_breakpoint`] places
    /// nothing.
    pub system_hash: u64,
}

/// Lay out `regions` as a system prompt.
///
/// Regions render in the order given, with an empty one skipped, then the
/// blocks are sorted most-stable first (a stable sort, so equal blocks keep
/// region order) and the volatile tier is split at its newest block. A region
/// this module cannot render itself - `custom`, and one that
/// [renders as turns](renders_as_turns) - is passed to `render` in its place, with the block list, so a caller's own blocks
/// sort and hash with the rest. A `custom` region reaches `render` even when
/// empty, since a script may emit static scaffolding.
///
/// Leave a region out of `regions` to hold it without showing it.
pub fn assemble_system<'a>(
    regions: impl IntoIterator<Item = &'a Region>,
    mut render: impl FnMut(&'a Region, &mut Vec<SystemBlock>),
) -> SystemAssembly<'a> {
    assemble_system_dyn(&mut regions.into_iter(), &mut render)
}

/// A request's whole context: the system side and the conversation.
#[derive(Debug, Clone)]
pub struct ContextAssembly<'a> {
    /// The system blocks and the parts lifted out of them, as
    /// [`assemble_system`] lays them out.
    pub system: SystemAssembly<'a>,
    /// The turns of every region that [renders as turns](renders_as_turns),
    /// in region order. See
    /// [`conversation::conversation_messages`] for what is and is not done
    /// to them.
    pub conversation: Vec<conversation::ConversationMessage<'a>>,
}

/// A `custom` region handed to [`assemble_context`]'s caller to render.
#[derive(Debug, Clone, Copy)]
pub struct CustomRegion<'a> {
    /// The region.
    pub region: &'a Region,
    /// Its `RegionKind::Custom` script path.
    pub script: &'a str,
    /// Its `RegionKind::Custom` lifecycle flag.
    pub pinned: bool,
}

/// Lay out `regions` as a system prompt and a conversation.
///
/// [`assemble_system`], with each region that
/// [renders as turns](renders_as_turns) rendered as conversation turns
/// rather than handed back. Only a `custom` region
/// reaches `render`, which pushes whatever blocks its script renders. The
/// stored parts in [`SystemAssembly::lifted`] go in one user turn ahead of
/// the conversation.
///
/// ```
/// use leviath_core::{EntryKind, EvictionStrategy, Region, RegionKind};
/// use leviath_core::context::assemble_context;
///
/// let mut task = Region::new("task".into(), RegionKind::Pinned, 1_000);
/// task.add_entry("Summarize the report.", 5).unwrap();
/// let mut history = Region::new(
///     "conversation".into(),
///     RegionKind::SlidingWindow { max_items: 50, eviction_strategy: EvictionStrategy::PerItem },
///     10_000,
/// );
/// history.add_typed_entry("Here it is.", 4, EntryKind::UserMessage).unwrap();
///
/// let assembled = assemble_context([&task, &history], |_custom, _blocks| {});
/// assert_eq!(assembled.system.blocks[0].text, "## task\nSummarize the report.");
/// assert_eq!(assembled.conversation.len(), 1);
/// ```
pub fn assemble_context<'a>(
    regions: impl IntoIterator<Item = &'a Region>,
    mut render: impl FnMut(CustomRegion<'a>, &mut Vec<SystemBlock>),
) -> ContextAssembly<'a> {
    let mut turns = Vec::new();
    // Only what `assemble_system` cannot render comes back here: a custom
    // region, and otherwise one that renders as turns.
    let system = assemble_system_dyn(&mut regions.into_iter(), &mut |region, blocks| {
        if let RegionKind::Custom { script, pinned } = &region.kind {
            let custom = CustomRegion {
                region,
                script,
                pinned: *pinned,
            };
            render(custom, blocks);
        } else {
            turns.extend(conversation::conversation_messages(region));
        }
    });
    ContextAssembly {
        system,
        conversation: turns,
    }
}

/// Whether `region` is sent as conversation turns rather than as a system
/// block: every `sliding_window` region, and the region named
/// [`crate::tool_routing::CONVERSATION`] whatever its kind.
///
/// The conversation is where a run's turns land: the model's replies with
/// the calls they made, and each call's result or the pointer saying where it
/// went. Rendered as a system block, an assistant turn is only its text,
/// which a turn that just called a tool does not have, so the model would
/// read a list of results with no calls behind them, and no turn of its own
/// at all. It then starts over every request, making its first call again. So
/// a conversation declared `compacting`, `clearable` or `temporary` keeps
/// what its kind does to its entries, and is still sent as the dialogue.
pub fn renders_as_turns(region: &Region) -> bool {
    matches!(region.kind, RegionKind::SlidingWindow { .. })
        || region.name == crate::tool_routing::CONVERSATION
}

/// [`assemble_system`] compiled once rather than per caller: one body, so a
/// caller whose `render` never runs leaves no copy of it behind uncalled.
fn assemble_system_dyn<'a>(
    regions: &mut dyn Iterator<Item = &'a Region>,
    render: &mut dyn FnMut(&'a Region, &mut Vec<SystemBlock>),
) -> SystemAssembly<'a> {
    let mut blocks = Vec::new();
    let mut lifted = Vec::new();
    // One entry per `UntilChanged` system block, in assembly order, holding
    // the newest entry timestamp of the region that produced it. Feeds the
    // cache-breakpoint split after the sort.
    let mut volatile_recency: Vec<i64> = Vec::new();
    for region in regions {
        let is_custom = matches!(region.kind, RegionKind::Custom { .. });
        if region.content.is_empty() && !is_custom {
            continue;
        }
        let is_conversation = !is_custom && renders_as_turns(region);
        if !is_conversation {
            lifted.extend(lifted_blocks(region));
        }
        // Where this region's blocks begin, so the recency mapping below
        // covers exactly the blocks it adds.
        let first_new_block = blocks.len();
        match is_custom || is_conversation {
            true => render(region, &mut blocks),
            false => push_region_blocks(&mut blocks, region),
        }
        // The newest entry timestamp stands in for "when did this region
        // last change". Regions are append-mostly and timestamps only move
        // forward, so the block holding the newest entry is the one that
        // mutated most recently.
        let newest = region
            .content
            .iter()
            .map(|e| e.timestamp)
            .max()
            .unwrap_or(i64::MIN);
        for block in &blocks[first_new_block..] {
            if block.cache_hint == CacheHint::UntilChanged {
                volatile_recency.push(newest);
            }
        }
    }
    // A provider caches by prefix, so a block that changes invalidates every
    // block behind it: the arrangement that pays is stable content first and
    // churn last. That is what the region declared, not what its kind implies
    // - see [`Volatility`].
    blocks.sort_by_key(block_sort_priority);
    // After the sort, because the order is part of what a provider matches.
    let system_hash = system_prefix_hash(&blocks);
    let block_hashes = blocks.iter().map(|b| block_hash(&b.text)).collect();
    // Strictly after the sort, so it can only change breakpoint metadata: the
    // block order and the block text are already final.
    mark_recently_changed_run(&mut blocks, &volatile_recency);
    SystemAssembly {
        blocks,
        lifted,
        block_hashes,
        system_hash,
    }
}

#[cfg(test)]
mod tests;
