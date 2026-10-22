//! How an entry's typed parts become provider content blocks.
//!
//! Assembly never reads a file. A text part becomes a `Text` block and a
//! stored part becomes a pointer `Text` block (its stand-in, which names it)
//! followed by an unhydrated `Mime` block carrying the reference. Hydration
//! fills the bytes in or drops the block right before the request is sent,
//! so the assembled request stays small and every lane that skips hydration
//! still sends a correct, text-only version of the same turn.
//!
//! The layout itself (which pieces, in what order, with which pointer text)
//! is [`leviath_core::context`]'s; this file turns its pieces into this
//! crate's provider blocks.

use leviath_core::context::PartBlock;
use leviath_core::mime::Part;
use leviath_core::region::EntryContent;
use leviath_providers::ContentBlock;

impl super::ContextWindow {
    /// Every stored part anywhere in the window, cloned. What `submit_output`
    /// searches to let a stage name a part the run produced (a picture from an
    /// image model, which lives in the store, not on disk) as an artifact to
    /// emit. Lives here, beside the other mime assembly, to keep the parent
    /// file under its production-line cap.
    pub(crate) fn stored_parts(&self) -> Vec<Part> {
        self.regions
            .iter()
            .flat_map(|r| &r.content)
            .flat_map(|e| e.content.stored())
            .cloned()
            .collect()
    }

    /// The stored part `needle` names (see [`Part::is_named`]), the most
    /// recently written one when several match, so a name a newer render
    /// reused finds the newer render.
    pub(crate) fn find_stored_part(&self, needle: &str) -> Option<&Part> {
        self.regions
            .iter()
            .flat_map(|r| r.content.iter())
            .rev()
            .flat_map(|e| e.content.stored())
            .find(|p| p.is_named(needle))
    }

    /// Name the entry most recently written to `region`, unless the write
    /// already named it (a custom region's `on_write` hook may). How a routed
    /// produced part gets its file name as the key `context_delete` and
    /// `context_read` find it by. A missing region or an empty one is a
    /// no-op: the write that would have preceded this already failed.
    pub(crate) fn key_last_entry(&mut self, region: &str, key: Option<&str>) {
        if let Some(key) = key
            && let Some(region) = self.get_region_mut(region)
            && let Some(entry) = region.content.last_mut()
            && entry.key.is_none()
        {
            entry.key = Some(key.to_string());
        }
    }

    /// Every stored part in the named regions, in the order the regions are
    /// given and entries within each. What the auto-emit output path reads to
    /// find the parts a stage routed as its produced answer: only its
    /// `output_routing` targets, never the whole window, so an input mesh sat
    /// in some other region is never mistaken for the one this stage made.
    pub(crate) fn stored_parts_in(&self, regions: &[&str]) -> Vec<Part> {
        regions
            .iter()
            .filter_map(|name| self.get_region(name))
            .flat_map(|r| &r.content)
            .flat_map(|e| e.content.stored())
            .cloned()
            .collect()
    }
}

/// Pieces laid out by [`leviath_core::context`] as provider blocks: text as a
/// `Text` block, a stored part as an unhydrated `Mime` block.
pub(super) fn provider_blocks(pieces: Vec<PartBlock<'_>>) -> Vec<ContentBlock> {
    pieces
        .into_iter()
        .filter_map(|piece| match piece {
            PartBlock::Text(text) => Some(ContentBlock::Text { text }),
            PartBlock::Stored(part) => ContentBlock::mime(part),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
    use leviath_core::region::{Region, RegionKind};
    use leviath_providers::MessageContent;

    fn stored(name: &str) -> Part {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named(name);
        Part::stored(blob.describe(&reg)).named(name)
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
        let blocks = provider_blocks(leviath_core::context::lifted_blocks(&region));
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
        cause: leviath_core::ContextCause,
        region_name: &str,
        kind: leviath_core::EntryKind,
        content: EntryContent,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        self.typed_write_content(
            super::TypedWrite {
                cause: Some(cause),
                origin: super::WriteOrigin::System,
                region: region_name,
                kind,
                taint: None,
            },
            content,
            tokens,
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
    ///
    /// `cause` rides along to the journal, and is `None` only for the two write
    /// paths that cannot yet name one (see
    /// [`add_to_region`](Self::add_to_region)).
    pub(crate) fn typed_write_content(
        &mut self,
        write: super::TypedWrite<'_>,
        content: EntryContent,
        tokens: usize,
    ) -> leviath_core::Result<()> {
        let super::TypedWrite {
            cause,
            origin,
            region: region_name,
            kind,
            taint,
        } = write;
        let before = self.begin_change(region_name);
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
        self.write_to_region(cause, region_name, before, tokens, &mut |region, tokens| {
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
                pinned: true,
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
        same.add_content_entry(
            leviath_core::ContextCause::ProducedPart,
            "brain",
            EntryKind::Text,
            content.clone(),
            10,
        )
        .unwrap();
        let entry = &same.get_region("brain").unwrap().content[0];
        assert_eq!(entry.content, content);
        assert_eq!(entry.key, None);

        let mut upper = custom_window(
            "fn render(ctx) { \"\" }\nfn on_write(ctx) { #{ content: ctx.entry.content.to_upper(), key: \"k\" } }",
        );
        upper
            .add_content_entry(
                leviath_core::ContextCause::ProducedPart,
                "brain",
                EntryKind::Text,
                content,
                10,
            )
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
            .add_content_entry(
                leviath_core::ContextCause::ProducedPart,
                "missing",
                EntryKind::Text,
                EntryContent::text("x"),
                1,
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            leviath_core::Error::RegionNotFound("missing".into()).to_string()
        );
    }
}
