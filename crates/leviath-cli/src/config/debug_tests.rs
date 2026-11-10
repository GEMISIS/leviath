use super::super::Config;

/// A config holding a marker everywhere it can hold a credential: every
/// provider's key and extra headers, an MCP server's environment and
/// headers, a gateway's key, headers and forwarded extras, a tool
/// credential, and a key the environment supplied on the way in. Each
/// marker starts with `SECRET`, so one search finds any that got out.
fn config_with_a_marker_everywhere() -> Config {
    let mut text = String::from("openrouter_api_key = \"SECRET-key-openrouter\"\n[providers]\n");
    for keyed in leviath_runtime::KEYED_PROVIDERS {
        let provider = keyed.provider;
        // Anthropic's key is left for the environment to supply, below.
        if !matches!(provider, "openrouter" | "anthropic") {
            text.push_str(&format!("{provider}_api_key = \"SECRET-key-{provider}\"\n"));
        }
        text.push_str(&format!(
            "{provider}_headers = {{ X-Token = \"SECRET-hdr-{provider}\" }}\n"
        ));
    }
    text.push_str(
        r#"
[[mcp_servers]]
name = "github"
command = "gh-mcp"
env = { GITHUB_TOKEN = "SECRET-mcp-env" }
headers = { Authorization = "SECRET-mcp-hdr" }

[model_providers.gateway]
kind = "openai-compatible"
base_url = "http://localhost:9/v1"
api_key = "SECRET-gw-key"
headers = { X-Gateway = "SECRET-gw-hdr" }

[model_providers.scripted]
script = "scripted.rhai"
api_token = "SECRET-gw-extra"

[tool_credentials]
SEARCH = "SECRET-tool"
"#,
    );
    let mut config: Config = toml::from_str(&text).expect("the config parses");
    config
        .fill_from_env(&|name| (name == "ANTHROPIC_API_KEY").then(|| "SECRET-key-env".to_string()));
    assert_eq!(
        config.providers.anthropic_api_key.as_deref(),
        Some("SECRET-key-env")
    );
    config
}

/// One `tracing::debug!(?config)`, a `dbg!`, or an error context that
/// formats a struct holding a config must not put a credential in a log.
/// The type cannot leak, so nobody has to remember not to print it.
#[test]
fn debug_prints_no_secret_wherever_the_config_holds_one() {
    let config = config_with_a_marker_everywhere();
    // The markers are really there: the same config written out has them.
    let planted = toml::to_string(&config)
        .expect("serializes")
        .matches("SECRET-")
        .count();
    assert!(planted >= 20, "only {planted} markers were planted");

    for rendered in [format!("{config:?}"), format!("{config:#?}")] {
        assert!(!rendered.contains("SECRET"), "a secret leaked:\n{rendered}");
        // What is configured is still there to read: which keys are set,
        // and the names of everything that carries a value.
        assert!(
            rendered.contains("openrouter_api_key: \"<set>\""),
            "{rendered}"
        );
        assert!(rendered.contains("\"github\""), "{rendered}");
        assert!(rendered.contains("GITHUB_TOKEN"), "{rendered}");
        assert!(rendered.contains("Authorization"), "{rendered}");
        assert!(rendered.contains("SEARCH"), "{rendered}");
        assert!(rendered.contains("agent_paths: []"), "{rendered}");
    }

    let unset = format!("{:?}", Config::default());
    assert!(unset.contains("openrouter_api_key: \"<unset>\""), "{unset}");
}
