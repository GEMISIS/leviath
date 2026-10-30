//! The published schemas and the fingerprint. A sibling file, because the
//! rewrite branch only runs when a person asks for it.

/// The published schemas are the ones this build generates. `cargo xtask
/// schema` runs this with `LEVIATH_WRITE_SCHEMAS=1` to rewrite them after a
/// type changes.
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
            "{name} is out of date; run `cargo xtask schema` and commit the file"
        );
    }
}

/// The hash of the binary samples below as this build encodes them. When it
/// changes, the binary layout changed: bump `LAYOUT_VERSION`, then record
/// the new hash here.
const LAYOUT_HASH: &str = "0105a37c57a8db0851093fc1b1b3a4214bd308f641ed7e834dab0ba41cb924a1";

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
    spec.launch.write_paths = vec![vec!["**".to_string()]];
    let routing = crate::spec::graph::RoutingDef {
        retry_attempts: std::num::NonZeroU32::new(4),
        on_error: Some(crate::spec::graph::OnRoutingError::ErrorEdge),
        on_unmatched: None,
    };
    spec.graph.routing = Some(routing.clone());
    spec.graph.stages[0].routing = Some(routing);
    spec.delivery.idempotency = Some(crate::spec::launch::Idempotency {
        key: crate::spec::names::IdempotencyKey::new("pw:task-1").unwrap(),
        request: crate::spec::names::Digest::of(b"request"),
    });
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

/// The hash of [`structure`] over every frame type's schema, the frame kinds
/// and the layout version. When it changes, a frame type changed shape: the
/// message in [`the_frame_types_keep_their_structure`] says what to do.
const STRUCTURE_HASH: &str = "5b301882a754f7f2c44434dab6c4550abf3ad1934ea3d91b73e5182937d4339a";

/// Keywords that only document a schema: how the readable form reads and what
/// a field means. Postcard writes none of them.
const DOC_ONLY: &[&str] = &[
    "$comment",
    "$schema",
    "default",
    "deprecated",
    "description",
    "examples",
    "readOnly",
    "required",
    "title",
    "writeOnly",
];

/// Keywords whose value is kept as it is.
const KEPT: &[&str] = &[
    "$ref", "const", "enum", "format", "maxItems", "maximum", "minItems", "minimum", "pattern",
    "type",
];

/// A schema with what only documents it taken out.
///
/// The walk knows where a schema sits. Under `properties` and `$defs` the keys
/// are names, so a field called `description` or `default` stays; `const`,
/// `enum` and the other kept keywords are values and are never walked. A
/// keyword it does not know panics, so a schemars upgrade that starts writing
/// a new one is looked at rather than hashed blind.
fn structure(schema: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    let Value::Object(map) = schema else {
        return schema.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, value) in map {
        let kept = match key.as_str() {
            k if DOC_ONLY.contains(&k) => continue,
            k if KEPT.contains(&k) => value.clone(),
            "properties" | "$defs" => Value::Object(
                value
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(name, s)| (name.clone(), structure(s)))
                    .collect(),
            ),
            "items" | "additionalProperties" => structure(value),
            "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                Value::Array(value.as_array().unwrap().iter().map(structure).collect())
            }
            other => panic!("the run file schema uses `{other}`, which `structure` does not know"),
        };
        out.insert(key.clone(), kept);
    }
    Value::Object(out)
}

/// Every frame kind this build reads, by byte.
fn frame_kinds() -> String {
    (0..=u8::MAX)
        .filter_map(|b| super::codec::FrameKind::from_byte(b).map(|k| format!("{b} {k:?}")))
        .collect::<Vec<_>>()
        .join(",")
}

/// The hash of `schemas`' structure, the frame kinds and the layout version.
fn structure_hash(schemas: &serde_json::Value) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for (name, schema) in schemas.as_object().unwrap() {
        h.update(name.as_bytes());
        h.update(structure(schema).to_string().as_bytes());
    }
    h.update(frame_kinds().as_bytes());
    h.update(super::LAYOUT_VERSION.to_le_bytes());
    hex::encode(h.finalize())
}

/// A frame type's shape is held still: a new field, a removed variant, a
/// changed type or a new frame kind fails here. A doc comment does not, so
/// documenting a frame type never asks for a new fingerprint.
#[test]
fn the_frame_types_keep_their_structure() {
    let hash = structure_hash(&super::frame_schemas());
    assert!(
        hash == STRUCTURE_HASH,
        "the run file's frame types changed shape. If the_binary_layout_matches_its_version \
         fails too, or a frame kind was added or removed, the bytes changed: bump \
         runfile::LAYOUT_VERSION, give runfile::FINGERPRINT a new value, add an upgrade from \
         the old fingerprint to leviath-legacy-runs, and set STRUCTURE_HASH to the hash below. \
         Otherwise only the readable form changed: set STRUCTURE_HASH alone. This build's \
         hash: {hash}"
    );
}

/// The fingerprint names one binary layout. A new layout version comes with a
/// new fingerprint, and both are written here together.
#[test]
fn the_fingerprint_names_this_layout() {
    assert_eq!(
        (super::LAYOUT_VERSION, hex::encode(super::FINGERPRINT)),
        (
            8,
            "507deb68ad34875226943112da903bed80610e25cb45a64030092f67e3f606ff".to_string()
        ),
    );
}

/// Rewording every description, default and title a schema carries leaves its
/// structure's hash where it was.
#[test]
fn documentation_is_not_structure() {
    fn reword(v: &mut serde_json::Value) {
        use serde_json::Value;
        let Value::Object(map) = v else { return };
        for (key, value) in map.iter_mut() {
            match key.as_str() {
                "description" | "title" => *value = Value::String("reworded".into()),
                "default" => *value = Value::String("another".into()),
                "properties" | "$defs" => {
                    value.as_object_mut().unwrap().values_mut().for_each(reword)
                }
                "items" | "additionalProperties" => reword(value),
                "anyOf" | "oneOf" | "allOf" | "prefixItems" => {
                    value.as_array_mut().unwrap().iter_mut().for_each(reword)
                }
                _ => {}
            }
        }
    }
    let schemas = super::frame_schemas();
    let mut reworded = schemas.clone();
    reworded
        .as_object_mut()
        .unwrap()
        .values_mut()
        .for_each(reword);
    assert_ne!(reworded, schemas);
    assert_eq!(structure_hash(&reworded), structure_hash(&schemas));
}

/// A field named like a keyword is a field: taking away `InputDecl`'s
/// `description` field changes the structure.
#[test]
fn a_field_named_like_a_keyword_is_structure() {
    let schemas = super::frame_schemas();
    let mut without = schemas.clone();
    let removed = without["spec"]["$defs"]["InputDecl"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("description");
    assert!(removed.is_some());
    assert_ne!(structure_hash(&without), structure_hash(&schemas));
}

/// The frame kinds are part of the structure.
#[test]
fn the_frame_kinds_are_listed_by_byte() {
    assert_eq!(frame_kinds(), "1 Spec,2 Code,4 Delta,5 State,6 Owner");
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
