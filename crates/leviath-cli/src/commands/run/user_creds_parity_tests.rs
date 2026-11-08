//! The runtime's `load_user_creds` against this crate's own loader, for one
//! config that reaches every provider both of them cover.
//!
//! The runtime copy of `provider_creds_from_config` and `model_defaults`
//! exists so an embedder can build the registry `lev` builds without the
//! CLI's `Config`. A copy drifts, so this holds the two to one answer: a
//! setting added here and not there fails this test rather than routing an
//! embedder's stages somewhere `lev` would not.

use super::*;

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

# The one documented exclusion: the runtime loader does not read these.
[model_providers.groq]
kind = "openai-compatible"
base_url = "https://groq.example/v1"
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

/// The two loaders register the same providers with the same settings, and
/// hand a stage the same defaults.
#[test]
fn the_runtime_loader_matches_the_cli_for_every_covered_provider() {
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
                "codex",
                "grok"
            ],
            "every covered provider is exercised"
        );
        // Everything the CLI registers, less only the `[model_providers]`
        // entries the runtime loader documents it does not read: a provider
        // the CLI registers and the runtime omits fails here.
        let cli: Vec<ProviderCreds> = provider_creds_from_config(&config)
            .into_iter()
            .filter(|c| !config.model_providers.contains_key(&c.name))
            .collect();
        assert!(
            provider_creds_from_config(&config)
                .iter()
                .any(|c| c.name == "groq"),
            "the exclusion is exercised"
        );
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
