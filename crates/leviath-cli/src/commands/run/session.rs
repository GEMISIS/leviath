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

/// Build the list of [`ProviderCreds`] a [`Config`] implies. `ollama` is always
/// present (it needs no key); the API-key providers are included only when their
/// key is configured, and `claude-code` only when explicitly enabled. This is the
/// sole point that reads provider settings out of `Config`.
pub fn provider_creds_from_config(config: &Config) -> Vec<ProviderCreds> {
    let caps = &config.model_capabilities;
    let timeout = config.request_timeout_secs;
    let mut creds = Vec::new();

    let keyed = [
        ("anthropic", config.providers.anthropic_api_key.as_deref()),
        ("openai", config.providers.openai_api_key.as_deref()),
        ("google", config.providers.google_api_key.as_deref()),
        ("openrouter", config.openrouter_api_key.as_deref()),
    ];
    for (name, key) in keyed {
        // A blank key is not a key: `lev setup` writes empty strings for
        // providers the user skipped, and registering one produces a provider
        // that authenticates as nobody and fails at the first call.
        if let Some(key) = key.map(str::trim).filter(|k| !k.is_empty()) {
            // Generic base_url comes from [model_providers.<name>] - no per-provider fields.
            // Env fallback lets enterprise proxy work without a file: ANTHROPIC_BASE_URL etc.
            let base_url = config
                .model_providers
                .get(name)
                .and_then(|mp| mp.base_url.clone())
                .or_else(|| match name {
                    "anthropic" => std::env::var("ANTHROPIC_BASE_URL").ok(),
                    "openai" => std::env::var("OPENAI_BASE_URL").ok(),
                    "google" => std::env::var("GOOGLE_BASE_URL").ok(),
                    "openrouter" => std::env::var("OPENROUTER_BASE_URL").ok(),
                    _ => None,
                })
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty());
            creds.push(ProviderCreds {
                name: name.to_string(),
                api_key: Some(key.to_string()),
                base_url,
                model_capabilities: caps.clone(),
                request_timeout_secs: timeout,
                rate_limit: config.rate_limits.get(name).cloned(),
                options: std::collections::HashMap::new(),
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
    let registry = leviath_runtime::provider_creds::build_provider_registry_with(
        &provider_creds_from_config(config),
        build_client,
    )?;
    Ok(attach_script_layer(
        registry,
        crate::config::providers_dir(),
        config,
    ))
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
    let overrides = config
        .model_providers
        .iter()
        .map(|(name, mp)| (name.clone(), script_provider_spec(mp)))
        .collect();
    let layer = leviath_runtime::script_provider::ScriptProviderLayer::new(
        dir,
        overrides,
        config.model_capabilities.clone(),
        config.request_timeout_secs,
        config.security.allow_env_vars.clone(),
    );
    registry.with_script_layer(std::sync::Arc::new(layer))
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_provider_registry_with_empty_config() {
        let config = Config::default();
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
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

    #[test]
    fn build_provider_registry_with_anthropic_key() {
        let config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test-key-12345".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(registry.has("openrouter"));
    }

    #[test]
    fn build_provider_registry_custom_ollama_url() {
        let config = Config {
            ollama_base_url: Some("http://my-server:11434".to_string()),
            ..Config::default()
        };
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(registry.has("ollama"));
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                fallback_order: Vec::new(),
            },
            openrouter_api_key: Some("sk-or-test".to_string()),
            ollama_base_url: Some("http://custom:11434".to_string()),
            ..Config::default()
        };
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
        // Every key in the world doesn't enable Claude Code - only opting in does.
        assert!(!registry.has("claude-code"));
    }

    // ─── ProviderCreds seam ─────────────────────────────────────────────

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
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
        // Ollama is present regardless of key configuration; claude-code is not,
        // until the user opts in.
        assert!(registry.has("ollama"));
        assert!(!registry.has("claude-code"));
    }

    #[test]
    fn enabling_claude_code_registers_it_with_its_options() {
        let config = Config {
            providers: crate::config::ProviderConfig {
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
            },
        );
        let config = crate::config::Config {
            model_capabilities: caps,
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test".to_string()),
                openai_api_key: None,
                google_api_key: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                fallback_order: Vec::new(),
            },
            ..crate::config::Config::default()
        };
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
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
            },
        );
        let config = crate::config::Config {
            ollama_base_url: Some("http://custom-ollama:11434".to_string()),
            model_capabilities: caps,
            ..crate::config::Config::default()
        };
        let registry =
            build_provider_registry_from_config(&config).expect("an HTTPS client builds in tests");
        assert!(registry.has("ollama"));
    }

    // ─── resolve_task: None arg, non-TTY stdin ───────────────────────────
}
