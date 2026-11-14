//! One prompt prefix per stage, so the conversation behind it caches.
//!
//! A provider that caches by prefix (Anthropic, and Anthropic through
//! OpenRouter) reads back only the bytes a request repeats from the start of
//! an earlier one, and the system prompt comes first. Tool routing writes a
//! stage's results into regions - `read_file` into `codebase`, the shell into
//! `test_results` - and each region renders as a system block, so before this
//! almost every call in a stage changed a system block. Everything after the
//! first changed block was then written to the cache again: the rest of the
//! system prompt and the whole conversation, on every call, at the write
//! premium, and never read back. Measured on a coder run through OpenRouter:
//! 15k tokens read and 20k to 30k written per call, with the reads never
//! growing as the conversation did. An offline replay of the same stage shape
//! put the first difference inside the system blocks on every call, with no
//! message ever ahead of it.
//!
//! The same happens one layer down. Every `sliding_window` region is sent as
//! turns, and the ones other than the conversation (a `decisions` log, a
//! `changelog`) sit ahead of the dialogue, so one `context_append` to them
//! inserted a turn in front of the whole history.
//!
//! So a stage keeps what its first request sent ahead of the dialogue - the
//! system blocks, and the turns of every other region sent as turns - and what
//! any region gains after that reaches the model after the turns that were
//! there when it was gained, and is never moved after: a block region's gain
//! as a user turn holding its continuation, a sliding window's notes as the
//! very turns they would have been in place. Each request then repeats the
//! previous one byte for byte and only appends, which is exactly what a prefix
//! cache can read back.
//!
//! The orphaned tool calls and results assembly strips are stripped before
//! any of this is counted or placed, so a turn added here never lands between
//! a call and the result that pairs with it a request later.
//!
//! Nothing the model saw before is lost or duplicated. A region's continuation
//! is exactly the entries it gained, so the request carries what the window
//! holds, the per-region budgets still bound it, and the pointer a routed
//! result leaves ("in your context under the 'codebase' heading") stays true:
//! the continuation is headed by the region's name.
//!
//! That only holds while every change to a region is an append, so anything
//! else shows the window afresh, as it would have been shown before: a re-read
//! that replaces an entry, a `context_write` over a region, a compaction of a
//! region, a cleared, evicted or hidden region, a region whose rendering is not
//! its entries in order (a checklist, a script's), a stored part, a turn of the
//! dialogue written into another region, and a block of no region that moved.
//! So does a new stage, and the first call of every stage, where the tools and
//! hints in front of the regions change anyway. Showing afresh costs one cache
//! write of what follows the first changed block.
//!
//! A conversation that lost or rewrote a turn (its oldest turns evicted, the
//! dialogue compacted, a turn rewritten), or a head that changed, does not
//! show the window afresh. The regions the system prompt renders did not
//! change in any way a turn cannot say; only the dialogue behind it did. So
//! the frame keeps its system blocks, and a provider reads the whole system
//! prompt back. What it cannot keep is the messages: none is promised to
//! repeat, so the request marks only its last message and the dialogue is
//! written to the cache once, to be read from the next request on. The updates
//! gathered so far are still owed to the model, since their regions' gains are
//! in no system block, and the messages they followed may be gone, so they
//! move to the front of the messages in the order they were gained; what the
//! regions gained since the last request follows the dialogue, as on any
//! request. Before this, one eviction in the middle of a stage wrote the system
//! prompt behind its stable start again, measured at about 40,000 tokens a
//! time, and sent the dialogue uncached.
//!
//! Volatility and cache hints keep their meaning: the blocks a stage holds
//! still are the ones assembly sorted and hinted when it showed them, so the
//! provider places its system markers the same way, and they are now read
//! back for the rest of the stage instead of the first change behind them
//! invalidating them.

use leviath_core::Region;
use leviath_core::context::{
    block_hash, continuation, conversation::conversation_messages, is_note, renders_as_turns,
    system_prefix_hash,
};
use leviath_core::tool_routing::CONVERSATION;
use leviath_providers::{Message, SystemBlock};

/// What a stage's requests have shown the model so far, so the next request
/// in the stage can send the same prefix and only add to the conversation.
///
/// Kept on the agent between requests and handed back to assembly through
/// [`crate::custom_region::AssembleMeta::frame`]. Not recorded with the run:
/// a resumed run shows its window afresh once and carries on from there.
#[derive(bevy_ecs::component::Component, Debug, Clone, Default)]
pub struct PromptFrame {
    /// The stage the frame belongs to.
    stage: String,
    /// The system blocks every request in the stage sends.
    blocks: Vec<SystemBlock>,
    /// Digest of the system blocks that belong to no region (a script's), as
    /// the stage's first request rendered them. A region's own blocks are
    /// accounted for in `shown`.
    loose: u64,
    /// Each region the frame shows, with what of it has been shown.
    shown: Vec<Shown>,
    /// How many entries of each region sent as turns, other than the
    /// conversation, the stage's first request sent in place. What they gain
    /// after that is sent as updates.
    in_place: Vec<(String, usize)>,
    /// Digest of each message the last request carried, before any update.
    messages: Vec<u64>,
    /// What regions gained during the stage, each placed after the messages
    /// that came before it.
    updates: Vec<Update>,
    /// How many messages the last request sent, updates included.
    sent: usize,
}

/// One region as a frame has shown it.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    /// The region's name.
    region: String,
    /// Digest of each entry shown, in order.
    entries: Vec<u64>,
    /// Digest of the system blocks the region rendered into.
    render: u64,
}

/// What regions gained before one request, said as turns.
#[derive(Debug, Clone)]
struct Update {
    /// How many of the conversation's messages come before it.
    after: usize,
    /// A user turn per block region's continuation, and the turns of each
    /// sliding window's notes, in region order.
    turns: Vec<Message>,
}

/// A request's system blocks and messages as the frame sends them.
pub(crate) struct Framed {
    /// The system blocks to send.
    pub system_blocks: Vec<SystemBlock>,
    /// Their prefix digest.
    pub system_hash: u64,
    /// The messages to send, with each update in its place.
    pub messages: Vec<Message>,
    /// How many leading messages are sent exactly as the previous request in
    /// the frame sent them.
    pub repeated: usize,
    /// The frame for the next request.
    pub frame: PromptFrame,
}

/// What assembly hands the frame: the stage, the regions in view, and what
/// they render into right now.
pub(crate) struct Live<'a> {
    /// The current stage.
    pub stage: &'a str,
    /// Requests already made in this stage. The first one shows afresh.
    pub iteration: usize,
    /// The regions the stage attends to, in window order.
    pub regions: Vec<&'a Region>,
    /// The system blocks they render into now.
    pub blocks: Vec<SystemBlock>,
    /// The messages that lead the conversation: stored parts lifted out of
    /// the system regions, and what a script rendered as messages.
    pub head: Vec<Message>,
}

/// Frame `live` after `previous`: the same prefix with what was gained since
/// sent as turns, or `live` itself as the start of a new frame when anything
/// moved in a way a turn cannot say.
pub(crate) fn frame(previous: Option<&PromptFrame>, live: Live<'_>) -> Framed {
    // Every region but the conversation: the conversation is the history this
    // all exists to keep appending to, and is sent whole.
    let shown: Vec<Shown> = live
        .regions
        .iter()
        .filter(|r| r.name != CONVERSATION)
        .map(|r| Shown {
            region: r.name.clone(),
            entries: r
                .content
                .iter()
                .map(|e| block_hash(&format!("{:?}\u{0}{:?}\u{0}{}", e.key, e.kind, e.content)))
                .collect(),
            render: blocks_digest(live.blocks.iter().filter(|b| b.region == r.name)),
        })
        .collect();
    let loose = blocks_digest(live.blocks.iter().filter(|b| b.region.is_empty()));
    let carried = previous
        .filter(|p| p.stage == live.stage && live.iteration > 0 && p.loose == loose)
        .and_then(|p| {
            let turns = gained(p, &shown, &live.regions)?;
            let messages = base(&live, Some(&p.in_place));
            let digests = digests(&messages);
            Some((p, turns, messages, digests))
        });
    let (mut frame, repeated, messages, digests) = match carried {
        Some((previous, turns, messages, digests)) => {
            let mut frame = previous.clone();
            // The dialogue lost or rewrote a turn (an eviction, a compaction,
            // a changed head): the system prompt still holds, but no message
            // is promised to repeat. The updates owed so far go to the front,
            // in the order they were gained, since the messages they followed
            // may be gone.
            let held = digests.starts_with(&previous.messages);
            if !held {
                for update in &mut frame.updates {
                    update.after = 0;
                }
            }
            if !turns.is_empty() {
                frame.updates.push(Update {
                    after: messages.len(),
                    turns,
                });
            }
            let repeated = match held {
                true => previous.sent,
                false => 0,
            };
            (frame, repeated, messages, digests)
        }
        None => {
            let messages = base(&live, None);
            let digests = digests(&messages);
            let in_place = live
                .regions
                .iter()
                .filter(|r| sent_as_turns(r) && r.name != CONVERSATION)
                .map(|r| (r.name.clone(), r.content.len()))
                .collect();
            let frame = PromptFrame {
                stage: live.stage.to_string(),
                blocks: live.blocks,
                loose,
                in_place,
                ..PromptFrame::default()
            };
            (frame, 0, messages, digests)
        }
    };
    frame.shown = shown;
    frame.messages = digests;
    let messages = place(messages, &frame.updates);
    frame.sent = messages.len();
    Framed {
        system_hash: system_prefix_hash(&frame.blocks),
        system_blocks: frame.blocks.clone(),
        messages,
        repeated,
        frame,
    }
}

/// The messages of `live` before any update: its head, then each region sent
/// as turns in window order, with orphaned tool blocks stripped. With
/// `in_place`, a region other than the conversation sends only the entries the
/// frame sent in place (none, for a region it did not), since what it gained
/// after is an update.
fn base(live: &Live<'_>, in_place: Option<&[(String, usize)]>) -> Vec<Message> {
    let mut messages = live.head.clone();
    for region in live.regions.iter().filter(|r| sent_as_turns(r)) {
        let kept = match in_place.filter(|_| region.name != CONVERSATION) {
            None => region.content.len(),
            Some(counts) => counts
                .iter()
                .find(|(name, _)| *name == region.name)
                .map_or(0, |(_, n)| *n),
        };
        match kept < region.content.len() {
            true => {
                let mut head = (*region).clone();
                head.content.truncate(kept);
                messages.extend(conversation_messages(&head).into_iter().map(Message::from));
            }
            false => messages.extend(conversation_messages(region).into_iter().map(Message::from)),
        }
    }
    super::strip_orphaned_tool_blocks(messages)
}

/// Whether `region`'s entries are sent as turns here: a region that
/// [renders as turns](renders_as_turns), unless a script renders it (its
/// messages arrive in the head).
fn sent_as_turns(region: &Region) -> bool {
    renders_as_turns(region) && !matches!(region.kind, leviath_core::RegionKind::Custom { .. })
}

/// Digest of each message.
fn digests(messages: &[Message]) -> Vec<u64> {
    messages
        .iter()
        .map(|m| block_hash(&serde_json::to_string(m).unwrap_or_default()))
        .collect()
}

/// Digest of some system blocks' text, in order.
fn blocks_digest<'a>(blocks: impl Iterator<Item = &'a SystemBlock>) -> u64 {
    block_hash(
        &blocks
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\0"),
    )
}

/// The turns that say what each region gained since `previous` showed it, in
/// region order, or `None` when some region changed in a way that is not an
/// append a turn can say.
fn gained(previous: &PromptFrame, now: &[Shown], regions: &[&Region]) -> Option<Vec<Message>> {
    // A region the frame showed and the window no longer does (cleared,
    // hidden, dropped) is a change no turn can take back.
    if previous
        .shown
        .iter()
        .any(|was| !was.entries.is_empty() && !now.iter().any(|n| n.region == was.region))
    {
        return None;
    }
    let mut turns = Vec::new();
    for (region, now) in regions.iter().filter(|r| r.name != CONVERSATION).zip(now) {
        let was = previous.shown.iter().find(|s| s.region == now.region);
        let shown = was.map_or(&[][..], |s| s.entries.as_slice());
        if now.entries == shown {
            // Same entries rendered differently: a script's region, or a
            // checklist whose items changed state.
            match was.is_none_or(|s| s.render == now.render) {
                true => continue,
                false => return None,
            }
        }
        if !now.entries.starts_with(shown) {
            return None;
        }
        turns.extend(said(region, shown.len())?);
    }
    Some(turns)
}

/// The turns that say what `region` gained after its first `shown` entries.
///
/// A sliding window's notes are sent as the turns they would have been in
/// place, so the model reads them the same way whenever they arrived; a turn of
/// the dialogue written into one cannot be moved and has none. Any other region
/// sends one user turn holding its continuation.
fn said(region: &Region, shown: usize) -> Option<Vec<Message>> {
    let gained = &region.content[shown..];
    match sent_as_turns(region) {
        true => gained.iter().all(is_note).then(|| {
            let mut notes = region.clone();
            notes.content = gained.to_vec();
            conversation_messages(&notes)
                .into_iter()
                .map(Message::from)
                .collect()
        }),
        false => continuation(region, shown).map(|text| {
            vec![Message {
                role: "user".to_string(),
                content: text.into(),
                cache_breakpoint: false,
                reasoning: None,
            }]
        }),
    }
}

/// `messages` with each update sent as a user turn of its own after the
/// messages that came before it.
///
/// A turn of its own rather than text added to the turn before it: that turn
/// is usually a tool result, and the chat-completions shape a tool result
/// becomes has nowhere to carry text beside it, so the text would be dropped
/// on the way to the model. Consecutive user turns are what a delivered
/// message after a tool result already produces, and each provider joins them
/// where its API requires it.
fn place(messages: Vec<Message>, updates: &[Update]) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len() + updates.len());
    let mut pending = updates.iter().peekable();
    let total = messages.len();
    let mut messages = messages.into_iter();
    for index in 0..=total {
        while let Some(update) = pending.next_if(|u| u.after <= index) {
            out.extend(update.turns.iter().cloned());
        }
        out.extend(messages.next());
    }
    out
}

#[cfg(test)]
#[path = "frame_tests.rs"]
mod tests;
