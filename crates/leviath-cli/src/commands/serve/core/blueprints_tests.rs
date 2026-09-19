//! Tests for reading and caching blueprints.

use std::sync::Arc;

use super::{
    BlueprintCache, BlueprintSource, MAX_PARSED, ManifestText, digest_of, manifest_for_run,
};
use crate::runstate::RunMeta;

/// The smallest manifest that parses, with a distinguishable name.
fn manifest(name: &str) -> String {
    format!(
        "[agent]\nname = \"{name}\"\ndescription = \"a test agent\"\n\n\
         [stages.only]\nmode = \"autonomous\"\n"
    )
}

/// A run record naming an installed blueprint path.
fn meta(installed: &std::path::Path) -> RunMeta {
    RunMeta::new(
        "run-a".to_string(),
        "test-agent".to_string(),
        installed.to_string_lossy().into_owned(),
        "do the thing".to_string(),
        None,
        "/work".to_string(),
        1,
    )
}

/// A manifest as read, for the cache tests.
fn text_of(source: &str) -> ManifestText {
    ManifestText::snapshot(source.to_string())
}

/// The digest is the SHA-256 of the bytes, so the same manifest always
/// identifies the same way and any edit is a different identity.
#[test]
fn the_digest_is_the_manifests_own_hash() {
    let one = digest_of("[agent]\nname = \"a\"\n");
    assert_eq!(one.len(), 64, "lowercase hex sha256");
    assert_eq!(one, digest_of("[agent]\nname = \"a\"\n"));
    assert_ne!(one, digest_of("[agent]\nname = \"b\"\n"));
    assert_eq!(
        one,
        leviath_core::mime::store::sha256_hex(b"[agent]\nname = \"a\"\n"),
        "the same function the spawn records with"
    );
}

/// A run with a snapshot answers from its own copy, whatever the installed
/// file says now. That is the whole point of taking one.
#[test]
fn a_snapshot_wins_over_the_installed_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let installed = dir.path().join("agent.leviath");
    std::fs::write(&installed, manifest("edited-since")).expect("installed written");
    let run_dir = dir.path().join("run-a");
    std::fs::create_dir_all(&run_dir).expect("run dir");
    std::fs::write(
        run_dir.join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE),
        manifest("what-ran"),
    )
    .expect("snapshot written");

    let read = manifest_for_run(&run_dir, &meta(&installed)).expect("the snapshot reads");
    assert_eq!(read.source, BlueprintSource::Snapshot);
    assert!(read.text.contains("what-ran"), "{}", read.text);
    assert_eq!(read.digest, digest_of(&manifest("what-ran")));
}

/// A run from before snapshots existed falls back to the installed file, and
/// says so rather than implying the two are the same.
#[test]
fn a_run_without_a_snapshot_falls_back_and_says_so() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let installed = dir.path().join("agent.leviath");
    std::fs::write(&installed, manifest("installed-now")).expect("installed written");
    let run_dir = dir.path().join("run-a");
    std::fs::create_dir_all(&run_dir).expect("run dir");

    let read = manifest_for_run(&run_dir, &meta(&installed)).expect("the installed file reads");
    assert_eq!(read.source, BlueprintSource::Installed);
    assert!(read.text.contains("installed-now"), "{}", read.text);
}

/// Neither copy is a failure about the blueprint, not about the run: the run is
/// right there, and what is missing is the file it names.
#[test]
fn neither_copy_is_a_not_found_naming_the_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let run_dir = dir.path().join("run-a");
    std::fs::create_dir_all(&run_dir).expect("run dir");
    let installed = dir.path().join("gone.leviath");

    let failure = manifest_for_run(&run_dir, &meta(&installed)).expect_err("nothing to read");
    assert_eq!(failure.code(), "NOT_FOUND");
    assert!(failure.to_string().contains("gone.leviath"), "{failure}");
    assert!(failure.to_string().contains("run-a"), "{failure}");
}

/// The two ways a manifest is read each carry where it came from, because that
/// is what tells "what ran" from "what is installed".
#[test]
fn a_manifest_carries_where_it_came_from() {
    let installed = ManifestText::installed(manifest("catalogued"));
    assert_eq!(installed.source, BlueprintSource::Installed);
    assert_eq!(installed.digest, digest_of(&manifest("catalogued")));

    let snapshot = ManifestText::snapshot(manifest("catalogued"));
    assert_eq!(snapshot.source, BlueprintSource::Snapshot);
    // The same bytes are the same identity whichever file they came from.
    assert_eq!(snapshot.digest, installed.digest);
}

/// One manifest parses once, and the cached parse is the same object rather
/// than an equal one.
#[test]
fn one_manifest_parses_once() {
    let cache = BlueprintCache::default();
    let text = text_of(&manifest("cached"));
    let first = cache.parse(&text).expect("parses");
    let second = cache.parse(&text).expect("parses");
    assert!(Arc::ptr_eq(&first, &second), "the parse was shared");
    assert_eq!(first.name, "cached");
}

/// An edit is a different digest, so it is a different entry: a cache keyed by
/// name would have served the old parse for new bytes.
#[test]
fn an_edited_manifest_is_a_different_entry() {
    let cache = BlueprintCache::default();
    let before = cache.parse(&text_of(&manifest("v1"))).expect("parses");
    let after = cache.parse(&text_of(&manifest("v2"))).expect("parses");
    assert_eq!(before.name, "v1");
    assert_eq!(after.name, "v2");
    assert!(!Arc::ptr_eq(&before, &after));
}

/// A manifest that will not parse is reported and not remembered, so fixing
/// the file is enough to fix the answer.
#[test]
fn a_manifest_that_will_not_parse_is_not_cached() {
    let cache = BlueprintCache::default();
    let broken = text_of("this is not a manifest at all");
    let failure = cache.parse(&broken).expect_err("it does not parse");
    assert_eq!(failure.code(), "INTERNAL");
    assert!(failure.to_string().contains("will not parse"), "{failure}");
    // Nothing was stored, so a fixed file is read afresh rather than answered
    // from a remembered failure.
    assert_eq!(
        cache
            .parse(&ManifestText::snapshot(manifest("fixed")))
            .expect("parses")
            .name,
        "fixed"
    );
}

/// The cache is bounded: a machine with thousands of distinct manifests cannot
/// grow it without limit.
#[test]
fn the_cache_is_bounded() {
    let cache = BlueprintCache::default();
    for i in 0..=MAX_PARSED {
        cache
            .parse(&text_of(&manifest(&format!("agent-{i}"))))
            .expect("parses");
    }
    // Cleared and refilling, rather than grown past the bound.
    assert!(
        leviath_core::sync::lock(&cache.parsed).len() <= MAX_PARSED,
        "held {} entries",
        leviath_core::sync::lock(&cache.parsed).len()
    );
}
