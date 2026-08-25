//! Provider registry construction from the CLI's `Config`.
//!
//! Turning what the caller typed into a task lives in
//! [`super::task`](crate::commands::run::task).

use crate::config::Config;
use leviath_runtime::ProviderRegistry;

// `ProviderCreds` + `build_provider_registry(&[ProviderCreds])` live in
// `leviath-runtime` (plain data + provider instantiation, no `Config`
// dependency). Re-exported here so `commands::run`'s public re-export and all
// existing call sites keep resolving. The `Config`-based translators
// (`provider_creds_from_config` / `build_provider_registry_from_config`) stay
// below because they need the CLI's `Config`.
pub use leviath_runtime::provider_creds::{ProviderCreds, build_provider_registry};

/// The `options` spelling of a cache TTL, matching what the config accepts.
///
/// The map is `String -> String`, so the enum has to be named somehow; using
/// the same spelling the TOML uses keeps one vocabulary rather than two.
fn cache_ttl_key(ttl: leviath_providers::anthropic::CacheTtl) -> &'static str {
    match ttl {
        leviath_providers::anthropic::CacheTtl::Ephemeral5m => "5m",
        leviath_providers::anthropic::CacheTtl::Ephemeral1h => "1h",
    }
}

/// Build the list of [`ProviderCreds`] a [`Config`] implies. `ollama` is always
/// present (it needs no key); the API-key providers are included only when their
/// key is configured, and `claude-code` only when explicitly enabled. This is the
/// sole point that reads provider settings out of `Config`.
pub fn provider_creds_from_config(config: &Config) -> Vec<ProviderCreds> {
    let caps = &config.model_capabilities;
    let timeout = config.request_timeout_secs;
    let mut creds = Vec::new();

    // The third column is the host this provider is reached on, when it is not
    // the vendor's own. Per provider, because a gateway usually fronts one
    // family and pointing the others at it would break them.
    let keyed = [
        (
            "anthropic",
            config.providers.anthropic_api_key.as_deref(),
            config.providers.anthropic_base_url.as_deref(),
        ),
        (
            "openai",
            config.providers.openai_api_key.as_deref(),
            config.providers.openai_base_url.as_deref(),
        ),
        (
            "google",
            config.providers.google_api_key.as_deref(),
            config.providers.google_base_url.as_deref(),
        ),
        (
            "openrouter",
            config.openrouter_api_key.as_deref(),
            config.providers.openrouter_base_url.as_deref(),
        ),
    ];
    for (name, key, base_url) in keyed {
        // A blank key is not a key: `lev setup` writes empty strings for
        // providers the user skipped, and registering one produces a provider
        // that authenticates as nobody and fails at the first call.
        if let Some(key) = key.map(str::trim).filter(|k| !k.is_empty()) {
            // The options map rather than a named field, for the reason it
            // exists: one provider's settings should not accrete onto every
            // provider's struct.
            let mut options = std::collections::HashMap::new();
            if name == "anthropic"
                && let Some(ttl) = config.providers.anthropic_cache_ttl
            {
                options.insert("cache_ttl".to_string(), cache_ttl_key(ttl).to_string());
            }
            creds.push(ProviderCreds {
                name: name.to_string(),
                api_key: Some(key.to_string()),
                // Blank is not a URL, for the same reason blank is not a key:
                // `lev setup` writes empty strings for what the user skipped.
                base_url: base_url
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                    .map(str::to_string),
                model_capabilities: caps.clone(),
                request_timeout_secs: timeout,
                rate_limit: config.rate_limits.get(name).cloned(),
                options,
            });
        }
    }

    // Ollama is always available (no key); carry any configured base URL.
    creds.push(ProviderCreds {
        name: "ollama".to_string(),
        api_key: None,
        base_url: Some(
            config
                .ollama_base_url
                .as_deref()
                .unwrap_or("http://localhost:11434")
                .to_string(),
        ),
        model_capabilities: caps.clone(),
        request_timeout_secs: timeout,
        rate_limit: None,
        options: std::collections::HashMap::new(),
    });

    // Claude Code needs no API key, but it is opt-in rather than always-on: the
    // CLI puts the user's account email address into every call and that cannot
    // be turned off. Leaving it unregistered is also how it stays out of an
    // agent's model fallback chain - `resolve_stage_model` skips any provider
    // the registry doesn't have.
    if config.providers.claude_code_enabled {
        let mut options = std::collections::HashMap::new();
        if let Some(binary) = &config.providers.claude_code_binary {
            options.insert("binary".to_string(), binary.clone());
        }
        if let Some(effort) = &config.providers.claude_code_effort {
            options.insert("effort".to_string(), effort.clone());
        }
        creds.push(ProviderCreds {
            name: "claude-code".to_string(),
            api_key: None,
            base_url: None,
            model_capabilities: caps.clone(),
            request_timeout_secs: None,
            rate_limit: None,
            options,
        });
    }

    creds
}

/// Convenience wrapper: build a [`ProviderRegistry`] straight from a [`Config`].
///
/// Kept as a `fn(&Config) -> ProviderRegistry` so it can be passed as the
/// registry-builder seam that `run`/`models`/`dashboard` inject for tests.
///
/// Native providers are registered eagerly from [`provider_creds_from_config`];
/// a [`ScriptProviderLayer`](leviath_runtime::script_provider::ScriptProviderLayer)
/// is then attached so Rhai *script providers* resolve lazily and
/// hot-reload from `~/.leviath/providers/`.
pub fn build_provider_registry_from_config(
    config: &Config,
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    build_provider_registry_from_config_with(
        config,
        &leviath_providers::provider::build_http_client,
    )
}

/// [`build_provider_registry_from_config`], with client construction injected.
///
/// The seam that makes "this machine cannot build an HTTPS client" reachable
/// from a test: reqwest will not fail to build one in any environment a test can
/// arrange, so the failure has to be handed in.
pub fn build_provider_registry_from_config_with(
    config: &Config,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    build_provider_registry_from_config_probing(
        config,
        build_client,
        &leviath_runtime::provider_creds::tcp_reachable,
    )
}

/// [`build_provider_registry_from_config_with`], with the Ollama reachability
/// probe injected too.
///
/// Ollama registers on something answering at its address rather than on a
/// key, so a test that wants it registered has to say so: the address in a
/// test config resolves nowhere, and whether the machine running the suite
/// happens to have Ollama up is not something a test should depend on.
pub fn build_provider_registry_from_config_probing(
    config: &Config,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
    reachable: &dyn Fn(&str) -> bool,
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    let registry = leviath_runtime::provider_creds::build_provider_registry_probing(
        &provider_creds_from_config(config),
        build_client,
        reachable,
    )?;
    Ok(attach_script_layer(
        registry,
        crate::config::providers_dir(),
        config,
    ))
}

/// [`build_provider_registry_from_config_with`], with the script-provider
/// layer reading `reloader` on every load rather than a snapshot of `config`.
///
/// The daemon builds its registry this way so an edit to
/// `[model_providers.<name>]` reaches the next provider load with no restart,
/// matching the `.rhai` file's own hot-reload (issue #533). Short-lived
/// processes keep the snapshot: there is nothing to reload inside one command.
pub fn build_provider_registry_live(
    config: &Config,
    reloader: std::sync::Arc<crate::daemon::config_reload::ConfigReloader>,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    let registry = leviath_runtime::provider_creds::build_provider_registry_with(
        &provider_creds_from_config(config),
        build_client,
    )?;
    Ok(attach_live_script_layer(
        registry,
        crate::config::providers_dir(),
        config,
        reloader,
    ))
}

/// [`attach_script_layer`], with the layer reading `reloader` on every load.
/// Split out for the same reason: both the with-dir and no-home paths are then
/// unit-testable.
fn attach_live_script_layer(
    registry: ProviderRegistry,
    dir: Option<std::path::PathBuf>,
    config: &Config,
    reloader: std::sync::Arc<crate::daemon::config_reload::ConfigReloader>,
) -> ProviderRegistry {
    let Some(dir) = dir else {
        return registry;
    };
    let layer = leviath_runtime::script_provider::ScriptProviderLayer::with_config_source(
        dir,
        script_provider_config_source(reloader),
        leviath_runtime::script_provider::ScriptProviderLayer::build_executor(
            config.request_timeout_secs,
        ),
    );
    registry.with_script_layer(std::sync::Arc::new(layer))
}

/// Attach a [`ScriptProviderLayer`](leviath_runtime::script_provider::ScriptProviderLayer)
/// over `dir` (the providers directory) when one is available; otherwise return
/// the registry unchanged. Split out so both the with-dir and no-home paths are
/// unit-testable.
fn attach_script_layer(
    registry: ProviderRegistry,
    dir: Option<std::path::PathBuf>,
    config: &Config,
) -> ProviderRegistry {
    let Some(dir) = dir else {
        return registry;
    };
    let layer = leviath_runtime::script_provider::ScriptProviderLayer::new(
        dir,
        script_provider_config(config).overrides,
        config.model_capabilities.clone(),
        config.request_timeout_secs,
        config.security.allow_env_vars.clone(),
    );
    registry.with_script_layer(std::sync::Arc::new(layer))
}

/// The [`ScriptProviderConfig`](leviath_runtime::script_provider::ScriptProviderConfig)
/// a script-provider load reads out of `config`.
pub fn script_provider_config(
    config: &Config,
) -> leviath_runtime::script_provider::ScriptProviderConfig {
    leviath_runtime::script_provider::ScriptProviderConfig {
        overrides: config
            .model_providers
            .iter()
            .map(|(name, mp)| (name.clone(), script_provider_spec(mp)))
            .collect(),
        default_caps: config.model_capabilities.clone(),
        request_timeout_secs: config.request_timeout_secs,
        env_allowlist: std::sync::Arc::new(config.security.allow_env_vars.clone()),
    }
}

/// A script-provider config source that follows `reloader`, so an edit to
/// `[model_providers.<name>]` reaches the next provider load without a daemon
/// restart - the same way an edit to the `.rhai` file beside it already did
/// (issue #533).
///
/// Memoised on the identity of the config the reloader hands back, which is a
/// requirement rather than an optimisation: the layer's cache compares its
/// stored config by pointer, so deriving a fresh one per call would recompile
/// every script on every lookup.
pub fn script_provider_config_source(
    reloader: std::sync::Arc<crate::daemon::config_reload::ConfigReloader>,
) -> Box<
    dyn Fn() -> std::sync::Arc<leviath_runtime::script_provider::ScriptProviderConfig>
        + Send
        + Sync,
> {
    use std::sync::{Arc, Mutex, PoisonError};
    type Memo = Mutex<
        Option<(
            Arc<Config>,
            Arc<leviath_runtime::script_provider::ScriptProviderConfig>,
        )>,
    >;
    let memo: Memo = Mutex::new(None);
    Box::new(move || {
        let current = reloader.current();
        let mut memo = memo.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((from, derived)) = memo.as_ref()
            && Arc::ptr_eq(from, &current)
        {
            return derived.clone();
        }
        let derived = Arc::new(script_provider_config(&current));
        *memo = Some((current, derived.clone()));
        derived
    })
}

/// Translate a CLI [`ModelProviderConfig`](crate::config::ModelProviderConfig)
/// into the runtime's plain-data
/// [`ScriptProviderSpec`](leviath_runtime::script_provider::ScriptProviderSpec):
/// `base_url`/`api_key`/extra keys become the `initialize(config)` map.
fn script_provider_spec(
    mp: &crate::config::ModelProviderConfig,
) -> leviath_runtime::script_provider::ScriptProviderSpec {
    let mut cfg = serde_json::Map::new();
    if let Some(b) = &mp.base_url {
        cfg.insert("base_url".to_string(), serde_json::Value::String(b.clone()));
    }
    if let Some(k) = &mp.api_key {
        cfg.insert("api_key".to_string(), serde_json::Value::String(k.clone()));
    }
    for (k, v) in &mp.extra {
        cfg.insert(
            k.clone(),
            serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
        );
    }
    leviath_runtime::script_provider::ScriptProviderSpec {
        script: mp.script.clone(),
        rate_limit: mp.rate_limit.clone(),
        init_config: serde_json::Value::Object(cfg),
        serves: mp.serves.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::LimitsSource;

    #[test]
    fn build_provider_registry_with_empty_config() {
        let config = Config::default();
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        // Ollama needs no key and is always on.
        assert!(registry.has("ollama"));
        // Claude Code needs no key either, but is opt-in - a default config
        // must not reach the user's Claude subscription (or send their account
        // email to it) without them having said yes.
        assert!(!registry.has("claude-code"));
        // Should NOT have anthropic, openai, google without keys
        assert!(!registry.has("anthropic"));
        assert!(!registry.has("openai"));
        assert!(!registry.has("google"));
    }

    /// One gateway fronts one family, so the URL has to arrive on the provider
    /// it was set for and nowhere else.
    #[test]
    fn a_gateway_url_reaches_only_the_provider_it_was_set_for() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: Some("sk-openai-test".to_string()),
                anthropic_base_url: Some("https://gateway.internal/v1".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };

        let creds = provider_creds_from_config(&config);

        let of = |name: &str| {
            creds
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.base_url.clone())
        };
        assert_eq!(
            of("anthropic"),
            Some(Some("https://gateway.internal/v1".to_string()))
        );
        assert_eq!(
            of("openai"),
            Some(None),
            "a provider with no gateway keeps its vendor default"
        );
    }

    /// `lev setup` writes an empty string for what the user skipped, and an
    /// empty string is not a URL - registering one would point a provider at
    /// nothing. Same rule the API keys already follow.
    #[test]
    fn a_blank_gateway_url_is_not_a_gateway() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                anthropic_base_url: Some("   ".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };

        let creds = provider_creds_from_config(&config);

        assert_eq!(
            creds
                .iter()
                .find(|c| c.name == "anthropic")
                .map(|c| c.base_url.clone()),
            Some(None)
        );
    }

    #[test]
    fn build_provider_registry_with_anthropic_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test-key-12345".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        assert!(registry.has("anthropic"));
    }

    #[test]
    fn build_provider_registry_with_openai_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                openai_api_key: Some("sk-test-key-12345".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(registry.has("openai"));
    }

    #[test]
    fn build_provider_registry_with_google_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                google_api_key: Some("AIzatest12345".to_string()),
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(registry.has("google"));
    }

    #[test]
    fn build_provider_registry_with_openrouter_key() {
        let config = Config {
            openrouter_api_key: Some("sk-or-test-12345".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        assert!(registry.has("openrouter"));
    }

    #[test]
    fn build_provider_registry_custom_ollama_url() {
        let config = Config {
            ollama_base_url: Some("http://my-server:11434".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        assert!(registry.has("ollama"));
    }

    /// The memo is a requirement, not an optimisation: the layer compares its
    /// cached config by pointer, so a source that derived a fresh one per call
    /// would recompile every script on every lookup.
    #[test]
    fn the_script_config_source_is_stable_until_the_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let reloader = std::sync::Arc::new(crate::daemon::config_reload::ConfigReloader::new(
            path.clone(),
            Config::default(),
        ));
        let source = script_provider_config_source(reloader);

        let first = source();
        assert!(
            std::sync::Arc::ptr_eq(&first, &source()),
            "an unchanged config must hand back the very same value"
        );
        assert!(first.overrides.is_empty());

        let mut edited = Config::default();
        edited.model_providers.insert(
            "cerebras".to_string(),
            crate::config::ModelProviderConfig {
                base_url: Some("https://api.cerebras.ai/v1".to_string()),
                ..Default::default()
            },
        );
        edited.save_to_path_public(&path).unwrap();
        // Strictly newer, so the reload is observable even in the same tick.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();

        let after = source();
        assert!(
            !std::sync::Arc::ptr_eq(&first, &after),
            "a change is a new value"
        );
        assert_eq!(
            after.overrides["cerebras"].init_config["base_url"],
            "https://api.cerebras.ai/v1"
        );
    }

    #[test]
    fn script_provider_spec_assembles_init_config() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("region".to_string(), toml::Value::String("us".to_string()));
        let mp = crate::config::ModelProviderConfig {
            script: Some("groq".to_string()),
            api_key: Some("k".to_string()),
            base_url: Some("http://api".to_string()),
            rate_limit: Some(leviath_providers::RateLimitConfig {
                requests_per_minute: 30,
                tokens_per_minute: 1000,
            }),
            serves: Vec::new(),
            extra,
        };
        let spec = script_provider_spec(&mp);
        assert_eq!(spec.script.as_deref(), Some("groq"));
        assert!(spec.rate_limit.is_some());
        assert_eq!(spec.init_config["base_url"], "http://api");
        assert_eq!(spec.init_config["api_key"], "k");
        assert_eq!(spec.init_config["region"], "us");
    }

    #[test]
    fn attach_live_script_layer_without_home_is_a_noop() {
        let registry = attach_live_script_layer(
            ProviderRegistry::new(),
            None,
            &Config::default(),
            std::sync::Arc::new(crate::daemon::config_reload::ConfigReloader::fixed(
                Config::default(),
            )),
        );
        assert!(
            registry.resolvable_names().is_empty(),
            "no providers directory means no script layer to enumerate"
        );
    }

    #[test]
    fn attach_script_layer_without_home_is_a_noop() {
        // No providers directory (no resolvable home) → registry unchanged, no
        // script provider resolves.
        let registry = attach_script_layer(ProviderRegistry::new(), None, &Config::default());
        assert!(!registry.has("groq"));
    }

    #[test]
    fn build_registry_resolves_a_configured_script_provider() {
        let home = tempfile::tempdir().unwrap();
        let providers = home.path().join(".leviath").join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(
            providers.join("groq.rhai"),
            "fn initialize(config) { #{} }\nfn inference(state, request) { #{ content: \"ok\" } }",
        )
        .unwrap();

        let mut model_providers = std::collections::HashMap::new();
        model_providers.insert(
            "groq".to_string(),
            crate::config::ModelProviderConfig::default(),
        );
        let config = Config {
            model_providers,
            ..Config::default()
        };
        temp_env::with_var("LEVIATH_HOME", Some(home.path().as_os_str()), || {
            let registry = build_provider_registry_from_config(&config)
                .expect("an HTTPS client builds in tests");
            assert!(registry.has("groq"));
            assert!(registry.get("groq").is_some());
        });
    }

    // ─── build_provider_registry with all keys ──────────────────────────

    #[test]
    fn build_provider_registry_all_keys_set() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: Some("sk-test".to_string()),
                google_api_key: Some("AIza-test".to_string()),
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
            },
            openrouter_api_key: Some("sk-or-test".to_string()),
            ollama_base_url: Some("http://custom:11434".to_string()),
            ..Config::default()
        };
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
        // Every key in the world doesn't enable Claude Code - only opting in does.
        assert!(!registry.has("claude-code"));
    }

    // ─── ProviderCreds seam ─────────────────────────────────────────────

    /// The cache TTL reaches the provider through the creds, which is the whole
    /// path #345 was missing: the enum existed and nothing could select it.
    #[test]
    fn provider_creds_carry_the_anthropic_cache_ttl() {
        use leviath_providers::anthropic::CacheTtl;

        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("k".to_string());
        config.providers.openai_api_key = Some("k".to_string());
        config.providers.anthropic_cache_ttl = Some(CacheTtl::Ephemeral1h);

        let creds = provider_creds_from_config(&config);
        let anthropic = creds
            .iter()
            .find(|c| c.name == "anthropic")
            .expect("anthropic is registered");
        assert_eq!(
            anthropic.options.get("cache_ttl").map(String::as_str),
            Some("1h")
        );

        // Only Anthropic's, since only Anthropic reads it.
        let openai = creds.iter().find(|c| c.name == "openai").expect("openai");
        assert!(!openai.options.contains_key("cache_ttl"));
    }

    #[test]
    fn the_five_minute_ttl_is_carried_explicitly_too() {
        use leviath_providers::anthropic::CacheTtl;

        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("k".to_string());
        config.providers.anthropic_cache_ttl = Some(CacheTtl::Ephemeral5m);
        let creds = provider_creds_from_config(&config);
        assert_eq!(
            creds[0].options.get("cache_ttl").map(String::as_str),
            Some("5m")
        );
    }

    /// Unset means unset: no entry, so the provider keeps its own default.
    #[test]
    fn no_configured_ttl_carries_nothing() {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("k".to_string());
        let creds = provider_creds_from_config(&config);
        assert!(!creds[0].options.contains_key("cache_ttl"));
    }

    #[test]
    fn provider_creds_from_config_includes_defaults_and_keyed() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant".to_string()),
                ..Config::default().providers
            },
            ollama_base_url: Some("http://custom:11434".to_string()),
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let names: Vec<&str> = creds.iter().map(|c| c.name.as_str()).collect();
        // anthropic (keyed) + ollama, but not openai/google/openrouter, and not
        // claude-code (opt-in, not enabled here).
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"ollama"));
        assert!(!names.contains(&"claude-code"));
        assert!(!names.contains(&"openai"));
        assert!(!names.contains(&"google"));
        assert!(!names.contains(&"openrouter"));
        // The ollama base URL is carried through.
        let ollama = creds.iter().find(|c| c.name == "ollama").unwrap();
        assert_eq!(ollama.base_url.as_deref(), Some("http://custom:11434"));
        assert!(ollama.api_key.is_none());
    }

    /// `lev setup` writes an empty string for a provider the user skipped, so
    /// a blank key must not register one: doing so produced a provider that
    /// authenticates as nobody and fails at the first call, and it crowded out
    /// the provider the user actually configured.
    #[test]
    fn provider_creds_from_config_ignores_blank_keys() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some(String::new()),
                openai_api_key: Some("   ".to_string()),
                google_api_key: Some("AIza-real".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let names: Vec<&str> = creds.iter().map(|c| c.name.as_str()).collect();
        assert!(
            names.contains(&"google"),
            "the configured provider must register: {names:?}"
        );
        assert!(!names.contains(&"anthropic"), "empty key must not register");
        assert!(
            !names.contains(&"openai"),
            "whitespace-only key must not register"
        );
    }

    #[test]
    fn provider_creds_from_config_carries_rate_limits() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant".to_string()),
                openai_api_key: Some("sk-oa".to_string()),
                ..Config::default().providers
            },
            rate_limits: std::collections::HashMap::from([(
                "anthropic".to_string(),
                leviath_providers::RateLimitConfig {
                    requests_per_minute: 50,
                    tokens_per_minute: 40_000,
                },
            )]),
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let anthropic = creds.iter().find(|c| c.name == "anthropic").unwrap();
        assert_eq!(
            anthropic.rate_limit.as_ref().map(|r| r.requests_per_minute),
            Some(50)
        );
        // A provider without a [rate_limits.<name>] entry stays unthrottled.
        let openai = creds.iter().find(|c| c.name == "openai").unwrap();
        assert!(openai.rate_limit.is_none());
    }

    // ─── resolve_task: multiline file content ───────────────────────────

    #[test]
    fn build_provider_registry_defaults_have_ollama_only() {
        let config = Config::default();
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        // Ollama is present regardless of key configuration; claude-code is not,
        // until the user opts in.
        assert!(registry.has("ollama"));
        assert!(!registry.has("claude-code"));
    }

    #[test]
    fn enabling_claude_code_registers_it_with_its_options() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: true,
                claude_code_binary: Some("/opt/bin/claude".to_string()),
                claude_code_effort: Some("low".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let cc = creds
            .iter()
            .find(|c| c.name == "claude-code")
            .expect("enabled ⇒ present");
        assert_eq!(
            cc.options.get("binary").map(String::as_str),
            Some("/opt/bin/claude")
        );
        assert_eq!(cc.options.get("effort").map(String::as_str), Some("low"));
        assert!(cc.api_key.is_none());
        assert!(
            build_provider_registry_from_config(&config)
                .expect("an HTTPS client builds in tests")
                .has("claude-code")
        );
    }

    #[test]
    fn enabling_claude_code_without_options_carries_none() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: true,
                ..Config::default().providers
            },
            ..Config::default()
        };
        let creds = provider_creds_from_config(&config);
        let cc = creds.iter().find(|c| c.name == "claude-code").unwrap();
        // Absent settings stay absent so the provider applies its own defaults
        // (the `claude` binary on PATH, DEFAULT_EFFORT).
        assert!(cc.options.is_empty());
    }

    // ─── resolve_task: file with only comments in editor-like format ────

    #[test]
    fn build_provider_registry_propagates_model_capabilities() {
        use leviath_providers::ModelCapabilities;
        let mut caps = std::collections::HashMap::new();
        caps.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 9999,
                max_output_tokens: 999,
                limits_source: LimitsSource::Builtin,
            }
            .into(),
        );
        let config = crate::config::Config {
            model_capabilities: caps,
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: None,
                google_api_key: None,
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
            },
            ..crate::config::Config::default()
        };
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        // Verify anthropic provider was registered
        assert!(registry.has("anthropic"));
        // Verify ollama always registered
        assert!(registry.has("ollama"));
    }

    // ─── launch_editor: candidates exhausted when no editors available ────

    #[test]
    fn build_provider_registry_ollama_with_custom_url_propagates_caps() {
        use leviath_providers::ModelCapabilities;
        let mut caps = std::collections::HashMap::new();
        caps.insert(
            "llama3-8b".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 99,
                max_output_tokens: 99,
                limits_source: LimitsSource::Builtin,
            }
            .into(),
        );
        let config = crate::config::Config {
            ollama_base_url: Some("http://custom-ollama:11434".to_string()),
            model_capabilities: caps,
            ..crate::config::Config::default()
        };
        let registry = build_provider_registry_from_config_probing(
            &config,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        assert!(registry.has("ollama"));
    }

    // ─── resolve_task: None arg, non-TTY stdin ───────────────────────────
}
