//! Tests for reading, caching and writing blueprints.

use std::sync::Arc;

use leviath_runtime::runfile::{CheckpointPolicy, RunFileReader, RunFileWriter};
use leviath_runtime::spec::env::CodeFiles;
use leviath_runtime::spec::run_spec::{RunSpec, SpecOrigin};

use super::super::run_file;
use super::super::run_file::tests::recorded;
use super::{
    BlueprintCache, BlueprintSource, MAX_PARSED, ManifestText, ParsedBlueprint, blueprint_for_run,
    digest_of, parse_blueprint, remove_blueprint, write_blueprint,
};
use crate::commands::serve::blueprints::TEST_AGENTS_DIR;
use crate::test_support::tiny_blueprint;

/// A blueprint's text as read, for the cache tests.
fn text_of(source: &str) -> ManifestText {
    ManifestText::installed(source.to_string())
}

/// Rewrite `run_id`'s file with `edit` made to its spec, from the state it
/// started in.
fn respec(run_id: &str, edit: impl FnOnce(&mut RunSpec)) {
    let path = run_file::path(run_id);
    let reader = RunFileReader::open(&path).unwrap();
    let mut spec = reader.spec().clone();
    edit(&mut spec);
    let start = reader.state_at(0).unwrap();
    RunFileWriter::create(
        &path,
        &spec,
        &CodeFiles::new(),
        &start,
        CheckpointPolicy::default(),
    )
    .unwrap();
}

/// The digest is the SHA-256 of the bytes, so the same file always
/// identifies the same way and any edit is a different identity.
#[test]
fn the_digest_is_the_files_own_hash() {
    let one = digest_of(&tiny_blueprint("a"));
    assert_eq!(one.len(), 64, "lowercase hex sha256");
    assert_eq!(one, digest_of(&tiny_blueprint("a")));
    assert_ne!(one, digest_of(&tiny_blueprint("b")));
    assert_eq!(
        one,
        leviath_runtime::spec::names::Digest::of(tiny_blueprint("a").as_bytes()).to_string(),
        "the digest a spawn pins the installed revision to"
    );
    assert_eq!(text_of(&tiny_blueprint("a")).digest, one);
}

/// A run answers from the graph in its own file, under the name and version
/// it was spawned from.
#[tokio::test]
async fn a_run_answers_from_its_own_file() {
    crate::runstate::with_isolated_runs_dir_async("bp-for-run", |_d| async move {
        let run_id = recorded();
        let read = blueprint_for_run(&run_id).expect("the run file reads");
        assert_eq!(read.source, BlueprintSource::Snapshot);
        assert_eq!(read.parsed.name, "coder");
        assert_eq!(read.parsed.version, "0.0.0");
        let spec = RunFileReader::open(&run_file::path(&run_id))
            .unwrap()
            .spec()
            .clone();
        assert_eq!(read.parsed.graph, spec.graph);
        assert_eq!(read.digest.len(), 64);
        assert_eq!(
            read.digest,
            blueprint_for_run(&run_id).unwrap().digest,
            "the same graph, the same digest"
        );

        // A run whose inputs changed its graph is a different graph, and
        // says so.
        respec(&run_id, |spec| {
            spec.graph.stages[0].max_iterations = Some(99)
        });
        assert_ne!(blueprint_for_run(&run_id).unwrap().digest, read.digest);
    })
    .await;
}

/// A run with no file is a miss naming the run.
#[tokio::test]
async fn a_run_without_a_file_is_not_found() {
    crate::runstate::with_isolated_runs_dir_async("bp-for-ghost", |_d| async move {
        let failure = blueprint_for_run("ghost").expect_err("nothing to read");
        assert_eq!(failure.code(), "NOT_FOUND");
        assert!(failure.to_string().contains("ghost"), "{failure}");
    })
    .await;
}

/// A graph its caller wrote has no blueprint name or version: it is called
/// by its title, or nothing.
#[tokio::test]
async fn a_raw_graph_is_named_by_its_title() {
    crate::runstate::with_isolated_runs_dir_async("bp-raw", |_d| async move {
        let run_id = recorded();
        let mut spec = RunFileReader::open(&run_file::path(&run_id))
            .unwrap()
            .spec()
            .clone();
        spec.origin = SpecOrigin::Raw;
        spec.graph.title = Some("a quick look".to_string());
        let titled = ParsedBlueprint::of_run(&spec);
        assert_eq!(titled.name, "a quick look");
        assert_eq!(titled.version, "");

        spec.graph.title = None;
        assert_eq!(ParsedBlueprint::of_run(&spec).name, "");

        // A run of an installed blueprint is called by its name and version,
        // whatever its graph's title says.
        spec.origin = SpecOrigin::Blueprint {
            blueprint: leviath_runtime::spec::names::BlueprintRef::parse("coder").unwrap(),
            version: "2.1.0".to_string(),
            manifest: String::new(),
        };
        spec.graph.title = Some("a quick look".to_string());
        let installed = ParsedBlueprint::of_run(&spec);
        assert_eq!(installed.name, "coder");
        assert_eq!(installed.version, "2.1.0");
    })
    .await;
}

/// A blueprint's description comes from `[blueprint]`, and reads as empty
/// when it gives none.
#[test]
fn a_description_reads_from_the_file() {
    let described = tiny_blueprint("a").replace(
        "version = \"1.0.0\"",
        "version = \"1.0.0\"\ndescription = \"does a\"",
    );
    assert_eq!(parse_blueprint(&described).unwrap().description(), "does a");
    assert_eq!(
        parse_blueprint(&tiny_blueprint("a")).unwrap().description(),
        ""
    );
}

/// A graph that does not hold together is refused with every problem, each
/// naming the key it is at.
#[test]
fn a_graph_that_does_not_hold_together_is_refused() {
    let broken = tiny_blueprint("a").replace("[graph]\n", "[graph]\nentry = \"nowhere\"\n");
    let problems = parse_blueprint(&broken).expect_err("the entry names no stage");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("graph.entry"), "{problems:?}");

    let unreadable = parse_blueprint("not a blueprint").expect_err("not TOML");
    assert_eq!(unreadable.len(), 1);
}

/// One file parses once, and the cached parse is the same object rather than
/// an equal one.
#[test]
fn one_file_parses_once() {
    let cache = BlueprintCache::default();
    let text = text_of(&tiny_blueprint("cached"));
    let first = cache.parse(&text).expect("parses");
    let second = cache.parse(&text).expect("parses");
    assert!(Arc::ptr_eq(&first, &second), "the parse was shared");
    assert_eq!(first.name, "cached");
}

/// An edit is a different digest, so it is a different entry: a cache keyed by
/// name would have served the old parse for new bytes.
#[test]
fn an_edited_file_is_a_different_entry() {
    let cache = BlueprintCache::default();
    let before = cache
        .parse(&text_of(&tiny_blueprint("v1")))
        .expect("parses");
    let after = cache
        .parse(&text_of(&tiny_blueprint("v2")))
        .expect("parses");
    assert_eq!(before.name, "v1");
    assert_eq!(after.name, "v2");
    assert!(!Arc::ptr_eq(&before, &after));
}

/// A file that will not parse is reported and not remembered, so fixing the
/// file is enough to fix the answer.
#[test]
fn a_file_that_will_not_parse_is_not_cached() {
    let cache = BlueprintCache::default();
    let broken = text_of("this is not a blueprint at all");
    let failure = cache.parse(&broken).expect_err("it does not parse");
    assert_eq!(failure.code(), "INTERNAL");
    assert!(failure.to_string().contains("will not parse"), "{failure}");
    assert!(leviath_core::sync::lock(&cache.parsed).is_empty());
    assert_eq!(
        cache
            .parse(&text_of(&tiny_blueprint("fixed")))
            .expect("parses")
            .name,
        "fixed"
    );
}

/// The cache is bounded: a machine with thousands of distinct files cannot
/// grow it without limit.
#[test]
fn the_cache_is_bounded() {
    let cache = BlueprintCache::default();
    for i in 0..=MAX_PARSED {
        cache
            .parse(&text_of(&tiny_blueprint(&format!("agent-{i}"))))
            .expect("parses");
    }
    // Cleared and refilling, rather than grown past the bound.
    assert!(
        leviath_core::sync::lock(&cache.parsed).len() <= MAX_PARSED,
        "held {} entries",
        leviath_core::sync::lock(&cache.parsed).len()
    );
}

/// A blueprint is installed under the name it calls itself. One that calls
/// itself something else is refused before anything is written: the daemon
/// finds a blueprint by its directory and would refuse to run it.
#[tokio::test]
async fn a_blueprint_is_installed_under_its_own_name() {
    let agents = tempfile::tempdir().unwrap();
    TEST_AGENTS_DIR
        .scope(agents.path().to_path_buf(), async {
            let refused = write_blueprint("other", tiny_blueprint("mine"), false)
                .err()
                .expect("the names differ");
            assert_eq!(refused.code(), "BAD_USER_INPUT");
            assert!(refused.to_string().contains("'mine'"), "{refused}");
            assert!(!agents.path().join("other").exists(), "nothing was written");

            let invalid = write_blueprint("mine", "not a blueprint".to_string(), false)
                .err()
                .expect("it does not parse");
            assert_eq!(invalid.code(), "BAD_USER_INPUT");

            let written = write_blueprint("mine", tiny_blueprint("mine"), false).expect("written");
            assert_eq!(written.parsed.name, "mine");
            assert!(written.dir.join(leviath_blueprint::FILE_NAME).is_file());
            assert_eq!(written.manifest.digest, digest_of(&tiny_blueprint("mine")));

            remove_blueprint("mine").expect("removed");
            assert!(!written.dir.exists());
        })
        .await;
}

/// An `agent.leviath`, as older clients still save one.
const OLD_MANIFEST: &str = r#"[agent]
name = "oldie"
version = "0.1.0"
description = "An old-format blueprint"
entry_stage = "work"

[context.regions]
task = { kind = "pinned", max_tokens = 1000, required = true, seed = "task" }
conversation = { kind = "sliding_window", max_tokens = 4000, max_items = 30 }

[stages.work]
mode = "autonomous"
description = "Do it"
model = { models = [{ provider = "openai", model = "gpt-mock" }] }
available_tools = ["read_file"]
max_iterations = 6
system_prompt = "Do the task."
"#;

/// An `agent.leviath` is converted before anything is written, and stored
/// as the `agent.toml` it converts to. One that does not convert is refused
/// with what the conversion found, and nothing is written.
#[tokio::test]
async fn an_old_blueprint_is_saved_as_the_agent_toml_it_converts_to() {
    let agents = tempfile::tempdir().unwrap();
    TEST_AGENTS_DIR
        .scope(agents.path().to_path_buf(), async {
            let written = write_blueprint("oldie", OLD_MANIFEST.to_string(), false)
                .expect("an old blueprint converts");
            assert_eq!(written.parsed.name, "oldie");
            let stored =
                std::fs::read_to_string(written.dir.join(leviath_blueprint::FILE_NAME)).unwrap();
            assert!(stored.contains("[blueprint]"), "{stored}");
            assert!(parse_blueprint(&stored).is_ok());

            let broken = OLD_MANIFEST.replace("[stages.work]", "[stages.work]\nbogus_key = 1");
            let refused = write_blueprint("oldie", broken, true)
                .err()
                .expect("it does not convert");
            assert_eq!(refused.code(), "BAD_USER_INPUT");
            assert!(
                refused
                    .to_string()
                    .contains("agent.leviath does not convert"),
                "{refused}"
            );
            let kept =
                std::fs::read_to_string(written.dir.join(leviath_blueprint::FILE_NAME)).unwrap();
            assert_eq!(kept, stored, "a refused save writes nothing");
        })
        .await;
}

/// The check a client runs before it saves judges an `agent.leviath` as the
/// `agent.toml` it would be saved as, and says it was converted.
#[test]
fn an_old_blueprint_is_checked_as_what_it_converts_to() {
    use crate::commands::serve::blueprints::validate_manifest_text;
    let dir = tempfile::tempdir().unwrap();
    let verdict = validate_manifest_text(OLD_MANIFEST, dir.path());
    assert!(verdict.valid, "{:?}", verdict.errors);
    assert_eq!(
        verdict
            .warnings
            .as_deref()
            .unwrap_or_default()
            .first()
            .map(String::as_str),
        Some(super::CONVERTED_NOTE)
    );
    let broken = OLD_MANIFEST.replace("[stages.work]", "[stages.work]\nbogus_key = 1");
    let verdict = validate_manifest_text(&broken, dir.path());
    assert!(!verdict.valid);
    let errors = verdict.errors.unwrap_or_default();
    assert!(
        errors[0].starts_with("agent.leviath does not convert"),
        "{errors:?}"
    );
    // An agent.toml gets no such note.
    let verdict = validate_manifest_text(&tiny_blueprint("t"), dir.path());
    assert!(verdict.valid);
    assert!(
        !verdict
            .warnings
            .unwrap_or_default()
            .iter()
            .any(|w| w == super::CONVERTED_NOTE)
    );
}

#[test]
fn only_an_old_blueprint_is_converted() {
    let (same, converted) = super::as_agent_toml(&tiny_blueprint("t")).unwrap();
    assert_eq!(same, tiny_blueprint("t"));
    assert!(!converted);
    // Text that is not TOML at all is left for the parser to refuse.
    let (same, converted) = super::as_agent_toml("not [ toml").unwrap();
    assert_eq!(same, "not [ toml");
    assert!(!converted);
    let (toml, converted) = super::as_agent_toml(OLD_MANIFEST).unwrap();
    assert!(converted);
    assert!(toml.contains("[blueprint]"));
}
