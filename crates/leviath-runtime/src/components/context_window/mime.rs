//! How an entry's typed parts become provider content blocks.
//!
//! Assembly never reads a file. A text part becomes a `Text` block and a
//! stored part becomes a pointer `Text` block (its stand-in, which names it)
//! followed by an unhydrated `Mime` block carrying the reference. Hydration
//! fills the bytes in or drops the block right before the request is sent,
//! so the assembled request stays small and every lane that skips hydration
//! still sends a correct, text-only version of the same turn.

use leviath_core::mime::PartBody;
use leviath_core::region::{EntryContent, Region};
use leviath_providers::{ContentBlock, MessageContent};

/// Every part of `content` as blocks, in order.
pub(super) fn content_blocks(content: &EntryContent) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    for part in content.parts() {
        match &part.body {
            PartBody::Inline(text) => {
                if !text.is_empty() {
                    blocks.push(ContentBlock::Text { text: text.clone() });
                }
            }
            PartBody::Stored(blob) => {
                blocks.push(ContentBlock::Text {
                    text: blob.stand_in.clone(),
                });
                blocks.extend(ContentBlock::mime(part));
            }
        }
    }
    blocks
}

/// Only the mime blocks of `content`, for a turn whose text has already
/// been placed (a tool result, whose text goes inside the `tool_result`).
pub(super) fn mime_blocks(content: &EntryContent) -> Vec<ContentBlock> {
    content
        .parts()
        .iter()
        .filter_map(ContentBlock::mime)
        .collect()
}

/// [`content_blocks`] with the mime blocks left out: inline text and the
/// stand-in for each stored part, but not the bytes. For an assistant turn,
/// whose media is lifted into a following user turn because a provider rejects
/// an image inside an assistant turn.
pub(super) fn text_blocks(content: &EntryContent) -> Vec<ContentBlock> {
    content_blocks(content)
        .into_iter()
        .filter(|b| !matches!(b, ContentBlock::Mime { .. }))
        .collect()
}

/// A message's content for `content`: plain text when every part is text,
/// which is the shape every provider has always seen, and blocks otherwise.
pub(super) fn message_content(content: &EntryContent) -> MessageContent {
    match content.is_text_only() {
        true => MessageContent::Text(content.to_string()),
        false => MessageContent::Blocks(content_blocks(content)),
    }
}

/// The stored parts of a region that renders into the system prompt, as
/// blocks for the one leading user message that carries them.
///
/// A system block is text, so a region's stored parts cannot travel inside
/// it. They are lifted into a user message placed before the conversation,
/// each after a pointer naming the region, the key and the part, so the
/// model can tie the bytes to the stand-in it reads in the system prompt.
pub(super) fn lifted_blocks(region: &Region) -> Vec<ContentBlock> {
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
            blocks.push(ContentBlock::Text { text: pointer });
            blocks.extend(ContentBlock::mime(part));
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
    use leviath_core::region::RegionKind;

    fn stored(name: &str) -> Part {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named(name);
        Part::stored(blob.describe(&reg)).named(name)
    }

    #[test]
    fn text_only_content_stays_a_plain_message() {
        let content = EntryContent::text("hello");
        assert_eq!(
            message_content(&content),
            MessageContent::Text("hello".to_string())
        );
        assert!(mime_blocks(&content).is_empty());
        let empty = EntryContent::from_parts(vec![Part::text("")]);
        assert!(content_blocks(&empty).is_empty());
    }

    #[test]
    fn a_stored_part_becomes_a_pointer_and_a_mime_block() {
        let content = EntryContent::from_parts(vec![Part::text("see"), stored("a.png")]);
        let blocks = content_blocks(&content);
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            blocks[1],
            ContentBlock::Text {
                text: "[image/png, 3 B] a.png".to_string()
            }
        );
        assert!(!blocks[2].is_hydrated_mime() && blocks[2].stand_in().is_some());
        assert_eq!(mime_blocks(&content).len(), 1);
        assert_eq!(
            message_content(&content),
            MessageContent::Blocks(blocks.clone())
        );
        // `text_blocks` keeps the inline text and the stand-in pointer but not
        // the mime block, for an assistant turn whose media is lifted away.
        let text = text_blocks(&content);
        assert_eq!(text.len(), 2);
        assert!(text.iter().all(|b| !matches!(b, ContentBlock::Mime { .. })));
        assert_eq!(text[0], ContentBlock::Text { text: "see".into() });
    }

    #[test]
    fn assembly_lifts_system_mime_first_and_inlines_conversation_mime() {
        use crate::components::ContextWindow;
        use leviath_core::EntryKind;
        use leviath_core::region::SerializedToolCall;
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new("art".into(), RegionKind::Pinned, 10_000));
        window.add_region(Region::new(
            "conversation".into(),
            RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            50_000,
        ));
        window
            .get_region_mut("art")
            .unwrap()
            .add_keyed_entry(
                "hero",
                EntryContent::from_parts(vec![Part::text("the hero"), stored("hero.png")]),
                5,
            )
            .unwrap();
        let conv = window.get_region_mut("conversation").unwrap();
        conv.add_typed_entry(
            EntryContent::from_parts(vec![Part::text("look at"), stored("u.png")]),
            5,
            EntryKind::UserMessage,
        )
        .unwrap();
        conv.add_typed_entry(
            EntryContent::from_parts(vec![Part::text("calling"), stored("a.png")]),
            5,
            EntryKind::AssistantTurn {
                tool_calls: vec![SerializedToolCall {
                    id: "c1".into(),
                    name: "render".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }],
            },
        )
        .unwrap();
        conv.add_typed_entry(
            EntryContent::from_parts(vec![Part::text("done"), stored("t.png")]),
            5,
            EntryKind::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "render".into(),
                is_error: false,
            },
        )
        .unwrap();
        conv.add_typed_entry(EntryContent::text("plain"), 1, EntryKind::UserMessage)
            .unwrap();

        let assembled = window.assemble();
        let messages = &assembled.messages;
        let blocks_of = |content: &MessageContent| -> Vec<ContentBlock> {
            match content {
                MessageContent::Blocks(b) => b.clone(),
                MessageContent::Text(_) => Vec::new(),
            }
        };
        // The lifted message leads, with a pointer naming region and key.
        let lifted = blocks_of(&messages[0].content);
        assert_eq!(messages[0].role, "user");
        assert_eq!(
            lifted[0],
            ContentBlock::Text {
                text: "[art / hero] [image/png, 3 B] hero.png".to_string()
            }
        );
        assert!(lifted[1].stand_in().is_some());
        // The user turn carries its text, the stand-in and the mime block.
        let user = blocks_of(&messages[1].content);
        assert_eq!(user.len(), 3);
        // The assistant turn that also called a tool keeps its text and the
        // stand-in naming any media it made, but not the media block: that turn
        // cannot carry bytes, and a user turn between its tool_use and the
        // tool_result would break the pairing a provider requires, so the rare
        // tool-and-media turn keeps only the stand-in.
        let assistant = blocks_of(&messages[2].content);
        assert!(
            assistant
                .iter()
                .all(|b| !matches!(b, ContentBlock::Mime { .. }))
        );
        assert_eq!(
            assistant.last(),
            Some(&ContentBlock::ToolUse {
                id: "c1".into(),
                name: "render".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            })
        );
        // The tool result's mime follows the result block in the same turn.
        let result = blocks_of(&messages[3].content);
        assert_eq!(
            result[0],
            ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "done\n[image/png, 3 B] t.png".into(),
                is_error: false,
            }
        );
        assert!(result[1].stand_in().is_some());
        // A text-only turn is still plain text.
        assert_eq!(
            messages[4].content,
            MessageContent::Text("plain".to_string())
        );
        assert!(blocks_of(&messages[4].content).is_empty());
        // Nothing here carries bytes: hydration is the job's business.
        assert!(
            messages
                .iter()
                .flat_map(|m| blocks_of(&m.content))
                .all(|b| !b.is_hydrated_mime())
        );
    }

    #[test]
    fn a_produced_image_is_lifted_off_the_assistant_turn_into_a_user_turn() {
        use crate::components::ContextWindow;
        use leviath_core::EntryKind;
        let mut window = ContextWindow::new(100_000);
        window.add_region(Region::new(
            "conversation".into(),
            RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            50_000,
        ));
        let conv = window.get_region_mut("conversation").unwrap();
        // The draw stage's reply: some text and the image it drew, no tool call.
        conv.add_typed_entry(
            EntryContent::from_parts(vec![Part::text("here it is"), stored("image-1.png")]),
            5,
            EntryKind::AssistantTurn { tool_calls: vec![] },
        )
        .unwrap();

        let messages = window.assemble().messages;
        let blocks_of = |content: &MessageContent| -> Vec<ContentBlock> {
            match content {
                MessageContent::Blocks(b) => b.clone(),
                MessageContent::Text(_) => Vec::new(),
            }
        };
        // The assistant turn keeps its text and the stand-in, no bytes.
        assert_eq!(messages[0].role, "assistant");
        assert_eq!(
            messages[0].content,
            MessageContent::Text("here it is\n[image/png, 3 B] image-1.png".to_string())
        );
        assert!(blocks_of(&messages[0].content).is_empty());
        // The image follows in a user turn, where a provider will actually
        // show it - an image inside the assistant turn is refused (Anthropic
        // 400s) or ignored, so the next stage would never see it.
        assert_eq!(messages[1].role, "user");
        let lifted = blocks_of(&messages[1].content);
        assert_eq!(lifted.len(), 1);
        assert!(lifted[0].stand_in().is_some() && !lifted[0].is_hydrated_mime());
    }

    #[test]
    fn a_system_region_lifts_its_stored_parts_with_pointers() {
        let mut region = Region::new("art".into(), RegionKind::Pinned, 10_000);
        region
            .add_entry(EntryContent::from_parts(vec![stored("a.png")]), 5)
            .unwrap();
        region
            .add_keyed_entry(
                "hero",
                EntryContent::from_parts(vec![Part::text("cap"), stored("b.png")]),
                5,
            )
            .unwrap();
        region
            .add_entry(EntryContent::text("just text"), 1)
            .unwrap();
        let blocks = lifted_blocks(&region);
        assert_eq!(blocks.len(), 4);
        assert_eq!(
            blocks[0],
            ContentBlock::Text {
                text: "[art] [image/png, 3 B] a.png".to_string()
            }
        );
        assert_eq!(
            blocks[2],
            ContentBlock::Text {
                text: "[art / hero] [image/png, 3 B] b.png".to_string()
            }
        );
    }
}

impl super::ContextWindow {
    /// Write an entry that carries typed parts, on the system's behalf.
    pub(crate) fn add_content_entry(
        &mut self,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: EntryContent,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.typed_write_content(
            super::WriteOrigin::System,
            region_name,
            kind,
            content,
            tokens,
            None,
        )
    }

    /// Shared core of every typed write: run the `on_write` seam with the
    /// caller's origin, then insert the entry with its kind and its taint
    /// level when given, honouring a key override from the hook.
    ///
    /// A custom region's `on_write` hook sees the entry's text rendering, as
    /// it always has. When the hook hands the same text back the parts are
    /// kept exactly; when it rewrites the text, the rewrite replaces the text
    /// parts and the stored parts follow it unchanged.
    pub(crate) fn typed_write_content(
        &mut self,
        origin: super::WriteOrigin,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: EntryContent,
        tokens: usize,
        taint: Option<leviath_core::TaintLevel>,
    ) -> leviath_core::Result<()> {
        let rendered = content.as_str().to_string();
        let (text, tokens, key_override) = match origin {
            super::WriteOrigin::Agent => {
                self.on_write_agent(region_name, rendered.clone(), tokens, &kind, None)?
            }
            super::WriteOrigin::System => {
                self.on_write_system(region_name, rendered.clone(), tokens, &kind, None)
            }
        };
        let content = if text == rendered {
            content
        } else {
            rewritten(content, text)
        };
        self.write_to_region(region_name, tokens, &mut |region, tokens| {
            match taint {
                Some(level) => {
                    region.add_typed_tainted_entry(content.clone(), tokens, kind.clone(), level)?;
                }
                None => region.add_typed_entry(content.clone(), tokens, kind.clone())?,
            }
            // A key override from the hook names the entry just pushed.
            if let Some(key) = key_override.as_deref()
                && let Some(entry) = region.content.last_mut()
            {
                entry.key = Some(key.to_string());
            }
            Ok(())
        })
    }
}

/// `content` with its text replaced by `text` and its stored parts kept.
fn rewritten(content: EntryContent, text: String) -> EntryContent {
    let mut parts = vec![leviath_core::mime::Part::text(text)];
    parts.extend(content.into_parts().into_iter().filter(|p| p.is_stored()));
    EntryContent::from_parts(parts)
}

#[cfg(test)]
mod writer_tests {
    use super::super::ContextWindow;
    use leviath_core::EntryKind;
    use leviath_core::mime::{Blob, BlobStore, MemoryBlobStore, MimeRegistry, MimeType, Part};
    use leviath_core::region::{EntryContent, Region, RegionKind};
    use std::sync::Arc;

    fn stored_png() -> Part {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
        let r = MemoryBlobStore::new().put("r", &blob, &reg).unwrap();
        Part::stored(r).named("a.png")
    }

    fn custom_window(src: &str) -> ContextWindow {
        let mut window = ContextWindow::new(10_000);
        window.add_region(Region::new(
            "brain".into(),
            RegionKind::Custom {
                script: "t.rhai".into(),
                persistent: true,
            },
            5_000,
        ));
        window.region_scripts.insert(
            "t.rhai".into(),
            Arc::new(leviath_scripting::region_hook::compile("t.rhai", src).unwrap()),
        );
        window
    }

    #[test]
    fn parts_survive_a_hook_that_keeps_the_text_and_follow_one_that_rewrites_it() {
        let content = EntryContent::from_parts(vec![Part::text("keep"), stored_png()]);
        let mut same =
            custom_window("fn render(ctx) { \"\" }\nfn on_write(ctx) { ctx.entry.content }");
        same.add_content_entry("brain", EntryKind::Text, content.clone(), 10)
            .unwrap();
        let entry = &same.get_region("brain").unwrap().content[0];
        assert_eq!(entry.content, content);
        assert_eq!(entry.key, None);

        let mut upper = custom_window(
            "fn render(ctx) { \"\" }\nfn on_write(ctx) { #{ content: ctx.entry.content.to_upper(), key: \"k\" } }",
        );
        upper
            .add_content_entry("brain", EntryKind::Text, content, 10)
            .unwrap();
        let entry = &upper.get_region("brain").unwrap().content[0];
        assert_eq!(entry.content.parts().len(), 2);
        assert_eq!(
            entry.content.parts()[0].inline_text(),
            Some("KEEP\n[IMAGE/PNG, 3 B] A.PNG")
        );
        assert!(entry.content.parts()[1].is_stored());
        assert_eq!(entry.key.as_deref(), Some("k"));

        let mut plain = ContextWindow::new(100);
        plain.add_region(Region::new("t".into(), RegionKind::Pinned, 100));
        let err = plain
            .add_content_entry("missing", EntryKind::Text, EntryContent::text("x"), 1)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            leviath_core::Error::RegionNotFound("missing".into()).to_string()
        );
    }
}
