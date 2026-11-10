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

/// Run `f` with the process environment a load reads provider settings from
/// holding exactly `vars`.
fn with_provider_env<R>(vars: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
    let mut all: Vec<(String, Option<String>)> = leviath_runtime::user_creds::env_names()
        .into_iter()
        .map(|name| (name.to_string(), None))
        .collect();
    for (name, value) in vars {
        all.retain(|(n, _)| n != name);
        all.push((name.to_string(), Some(value.to_string())));
    }
    temp_env::with_vars(all, f)
}

/// A store a test keeps a handle on after handing it to a load or a save.
struct Kept(std::sync::Arc<leviath_core::MemoryStore>);
impl leviath_core::CredentialStore for Kept {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        self.0.get(account)
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        self.0.set(account, secret)
    }
    fn delete(&self, account: &str) -> Result<bool, String> {
        self.0.delete(account)
    }
}

/// Every command that edits the config loads it, changes one thing and saves
/// it. The load fills what the file leaves unset from the environment, and a
/// save that wrote the struct as it stood would pin a key the user keeps in
/// their environment, a gateway address and a region into `config.toml`.
#[test]
fn a_save_does_not_write_what_the_environment_supplied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[providers]\nopenai_api_key = \"file-key\"\n").unwrap();
    let from_env = [
        ("ANTHROPIC_API_KEY", "sk-ant-env"),
        ("OPENROUTER_API_KEY", "sk-or-env"),
        ("OPENAI_BASE_URL", "http://gateway.example"),
        ("AWS_REGION", "eu-west-1"),
        ("OLLAMA_HOST", "http://ollama.box:11434"),
    ];
    with_provider_env(&from_env, || {
        let mut config = Config::load_from_path(&path).expect("loads");
        // What a writer does: one unrelated change.
        config.update_check = !config.update_check;
        config.save_to_path(&path).expect("saves");

        let text = std::fs::read_to_string(&path).unwrap();
        for (name, value) in from_env {
            assert!(!text.contains(value), "{name} was written:\n{text}");
        }
        assert!(text.contains("file-key"), "{text}");
        // The config in hand is the one a run is using: it keeps them.
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-env")
        );
        assert_eq!(
            config.providers.openai_base_url.as_deref(),
            Some("http://gateway.example")
        );

        // And what was written reads back to the same settings.
        let again = Config::load_from_path(&path).expect("loads again");
        assert!(
            provider_creds_from_config(&again) == provider_creds_from_config(&config),
            "the saved file builds different providers"
        );
    });
}

/// A value set after the load is the user's, wherever the environment had
/// one of its own.
#[test]
fn a_value_set_after_the_load_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let env = |name: &str| match name {
        "ANTHROPIC_API_KEY" => Some("sk-ant-env".to_string()),
        "OLLAMA_HOST" => Some("http://ollama.box:11434".to_string()),
        "GOOGLE_BASE_URL" => Some("http://google.gateway".to_string()),
        "AWS_DEFAULT_REGION" => Some("eu-west-1".to_string()),
        _ => None,
    };
    let mut config = Config::default();
    config.fill_from_env(&env);
    config.providers.anthropic_api_key = Some("sk-ant-typed".to_string());
    config.ollama_base_url = Some("http://typed:11434".to_string());
    config.providers.google_base_url = Some("http://typed.gateway".to_string());
    config.providers.bedrock_region = Some("ap-south-1".to_string());
    config.write_to(&path, Ok(None)).expect("saves");

    let text = std::fs::read_to_string(&path).unwrap();
    for typed in [
        "sk-ant-typed",
        "http://typed:11434",
        "http://typed.gateway",
        "ap-south-1",
    ] {
        assert!(text.contains(typed), "{typed} was dropped:\n{text}");
    }
    assert!(!text.contains("sk-ant-env"), "{text}");
}

/// In keychain mode a save copies every key it holds into the OS store. A
/// key that came from the environment is not one to keep there, and one the
/// store supplied is: it reaches the file when the store is given up.
#[test]
fn a_keychain_save_stores_no_key_the_environment_supplied() {
    use leviath_core::CredentialStore;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let store = std::sync::Arc::new(leviath_core::MemoryStore::new());
    store.set("provider/openai", "sk-openai-kept").unwrap();
    let kept = || {
        Ok(Some(
            Box::new(Kept(store.clone())) as Box<dyn CredentialStore>
        ))
    };

    let mut config = Config::default();
    config.security.credential_store = leviath_core::CredentialStoreKind::Keychain;
    config.fill_from_env(&|name| (name == "ANTHROPIC_API_KEY").then(|| "sk-ant-env".to_string()));
    config.fill_from_credential_store_with(kept());
    assert_eq!(
        config.providers.openai_api_key.as_deref(),
        Some("sk-openai-kept")
    );
    config.write_to(&path, kept()).expect("saves");
    assert_eq!(store.get("provider/anthropic").unwrap(), None);
    assert_eq!(
        store.get("provider/openai").unwrap().as_deref(),
        Some("sk-openai-kept")
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("sk-ant-env") && !text.contains("sk-openai-kept"),
        "{text}"
    );

    // Giving the store up, as `lev auth migrate --to-file` does: what it
    // supplied is written, what the environment supplied still is not.
    config.security.credential_store = leviath_core::CredentialStoreKind::File;
    config.write_to(&path, Ok(None)).expect("saves");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("sk-openai-kept"), "{text}");
    assert!(!text.contains("sk-ant-env"), "{text}");
}

/// A key variable that is set but empty fills its slot, so the provider is
/// not registered and the credential store is not asked for it. A pipeline
/// that exports every secret it might have does this on every command, so
/// it is noted at debug level and never as a warning.
#[test]
fn an_empty_key_variable_is_noted_at_debug_level() {
    let (config, lines) = leviath_testkit::captured_logs(|| {
        let mut config = Config::default();
        config.fill_from_env(&|name| (name == "OPENAI_API_KEY").then(String::new));
        config
    });
    assert_eq!(config.providers.openai_api_key.as_deref(), Some(""));
    assert!(provider_creds_from_config(&config).is_empty());
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("DEBUG ")
                && line.contains("OPENAI_API_KEY is set but empty")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("WARN ")),
        "{lines:?}"
    );
    // Nothing of it is kept: an empty variable is the environment's too.
    assert_eq!(config.as_stored().providers.openai_api_key, None);
}
