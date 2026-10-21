//! The published schema. A sibling file, because the rewrite branch only
//! runs when a person asks for it.

/// The published schema is the one this build generates. `cargo xtask
/// schema` runs this with `LEVIATH_WRITE_SCHEMAS=1` to rewrite it after a
/// type changes.
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
    // A Windows checkout may turn the file's newlines into CRLF.
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        on_disk == text,
        "blueprint.schema.json is out of date; run `cargo xtask schema` and commit the file"
    );
}

/// The hints land as completions on the nodes that already exist, sorted
/// and once each, and add no definition of their own.
#[test]
fn hints_become_examples_on_the_existing_nodes() {
    let hints = super::SchemaHints {
        tools: vec![
            "shell".into(),
            "github__create_issue".into(),
            "shell".into(),
        ],
        providers: vec!["openai".into(), "anthropic".into()],
    };
    let base = super::schema();
    let hinted = super::schema_with(&hints);
    let names = |s: &serde_json::Value| -> Vec<String> {
        s["$defs"].as_object().unwrap().keys().cloned().collect()
    };
    assert_eq!(names(&base), names(&hinted));
    let defs = &hinted["$defs"];
    assert_eq!(
        defs["ToolSelector"]["examples"],
        serde_json::json!([
            "@all",
            "@builtin",
            "@subagent",
            "@scripts",
            "@mcp",
            "github__create_issue",
            "shell"
        ])
    );
    assert_eq!(defs["ToolSelector"]["type"], "string");
    let provider = &defs["ModelRef"]["properties"]["provider"];
    assert_eq!(
        provider["examples"],
        serde_json::json!(["anthropic", "openai"])
    );
    assert_eq!(
        provider["type"],
        base["$defs"]["ModelRef"]["properties"]["provider"]["type"]
    );
    for owner in ["RunGraph", "StageDef"] {
        let permissions = &defs[owner]["properties"]["tool_permissions"];
        assert_eq!(
            permissions["additionalProperties"],
            serde_json::json!({ "$ref": "#/$defs/ToolPolicy" })
        );
        assert_eq!(
            permissions["properties"]["shell"],
            permissions["additionalProperties"]
        );
    }
}

/// With no hints, only the tool groups are added.
#[test]
fn no_hints_add_only_the_tool_groups() {
    let hinted = super::schema_with(&super::SchemaHints::default());
    let groups = hinted["$defs"]["ToolSelector"]["examples"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(groups, 5);
    assert_eq!(
        hinted["$defs"]["ModelRef"]["properties"]["provider"]["examples"],
        serde_json::json!([])
    );
}
