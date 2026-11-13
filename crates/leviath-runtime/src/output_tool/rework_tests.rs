//! A part and a file that are two versions of one thing: which a
//! submission records.
//!
//! A run can hold a part and have a file of the same name on disk three
//! ways. It attached the file (`context_attach`), it exported the part
//! (`context_export`), or an earlier submission wrote the part out. In each
//! the run may have reworked the file since, and the file is what a
//! submission naming its path must record.

use super::*;
use crate::components::placed_key;
use leviath_core::mime::{Blob, BlobStore, MemoryBlobStore, MimeRegistry, MimeType, sha256_hex};
use leviath_core::{Region, RegionKind};
use serde_json::json;

/// The picture as a model drew it, as a file first held it, and as the run
/// reworked it.
const DRAWN: &[u8] = b"\x89PNG\r\n\x1a\n drawn by a model";
const FIRST: &[u8] = b"\x89PNG\r\n\x1a\n the first version";
const REWORKED: &[u8] = b"\x89PNG\r\n\x1a\n the reworked version, which is longer";

/// A run with a working directory, a store, and a window to hold parts in.
struct Run {
    window: ContextWindow,
    store: MemoryBlobStore,
    registry: MimeRegistry,
    dir: tempfile::TempDir,
}

impl Run {
    fn new() -> Self {
        let mut window = ContextWindow::new(100_000);
        for (name, budget) in [("artwork", 50_000), (FINAL_OUTPUT_REGION, 20_000)] {
            window.add_region(Region::new(name.to_string(), RegionKind::Pinned, budget));
        }
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(dir.path().join("art")).expect("art dir");
        Self {
            window,
            store: MemoryBlobStore::new(),
            registry: MimeRegistry::builtin(),
            dir,
        }
    }

    /// Put `bytes` in the store and the window as a part named `name`, the
    /// way a model's picture lands. Returns its sha256.
    fn draw(&mut self, name: &str, bytes: &[u8]) -> String {
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), bytes.to_vec()).named(name);
        let reference = self
            .store
            .put("run-1", &blob, &self.registry)
            .expect("stored");
        let sha256 = reference.sha256.clone();
        let content = leviath_core::region::EntryContent::from_parts(vec![
            leviath_core::mime::Part::stored(reference).named(name),
        ]);
        let tokens = content.tokens(None);
        self.window
            .add_content_entry(
                leviath_core::ContextCause::ProducedPart,
                "artwork",
                leviath_core::EntryKind::Text,
                content,
                tokens,
            )
            .expect("held");
        sha256
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        std::fs::write(self.dir.path().join(path), bytes).expect("written");
    }

    fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.dir.path().join(path)).expect("read")
    }

    /// What `context_attach` does with the file at `path`: a part named by
    /// the file's last component, on an entry marked a file copy.
    fn attach(&mut self, path: &str) -> String {
        let name = path.rsplit('/').next().unwrap().to_string();
        let sha256 = self.draw(&name, &self.read(path));
        self.window.mark_file_copy("artwork", None);
        sha256
    }

    /// What `context_export` does with the part `sha256`: its bytes at
    /// `path`, and the place recorded on the part's entry.
    fn export(&mut self, sha256: &str, path: &str) {
        let bytes = self.store.read("run-1", sha256).expect("stored");
        self.write(path, &bytes);
        let key = placed_key(&self.dir.path().join(path), self.dir.path());
        self.window.mark_placed(sha256, &key);
    }

    fn submit_args(
        &mut self,
        args: serde_json::Value,
        overwrite_artifacts: bool,
        given: &[String],
    ) -> (String, Option<FinalOutput>) {
        let sink = crate::context_setup::PartSink {
            store: &self.store,
            registry: &self.registry,
            run_id: "run-1",
            max_part_bytes: 1 << 20,
            inline_text_bytes: 1024,
        };
        handle_output_tool(
            &args,
            &OutputContext {
                write_fence: None,
                tool_verdict: None,
                given,
                spec: None,
                validators: None,
                stage: "draw",
                stage_names: &[],
                workdir: Some(self.dir.path()),
                sink: Some(&sink),
                overwrite_artifacts,
            },
            0,
            &mut self.window,
        )
    }

    /// Submit `path` as the one artifact, and return the acknowledgement
    /// and the artifact recorded.
    fn submit(
        &mut self,
        path: &str,
        overwrite_artifacts: bool,
    ) -> (String, leviath_core::output::Artifact) {
        let (ack, output) = self.submit_args(
            json!({ "content": "the picture is done", "artifacts": [path] }),
            overwrite_artifacts,
            &[],
        );
        let artifact = output.expect(&ack).artifacts[0].clone();
        (ack, artifact)
    }

    /// Every file in the working directory, by path relative to it, sorted.
    fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![self.dir.path().to_path_buf()];
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                match path.is_dir() {
                    true => stack.push(path),
                    false => out.push(
                        path.strip_prefix(self.dir.path())
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    ),
                }
            }
        }
        out.sort();
        out
    }
}

/// The reported failure (issue 1134): a stage attached `art/hero.png`,
/// rewrote the file, and submitted its path. The attached copy answered the
/// name ahead of the file. The rewritten file is the artifact, at the path
/// named, and the older copy is written nowhere, whichever way
/// `overwrite_artifacts` is set.
#[test]
fn an_attached_copy_gives_way_to_the_file_rewritten_since() {
    for overwrite in [false, true] {
        let mut run = Run::new();
        run.write("art/hero.png", FIRST);
        let copy = run.attach("art/hero.png");
        run.write("art/hero.png", REWORKED);
        let (ack, artifact) = run.submit("art/hero.png", overwrite);
        assert_eq!(artifact.path, "art/hero.png", "{ack}");
        assert_eq!(artifact.sha256, sha256_hex(REWORKED));
        assert_ne!(artifact.sha256, copy);
        assert_eq!(artifact.size, REWORKED.len() as u64);
        assert!(!ack.contains("beside"), "{ack}");
        assert_eq!(run.files(), ["art/hero.png"]);
        assert_eq!(run.read("art/hero.png"), REWORKED);
    }
}

/// An attached copy answers a name nothing is at: the file it was read
/// from is gone, so the copy is all there is, and it is put back.
#[test]
fn an_attached_copy_answers_when_nothing_is_at_the_path() {
    let mut run = Run::new();
    run.write("art/hero.png", FIRST);
    let copy = run.attach("art/hero.png");
    std::fs::remove_file(run.dir.path().join("art/hero.png")).unwrap();
    let (ack, artifact) = run.submit("art/hero.png", false);
    assert_eq!(artifact.path, "art/hero.png", "{ack}");
    assert_eq!(artifact.sha256, copy);
    assert_eq!(run.read("art/hero.png"), FIRST);
}

/// A copy whose bytes the store no longer holds is refused, saying so.
#[test]
fn an_attached_copy_the_store_lost_is_refused() {
    let mut run = Run::new();
    run.write("art/hero.png", FIRST);
    run.attach("art/hero.png");
    std::fs::remove_file(run.dir.path().join("art/hero.png")).unwrap();
    run.store = MemoryBlobStore::new();
    let (message, output) = run.submit_args(
        json!({ "content": "the picture is done", "artifacts": ["art/hero.png"] }),
        false,
        &[],
    );
    assert!(output.is_none());
    assert!(
        message.contains("could not be read from the run's store"),
        "{message}"
    );
}

/// A copy never outranks a file, whatever path the file is at: a stage
/// that attached `art/hero.png` to look at and wrote its own
/// `final/hero.png` hands in the file it wrote.
#[test]
fn an_attached_copy_does_not_outrank_another_file_of_its_name() {
    let mut run = Run::new();
    run.write("art/hero.png", FIRST);
    run.attach("art/hero.png");
    std::fs::create_dir(run.dir.path().join("final")).unwrap();
    run.write("final/hero.png", REWORKED);
    let (ack, artifact) = run.submit("final/hero.png", false);
    assert_eq!(artifact.path, "final/hero.png", "{ack}");
    assert_eq!(artifact.sha256, sha256_hex(REWORKED));
    assert_eq!(run.files(), ["art/hero.png", "final/hero.png"]);
}

/// A picture a model drew still comes before a file of its name that the
/// run never put there, an attached copy of that file included: the file
/// may be an earlier run's.
#[test]
fn a_drawn_part_still_answers_ahead_of_a_copy_and_its_file() {
    let mut run = Run::new();
    run.write("art/hero.png", FIRST);
    run.attach("art/hero.png");
    let drawn = run.draw("hero.png", DRAWN);
    let (ack, artifact) = run.submit("art/hero.png", false);
    let beside = format!("art/hero-{}.png", drawn.get(..8).unwrap());
    assert_eq!(artifact.path, beside, "{ack}");
    assert_eq!(artifact.sha256, drawn);
    assert_eq!(run.read("art/hero.png"), FIRST, "the file is left alone");
}

/// A copy of something the run was given is still given: with its file
/// gone it is refused as an input, not handed back as the run's work.
#[test]
fn a_copy_of_a_given_file_stays_given() {
    let mut run = Run::new();
    run.write("art/hero.png", FIRST);
    let copy = run.attach("art/hero.png");
    std::fs::remove_file(run.dir.path().join("art/hero.png")).unwrap();
    let (message, output) = run.submit_args(
        json!({ "content": "the picture is done", "artifacts": ["art/hero.png"] }),
        false,
        &[copy],
    );
    assert!(output.is_none());
    assert!(
        message.contains("something this run was given"),
        "{message}"
    );
}

/// A drawn picture the stage exported and then edited: the edited file is
/// the artifact, however its path is spelled, and the drawn original is
/// written nowhere.
#[test]
fn an_exported_part_gives_way_to_the_file_edited_since() {
    for (overwrite, named) in [
        (false, "hero.png"),
        (true, "hero.png"),
        (false, "./hero.png"),
    ] {
        let mut run = Run::new();
        let drawn = run.draw("hero.png", DRAWN);
        run.export(&drawn, "hero.png");
        run.write("hero.png", REWORKED);
        let (ack, artifact) = run.submit(named, overwrite);
        assert_eq!(artifact.path, named, "{ack}");
        assert_eq!(artifact.sha256, sha256_hex(REWORKED), "{ack}");
        assert!(!ack.contains("beside"), "{ack}");
        assert_eq!(run.files(), ["hero.png"]);
        assert_eq!(run.read("hero.png"), REWORKED);
    }
}

/// An exported part whose file is untouched is recorded as it is, and one
/// whose file is gone is written out again.
#[test]
fn an_exported_part_is_itself_when_its_file_is_unchanged_or_gone() {
    let mut run = Run::new();
    let drawn = run.draw("hero.png", DRAWN);
    run.export(&drawn, "hero.png");
    let (_, artifact) = run.submit("hero.png", false);
    assert_eq!(
        (artifact.path.as_str(), &artifact.sha256),
        ("hero.png", &drawn)
    );
    std::fs::remove_file(run.dir.path().join("hero.png")).unwrap();
    let (_, artifact) = run.submit("hero.png", false);
    assert_eq!(
        (artifact.path.as_str(), &artifact.sha256),
        ("hero.png", &drawn)
    );
    assert_eq!(run.read("hero.png"), DRAWN);
    assert_eq!(run.files(), ["hero.png"]);
}

/// A part exported to one path still comes before a file at another: only
/// the place the run put it is the run's working copy.
#[test]
fn an_exported_part_gives_way_only_where_it_was_put() {
    let mut run = Run::new();
    let drawn = run.draw("hero.png", DRAWN);
    run.export(&drawn, "art/hero.png");
    run.write("hero.png", b"an earlier run's file");
    let (ack, artifact) = run.submit("hero.png", false);
    assert_eq!(artifact.sha256, drawn, "{ack}");
    assert_eq!(
        artifact.path,
        format!("hero-{}.png", drawn.get(..8).unwrap())
    );
    assert_eq!(run.read("hero.png"), b"an earlier run's file");
}

/// A stage sent back to fix a picture it already submitted: the first
/// submission wrote the drawn part to disk, the stage reworked that file,
/// and the second submission records the rework.
#[test]
fn a_resubmitted_fix_to_a_part_already_written_out_is_recorded() {
    for overwrite in [false, true] {
        let mut run = Run::new();
        let drawn = run.draw("hero.png", DRAWN);
        let (_, first) = run.submit("art/hero.png", overwrite);
        assert_eq!(first.sha256, drawn);
        assert_eq!(run.read("art/hero.png"), DRAWN);
        run.write("art/hero.png", REWORKED);
        let (ack, second) = run.submit("art/hero.png", overwrite);
        assert_eq!(second.path, "art/hero.png", "{ack}");
        assert_eq!(second.sha256, sha256_hex(REWORKED), "{ack}");
        assert!(!ack.contains("beside"), "{ack}");
        assert_eq!(run.files(), ["art/hero.png"]);
    }
}

/// The place a part was written is kept when the submission that wrote it
/// is refused for another reason, so the stage can rework the file and
/// name it again.
#[test]
fn a_part_written_out_by_a_refused_submission_can_be_reworked() {
    let mut run = Run::new();
    run.draw("hero.png", DRAWN);
    let (message, output) = run.submit_args(
        json!({ "content": "done", "artifacts": ["hero.png", "missing.png"] }),
        false,
        &[],
    );
    assert!(output.is_none());
    assert!(message.contains("'missing.png' is neither"), "{message}");
    assert_eq!(run.read("hero.png"), DRAWN, "the part was written first");
    run.write("hero.png", REWORKED);
    let (ack, artifact) = run.submit("hero.png", false);
    assert_eq!(artifact.sha256, sha256_hex(REWORKED), "{ack}");
    assert_eq!(run.files(), ["hero.png"]);
}

/// A part written beside an earlier run's file is found beside it again on
/// a repeat: the stale file at the name is still not the run's.
#[test]
fn a_part_written_beside_a_stale_file_stays_beside_it() {
    let mut run = Run::new();
    run.write("hero.png", b"an earlier run's file");
    let drawn = run.draw("hero.png", DRAWN);
    let beside = format!("hero-{}.png", drawn.get(..8).unwrap());
    for _ in 0..2 {
        let (ack, artifact) = run.submit("hero.png", false);
        assert_eq!(artifact.path, beside, "{ack}");
        assert_eq!(artifact.sha256, drawn);
        assert_eq!(run.read("hero.png"), b"an earlier run's file");
    }
    assert_eq!(run.files(), [beside.as_str(), "hero.png"]);
}
