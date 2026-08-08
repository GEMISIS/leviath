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

/// Outbound HTTPS clients, one per distinct request timeout.
#[derive(Default)]
struct ClientCache {
    by_timeout: std::collections::HashMap<Option<u64>, leviath_providers::provider::HttpClient>,
}

impl ClientCache {
    /// The client for `timeout`, building it on first request.
    ///
    /// Providers sharing a timeout share a connection pool; before this, each
    /// provider built its own client, so a daemon with five configured held
    /// five pools.
    fn get_or_build(
        &mut self,
        timeout: Option<u64>,
        build: leviath_providers::provider::HttpClientFactory<'_>,
    ) -> Result<leviath_providers::provider::HttpClient, leviath_providers::ProviderError> {
        if let Some(client) = self.by_timeout.get(&timeout) {
            return Ok(client.clone());
        }
        let built = build(timeout)
            .map_err(|e| leviath_providers::ProviderError::ClientBuild(e.to_string()))?;
        self.by_timeout.insert(timeout, built.clone());
        Ok(built)
    }
}

/// Build a [`ProviderRegistry`] from decoupled [`ProviderCreds`].
pub fn build_provider_registry(
    creds: &[ProviderCreds],
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    build_provider_registry_with(creds, &leviath_providers::provider::build_http_client)
}

/// [`build_provider_registry`], with client construction injected.
///
/// One client per distinct request timeout, shared by every provider that wants
/// it. Previously each provider built its own, so a daemon with five providers
/// configured held five connection pools; the timeout is part of the key because
/// `apply_request_timeout` deliberately defers to the client-level timeout when
/// a stage sets none, so collapsing distinct timeouts onto one client would
/// silently retime requests.
pub fn build_provider_registry_with(
    creds: &[ProviderCreds],
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    let mut registry = ProviderRegistry::new();
    // One client per distinct timeout, built on first use. Lazy because
    // `claude-code` drives a local CLI and needs no HTTP client at all - eager
    // construction would let a certificate-store failure block a provider that
    // never touches a certificate.
    let mut clients = ClientCache::default();

    for c in creds {
        let caps = c.model_capabilities.clone();
        let timeout = c.request_timeout_secs;
        match c.name.as_str() {
            "anthropic" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "anthropic".to_string(),
                        Arc::new(leviath_providers::AnthropicProvider::with_overrides(
                            clients.get_or_build(timeout, build_client)?,
                            key.clone(),
                            caps,
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
                            clients.get_or_build(timeout, build_client)?,
                            key.clone(),
                            caps,
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
                            clients.get_or_build(timeout, build_client)?,
                            key.clone(),
                            caps,
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
                            clients.get_or_build(timeout, build_client)?,
                            key.clone(),
                            caps,
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
                        clients.get_or_build(timeout, build_client)?,
                        url,
                        caps,
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

    Ok(registry)
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
        // Drives `build_provider_registry(&[ProviderCreds]).expect("an HTTPS client builds in tests")` directly:
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
        let registry = build_provider_registry(&creds).expect("an HTTPS client builds in tests");
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
        let registry = build_provider_registry(&creds).expect("an HTTPS client builds in tests");
        assert!(!registry.has("anthropic"));
        assert!(!registry.has("openai"));
        assert!(!registry.has("google"));
        assert!(!registry.has("openrouter"));
    }

    #[test]
    fn claude_code_reads_its_binary_and_effort_options() {
        // The registry arm must thread both options through: constructing a
        // default provider here would silently ignore a configured binary path
        // or effort level.
        let mut creds = ProviderCreds::simple("claude-code");
        creds
            .options
            .insert("binary".to_string(), "/opt/bin/claude".to_string());
        creds
            .options
            .insert("effort".to_string(), "low".to_string());
        let registry = build_provider_registry(std::slice::from_ref(&creds))
            .expect("an HTTPS client builds in tests");
        assert!(registry.has("claude-code"));

        // Options are consumed by the provider constructor, which is where the
        // effort allow-list lives; an unusable value must not reach the CLI.
        creds
            .options
            .insert("effort".to_string(), "warp-speed".to_string());
        assert!(
            build_provider_registry(&[creds])
                .expect("an HTTPS client builds in tests")
                .has("claude-code")
        );
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

    // ─── The client-build failure path ──────────────────────────────────────

    /// A factory that always fails, standing in for a machine whose root
    /// certificate store cannot be read.
    fn failing_client(
        _timeout: Option<u64>,
    ) -> std::result::Result<
        leviath_providers::provider::HttpClient,
        leviath_providers::provider::HttpError,
    > {
        // The only way to obtain a `reqwest::Error` is to have reqwest produce
        // one; a request to an unroutable scheme does that without any I/O.
        Err(leviath_providers::provider::malformed_url_error())
    }

    #[test]
    fn every_http_provider_fails_the_registry_when_its_client_will_not_build() {
        // One case per branch that needs a client. A single provider would leave
        // the other arms' error paths unproven, which is exactly the hole this
        // seam exists to close.
        for name in ["anthropic", "openai", "google", "openrouter", "ollama"] {
            let mut cred = ProviderCreds::simple(name);
            cred.api_key = Some("k".to_string());
            let err = build_provider_registry_with(&[cred], &failing_client)
                .err()
                .expect("a failing client factory should fail the registry");
            // Discriminant rather than `matches!`: the macro expands to a
            // match with a `_ => false` arm that nothing reaches, which the
            // 100% gate reads as an uncovered region.
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&leviath_providers::ProviderError::ClientBuild(
                    String::new()
                ))
            );
            // The message has to name the cause; a bare "request failed" would
            // send someone looking at their network, not their cert store.
            assert!(err.to_string().contains("root certificate store"));
        }
    }

    #[test]
    fn a_provider_that_needs_no_http_client_is_unaffected() {
        // `claude-code` drives a local CLI. Building its entry must not depend
        // on an HTTPS client, so a failing factory leaves it registered.
        let registry =
            build_provider_registry_with(&[ProviderCreds::simple("claude-code")], &failing_client)
                .expect("claude-code needs no HTTPS client");
        assert!(registry.has("claude-code"));
    }

    #[test]
    fn providers_sharing_a_timeout_share_one_client() {
        // Atomic rather than `Cell`: the factory is `Send + Sync`, because the
        // one in `verify` is held across an await.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let builds = AtomicUsize::new(0);
        let counting = |timeout: Option<u64>| {
            builds.fetch_add(1, Ordering::SeqCst);
            leviath_providers::provider::build_http_client(timeout)
        };
        let creds: Vec<ProviderCreds> = [("anthropic", 30), ("openai", 30), ("google", 60)]
            .into_iter()
            .map(|(name, secs)| {
                let mut c = ProviderCreds::simple(name);
                c.api_key = Some("k".to_string());
                c.request_timeout_secs = Some(secs);
                c
            })
            .collect();
        let registry =
            build_provider_registry_with(&creds, &counting).expect("clients build in tests");
        assert!(registry.has("anthropic") && registry.has("openai") && registry.has("google"));
        // Two distinct timeouts, so two clients - not one per provider, which is
        // what this crate did before and what the connection pools paid for.
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "expected one client per distinct timeout"
        );
    }
}
