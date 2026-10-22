//! A `sliding_window` region as a conversation, before any provider shape.
//!
//! The conversation is the one region that does not render into the system
//! prompt. Its entries become turns: user messages, assistant turns with the
//! tool calls they made, and the tool results that answer them. Getting that
//! right is fiddly in ways every provider cares about. Consecutive tool
//! results share one user turn, a stored part an assistant produced follows
//! it in a user turn of its own, and an old `Assistant: ` prefix still reads
//! as an assistant turn.
//!
//! [`conversation_messages`] does all of it and returns
//! [`ConversationMessage`]s, which name no provider. `leviath-providers`
//! converts them into its request messages, and a program that calls a model
//! API itself converts them into whatever that API takes.
//!
//! ```
//! use leviath_core::{EntryKind, EvictionStrategy, Region, RegionKind};
//! use leviath_core::context::conversation::{conversation_messages, ConversationContent, Role};
//!
//! let mut history = Region::new(
//!     "conversation".into(),
//!     RegionKind::SlidingWindow { max_items: 50, eviction_strategy: EvictionStrategy::PerItem },
//!     10_000,
//! );
//! history.add_typed_entry("What is 2 + 2?", 5, EntryKind::UserMessage).unwrap();
//! history.add_typed_entry("4", 1, EntryKind::AssistantTurn { tool_calls: vec![] }).unwrap();
//!
//! let messages = conversation_messages(&history);
//! assert_eq!(messages[1].role, Role::Assistant);
//! assert_eq!(messages[1].content, ConversationContent::Text("4".into()));
//! ```

use crate::mime::Part;
use crate::region::{EntryContent, EntryKind, Region, SerializedToolCall};

use super::{PartBlock, content_blocks};

/// Who a turn is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The person, a tool's result, or the runtime speaking for either.
    User,
    /// The model.
    Assistant,
}

impl Role {
    /// The role as chat APIs spell it: `user` or `assistant`.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One piece of a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum ConversationBlock<'a> {
    /// Text, sent as it is.
    Text(String),
    /// A stored part, still a reference: [`crate::mime::hydrate`] decides
    /// whether it is sent as bytes, as text or as its stand-in. Always
    /// preceded by a text piece naming it.
    Stored(&'a Part),
    /// A call the model made, with its arguments exactly as stored. A call
    /// cut off mid-arguments is stored as their partial text, which most APIs
    /// refuse as tool input; wrap a non-object before sending it.
    ToolUse(&'a SerializedToolCall),
    /// A tool's result, answering the call with `tool_use_id`.
    ToolResult {
        /// The id of the call it answers.
        tool_use_id: String,
        /// The result as text.
        content: String,
        /// Whether the tool failed.
        is_error: bool,
    },
}

/// What a turn holds.
#[derive(Debug, Clone, PartialEq)]
pub enum ConversationContent<'a> {
    /// Plain text: a turn whose every part is text, the shape every provider
    /// has always been sent.
    Text(String),
    /// Pieces, for a turn with tool calls, tool results or stored parts.
    Blocks(Vec<ConversationBlock<'a>>),
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationMessage<'a> {
    /// Who it is from.
    pub role: Role,
    /// What it holds.
    pub content: ConversationContent<'a>,
    /// The opaque provider token the turn has to be replayed with; see
    /// [`crate::RegionEntry::reasoning`]. Only on assistant turns.
    pub reasoning: Option<String>,
}

impl<'a> ConversationMessage<'a> {
    /// A turn with no reasoning token.
    fn new(role: Role, content: ConversationContent<'a>) -> Self {
        Self {
            role,
            content,
            reasoning: None,
        }
    }
}

/// Every piece of `content` in order: text as itself, a stored part as its
/// stand-in and then the part.
fn blocks(content: &EntryContent) -> Vec<ConversationBlock<'_>> {
    content_blocks(content)
        .into_iter()
        .map(|piece| match piece {
            PartBlock::Text(text) => ConversationBlock::Text(text),
            PartBlock::Stored(part) => ConversationBlock::Stored(part),
        })
        .collect()
}

/// Only the stored parts of `content`, for a turn whose text is already
/// placed.
fn stored_blocks(content: &EntryContent) -> Vec<ConversationBlock<'_>> {
    content.stored().map(ConversationBlock::Stored).collect()
}

/// Plain text when every part of `content` is text, pieces otherwise.
fn message_content(content: &EntryContent) -> ConversationContent<'_> {
    match content.is_text_only() {
        true => ConversationContent::Text(content.to_string()),
        false => ConversationContent::Blocks(blocks(content)),
    }
}

/// `region`'s entries as conversation turns, in order.
///
/// Meant for a `sliding_window` region but reads any. Consecutive tool
/// results are merged into one user turn, after which the stored parts they
/// carried follow. A text entry starting `Assistant: ` or `User: ` becomes a
/// turn from that role with the prefix removed; any other text entry is a
/// user turn. Nothing is dropped or added here: an empty turn, a missing
/// opening user turn and a conversation ending on the assistant are the
/// caller's to handle, since what to send instead is a policy.
pub fn conversation_messages(region: &Region) -> Vec<ConversationMessage<'_>> {
    let mut messages = Vec::new();
    let mut pending_results: Vec<ConversationBlock<'_>> = Vec::new();

    for entry in &region.content {
        if !matches!(entry.kind, EntryKind::ToolResult { .. }) && !pending_results.is_empty() {
            messages.push(ConversationMessage::new(
                Role::User,
                ConversationContent::Blocks(std::mem::take(&mut pending_results)),
            ));
        }
        match &entry.kind {
            EntryKind::UserMessage => {
                messages.push(ConversationMessage::new(
                    Role::User,
                    message_content(&entry.content),
                ));
            }
            EntryKind::AssistantTurn { tool_calls } => {
                // A provider rejects media inside an assistant turn, so the
                // turn keeps its text and the stand-ins naming the media, and
                // the parts follow in a user turn.
                let lifted = stored_blocks(&entry.content);
                let content = match tool_calls.is_empty() {
                    true => ConversationContent::Text(entry.content.to_string()),
                    false => {
                        let mut pieces: Vec<_> = blocks(&entry.content)
                            .into_iter()
                            .filter(|b| !matches!(b, ConversationBlock::Stored(_)))
                            .collect();
                        pieces.extend(tool_calls.iter().map(ConversationBlock::ToolUse));
                        ConversationContent::Blocks(pieces)
                    }
                };
                messages.push(ConversationMessage {
                    role: Role::Assistant,
                    content,
                    reasoning: entry.reasoning.clone(),
                });
                // Only when the turn made no calls: a user turn between a
                // tool_use and its tool_result breaks the pairing, so that
                // rare turn's media stays a stand-in.
                if tool_calls.is_empty() && !lifted.is_empty() {
                    messages.push(ConversationMessage::new(
                        Role::User,
                        ConversationContent::Blocks(lifted),
                    ));
                }
            }
            EntryKind::ToolResult {
                tool_call_id,
                is_error,
                ..
            } => {
                pending_results.push(ConversationBlock::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: entry.content.to_string(),
                    is_error: *is_error,
                });
                pending_results.extend(stored_blocks(&entry.content));
            }
            EntryKind::Text => {
                let trimmed = entry.content.trim();
                let message = if let Some(rest) = trimmed.strip_prefix("Assistant: ") {
                    ConversationMessage::new(
                        Role::Assistant,
                        ConversationContent::Text(rest.to_string()),
                    )
                } else if let Some(rest) = trimmed.strip_prefix("User: ") {
                    ConversationMessage::new(
                        Role::User,
                        ConversationContent::Text(rest.to_string()),
                    )
                } else {
                    ConversationMessage::new(Role::User, message_content(&entry.content))
                };
                messages.push(message);
            }
        }
    }
    if !pending_results.is_empty() {
        messages.push(ConversationMessage::new(
            Role::User,
            ConversationContent::Blocks(pending_results),
        ));
    }
    messages
}

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod tests;
