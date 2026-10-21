use super::*;
use crate::bundled::tests::{accepted_blueprints, schema_problems, toml_to_json};

/// A config naming the MCP servers `servers`.
fn config_with(servers: &[&str]) -> Config {
    Config {
        mcp_servers: servers
            .iter()
            .map(|name| leviath_mcp::MCPServerConfig {
                name: (*name).to_string(),
                command: Some("true".into()),
                ..Default::default()
            })
            .collect(),
        ..Config::default()
    }
}

/// The examples at a node of `schema`, as strings.
fn examples(node: &serde_json::Value) -> Vec<String> {
    node["examples"]
        .as_array()
        .expect("examples")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn tool_examples(schema: &serde_json::Value) -> Vec<String> {
    examples(&schema["$defs"]["ToolSelector"])
}

fn provider_examples(schema: &serde_json::Value) -> Vec<String> {
    examples(&schema["$defs"]["ModelRef"]["properties"]["provider"])
}

#[test]
fn the_schema_offers_this_machines_tools_and_providers() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    std::fs::create_dir_all(data.join("tools")).unwrap();
    std::fs::write(
        data.join("tools").join("summarize.rhai"),
        "// @tool summarize\n// @description sums\n2",
    )
    .unwrap();
    std::fs::create_dir_all(data.join("providers")).unwrap();
    std::fs::write(data.join("providers").join("groq.rhai"), "").unwrap();
    std::fs::write(data.join("providers").join("notes.txt"), "").unwrap();
    let mut config = config_with(&["github"]);
    config
        .model_providers
        .insert("azure".into(), Default::default());

    let listed = ["create_issue".to_string()];
    let schema = render(data, &config, Some(("github", &listed)));

    let tools = tool_examples(&schema);
    for name in ["@mcp", "read_file", "summarize", "github__create_issue"] {
        assert!(tools.contains(&name.to_string()), "{name}: {tools:?}");
    }
    let providers = provider_examples(&schema);
    for name in ["groq", "azure"] {
        assert!(
            providers.contains(&name.to_string()),
            "{name}: {providers:?}"
        );
    }
    assert!(!providers.contains(&"notes".to_string()), "{providers:?}");
    for kind in leviath_providers::compiled::COMPILED {
        assert!(providers.contains(&kind.to_string()), "{kind}");
    }
    assert_eq!(
        schema[MCP_KEY],
        serde_json::json!({ "github": ["create_issue"] })
    );
}

#[test]
fn listed_mcp_tools_outlive_a_rewrite_until_their_server_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let listed = ["create_issue".to_string()];
    let path = write(data, &config_with(&["github"]), Some(("github", &listed))).unwrap();
    assert_eq!(path, data.join(FILE_NAME));

    // A later write that reached no server keeps what the last one listed.
    write(data, &config_with(&["github", "jira"]), None).unwrap();
    let kept: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(tool_examples(&kept).contains(&"github__create_issue".to_string()));

    // Once the server is gone from the config, so are its tools.
    write(data, &config_with(&["jira"]), None).unwrap();
    let dropped: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(!tool_examples(&dropped).contains(&"github__create_issue".to_string()));
    assert_eq!(dropped[MCP_KEY], serde_json::json!({}));
}

#[test]
fn a_file_that_is_not_a_schema_remembers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FILE_NAME);
    assert!(remembered(&path).is_empty(), "no file");
    std::fs::write(&path, "not json").unwrap();
    assert!(remembered(&path).is_empty(), "not JSON");
    std::fs::write(&path, "{}").unwrap();
    assert!(remembered(&path).is_empty(), "no record");
}

#[test]
fn refresh_writes_skips_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::default();
    // No data directory: nothing is written anywhere.
    refresh(None, &config, None);
    let data = dir.path().join("home");
    refresh(Some(&data), &config, None);
    assert!(data.join(FILE_NAME).is_file());
    // A data directory that is a file: a warning, not a failure.
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, "").unwrap();
    refresh(Some(&blocked), &config, None);
    assert!(write(&blocked, &config, None).is_err());
}

#[test]
fn the_hinted_schema_is_never_stricter_than_the_loader() {
    // The completions are hints. Every blueprint `lev validate` accepts
    // passes the schema written for a machine, including one that names a
    // tool and a provider this machine has never heard of.
    let dir = tempfile::tempdir().unwrap();
    let listed = ["create_issue".to_string()];
    let schema = render(
        dir.path(),
        &config_with(&["github"]),
        Some(("github", &listed)),
    );
    let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
    let mut corpus = accepted_blueprints();
    let unknown = crate::test_support::tiny_blueprint("a").replace(
        "system_prompt = \"work\"",
        "system_prompt = \"work\", tools = [\"nowhere__tool\", \"not_a_tool\"], \
             tool_permissions = { not_a_tool = \"ask\" }, \
             model = { models = [{ provider = \"made-up\", model = \"m\" }] }",
    );
    assert!(
        leviath_blueprint::BlueprintFile::parse(&unknown).is_ok(),
        "{unknown}"
    );
    let parsed: toml::Value = toml::from_str(&unknown).unwrap();
    corpus.push(("unknown names".into(), toml_to_json(&parsed)));
    let mut failures = Vec::new();
    for (label, json) in &corpus {
        let problems = schema_problems(&validator, json);
        if !problems.is_empty() {
            failures.push(format!("{label}: {problems:?}"));
        }
    }
    assert_eq!(failures, Vec::<String>::new());
}
