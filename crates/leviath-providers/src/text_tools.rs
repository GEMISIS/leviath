//! Rewriting tool turns for a model that cannot call tools.
//!
//! A conversation shared across a run keeps every `tool_use` and
//! `tool_result` block a stage produced. A later stage may run on a model
//! whose provider refuses any request that carries a function call, so the
//! blocks are turned into plain text before the request goes out. Pure: no
//! I/O and no clock, so the rewrite is tested on messages alone.

use crate::provider::{ContentBlock, Message, MessageContent};

/// Rewrite a conversation for a model that cannot call tools.
///
/// A model whose capabilities say `supports_tools = false` (an image model,
/// say) is refused by its provider the moment a request carries a function
/// call, and that includes the history: a stage that used tools earlier in
/// the run leaves `tool_use` and `tool_result` blocks in the shared
/// conversation, and Google answers the next image request with "Function
/// calling is not enabled for this model". The message array stays (this is
/// not a text-only transport), but every tool block in it becomes plain
/// text that reads as what happened, so the model sees the story and the
/// provider sees no function call. Mime blocks and text are left alone.
pub fn flatten_tool_turns(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|mut msg| {
            let MessageContent::Blocks(blocks) = &msg.content else {
                return msg;
            };
            if !blocks.iter().any(|b| {
                matches!(
                    b,
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
                )
            }) {
                return msg;
            }
            let mut out: Vec<ContentBlock> = Vec::with_capacity(blocks.len());
            for block in blocks {
                let text = match block {
                    ContentBlock::ToolUse { name, input, .. } => {
                        format!("[called {name} with {input}]")
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let marker = if *is_error {
                            "tool error"
                        } else {
                            "tool result"
                        };
                        format!("[{marker}]\n{content}")
                    }
                    other => {
                        out.push(other.clone());
                        continue;
                    }
                };
                // Adjacent text folds into one block, so a turn that was only
                // tool calls reads as one paragraph rather than a list of
                // fragments.
                match out.last_mut() {
                    Some(ContentBlock::Text { text: last }) => {
                        last.push('\n');
                        last.push_str(&text);
                    }
                    _ => out.push(ContentBlock::Text { text }),
                }
            }
            msg.content = MessageContent::Blocks(out);
            msg
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_msg(role: &str, body: &str) -> Message {
        Message {
            role: role.to_string(),
            content: body.into(),
            cache_breakpoint: false,
            reasoning: None,
        }
    }

    /// Every tool block becomes prose a model without tools can read, folded
    /// into the text beside it; plain text and mime are left as they are.
    #[test]
    fn flatten_tool_turns_rewrites_only_the_tool_blocks() {
        let reg = leviath_core::mime::MimeRegistry::builtin();
        let blob = leviath_core::mime::Blob::new(
            leviath_core::mime::MimeType::parse("image/png").unwrap(),
            vec![1, 2, 3],
        )
        .named("a.png");
        let part = leviath_core::mime::Part::stored(blob.describe(&reg)).named("a.png");
        let mime = ContentBlock::mime(&part).unwrap();
        let blocks_msg = |role: &str, blocks: Vec<ContentBlock>| Message {
            role: role.to_string(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: None,
        };
        let input = vec![
            text_msg("user", "draw it"),
            blocks_msg(
                "user",
                vec![
                    ContentBlock::Text {
                        text: "see".to_string(),
                    },
                    mime.clone(),
                ],
            ),
            blocks_msg(
                "assistant",
                vec![
                    ContentBlock::Text {
                        text: "Reading it.".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "c1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "a.txt"}),
                        thought_signature: None,
                    },
                ],
            ),
            blocks_msg(
                "user",
                vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "c1".to_string(),
                        content: "hello".to_string(),
                        is_error: false,
                    },
                    mime.clone(),
                    ContentBlock::ToolResult {
                        tool_use_id: "c2".to_string(),
                        content: "no such file".to_string(),
                        is_error: true,
                    },
                ],
            ),
        ];
        let out = flatten_tool_turns(input.clone());
        assert_eq!(out.len(), 4);
        // Untouched: a text message, and blocks with no tool in them.
        assert_eq!(out[0].content, input[0].content);
        assert_eq!(out[1].content, input[1].content);
        assert_eq!(
            out[2].content,
            MessageContent::Blocks(vec![ContentBlock::Text {
                text: "Reading it.\n[called read_file with {\"path\":\"a.txt\"}]".to_string()
            }])
        );
        assert_eq!(
            out[3].content,
            MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "[tool result]\nhello".to_string()
                },
                mime,
                ContentBlock::Text {
                    text: "[tool error]\nno such file".to_string()
                },
            ])
        );
        assert_eq!(out[2].role, "assistant");
    }
}
