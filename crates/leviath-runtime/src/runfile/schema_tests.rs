//! The published schemas and the fingerprint. A sibling file, because the
//! rewrite branch only runs when a person asks for it.

/// The published schemas are the ones this build generates. Run with
/// `LEVIATH_WRITE_SCHEMAS=1` to rewrite them after changing a type.
#[test]
fn the_published_schemas_match_this_build() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schema");
    let files = [
        ("spawn-request.schema.json", super::spawn_request_schema()),
        ("run-file.schema.json", super::frame_schemas()),
    ];
    for (name, schema) in files {
        let path = dir.join(name);
        let text = format!("{}\n", serde_json::to_string_pretty(&schema).unwrap());
        if std::env::var_os("LEVIATH_WRITE_SCHEMAS").is_some() {
            std::fs::write(&path, &text).unwrap();
        }
        // A Windows checkout may turn the file's newlines into CRLF.
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert!(
            on_disk == text,
            "{name} is out of date; rerun this test with LEVIATH_WRITE_SCHEMAS=1 and commit the file"
        );
    }
}

/// The hash of the binary samples below as this build encodes them. When it
/// changes, the binary layout changed: bump `LAYOUT_VERSION`, then record
/// the new hash here.
const LAYOUT_HASH: &str = "30639bbfa0ef40dbc6a26309e5f52895232fea45f76e4716617f0b1514e99401";

/// A fully populated sample of every frame type, so a change to any type's
/// binary encoding changes the bytes.
fn layout_samples() -> Vec<u8> {
    use crate::spec::graph::RunGraph;
    use crate::spec::inputs::{InputDecl, InputType, PathKind};
    use crate::spec::names::{ChoiceName, InputName, MimePattern};
    // A large real graph (the coder blueprint's, frozen here so later edits
    // to the bundled blueprint do not move the hash).
    let mut spec = crate::spec::run_spec::tests::spec();
    spec.listed = Some(crate::spec::run_spec::tests::listed());
    // Read with LF newlines whatever the checkout gave the file: its
    // multi-line strings keep a CRLF, and the bytes would move.
    let sample = include_str!("layout_sample.toml").replace("\r\n", "\n");
    spec.graph = toml::from_str::<RunGraph>(&sample).unwrap();
    let text = InputType::Text {
        multiline: true,
        min_len: Some(1),
        max_len: Some(9),
    };
    let types = [
        text.clone(),
        InputType::Bool,
        InputType::Int {
            min: Some(1),
            max: Some(5),
        },
        InputType::Float {
            min: Some(0.5),
            max: None,
        },
        InputType::Choice {
            options: vec![ChoiceName::new("a").unwrap()],
        },
        InputType::List {
            item: Box::new(text.clone()),
            min: Some(1),
            max: None,
        },
        InputType::Record { fields: vec![] },
        InputType::File {
            accepts: vec![MimePattern::new("image/*").unwrap()],
        },
        InputType::Path {
            kind: PathKind::Dir,
            must_exist: true,
        },
        InputType::Model,
        InputType::Blueprint,
        InputType::Duration,
        InputType::Url,
    ];
    for (i, ty) in types.into_iter().enumerate() {
        spec.graph.inputs.push(InputDecl {
            name: InputName::new(format!("in{i}")).unwrap(),
            ty,
            required: false,
            default: None,
            description: None,
            binds: vec![],
        });
    }
    let base = crate::state::tests::base();
    let mut busy = crate::state::tests::busy();
    busy.title_error = Some("no title".into());
    busy.read_paths = Some(crate::state::ReadPathCounts {
        declared: 2,
        granted: 1,
    });
    let delta = crate::state::StateDelta::between(
        &base,
        &busy,
        1,
        crate::state::journal::tests::every_event(),
    );
    let mut bytes = postcard::to_stdvec(&spec).unwrap();
    bytes.extend(postcard::to_stdvec(&busy).unwrap());
    bytes.extend(postcard::to_stdvec(&delta).unwrap());
    bytes
}

#[test]
fn the_binary_layout_matches_its_version() {
    use sha2::Digest as _;
    let hash = hex::encode(sha2::Sha256::digest(layout_samples()));
    assert_eq!(
        hash, LAYOUT_HASH,
        "the run file's binary layout changed: bump runfile::LAYOUT_VERSION and set LAYOUT_HASH to {hash}"
    );
}

#[test]
fn the_fingerprint_is_stable_and_covers_every_frame_type() {
    assert_eq!(super::fingerprint(), super::fingerprint());
    let text = super::frame_schemas().to_string();
    for name in [
        "RunSpec",
        "RunState",
        "StateDelta",
        "ContextState",
        "PipelinePhase",
    ] {
        assert!(text.contains(name), "{name} missing");
    }
}
