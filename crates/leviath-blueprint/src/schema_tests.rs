//! The published schema. A sibling file, because the rewrite branch only
//! runs when a person asks for it.

/// The published schema is the one this build generates. Run with
/// `LEVIATH_WRITE_SCHEMAS=1` to rewrite it after changing a type.
#[test]
fn the_published_blueprint_schema_matches_this_build() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/schema/blueprint.schema.json");
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&super::schema()).unwrap()
    );
    if std::env::var_os("LEVIATH_WRITE_SCHEMAS").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        on_disk == text,
        "blueprint.schema.json is out of date; rerun this test with LEVIATH_WRITE_SCHEMAS=1 and commit the file"
    );
}
