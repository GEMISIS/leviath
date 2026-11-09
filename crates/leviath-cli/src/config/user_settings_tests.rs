//! One file read two ways: by `lev`, through [`Config`] and
//! [`Config::user_settings`], and by the runtime's `load_user_creds`, which
//! deserializes the same settings directly.
//!
//! Both build providers with the same code, so the only way they can differ
//! is the copy in `user_settings`: a top-level field read from the wrong
//! place, or a fill whose result is not carried back. One config that sets
//! every field the copy carries holds the two readings to one answer.

use super::*;
use crate::commands::run::session::{ProviderCreds, provider_creds_from_config};

/// Every keyed provider with a host and a header, Ollama, both sign-ins,
/// limits, overrides and routing preferences, in the file backend, plus one
/// `[model_providers]` endpoint. Some keys and hosts are left out of the file
/// so the environment fills them.
const FIXTURE: &str = r#"
default_provider = "openai"
override_model = "gpt-override"
fallback_model = "gpt-fallback"
request_timeout_secs = 77
taint_tracking = true

[model_capabilities.my-local]
max_output_tokens = 1234
retention = "none"

[rate_limits.anthropic]
requests_per_minute = 10
tokens_per_minute = 2000

[rate_limits.codex]
requests_per_minute = 3
tokens_per_minute = 300

[security]
credential_store = "file"

[providers]
anthropic_api_key = "sk-ant-file"
openai_api_key = " sk-openai-file "
google_api_key = ""
meshy_api_key = "msy-file"
bedrock_api_key = "bedrock-file"
meta_api_key = "meta-file"
anthropic_base_url = "https://anthropic.gateway"
meshy_base_url = "  "
anthropic_cache_ttl = "1h"
ollama_enabled = true
codex_enabled = true
codex_originator = "embedder"
codex_reasoning_effort = "high"
codex_verbosity = "low"
codex_replay_reasoning = false
grok_enabled = true
provider_order = ["codex", "openai", "anthropic"]
fallback_order = ["openrouter/meta/muse", "anthropic/claude-x"]
zero_retention = true
zero_retention_agreements = ["openai"]
file_uploads = false

[providers.anthropic_headers]
X-Gateway = "gw"

[providers.xai_headers]
X-Tenant = "t"

[model_providers.groq]
kind = "openai-compatible"
base_url = " https://groq.example/v1 "
api_key = " gsk "
models = ["llama-x"]
serves = ["llama-y"]
retention = "30d"
rate_limit = { requests_per_minute = 5, tokens_per_minute = 6 }
headers = { X-Org = "research" }

[model_providers.azure-east]
kind = "openai"
base_url = "https://east.openai.azure.com/openai/v1"
api_key = "east"
auth_header = "api-key"
serves = ["prod-gpt"]
models = ["prod-gpt-mini"]
retention = "zero"
zero_retention_request = "openai"

[model_providers.local]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"

# A script entry is no provider of the loader's, and its retention word counts.
[model_providers.scripted]
script = "scripted.rhai"
base_url = "https://scripted.example"
retention = "indefinite"
"#;

/// The variables the two loaders read, each set or cleared explicitly so the
/// machine running the suite cannot leak into either side.
fn environment(home: &std::path::Path) -> Vec<(String, Option<String>)> {
    let mut vars: Vec<(String, Option<String>)> = leviath_runtime::KEYED_PROVIDERS
        .iter()
        .flat_map(|k| std::iter::once(k.key_env).chain(k.base_url_env))
        .map(|var| (var.to_string(), None))
        .collect();
    vars.extend(
        [
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "OLLAMA_HOST",
            "LEVIATH_CONFIG_PATH",
        ]
        .map(|k| (k.to_string(), None)),
    );
    let mut overlay = |k: &str, v: &str| {
        vars.retain(|(name, _)| name != k);
        vars.push((k.to_string(), Some(v.to_string())));
    };
    overlay("LEVIATH_HOME", &home.display().to_string());
    // Filled from the environment: keys the file leaves unset...
    overlay("OPENROUTER_API_KEY", "sk-or-env");
    overlay("XAI_API_KEY", "xai-env");
    // ...a key the file set, which the file keeps...
    overlay("ANTHROPIC_API_KEY", "sk-ant-env");
    // ...a key the file left blank, which stays blank...
    overlay("GOOGLE_API_KEY", "google-env");
    // ...and hosts, region and Ollama's address.
    overlay("OPENAI_BASE_URL", "https://openai.gateway");
    overlay("ANTHROPIC_BASE_URL", "https://loses.to.file");
    overlay("XAI_BASE_URL", "https://xai.gateway");
    overlay("AWS_DEFAULT_REGION", " eu-central-1 ");
    overlay("OLLAMA_HOST", "http://ollama.box:11434");
    vars
}

/// `lev` and an embedding host register the same providers with the same
/// settings from one file, and hand a stage the same defaults.
#[test]
fn lev_and_an_embedding_host_read_one_file_the_same_way() {
    let home = tempfile::tempdir().unwrap();
    let data_dir = home.path().join(".leviath");
    std::fs::create_dir_all(&data_dir).unwrap();
    let path = data_dir.join("config.toml");
    std::fs::write(&path, FIXTURE).unwrap();

    temp_env::with_vars(environment(home.path()), || {
        let config = Config::load_from_path(&path).expect("the CLI loads the fixture");
        let env = |name: &str| std::env::var(name).ok();
        let source = leviath_runtime::UserCredsSource {
            data_dir: data_dir.clone(),
            config_path: None,
            env: Some(&env),
            keychain: None,
        };
        let loaded = leviath_runtime::load_user_creds(&source).expect("the runtime loads it");

        let names: Vec<&str> = loaded.creds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "anthropic",
                "openai",
                "openrouter",
                "meshy",
                "bedrock",
                "xai",
                "meta",
                "ollama",
                "azure-east",
                "groq",
                "local",
                "codex",
                "grok"
            ],
            "every kind of provider is exercised"
        );
        // Everything the CLI registers: a provider one side registers and
        // the other omits fails here.
        let cli: Vec<ProviderCreds> = provider_creds_from_config(&config);
        let cli_names: Vec<&str> = cli.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cli_names, names, "the same providers, in the same order");
        for (cli, runtime) in cli.iter().zip(&loaded.creds) {
            assert!(
                cli == runtime,
                "{} differs:\n{cli:?}\n{runtime:?}",
                cli.name
            );
        }

        assert_eq!(
            loaded.defaults,
            crate::daemon::spawn::model_defaults(&config)
        );
        assert_eq!(loaded.defaults.retention.provider_declarations.len(), 3);
        assert_eq!(loaded.defaults.retention.request_knob_aliases.len(), 1);
        assert_eq!(
            Some(loaded.grants_path.clone()),
            leviath_providers::oauth::ProviderAuthStore::default_path()
        );
        assert_eq!(
            Some(loaded.capability_cache.clone()),
            leviath_core::paths::capability_cache_path()
        );
        assert_eq!(loaded.credential_store, config.security.credential_store);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    });
}

/// A provider setting that is being ignored is said once, where the file is
/// loaded. The daemon asks for the retention settings every two seconds and
/// for the model defaults at every stage, so saying it where it is asked for
/// fills the log with one line.
#[test]
fn an_ignored_provider_setting_is_said_once_where_the_file_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[providers]\nfallback_order = [\"bare\"]\n\
         [model_providers.x]\nscript = \"x.rhai\"\nretention = \"sometimes\"\n\
         zero_retention_request = \"nobody\"\n",
    )
    .unwrap();

    let (config, at_load) = leviath_testkit::captured_logs(|| Config::read_file(&path));
    let config = config.expect("the file loads");
    let said = |lines: &[String], what: &str| {
        lines
            .iter()
            .filter(|line| line.starts_with("WARN ") && line.contains(what))
            .count()
    };
    assert_eq!(
        said(&at_load, "fallback_order entry \"bare\""),
        1,
        "{at_load:?}"
    );
    assert_eq!(
        said(&at_load, "[model_providers.x] retention"),
        1,
        "{at_load:?}"
    );
    assert_eq!(
        said(&at_load, "[model_providers.x] zero_retention_request"),
        1,
        "{at_load:?}"
    );

    // Asked for again, as the daemon asks, they say nothing.
    let ((), asked) = leviath_testkit::captured_logs(|| {
        for _ in 0..3 {
            crate::commands::run::session::retention_settings(&config);
            crate::daemon::spawn::model_defaults(&config);
            provider_creds_from_config(&config);
        }
    });
    assert_eq!(said(&asked, "ignoring"), 0, "{asked:?}");
}
