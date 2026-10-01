use super::*;

#[test]
fn a_migrated_file_leaves_defaults_out_and_writes_short_tables_inline() {
    let old = r#"
[agent]
name = "small"
version = "0.3.0"

[context.regions]
task = { type = "pinned", max_tokens = 500, seed = "task" }

[stages.main]
system_prompt = "work"
available_tools = ["read_file", "list_dir", "bash", "write_file", "edit_file", "context_read"]
"#;
    let text = migrate(old).unwrap();
    assert!(text.starts_with("[blueprint]\nname = \"small\"\nversion = \"0.3.0\"\n"));
    assert!(!text.contains("description"), "{text}");
    assert!(!text.contains("title"), "{text}");
    assert!(!text.contains("= false"), "{text}");
    assert!(!text.contains("= []"), "{text}");
    assert!(text.contains("budget = 500 }"), "{text}");
    assert!(text.contains("tools = [\n    \"read_file\",\n"), "{text}");
    assert!(text.contains("binds = [{ region = \"task\" }]"), "{text}");
    assert!(
        text.contains("type = { kind = \"text\", multiline = true }"),
        "{text}"
    );
    let file = BlueprintFile::parse(&text).unwrap();
    assert_eq!(file.run_graph().title.as_deref(), Some("small"));
    // Reading the written file gives the graph the manifest describes.
    let expected = RunGraph::from_blueprint(&parse_manifest(old).unwrap()).unwrap();
    assert_eq!(file.run_graph(), expected);
}

#[test]
fn a_manifest_that_does_not_parse_is_reported() {
    let problems = migrate("[stages.main]\n").unwrap_err();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("[agent]"), "{problems:?}");
}

#[test]
fn every_problem_with_a_manifest_is_reported_at_once() {
    let old = r#"
[agent]
name = " padded"

[stages.main]
system_prompt = "work"
available_tools = ["read file"]
"#;
    let problems = migrate_file(old).unwrap_err();
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].contains("tool name"), "{problems:?}");
    assert!(problems[1].contains("[agent] name"), "{problems:?}");
}
