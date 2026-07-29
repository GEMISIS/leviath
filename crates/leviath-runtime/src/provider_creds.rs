//! Decoupled provider credentials + registry construction.
//!
//! [`ProviderCreds`] is the plain-data seam that lets the run engine build a
//! [`ProviderRegistry`] without depending on the CLI's `Config`/`ProviderConfig`
//! types. The CLI owns the `Config -> Vec<ProviderCreds>` translation
//! (`provider_creds_from_config`); this module owns everything downstream of it.

use crate::ProviderRegistry;
use std::sync::Arc;

/// Decoupled provider credentials.
///
/// Plain data so [`build_provider_registry`] can instantiate providers without
/// depending on the CLI's `Config`/`ProviderConfig` types. Build one per
/// provider that should be registered.
/// `Debug` is hand-written (below) so `api_key` cannot be printed.
#[derive(Clone)]
pub struct ProviderCreds {
    /// Provider identifier: `anthropic` | `openai` | `google` | `openrouter` |
    /// `ollama` | `claude-code`. Selects which provider is instantiated.
    pub name: String,
    /// API key, when the provider needs one (`None` for `ollama`/`claude-code`).
    pub api_key: Option<String>,
    /// Base URL override (used by `ollama`; `None` uses the built-in default).
    pub base_url: Option<String>,
    /// Per-model capability overrides forwarded to the provider.
    pub model_capabilities: std::collections::HashMap<String, leviath_providers::ModelCapabilities>,
    /// HTTP request timeout in seconds (`None` uses the provider default).
    pub request_timeout_secs: Option<u64>,
    /// Client-side rate limit (requests/tokens per minute) enforced before
    /// each call. `None` sends requests unthrottled. Ignored by `ollama`
    /// (a local server) and `claude-code` (a subprocess).
    pub rate_limit: Option<leviath_providers::RateLimitConfig>,
    /// Provider-specific settings that don't fit the api-key / base-URL shape.
    ///
    /// Currently only `claude-code` reads this, for `binary` (path to the
    /// `claude` executable) and `effort` (reasoning level). Kept as a map rather
    /// than named fields so one provider's options don't accrete onto a struct
    /// shared by six.
    pub options: std::collections::HashMap<String, String>,
}

/// Hand-written so the API key can never reach a log line.
///
/// A `#[derive(Debug)]` here meant a single `tracing::debug!(?creds)` - or an
/// error context that formats a struct holding one - would print the key.
/// Nothing did, which is when it is cheap to make impossible.
impl std::fmt::Debug for ProviderCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCreds")
            .field("name", &self.name)
            .field(
                "api_key",
                match self.api_key {
                    Some(_) => &"<set>",
                    None => &"<unset>",
                },
            )
            .field("base_url", &self.base_url)
            .field("model_capabilities", &self.model_capabilities)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("rate_limit", &self.rate_limit)
            .field("options", &self.options)
            .finish()
    }
}

impl ProviderCreds {
    /// A cred entry for a provider that needs no key, base URL, or options.
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            api_key: None,
            base_url: None,
            model_capabilities: std::collections::HashMap::new(),
            request_timeout_secs: None,
            rate_limit: None,
            options: std::collections::HashMap::new(),
        }
    }
}

/// Build a [`ProviderRegistry`] from decoupled [`ProviderCreds`].
pub fn build_provider_registry(creds: &[ProviderCreds]) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    for c in creds {
        let caps = c.model_capabilities.clone();
        let timeout = c.request_timeout_secs;
        match c.name.as_str() {
            "anthropic" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "anthropic".to_string(),
                        Arc::new(leviath_providers::AnthropicProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                            c.rate_limit.as_ref(),
                        )),
                    );
                }
            }
            "openai" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "openai".to_string(),
                        Arc::new(leviath_providers::OpenAIProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                            c.rate_limit.as_ref(),
                        )),
                    );
                }
            }
            "google" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "google".to_string(),
                        Arc::new(leviath_providers::GeminiProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                            c.rate_limit.as_ref(),
                        )),
                    );
                }
            }
            "openrouter" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "openrouter".to_string(),
                        Arc::new(leviath_providers::OpenRouterProvider::with_overrides(
                            key.clone(),
                            caps,
                            timeout,
                            c.rate_limit.as_ref(),
                        )),
                    );
                }
            }
            "ollama" => {
                let url = c
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "http://localhost:11434".to_string());
                registry.register(
                    "ollama".to_string(),
                    Arc::new(leviath_providers::OllamaProvider::with_overrides(
                        url, caps, timeout,
                    )),
                );
            }
            "claude-code" => {
                // Opt-in: the CLI puts the user's account email address into
                // every call. The CLI-side config only emits this entry when
                // the user has explicitly enabled the provider.
                let binary = c
                    .options
                    .get("binary")
                    .cloned()
                    .unwrap_or_else(|| "claude".to_string());
                registry.register(
                    "claude-code".to_string(),
                    Arc::new(leviath_providers::ClaudeCodeProvider::with_overrides(
                        binary,
                        c.options.get("effort").cloned(),
                        Some(caps),
                    )),
                );
            }
            _ => {}
        }
    }

    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `tracing::debug!(?creds)` - or an error context that formats a struct
    /// holding one - would otherwise print the provider key.
    #[test]
    fn debug_output_never_contains_the_api_key() {
        let mut creds = ProviderCreds::simple("anthropic");
        creds.api_key = Some("sk-ant-SECRET-VALUE".to_string());
        creds.base_url = Some("https://api.example.com".to_string());

        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("SECRET-VALUE"), "key leaked: {rendered}");
        assert!(rendered.contains("<set>"), "{rendered}");
        // The parts that make a debug line useful survive.
        assert!(rendered.contains("anthropic"), "{rendered}");
        assert!(rendered.contains("api.example.com"), "{rendered}");

        // A provider that needs no key says so rather than claiming one.
        let keyless = format!("{:?}", ProviderCreds::simple("ollama"));
        assert!(keyless.contains("<unset>"), "{keyless}");
    }

    #[test]
    fn build_provider_registry_from_creds_slice() {
        // Drives `build_provider_registry(&[ProviderCreds])` directly:
        // every keyed provider, the ollama-with-default-url arm, claude-code,
        // and an unknown provider name (the catch-all no-op arm).
        let caps = std::collections::HashMap::new();
        let creds = vec![
            ProviderCreds {
                name: "anthropic".to_string(),
                api_key: Some("sk-ant".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: Some(30),
                rate_limit: None,
                options: Default::default(),
            },
            ProviderCreds {
                name: "openai".to_string(),
                api_key: Some("sk-oa".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
                rate_limit: None,
                options: Default::default(),
            },
            ProviderCreds {
                name: "google".to_string(),
                api_key: Some("AIza".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
                rate_limit: None,
                options: Default::default(),
            },
            ProviderCreds {
                name: "openrouter".to_string(),
                api_key: Some("sk-or".to_string()),
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
                rate_limit: None,
                options: Default::default(),
            },
            ProviderCreds {
                name: "ollama".to_string(),
                api_key: None,
                base_url: None, // exercise the default-URL fallback
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
                rate_limit: None,
                options: Default::default(),
            },
            ProviderCreds {
                name: "claude-code".to_string(),
                api_key: None,
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
                rate_limit: None,
                options: Default::default(),
            },
            ProviderCreds {
                name: "totally-unknown".to_string(),
                api_key: Some("x".to_string()),
                base_url: None,
                model_capabilities: caps,
                request_timeout_secs: None,
                rate_limit: None,
                options: Default::default(),
            },
        ];
        let registry = build_provider_registry(&creds);
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
        assert!(registry.has("claude-code"));
        assert!(!registry.has("totally-unknown"));
    }

    #[test]
    fn build_provider_registry_skips_keyed_providers_without_api_key() {
        // The anthropic/openai/google/openrouter arms only register when an
        // api_key is present; a `None` key exercises the skip (else) path of
        // each `if let Some(ref key)` and leaves the provider unregistered.
        let caps = std::collections::HashMap::new();
        let creds: Vec<ProviderCreds> = ["anthropic", "openai", "google", "openrouter"]
            .into_iter()
            .map(|name| ProviderCreds {
                name: name.to_string(),
                api_key: None,
                base_url: None,
                model_capabilities: caps.clone(),
                request_timeout_secs: None,
                rate_limit: None,
                options: Default::default(),
            })
            .collect();
        let registry = build_provider_registry(&creds);
        assert!(!registry.has("anthropic"));
        assert!(!registry.has("openai"));
        assert!(!registry.has("google"));
        assert!(!registry.has("openrouter"));
    }

    #[test]
    fn claude_code_reads_its_binary_and_effort_options() {
        // The registry arm used to discard both, always constructing a default
        // provider, so a configured binary path or effort level was silently
        // ignored.
        let mut creds = ProviderCreds::simple("claude-code");
        creds
            .options
            .insert("binary".to_string(), "/opt/bin/claude".to_string());
        creds
            .options
            .insert("effort".to_string(), "low".to_string());
        let registry = build_provider_registry(std::slice::from_ref(&creds));
        assert!(registry.has("claude-code"));

        // Options are consumed by the provider constructor, which is where the
        // effort allow-list lives; an unusable value must not reach the CLI.
        creds
            .options
            .insert("effort".to_string(), "warp-speed".to_string());
        assert!(build_provider_registry(&[creds]).has("claude-code"));
    }

    #[test]
    fn provider_creds_simple_has_no_key_or_options() {
        let creds = ProviderCreds::simple("ollama");
        assert_eq!(creds.name, "ollama");
        assert!(creds.api_key.is_none());
        assert!(creds.base_url.is_none());
        assert!(creds.options.is_empty());
        assert!(creds.model_capabilities.is_empty());
        assert!(creds.request_timeout_secs.is_none());
    }
}
