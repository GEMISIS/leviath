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
use leviath_core::region::{EntryContent, KeyedWrite};
use leviath_providers::ContentBlock;

impl super::ContextWindow {
    /// The stored parts in the window, cloned and split by where they came
    /// from: what the run produced, what it was given, and what it copied
    /// from a working-directory file. What `submit_output` searches to let a
    /// stage name a part the run produced (a picture from an image model,
    /// which lives in the store, not on disk) as an artifact to emit, and to
    /// tell a stage that names one of its inputs that it did not make it.
    ///
    /// A part is given when it sits on an entry its writer marked given
    /// ([`mark_given`](Self::mark_given)): every seed, a reseed on continue
    /// included, and what a person sent. `given`, the digests of the parts
    /// the run's spec seeded it with, covers a run saved before that mark
    /// existed, whose seeded entries carry none.
    /// Bytes the run was given are given wherever else they turn up, so a
    /// tool that echoes an input back has not produced it. A part that is
    /// not given and sits on an entry marked a file copy
    /// ([`mark_file_copy`](Self::mark_file_copy)) is copied; that mark is the
    /// entry's alone, so a picture a model drew stays produced when the run
    /// also attaches a file holding the same bytes. Every other stored part
    /// was produced, a worker's file handed up included. The `final_output`
    /// region is left out of all three: its parts are copies of an earlier
    /// submission's artifacts, not anything a tool or a model handed the run.
    /// Lives here, beside the other mime assembly, to keep the parent file
    /// under its production-line cap.
    pub(crate) fn stored_parts_by_origin(&self, given: &[String]) -> PartsByOrigin {
        let sent = self.sent_digests();
        let mut out = PartsByOrigin::default();
        for entry in self.answering_entries() {
            out.placed.extend(entry.placed());
            for part in entry.content.stored() {
                let made = part
                    .blob()
                    .is_some_and(|b| !given.contains(&b.sha256) && !sent.contains(&b.sha256));
                match (made, entry.is_file_copy()) {
                    (false, _) => out.given.push(part.clone()),
                    (true, true) => out.copied.push(part.clone()),
                    (true, false) => out.produced.push(part.clone()),
                }
            }
        }
        out
    }

    /// The stored parts the run produced that sit in the named regions, in
    /// the order the regions are given and entries within each: what
    /// [`stored_parts_by_origin`](Self::stored_parts_by_origin) calls
    /// produced, so nothing the run was given and no copy of a
    /// working-directory file. What the auto-emit output path reads to find
    /// the parts a stage routed as its produced answer: only its
    /// `output_routing` targets, never the whole window, so an input mesh sat
    /// in some other region is never mistaken for the one this stage made.
    pub(crate) fn produced_parts_in(&self, regions: &[&str], given: &[String]) -> Vec<Part> {
        let sent = self.sent_digests();
        regions
            .iter()
            .filter_map(|name| self.get_region(name))
            .flat_map(|r| &r.content)
            .filter(|e| !e.is_file_copy())
            .flat_map(|e| e.content.stored())
            .filter(|p| {
                p.blob()
                    .is_some_and(|b| !given.contains(&b.sha256) && !sent.contains(&b.sha256))
            })
            .cloned()
            .collect()
    }

    /// Every entry whose parts may answer a name: all but `final_output`'s.
    fn answering_entries(&self) -> impl Iterator<Item = &leviath_core::region::RegionEntry> {
        self.regions
            .iter()
            .filter(|r| r.name != crate::output_tool::FINAL_OUTPUT_REGION)
            .flat_map(|r| &r.content)
    }

    /// The digest of every stored part on an entry marked given.
    fn sent_digests(&self) -> Vec<String> {
        self.answering_entries()
            .filter(|e| e.is_given())
            .flat_map(|e| e.content.stored())
            .filter_map(|p| p.blob().map(|b| b.sha256.clone()))
            .collect()
    }

    /// The entry a write just landed in `region`: the one filed under `key`
    /// when the write was keyed, else the last. `None` for a region the
    /// window does not hold, or no such entry.
    fn entry_just_written(
        &mut self,
        region: &str,
        key: Option<&str>,
    ) -> Option<&mut leviath_core::region::RegionEntry> {
        let region = self.get_region_mut(region)?;
        match key {
            Some(key) => region
                .content
                .iter_mut()
                .find(|e| e.key.as_deref() == Some(key)),
            None => region.content.last_mut(),
        }
    }

    /// Mark the entry a write just landed in `region` as given to the run
    /// (see [`RegionEntry::mark_given`](leviath_core::region::RegionEntry::mark_given)):
    /// the one filed under `key` when the write was keyed, else the last.
    /// Called by the writer that knows a person sent what it wrote, after
    /// the write succeeded, the way a turn's reasoning token is attached.
    /// A region the window does not hold, or no such entry, marks nothing.
    pub(crate) fn mark_given(&mut self, region: &str, key: Option<&str>) {
        if let Some(entry) = self.entry_just_written(region, key) {
            entry.mark_given();
        }
    }

    /// Mark the entry a write just landed in `region` as a copy of a
    /// working-directory file (see
    /// [`RegionEntry::mark_file_copy`](leviath_core::region::RegionEntry::mark_file_copy)),
    /// found the way [`mark_given`](Self::mark_given) finds its entry.
    /// Called by `context_attach`, the tool that reads a file into the window.
    pub(crate) fn mark_file_copy(&mut self, region: &str, key: Option<&str>) {
        if let Some(entry) = self.entry_just_written(region, key) {
            entry.mark_file_copy();
        }
    }

    /// Record that the run put the part with digest `sha256` on disk at
    /// `path` (a [`placed_key`]), on every entry holding that part whose
    /// parts answer a name as produced: not one given, not a file copy, and
    /// none in `final_output`. Called by the two writers that put a part in
    /// the working directory, `context_export` and a submission that names a
    /// produced part.
    pub(crate) fn mark_placed(&mut self, sha256: &str, path: &str) {
        self.regions
            .iter_mut()
            .filter(|r| r.name != crate::output_tool::FINAL_OUTPUT_REGION)
            .flat_map(|r| &mut r.content)
            .filter(|e| !e.is_given() && !e.is_file_copy())
            .filter(|e| {
                e.content
                    .stored()
                    .any(|p| p.blob().is_some_and(|b| b.sha256 == sha256))
            })
            .for_each(|e| e.mark_placed(path, sha256));
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
}

/// The stored parts in a window that may answer a name, by where they came
/// from, and where the run has put its produced parts on disk. See
/// [`ContextWindow::stored_parts_by_origin`](super::ContextWindow::stored_parts_by_origin).
#[derive(Debug, Default)]
pub(crate) struct PartsByOrigin {
    /// What a tool or a model made new, or a worker handed up.
    pub(crate) produced: Vec<Part>,
    /// What the run was given: its attachments, inputs, seeds and the files
    /// a person sent it.
    pub(crate) given: Vec<Part>,
    /// What the run read from a working-directory file with
    /// `context_attach`: the file as it was then.
    pub(crate) copied: Vec<Part>,
    /// Each `(path, sha256)` the run put a produced part on disk at, the
    /// path as a [`placed_key`].
    pub(crate) placed: Vec<(String, String)>,
}

/// How a place in the working directory is spelled where the run records
/// having put a part there: `full` with `.` and `..` folded, relative to
/// `workdir`, its components joined by `/`. `./art/hero.png` and
/// `art/hero.png` are one place. A path that will not fold, or is not
/// spelled under `workdir`, is kept whole.
pub(crate) fn placed_key(full: &std::path::Path, workdir: &std::path::Path) -> String {
    let folded = leviath_core::paths::fold_dot_dot(full);
    let folded = folded.as_deref().unwrap_or(full);
    folded
        .strip_prefix(workdir)
        .unwrap_or(folded)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
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

    /// A mark lands on the entry a keyed write filed under its key, else on
    /// the last entry; a missing region, a key nothing holds and an empty
    /// region mark nothing.
    #[test]
    fn a_mark_lands_on_the_entry_just_written() {
        use crate::components::ContextWindow;
        let mut window = ContextWindow::new(10_000);
        window.add_region(Region::new("files".into(), RegionKind::Pinned, 5_000));
        window.add_region(Region::new("empty".into(), RegionKind::Pinned, 5_000));
        let files = window.get_region_mut("files").unwrap();
        files
            .add_keyed_entry("a", EntryContent::text("a"), 1)
            .unwrap();
        files
            .add_keyed_entry("b", EntryContent::text("b"), 1)
            .unwrap();
        window.mark_given("nowhere", None);
        window.mark_given("empty", None);
        window.mark_given("files", Some("ghost"));
        let given = |w: &ContextWindow| -> Vec<bool> {
            w.get_region("files")
                .unwrap()
                .content
                .iter()
                .map(|e| e.is_given())
                .collect()
        };
        assert_eq!(given(&window), [false, false]);
        window.mark_given("files", Some("a"));
        assert_eq!(given(&window), [true, false]);
        window.mark_given("files", None);
        assert_eq!(given(&window), [true, true]);
    }

    /// A window with `files`, `art` and `final_output`, holding in order: a
    /// given part, an attached copy, a drawn part (all in `files`), a second
    /// drawn part in `art`, and a copy of the first drawn part in
    /// `final_output`. Returns the window and the sha of each of the three
    /// distinct pictures: given, copied, drawn.
    fn mixed() -> (crate::components::ContextWindow, [String; 3]) {
        use crate::components::ContextWindow;
        let mut window = ContextWindow::new(100_000);
        for name in ["files", "art", crate::output_tool::FINAL_OUTPUT_REGION] {
            window.add_region(Region::new(name.into(), RegionKind::Pinned, 10_000));
        }
        let part = |name: &str, bytes: &[u8]| {
            let reg = MimeRegistry::builtin();
            let blob = Blob::new(MimeType::parse("image/png").unwrap(), bytes.to_vec());
            Part::stored(blob.named(name).describe(&reg)).named(name)
        };
        let sha = |p: &Part| p.blob().unwrap().sha256.clone();
        let (given, copied, drawn) = (
            part("in.png", b"given"),
            part("hero.png", b"copied"),
            part("hero.png", b"drawn"),
        );
        let shas = [sha(&given), sha(&copied), sha(&drawn)];
        let mut put = |region: &str, part: Part| {
            window
                .get_region_mut(region)
                .unwrap()
                .add_entry(EntryContent::from_parts(vec![part]), 1)
                .unwrap();
        };
        put("files", given);
        put("files", copied);
        put("files", drawn.clone());
        put("art", part("mesh.png", b"second"));
        put(crate::output_tool::FINAL_OUTPUT_REGION, drawn);
        let files = window.get_region_mut("files").unwrap();
        files.content[0].mark_given();
        files.content[1].mark_file_copy();
        (window, shas)
    }

    /// A part is given, copied or produced by the entry it sits on, and the
    /// places the run put a produced part are read off the same entries.
    #[test]
    fn stored_parts_split_by_where_they_came_from() {
        let (mut window, [given, copied, drawn]) = mixed();
        // Recorded on the drawn part's own entry only: not on the copy of
        // it in `final_output`, and not on a given or a copied entry.
        for sha in [&given, &copied, &drawn] {
            window.mark_placed(sha, "out/hero.png");
        }
        let by = window.stored_parts_by_origin(&[]);
        let shas = |parts: &[Part]| -> Vec<String> {
            parts
                .iter()
                .map(|p| p.blob().unwrap().sha256.clone())
                .collect()
        };
        assert_eq!(shas(&by.given), std::slice::from_ref(&given));
        assert_eq!(shas(&by.copied), std::slice::from_ref(&copied));
        assert_eq!(by.produced.len(), 2);
        assert_eq!(shas(&by.produced)[0], drawn);
        assert_eq!(by.placed, [("out/hero.png".to_string(), drawn.clone())]);
        let output = window
            .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
            .unwrap();
        assert!(output.content[0].placed().is_empty());

        // A digest the spec lists as given is given wherever it sits, the
        // attached copy of it included.
        let by = window.stored_parts_by_origin(std::slice::from_ref(&copied));
        assert_eq!(shas(&by.given), [given.clone(), copied.clone()]);
        assert!(by.copied.is_empty());

        // Only what was produced, only in the regions named, in their order.
        let made = window.produced_parts_in(&["art", "files", "nowhere"], &[]);
        let names: Vec<&str> = made.iter().filter_map(|p| p.name.as_deref()).collect();
        assert_eq!(names, ["mesh.png", "hero.png"]);
        assert_eq!(shas(&made)[1], drawn);
        assert!(
            window
                .produced_parts_in(&["files"], std::slice::from_ref(&drawn))
                .is_empty()
        );
    }

    /// Each mark survives the window being saved and loaded back.
    #[test]
    fn the_marks_survive_the_saved_state() {
        use crate::components::ContextWindow;
        let (mut window, [_, _, drawn]) = mixed();
        window.mark_placed(&drawn, "out/hero.png");
        let shape =
            |name: &str, budget: usize| Region::new(name.to_string(), RegionKind::Pinned, budget);
        let again = ContextWindow::from_state(&window.to_state(), &shape);
        let files = &again.get_region("files").unwrap().content;
        assert!(files[0].is_given());
        assert!(files[1].is_file_copy());
        assert_eq!(files[2].placed(), [("out/hero.png".to_string(), drawn)]);
        assert_eq!(again.get_region("art").unwrap().content[0].metadata, None);
    }

    /// A copy mark lands the way a given mark does.
    #[test]
    fn a_copy_mark_lands_on_the_entry_just_written() {
        let (mut window, _) = mixed();
        window.mark_file_copy("nowhere", None);
        window.mark_file_copy("art", None);
        assert!(window.get_region("art").unwrap().content[0].is_file_copy());
    }

    /// A place is spelled relative to the working directory with its dots
    /// folded; a path that will not fold, or is not under it, is kept whole.
    #[test]
    fn a_placed_key_is_the_folded_path_under_the_workdir() {
        use std::path::Path;
        let workdir = Path::new("work");
        for spelled in ["art/hero.png", "./art/hero.png", "art/x/../hero.png"] {
            assert_eq!(placed_key(&workdir.join(spelled), workdir), "art/hero.png");
        }
        assert_eq!(
            placed_key(Path::new("else/hero.png"), workdir),
            "else/hero.png"
        );
        assert_eq!(placed_key(Path::new("../hero.png"), workdir), "../hero.png");
    }

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
        self.typed_write_keyed(write, None, content, tokens)
            .map(|_| ())
    }

    /// [`typed_write_content`](Self::typed_write_content), filing the entry
    /// under `key` when there is one: the entry the key already holds is
    /// replaced, or left exactly as it is when this write is the same
    /// content, the way
    /// [`Region::replace_keyed_entry`](leviath_core::Region::replace_keyed_entry)
    /// says. A write with no key is appended, and reports itself as
    /// [`KeyedWrite::Added`].
    ///
    /// Shared core of every typed write: run the `on_write` seam with the
    /// caller's origin, then insert the entry with its kind and its taint
    /// level when given, honouring a key override from the hook.
    pub(crate) fn typed_write_keyed(
        &mut self,
        write: super::TypedWrite<'_>,
        key: Option<&str>,
        content: EntryContent,
        tokens: usize,
    ) -> leviath_core::Result<KeyedWrite> {
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
        let mut written = KeyedWrite::Added;
        self.write_to_region(
            cause,
            region_name,
            before,
            tokens,
            key.is_some(),
            &mut |region, tokens| {
                // A region with taint tracking off ignores the level, so
                // `Public` stands in for "none given" on the keyed path.
                let level = taint.unwrap_or(leviath_core::TaintLevel::Public);
                match (key, taint) {
                    (Some(key), _) => {
                        written = region.replace_keyed_entry(
                            key,
                            content.clone(),
                            tokens,
                            kind.clone(),
                            level,
                        )?;
                    }
                    (None, Some(level)) => {
                        region.add_typed_tainted_entry(
                            content.clone(),
                            tokens,
                            kind.clone(),
                            level,
                        )?;
                    }
                    (None, None) => {
                        region.add_typed_entry(content.clone(), tokens, kind.clone())?;
                    }
                }
                // A key override from the hook names the entry just pushed.
                if let Some(key) = key_override.as_deref()
                    && let Some(entry) = region.content.last_mut()
                {
                    entry.key = Some(key.to_string());
                }
                Ok(())
            },
        )?;
        Ok(written)
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
    use leviath_core::region::{EntryContent, KeyedWrite, Region, RegionKind};
    use std::sync::Arc;

    fn stored_png() -> Part {
        let reg = MimeRegistry::builtin();
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
        let r = MemoryBlobStore::new().put("r", &blob, &reg).unwrap();
        Part::stored(r).named("a.png")
    }

    /// A keyed write through the window is the region's own
    /// `replace_keyed_entry`: a repeat of the same bytes leaves the window
    /// exactly as it was, revision included.
    #[test]
    fn a_keyed_write_of_the_same_bytes_changes_nothing() {
        let mut window = ContextWindow::new(10_000);
        window.add_region(Region::new("codebase".into(), RegionKind::Temporary, 1_000));
        let write = |window: &mut ContextWindow, body: &str| {
            window.typed_write_keyed(
                super::super::TypedWrite {
                    cause: Some(leviath_core::ContextCause::ToolResult),
                    origin: super::super::WriteOrigin::Agent,
                    region: "codebase",
                    kind: EntryKind::Text,
                    taint: Some(leviath_core::TaintLevel::Public),
                },
                Some("read_file a.rs"),
                EntryContent::text(body),
                3,
            )
        };
        assert_eq!(write(&mut window, "fn a()").unwrap(), KeyedWrite::Added);
        let before = window.get_region("codebase").unwrap().clone();
        assert_eq!(write(&mut window, "fn a()").unwrap(), KeyedWrite::Unchanged);
        let after = window.get_region("codebase").unwrap();
        assert_eq!(after.content.len(), 1);
        assert_eq!(after.content[0].content, before.content[0].content);
        assert_eq!(after.current_tokens, before.current_tokens);
        assert_eq!(window.current_tokens, 3);
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
