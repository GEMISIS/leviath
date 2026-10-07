//! A run's file as the bundle carries it.

use std::path::Path;

use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::spec::names::Digest;
use leviath_runtime::spec::request::SpawnSource;
use leviath_runtime::spec::run_spec::SpecOrigin;

use super::super::collect::{Bundle, copy_run_file};
use super::super::scrub::Scrubber;
use super::request_of;
use crate::runstate::run_file::tests::{recorded, say, step};

/// A recorded run with two steps, the second carrying a planted secret and
/// naming one stored part.
fn a_run(runs: &Path) -> std::path::PathBuf {
    let dir = recorded(runs);
    step(&dir, 10, |s| say(s, "hello"));
    step(&dir, 20, |s| {
        say(s, "the key is sk-ant-api03-PLANTEDPLANTEDPLANTED");
        s.blobs.push(leviath_runtime::state::BlobFile {
            digest: Digest::of(b"a part"),
            mime_type: "text/plain".into(),
            size: 6,
            name: None,
            region: None,
            tool: None,
        });
    });
    dir
}

/// The members `copy_run_file` adds for the run in `dir`.
fn copied(dir: &Path, scrubber: &Scrubber, scratch: &Path) -> Bundle {
    let mut bundle = Bundle::default();
    copy_run_file(dir, "runs/r", scrubber, &mut bundle, scratch);
    bundle
}

fn member<'a>(bundle: &'a Bundle, path: &str) -> &'a [u8] {
    &bundle
        .members
        .iter()
        .find(|m| m.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the bundle"))
        .bytes
}

/// The rewritten file reads back as a run, every step there, the secret out
/// of it and its part named in it, never copied in; `request.json` starts
/// the same run again.
#[test]
fn the_run_file_is_rewritten_without_its_secrets() {
    let runs = tempfile::tempdir().unwrap();
    let dir = a_run(runs.path());
    let scratch = tempfile::tempdir().unwrap();
    let bundle = copied(&dir, &Scrubber::new(Vec::new()), scratch.path());
    let original = RunFileReader::open(&crate::runstate::run_file::path_in(&dir)).unwrap();
    let bytes = member(&bundle, "runs/r/run.lvr").to_vec();
    let rewritten = RunFileReader::from_bytes(Path::new("run.lvr"), bytes).unwrap();
    assert_eq!(rewritten.last_seq(), original.last_seq());
    assert_eq!(rewritten.spec().run_id, original.spec().run_id);
    let steps = serde_json::to_string(&rewritten.deltas(1, rewritten.last_seq()).unwrap()).unwrap();
    assert!(steps.contains("hello"), "{steps}");
    assert!(!steps.contains("PLANTED"), "{steps}");
    let named = rewritten.latest_state().unwrap().blobs;
    assert_eq!(named[0].digest, Digest::of(b"a part"));
    let bytes = member(&bundle, "runs/r/run.lvr");
    assert!(!bytes.windows(6).any(|w| w == b"a part"));
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "the scratch file is removed"
    );
    let run: serde_json::Value =
        serde_json::from_slice(member(&bundle, "runs/r/run.json")).unwrap();
    assert!(run["start"].is_object() && run["state"].is_object());
    assert!(!run.to_string().contains("PLANTED"));
    let request: serde_json::Value =
        serde_json::from_slice(member(&bundle, "runs/r/request.json")).unwrap();
    assert_eq!(request["source"]["blueprint"]["name"], "coder");
    assert!(request["inputs"].is_object(), "{request}");
}

/// A file that cannot be rewritten is left out and says why; its values
/// still come as `run.json`.
#[test]
fn a_run_file_that_cannot_be_rewritten_is_left_out_saying_why() {
    let runs = tempfile::tempdir().unwrap();
    let dir = a_run(runs.path());
    let nowhere = runs.path().join("no-such-scratch");
    let bundle = copied(&dir, &Scrubber::new(Vec::new()), &nowhere);
    let skipped = &bundle.skipped[0];
    assert_eq!(skipped.path, "runs/r/run.lvr");
    assert!(
        skipped.reason.contains("could not be rewritten"),
        "{}",
        skipped.reason
    );
    assert!(!member(&bundle, "runs/r/run.json").is_empty());

    // A secret that is also a name in the run: taken out, the name no
    // longer reads, so the file is not rewritten from it.
    let scratch = tempfile::tempdir().unwrap();
    let bundle = copied(
        &dir,
        &Scrubber::new(vec!["read_file".to_string()]),
        scratch.path(),
    );
    assert!(
        bundle.skipped[0].reason.contains("no longer reads"),
        "{}",
        bundle.skipped[0].reason
    );
    let run = String::from_utf8_lossy(member(&bundle, "runs/r/run.json")).into_owned();
    assert!(
        run.contains("[REDACTED"),
        "the copy still has the secret taken out"
    );
}

/// A run file that does not read is left out, with nothing else of it.
#[test]
fn a_run_file_that_does_not_read_is_left_out() {
    let runs = tempfile::tempdir().unwrap();
    let dir = a_run(runs.path());
    let file = crate::runstate::run_file::path_in(&dir);
    let mut bytes = std::fs::read(&file).unwrap();
    bytes.extend(
        leviath_runtime::runfile::codec::encode(
            leviath_runtime::runfile::codec::FrameKind::Delta,
            &3u64,
        )
        .unwrap(),
    );
    std::fs::write(&file, bytes).unwrap();
    let bundle = copied(&dir, &Scrubber::new(Vec::new()), runs.path());
    assert!(bundle.members.is_empty());
    assert!(bundle.skipped[0].reason.contains("could not be read"));
}

/// The request names what ran: an installed blueprint, one read from a
/// directory by its name, or the graph itself.
#[test]
fn the_request_names_what_ran() {
    let runs = tempfile::tempdir().unwrap();
    let dir = recorded(runs.path());
    let mut spec = crate::runstate::run_file::open_in(&dir)
        .unwrap()
        .spec()
        .clone();
    let name = leviath_runtime::spec::names::BlueprintName::new("coder").unwrap();
    spec.origin = SpecOrigin::Blueprint {
        blueprint: leviath_runtime::spec::names::BlueprintRef {
            name: name.clone(),
            digest: Some(Digest::of(b"rev")),
        },
        version: "1".into(),
        manifest: String::new(),
    };
    let SpawnSource::Blueprint(named) = request_of(&spec).source else {
        panic!("an installed blueprint is named")
    };
    assert_eq!((named.name.as_str(), named.digest), ("coder", None));
    spec.origin = SpecOrigin::BlueprintFile {
        path: leviath_runtime::spec::names::BlueprintPath::new(
            std::env::temp_dir().join("coder").to_string_lossy(),
        )
        .unwrap(),
        name: name.clone(),
        digest: None,
        version: "1".into(),
    };
    assert!(matches!(
        request_of(&spec).source,
        SpawnSource::Blueprint(_)
    ));
    // A run converted from what it recorded names the blueprint it ran.
    spec.origin = SpecOrigin::Recorded {
        name,
        manifest: String::new(),
        why: "not installed".into(),
    };
    assert!(matches!(
        request_of(&spec).source,
        SpawnSource::Blueprint(_)
    ));
    spec.origin = SpecOrigin::Raw;
    assert!(matches!(request_of(&spec).source, SpawnSource::Raw(_)));
}
