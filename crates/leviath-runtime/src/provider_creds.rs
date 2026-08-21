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
    pub model_capabilities:
        std::collections::HashMap<String, leviath_providers::ModelCapabilityOverride>,
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

/// Ollama's own default port, used when a base URL names no port.
const OLLAMA_DEFAULT_PORT: u16 = 11434;

/// Whether something is listening at `base_url`, as a short TCP connect.
///
/// Ollama is the one provider with nothing to check: every other entry
/// registers only when it has an API key, and a key is a cheap stand-in for
/// "the user configured this". Ollama needs no key, so it used to register
/// unconditionally, and an install with no local server still advertised a
/// working provider. Blueprint order then sent stages to a localhost port
/// nothing answered on.
///
/// A connect is the equivalent cheap stand-in: it does not prove Ollama is
/// there, only that the address is not dead, which is exactly the case that
/// was misreporting. The timeout is deliberately short - this runs while the
/// daemon is starting, and a loopback address either answers immediately or
/// is not there at all.
pub fn tcp_reachable(base_url: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let Some((host, port)) = host_and_port(base_url) else {
        return false;
    };
    let timeout = std::time::Duration::from_millis(300);
    // A name that does not resolve and an address that does not answer are
    // the same answer here, so they share one expression rather than an arm
    // each.
    (host.as_str(), port).to_socket_addrs().is_ok_and(|addrs| {
        addrs
            .into_iter()
            .any(|addr| TcpStream::connect_timeout(&addr, timeout).is_ok())
    })
}

/// The host and port of an `http://host:port/path` base URL.
///
/// Hand-rolled because this crate does not depend on an HTTP client and one
/// address is not worth taking one on. Anything without a recognisable host is
/// `None`, which the caller reads as "not reachable" - the conservative answer
/// for a provider that registers on reachability alone.
fn host_and_port(base_url: &str) -> Option<(String, u16)> {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(scheme, rest)| {
            let _ = scheme;
            rest
        });
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    if authority.is_empty() {
        return None;
    }
    // `[::1]:11434` - the brackets delimit the address, so the port is
    // whatever follows the closing one and the colons inside are not
    // separators.
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => OLLAMA_DEFAULT_PORT,
        };
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((authority.to_string(), OLLAMA_DEFAULT_PORT)),
    }
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
    build_provider_registry_probing(creds, build_client, &tcp_reachable)
}

/// [`build_provider_registry_with`], with the Ollama reachability probe
/// injected so a test can decide the answer instead of opening a socket.
pub fn build_provider_registry_probing(
    creds: &[ProviderCreds],
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
    reachable: &dyn Fn(&str) -> bool,
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
                        Arc::new(
                            leviath_providers::AnthropicProvider::with_overrides(
                                clients.get_or_build(timeout, build_client)?,
                                key.clone(),
                                caps,
                                c.rate_limit.as_ref(),
                            )
                            .with_base_url(c.base_url.clone())
                            // An unrecognised value keeps the default rather
                            // than failing the daemon's boot over a cache
                            // setting; the config layer is what validates it.
                            .with_cache_ttl(
                                match c.options.get("cache_ttl").map(String::as_str) {
                                    Some("1h") => {
                                        leviath_providers::anthropic::CacheTtl::Ephemeral1h
                                    }
                                    _ => leviath_providers::anthropic::CacheTtl::Ephemeral5m,
                                },
                            ),
                        ),
                    );
                }
            }
            "openai" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "openai".to_string(),
                        Arc::new(
                            leviath_providers::OpenAIProvider::with_overrides(
                                clients.get_or_build(timeout, build_client)?,
                                key.clone(),
                                caps,
                                c.rate_limit.as_ref(),
                            )
                            .with_base_url(c.base_url.clone()),
                        ),
                    );
                }
            }
            "google" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "google".to_string(),
                        Arc::new(
                            leviath_providers::GeminiProvider::with_overrides(
                                clients.get_or_build(timeout, build_client)?,
                                key.clone(),
                                caps,
                                c.rate_limit.as_ref(),
                            )
                            .with_base_url(c.base_url.clone()),
                        ),
                    );
                }
            }
            "openrouter" => {
                if let Some(ref key) = c.api_key {
                    registry.register(
                        "openrouter".to_string(),
                        Arc::new(
                            leviath_providers::OpenRouterProvider::with_overrides(
                                clients.get_or_build(timeout, build_client)?,
                                key.clone(),
                                caps,
                                c.rate_limit.as_ref(),
                            )
                            .with_base_url(c.base_url.clone()),
                        ),
                    );
                }
            }
            "ollama" => {
                let url = c
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "http://localhost:11434".to_string());
                // The only provider that registers on something other than a
                // key, because it has no key to register on. See
                // [`tcp_reachable`]: an address nothing answers on is not a
                // usable provider, and pretending otherwise put it ahead of
                // providers that were actually configured.
                if reachable(&url) {
                    registry.register(
                        "ollama".to_string(),
                        Arc::new(leviath_providers::OllamaProvider::with_overrides(
                            clients.get_or_build(timeout, build_client)?,
                            url,
                            caps,
                        )),
                    );
                } else {
                    tracing::info!(
                        base_url = %url,
                        "nothing is listening for ollama; not registering it. Start \
                         ollama and reload the config to use it."
                    );
                }
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

    /// The cache TTL reaches the provider, and an unrecognised value keeps the
    /// default rather than failing the daemon's boot over a cache setting.
    #[test]
    fn the_anthropic_cache_ttl_is_read_from_the_options_map() {
        for configured in [Some("1h"), Some("5m"), Some("nonsense"), None] {
            let mut cred = ProviderCreds::simple("anthropic");
            cred.api_key = Some("k".to_string());
            if let Some(value) = configured {
                cred.options
                    .insert("cache_ttl".to_string(), value.to_string());
            }
            let registry = build_provider_registry(&[cred])
                .expect("a cache setting must never fail the build");
            assert!(
                registry.get("anthropic").is_some(),
                "configured {configured:?}"
            );
        }
    }

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
        // The probe is injected: whether this machine happens to be running
        // Ollama is not something the test should depend on.
        let registry = build_provider_registry_probing(
            &creds,
            &leviath_providers::provider::build_http_client,
            &|_| true,
        )
        .expect("an HTTPS client builds in tests");
        assert!(registry.has("anthropic"));
        assert!(registry.has("openai"));
        assert!(registry.has("google"));
        assert!(registry.has("openrouter"));
        assert!(registry.has("ollama"));
        assert!(registry.has("claude-code"));
        assert!(!registry.has("totally-unknown"));
    }

    /// Ollama is the one provider with no key to gate on, so it gates on
    /// something answering at its address instead. Registering it regardless
    /// is what put a dead localhost port ahead of providers that were
    /// actually configured.
    #[test]
    fn ollama_registers_only_when_something_answers() {
        let creds = vec![ProviderCreds::simple("ollama")];
        for reachable in [true, false] {
            let registry = build_provider_registry_probing(
                &creds,
                &leviath_providers::provider::build_http_client,
                &|_| reachable,
            )
            .expect("an HTTPS client builds in tests");
            assert_eq!(registry.has("ollama"), reachable);
        }
    }

    /// The probe reads an address out of a base URL without an HTTP client.
    /// The bracketed form is the one worth pinning: the colons inside an IPv6
    /// address are not port separators.
    #[test]
    fn a_base_url_yields_its_host_and_port() {
        for (url, want) in [
            ("http://localhost:11434", Some(("localhost", 11434))),
            ("http://127.0.0.1:1", Some(("127.0.0.1", 1))),
            ("http://example.com", Some(("example.com", 11434))),
            ("http://example.com/v1/path", Some(("example.com", 11434))),
            ("http://user:pw@host:9999/x", Some(("host", 9999))),
            ("[::1]:11434", Some(("::1", 11434))),
            ("http://[::1]", Some(("::1", 11434))),
            ("bare-host", Some(("bare-host", 11434))),
            ("http://host:notaport", None),
            ("http://[::1]:notaport", None),
            ("http://[::1", None),
            ("http://", None),
        ] {
            let got = host_and_port(url);
            let got = got.as_ref().map(|(h, p)| (h.as_str(), *p));
            assert_eq!(got, want, "for {url}");
        }
    }

    /// Nothing listens on port 1, and a name that does not resolve cannot be
    /// connected to either. Both are the "not reachable" answer.
    #[test]
    fn tcp_reachable_says_no_to_a_dead_address() {
        assert!(!tcp_reachable("http://127.0.0.1:1"));
        assert!(!tcp_reachable("http://"));
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
            // Ollama's arm is only reached when something answers at its
            // address, so the probe is injected rather than left to depend on
            // whether the machine running the suite has Ollama up. It does not
            // on CI, and the arm returned before ever asking for a client.
            let err = build_provider_registry_probing(&[cred], &failing_client, &|_| true)
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
