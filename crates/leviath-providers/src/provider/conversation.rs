//! Request messages from `leviath-core`'s provider-neutral conversation.
//!
//! `leviath_core::context::conversation` lays a `sliding_window` region out
//! as turns that name no provider. This is the one place those turns become
//! this crate's [`Message`]s, so every lane that builds a request from a
//! region sends the same bytes.

use leviath_core::context::conversation::{
    ConversationBlock, ConversationContent, ConversationMessage,
};

use super::{ContentBlock, Message, MessageContent, tool_input_object};

impl ContentBlock {
    /// The block for one piece of a conversation turn.
    ///
    /// A stored part becomes an unhydrated mime block (`None` for a part
    /// that is not stored, which has nothing to reference). A tool call's
    /// arguments go through [`tool_input_object`], since a call cut off
    /// mid-arguments is stored as text and providers want an object.
    pub fn from_conversation(block: ConversationBlock<'_>) -> Option<Self> {
        match block {
            ConversationBlock::Text(text) => Some(ContentBlock::Text { text }),
            ConversationBlock::Stored(part) => ContentBlock::mime(part),
            ConversationBlock::ToolUse(call) => Some(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: tool_input_object(&call.arguments),
                thought_signature: call.thought_signature.clone(),
            }),
            ConversationBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }),
        }
    }
}

impl From<ConversationContent<'_>> for MessageContent {
    fn from(content: ConversationContent<'_>) -> Self {
        match content {
            ConversationContent::Text(text) => MessageContent::Text(text),
            ConversationContent::Blocks(blocks) => MessageContent::Blocks(
                blocks
                    .into_iter()
                    .filter_map(ContentBlock::from_conversation)
                    .collect(),
            ),
        }
    }
}

impl From<ConversationMessage<'_>> for Message {
    /// The turn as a request message, with no cache breakpoint: placing one
    /// is a decision about the whole request.
    fn from(message: ConversationMessage<'_>) -> Self {
        Message {
            role: message.role.as_str().to_string(),
            content: message.content.into(),
            cache_breakpoint: false,
            reasoning: message.reasoning,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::context::conversation::Role;
    use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
    use leviath_core::region::SerializedToolCall;

    #[test]
    fn every_piece_converts_to_its_block() {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1]).named("a.png");
        let stored = Part::stored(blob.describe(&reg)).named("a.png");
        let inline = Part::text("not stored");
        let call = SerializedToolCall {
            id: "c1".into(),
            name: "grep".into(),
            arguments: serde_json::json!("cut off"),
            thought_signature: Some("sig".into()),
        };
        let message: Message = ConversationMessage {
            role: Role::Assistant,
            content: ConversationContent::Blocks(vec![
                ConversationBlock::Text("hi".into()),
                ConversationBlock::Stored(&stored),
                ConversationBlock::Stored(&inline),
                ConversationBlock::ToolUse(&call),
                ConversationBlock::ToolResult {
                    tool_use_id: "c0".into(),
                    content: "ok".into(),
                    is_error: true,
                },
            ]),
            reasoning: Some("sealed".into()),
        }
        .into();
        assert_eq!(message.role, "assistant");
        assert_eq!(message.reasoning.as_deref(), Some("sealed"));
        assert!(!message.cache_breakpoint);
        assert_eq!(
            message.content,
            MessageContent::Blocks(vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::mime(&stored).unwrap(),
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "grep".into(),
                    input: serde_json::json!({ "_raw": "cut off" }),
                    thought_signature: Some("sig".into()),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "c0".into(),
                    content: "ok".into(),
                    is_error: true,
                },
            ])
        );
        let text: Message = ConversationMessage {
            role: Role::User,
            content: ConversationContent::Text("yo".into()),
            reasoning: None,
        }
        .into();
        assert_eq!(text.role, "user");
        assert_eq!(text.content, MessageContent::Text("yo".into()));
    }
}
