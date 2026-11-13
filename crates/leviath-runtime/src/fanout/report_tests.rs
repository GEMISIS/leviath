//! A fan-out parent's handed-up files, as `submit_output` reads them. A
//! sibling file, because the fixtures need a world, a store and a working
//! directory.

use super::*;
use crate::output_tool::{FINAL_OUTPUT_REGION, OutputContext, handle_output_tool};
use leviath_core::mime::{Blob, BlobStore, MemoryBlobStore, MimeRegistry, MimeType, Part};
use leviath_core::{Region, RegionKind};

const WORKER: &[u8] = b"{\"tiles\": [\"the worker's map\"]}";
const SENT: &[u8] = b"{\"note\": \"what a person sent\"}";

/// A part of `bytes` named `name`, stored for `run-1`.
fn stored(store: &MemoryBlobStore, registry: &MimeRegistry, name: &str, bytes: &[u8]) -> Part {
    let blob = Blob::new(MimeType::parse("application/json").unwrap(), bytes.to_vec()).named(name);
    Part::stored(store.put("run-1", &blob, registry).unwrap()).named(name)
}

/// Submit `path` as an artifact from `window`, into `dir`.
fn submit(
    window: &mut ContextWindow,
    store: &MemoryBlobStore,
    registry: &MimeRegistry,
    dir: &std::path::Path,
    path: &str,
) -> (String, Option<leviath_core::output::FinalOutput>) {
    let sink = crate::context_setup::PartSink {
        store,
        registry,
        run_id: "run-1",
        max_part_bytes: 1 << 20,
        inline_text_bytes: 1024,
    };
    handle_output_tool(
        &serde_json::json!({ "content": "merged", "artifacts": [path] }),
        &OutputContext {
            spec: None,
            validators: None,
            stage: "merge",
            stage_names: &[],
            workdir: Some(dir),
            sink: Some(&sink),
            overwrite_artifacts: false,
            write_fence: None,
            tool_verdict: None,
            given: &[],
        },
        0,
        window,
    )
}

/// What a fan-out parent ends up holding: the workers' report with a
/// handed-up file, written by the report's own writer, and a person's
/// message with a file of its own, written the way message delivery writes
/// one.
fn parent_window(store: &MemoryBlobStore, registry: &MimeRegistry) -> ContextWindow {
    let mut window = ContextWindow::new(100_000);
    for name in ["conversation", "results", FINAL_OUTPUT_REGION] {
        window.add_region(Region::new(name.to_string(), RegionKind::Pinned, 20_000));
    }
    let mut world = World::new();
    let parent = world.spawn(window).id();
    let handed_up = stored(store, registry, "w1/map.json", WORKER);
    inject_results(
        &mut world,
        parent,
        "results",
        "## worker w1\ndone",
        vec![handed_up],
    );
    let mut window = world.get::<ContextWindow>(parent).unwrap().clone();
    let content = leviath_core::region::EntryContent::from_parts(vec![
        Part::text("here is the note"),
        stored(store, registry, "note.json", SENT),
    ]);
    let tokens = content.tokens(None);
    window
        .add_content_entry(
            leviath_core::ContextCause::Message,
            "conversation",
            leviath_core::EntryKind::UserMessage,
            content,
            tokens,
        )
        .unwrap();
    window.mark_given("conversation", None);
    window
}

/// Both answers for one window: the worker's file is the run's work and is
/// written where it is named, and the person's file is refused as given.
fn check(window: &mut ContextWindow, store: &MemoryBlobStore, registry: &MimeRegistry) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("w1")).unwrap();
    let (ack, output) = submit(window, store, registry, dir.path(), "w1/map.json");
    let artifact = &output.expect("the worker's file is recorded").artifacts[0];
    assert_eq!(artifact.path, "w1/map.json");
    assert_eq!(artifact.sha256, leviath_core::mime::sha256_hex(WORKER));
    assert!(!ack.contains("beside"), "{ack}");
    assert_eq!(
        std::fs::read(dir.path().join("w1/map.json")).unwrap(),
        WORKER
    );
    let (message, output) = submit(window, store, registry, dir.path(), "note.json");
    assert!(output.is_none());
    assert!(
        message.contains("the name matches something this run was given"),
        "{message}"
    );
    assert!(!dir.path().join("note.json").exists());
}

/// The report lands as a user-message entry, which says how the merge model
/// is shown it, not who made the files on it: a worker's file handed up is
/// produced, and answers its name. A person's file beside it is given, and
/// both answers survive the window being saved and loaded back.
#[test]
fn a_handed_up_file_answers_its_name_and_a_sent_one_does_not() {
    let store = MemoryBlobStore::new();
    let registry = MimeRegistry::builtin();
    let window = parent_window(&store, &registry);
    let results = window.get_region("results").unwrap();
    assert_eq!(
        results.content[0].kind,
        leviath_core::EntryKind::UserMessage
    );
    assert!(!results.content[0].is_given());

    // Through the run's saved state, before anything is submitted.
    let state = window.to_state();
    let shape =
        |name: &str, budget: usize| Region::new(name.to_string(), RegionKind::Pinned, budget);
    let mut reloaded = ContextWindow::from_state(&state, &shape);
    assert!(
        reloaded.get_region("conversation").unwrap().content[0].is_given(),
        "the mark survives the snapshot"
    );

    let mut window = window;
    check(&mut window, &store, &registry);
    check(&mut reloaded, &store, &registry);
}
