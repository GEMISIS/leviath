use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use leviath_core::JsonDoc;
use leviath_runtime::spec::graph::OutputDef;
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::issues::IssueCode;
use leviath_runtime::spec::names::{BlueprintRef, Digest};
use leviath_runtime::spec::request::SpawnSource;

use super::*;

const TINY: &str = r#"[blueprint]
name = "tiny"
version = "1.0.0"

[graph]
stages = [{ name = "main", system_prompt = "work" }]
layout = { total_budget_tokens = 1000, regions = [
    { name = "task", kind = "pinned", budget = 1000 },
] }
"#;

/// Each bundled blueprint's `agent.toml`, by directory name.
fn bundled() -> Vec<(String, PathBuf)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../leviath-cli/agents");
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .expect("the bundled agents are there")
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name, e.path().join(FILE_NAME))
        })
        .collect();
    out.sort();
    out
}

/// A directory holding `name/agent.toml` with `text`.
fn install(root: &Path, name: &str, text: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(FILE_NAME), text).unwrap();
    dir
}

/// Every bundled blueprint loads, holds together, and writes back as the
/// same file: what `to_toml` writes reads back equal to what was read.
#[test]
fn every_bundled_blueprint_validates_and_writes_back_stably() {
    let all = bundled();
    assert!(all.len() >= 11, "found {} bundled blueprints", all.len());
    for (name, path) in all {
        let loaded = validate(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(loaded.reference.name.as_str(), name);
        let file = BlueprintFile::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let written = file.to_toml().unwrap();
        let back = BlueprintFile::parse(&written).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(back, file, "{name}");
        // And writing that again changes nothing.
        assert_eq!(back.to_toml().unwrap(), written, "{name}");
    }
}

#[test]
fn unknown_keys_are_refused_with_their_line() {
    let err = BlueprintFile::parse(&TINY.replace("version", "verison")).unwrap_err();
    assert!(err.contains("unknown field `verison`"), "{err}");
    assert!(err.contains("line 3"), "{err}");
}

#[test]
fn the_graph_takes_its_title_and_description_from_the_blueprint() {
    let mut file = BlueprintFile::parse(TINY).unwrap();
    assert_eq!(file.run_graph().title.as_deref(), Some("tiny"));
    assert_eq!(file.run_graph().description, None);
    file.blueprint.description = Some("Small.".into());
    assert_eq!(file.run_graph().description.as_deref(), Some("Small."));
    file.graph.title = Some("Tiny run".into());
    file.graph.description = Some("Its own.".into());
    let graph = file.run_graph();
    assert_eq!(graph.title.as_deref(), Some("Tiny run"));
    assert_eq!(graph.description.as_deref(), Some("Its own."));
}

#[test]
fn a_graph_holding_a_json_null_cannot_be_written() {
    let mut file = BlueprintFile::parse(TINY).unwrap();
    file.graph.output = Some(OutputDef {
        schema: Some(JsonDoc::parse("null").unwrap()),
        ..OutputDef::default()
    });
    let err = file.to_toml().unwrap_err();
    assert!(err.contains("cannot be written as TOML"), "{err}");
}

#[test]
fn the_head_of_a_file_reads_without_its_graph() {
    let meta = BlueprintMeta::read("[blueprint]\nname = \"a\"\nversion = \"1\"\n[graph]\nx = 1\n")
        .unwrap();
    assert_eq!(meta.name.as_str(), "a");
    assert_eq!(meta.version, "1");
    let err = BlueprintMeta::read("[blueprint]\nname = \"a\"\n").unwrap_err();
    assert!(err.contains("missing field `version`"), "{err}");
}

#[test]
fn loading_pins_the_file_and_reads_beside_it() {
    let root = tempfile::tempdir().unwrap();
    let dir = install(root.path(), "tiny", TINY);
    let loaded = load(&dir).unwrap();
    assert_eq!(loaded.reference.name.as_str(), "tiny");
    assert_eq!(loaded.reference.digest, Some(Digest::of(TINY.as_bytes())));
    assert_eq!(loaded.version, "1.0.0");
    assert_eq!(loaded.base_dir, dir);
    assert_eq!(loaded.graph.title.as_deref(), Some("tiny"));
    assert_eq!(load(&dir.join(FILE_NAME)).unwrap(), loaded);
}

#[test]
fn loading_says_what_went_wrong() {
    let root = tempfile::tempdir().unwrap();
    let missing = load(&root.path().join("nope")).unwrap_err();
    assert!(matches_read(&missing), "{missing}");
    assert!(missing.to_string().starts_with("cannot read "), "{missing}");

    let bad = install(root.path(), "bad", "[blueprint]\nname = 1\n");
    let err = load(&bad).unwrap_err();
    assert!(
        err.to_string().contains("is not a valid blueprint"),
        "{err}"
    );

    let binary = root.path().join("binary");
    std::fs::create_dir_all(&binary).unwrap();
    std::fs::write(binary.join(FILE_NAME), [0xff, 0xfe]).unwrap();
    let err = load(&binary).unwrap_err();
    assert!(err.to_string().contains("utf-8"), "{err}");
}

fn matches_read(err: &BlueprintError) -> bool {
    std::error::Error::source(err).is_some()
}

#[test]
fn validating_checks_the_graph() {
    let root = tempfile::tempdir().unwrap();
    let good = install(root.path(), "tiny", TINY);
    assert_eq!(validate(&good).unwrap().reference.name.as_str(), "tiny");

    let dangling = TINY.replace(
        "[graph]\n",
        "[graph]\nedges = [{ name = \"on\", from = \"main\", to = \"nowhere\" }]\n",
    );
    let bad = install(root.path(), "dangling", &dangling);
    let err = validate(&bad).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("does not hold together"), "{text}");
    assert!(text.contains("nowhere"), "{text}");
    assert!(text.contains(FILE_NAME), "{text}");

    let unreadable = validate(&root.path().join("nope")).unwrap_err();
    assert!(
        unreadable.to_string().starts_with("cannot read"),
        "{unreadable}"
    );
}

#[test]
fn finding_an_installed_blueprint_checks_its_name_and_pin() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    install(&second, "tiny", TINY);
    install(&second, "renamed", TINY);
    install(&second, "broken", "not toml");
    std::fs::create_dir_all(second.join("empty")).unwrap();
    let dirs = vec![first.clone(), second.clone()];

    let tiny = BlueprintRef::parse("tiny").unwrap();
    let found = find(&dirs, &tiny).unwrap();
    assert_eq!(found.base_dir, second.join("tiny"));

    let pinned = BlueprintRef {
        digest: found.reference.digest.clone(),
        ..tiny.clone()
    };
    assert_eq!(find(&dirs, &pinned).unwrap(), found);

    let stale = BlueprintRef {
        digest: Some(Digest::of(b"another revision")),
        ..tiny
    };
    let issue = find(&dirs, &stale).unwrap_err();
    assert_eq!(issue.code, IssueCode::Changed);
    assert_eq!(issue.path.to_string(), "digest");

    let missing = find(&dirs, &BlueprintRef::parse("absent").unwrap()).unwrap_err();
    assert_eq!(missing.code, IssueCode::Unresolvable);
    assert_eq!(missing.known, ["broken", "renamed", "tiny"]);

    let renamed = find(&dirs, &BlueprintRef::parse("renamed").unwrap()).unwrap_err();
    assert_eq!(renamed.code, IssueCode::Invalid);
    assert_eq!(renamed.got.as_deref(), Some("tiny"));

    let broken = find(&dirs, &BlueprintRef::parse("broken").unwrap()).unwrap_err();
    assert_eq!(broken.code, IssueCode::Invalid);
    assert!(broken.hint.is_some());
}

#[test]
fn lint_findings_read_as_one_line_and_sort_worst_first() {
    use lint::{LintFinding, LintSeverity};
    let labels: Vec<&str> = [
        LintSeverity::Error,
        LintSeverity::Warning,
        LintSeverity::Note,
    ]
    .into_iter()
    .map(LintSeverity::label)
    .collect();
    assert_eq!(labels, ["ERR ", "WARN", "NOTE"]);
    assert!(LintSeverity::Error < LintSeverity::Note);

    let plain = LintFinding::new(LintSeverity::Warning, "x", "said".into());
    assert!(!plain.is_error());
    assert_eq!(plain.one_line(), "said");
    let placed = LintFinding::new(LintSeverity::Error, "y", "wrong".into())
        .in_stage("plan")
        .with_fix("fix it");
    assert!(placed.is_error());
    assert_eq!(placed.one_line(), "stage 'plan': wrong");
    assert_eq!(placed.fix.as_deref(), Some("fix it"));
    let json = toml::to_string(&placed).unwrap();
    assert!(json.contains("severity = \"error\""), "{json}");
}

#[test]
fn expanding_names_the_blueprint_and_carries_the_inputs() {
    let reference = BlueprintRef::parse("tiny").unwrap();
    let inputs = BTreeMap::from([("task".to_string(), RawInput::Text("fix it".into()))]);
    let req = expand(reference.clone(), inputs.clone());
    assert_eq!(req.source, SpawnSource::Blueprint(reference));
    assert_eq!(req.inputs, inputs);
    assert!(req.attachments.is_empty());
}
