//! Tests for the two inputs that name a blueprint and a region.
//!
//! The digest pin is the part worth testing hard: it exists to turn a silent
//! "acted on whatever was installed" into a refusal, so each of its three
//! answers - the pin holds, the pin is stale, nothing is installed - is checked
//! against a real agents directory.

use std::path::Path;

use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

use super::{BlueprintInput, RegionInput};
use crate::commands::serve::blueprints::TEST_AGENTS_DIR;
use crate::commands::serve::core::error::ServeError;
use crate::commands::serve::testutil::state_with_agent_paths;

/// A manifest that parses, under the given name.
fn manifest(name: &str) -> String {
    format!(
        r#"
[agent]
name = "{name}"
version = "1.0.0"
description = "for the digest pin"

[stages.plan]
mode = "autonomous"
model = {{ models = ["claude-sonnet-5"] }}
"#
    )
}

/// Install one blueprint under `root` and hand back its digest.
fn install(root: &Path, name: &str) -> String {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("the agent directory");
    let text = manifest(name);
    std::fs::write(dir.join(leviath_core::files::MANIFEST_FILENAME), &text)
        .expect("the manifest is written");
    crate::commands::serve::core::blueprints::digest_of(&text)
}

/// A pointer with no pin needs no agents directory at all.
#[tokio::test]
async fn a_pointer_without_a_pin_is_the_name_it_was_given() {
    let state = state_with_agent_paths(Vec::new());
    let input = BlueprintInput {
        name: "coder".to_string(),
        digest: None,
    };
    let name = input.installed(&state).await.expect("a bare name is fine");
    assert_eq!(name, "coder");
}

/// A pin that matches what is installed passes, and the name comes back.
#[tokio::test]
async fn a_pin_that_matches_the_installed_manifest_passes() {
    let agents = tempfile::tempdir().expect("a temp agents dir");
    let digest = install(agents.path(), "pinned");
    TEST_AGENTS_DIR
        .scope(agents.path().to_path_buf(), async move {
            let state = state_with_agent_paths(Vec::new());
            let input = BlueprintInput {
                name: "pinned".to_string(),
                digest: Some(digest),
            };
            let name = input.installed(&state).await.expect("the pin holds");
            assert_eq!(name, "pinned");
        })
        .await;
}

/// A pin against a different revision is loud rather than silently ignored.
#[tokio::test]
async fn a_stale_pin_is_a_conflict_naming_both_digests() {
    let agents = tempfile::tempdir().expect("a temp agents dir");
    let digest = install(agents.path(), "drifted");
    TEST_AGENTS_DIR
        .scope(agents.path().to_path_buf(), async move {
            let state = state_with_agent_paths(Vec::new());
            let stale = "0".repeat(64);
            let input = BlueprintInput {
                name: "drifted".to_string(),
                digest: Some(stale.clone()),
            };
            let error = input.installed(&state).await.expect_err("the pin is stale");
            let ServeError::Conflict(message) = &error else {
                panic!("drift is a conflict, not {error}");
            };
            assert!(
                message.contains(&digest),
                "it names what is there: {message}"
            );
            assert!(
                message.contains(&stale),
                "and what was asked for: {message}"
            );
        })
        .await;
}

/// A pin on a name nothing is installed under cannot be checked, so it fails.
#[tokio::test]
async fn a_pin_on_an_uninstalled_name_is_a_not_found() {
    let agents = tempfile::tempdir().expect("a temp agents dir");
    TEST_AGENTS_DIR
        .scope(agents.path().to_path_buf(), async move {
            let state = state_with_agent_paths(Vec::new());
            let input = BlueprintInput {
                name: "absent".to_string(),
                digest: Some("a".repeat(64)),
            };
            let error = input
                .installed(&state)
                .await
                .expect_err("there is nothing to check against");
            assert!(
                matches!(&error, ServeError::NotFound(message) if message.contains("absent")),
                "it names the blueprint: {error}"
            );
        })
        .await;
}

/// One object with a single field set.
fn one(field: &str, value: Value) -> Option<Value> {
    let mut map = IndexMap::new();
    map.insert(Name::new(field), value);
    Some(Value::Object(map))
}

/// Both inputs read back from their own value form.
///
/// An input type is written for one direction and generated for both: the schema
/// reads one off the wire, and the executor writes one back when it reports a bad
/// value. A type whose two halves disagree would report a rejected value as
/// something the caller did not send.
#[test]
fn both_inputs_round_trip_through_their_own_value_form() {
    let digest = "b".repeat(64);
    let blueprint = BlueprintInput {
        name: "coder".to_string(),
        digest: Some(digest.clone()),
    };
    let Ok(read_back) = BlueprintInput::parse(Some(blueprint.to_value())) else {
        panic!("a blueprint input reads back from its own value");
    };
    assert_eq!(read_back.name, "coder");
    assert_eq!(read_back.digest, Some(digest));

    let region = RegionInput {
        name: "plan".to_string(),
    };
    let Ok(read_back) = RegionInput::parse(Some(region.to_value())) else {
        panic!("a region input reads back from its own value");
    };
    assert_eq!(read_back.name, "plan");
}

/// Both inputs refuse what they cannot read, field by field.
///
/// Each field is read in turn, so an object whose *last* field is wrong takes a
/// path no test of the first one enters. `BlueprintInput` reads `name` and then
/// `digest`, so both positions are checked here.
#[test]
fn both_inputs_refuse_what_they_cannot_read() {
    let number = || Value::Number(7.into());
    let text = |s: &str| Value::String(s.to_string());
    let scalar = || Some(Value::String("nope".to_string()));

    assert!(BlueprintInput::parse(scalar()).is_err(), "not an object");
    assert!(
        BlueprintInput::parse(one("name", number())).is_err(),
        "a name is a string"
    );
    assert!(
        BlueprintInput::parse(one("digest", text("abc"))).is_err(),
        "a pin without a name points at nothing"
    );
    assert!(
        BlueprintInput::parse(None).is_err(),
        "no value at all is no pointer"
    );

    let mut with_bad_digest = IndexMap::new();
    with_bad_digest.insert(Name::new("name"), text("coder"));
    with_bad_digest.insert(Name::new("digest"), number());
    assert!(
        BlueprintInput::parse(Some(Value::Object(with_bad_digest))).is_err(),
        "a good first field does not excuse a bad second one"
    );

    assert!(RegionInput::parse(scalar()).is_err(), "not an object");
    assert!(RegionInput::parse(None).is_err(), "a region needs a name");
    assert!(
        RegionInput::parse(one("name", number())).is_err(),
        "a name is a string"
    );
}
