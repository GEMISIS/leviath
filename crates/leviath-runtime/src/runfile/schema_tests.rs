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
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == text,
            "{name} is out of date; rerun this test with LEVIATH_WRITE_SCHEMAS=1 and commit the file"
        );
    }
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
