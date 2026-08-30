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
/// `PartialEq` is what lets the daemon tell whether a config edit changed a
/// provider at all, key included, without printing one.
#[derive(Clone, PartialEq)]
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
    /// `claude-code` reads `binary` (path to the `claude` executable) and
    /// `effort` (reasoning level). An OpenAI-compatible endpoint is marked by
    /// `kind` and carries its headers and model list here too; see
    /// [`Self::openai_compatible`] and [`EndpointSpec`], which are the only
    /// two places that spell those keys. Kept as a map rather than named
    /// fields so one provider's options don't accrete onto a struct shared by
    /// seven.
    pub options: std::collections::HashMap<String, String>,
}

/// The `options` key that marks an entry as an OpenAI-compatible endpoint.
const KIND_OPTION: &str = "kind";
/// The `kind` value for one.
const OPENAI_COMPATIBLE: &str = "openai-compatible";
/// The `options` key prefix a request header travels under: `header:0:X-Org`.
const HEADER_PREFIX: &str = "header:";
/// The `options` key holding the configured model ids, as a JSON array.
const MODELS_OPTION: &str = "models";
/// The `options` key holding the `serves` routing hints, as a JSON array.
const SERVES_OPTION: &str = "serves";

/// What [`build_provider_registry`] needs to build an
/// [`leviath_providers::EndpointProvider`], read back out of a
/// [`ProviderCreds`] made by [`ProviderCreds::openai_compatible`].
///
/// A struct rather than five loose reads so the encoding is written once
/// there and read once in [`Self::from_creds`], and nothing else has to know
/// how a header is spelled in the options map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSpec {
    /// Where the server listens, including any path prefix.
    pub base_url: String,
    /// Extra headers on every request, in the order the config listed them.
    pub headers: Vec<(String, String)>,
    /// The ids to fall back to when the server will not list. `None` when the
    /// config named none.
    pub models: Option<Vec<String>>,
    /// Ids a bare model name may route here on.
    pub serves: Vec<String>,
}

impl EndpointSpec {
    /// The endpoint `creds` describes, or `Ok(None)` when it is not one.
    ///
    /// An endpoint with no base URL is not an endpoint: the config layer
    /// refuses to load one, so this is a second guard rather than the first.
    ///
    /// An option that is there but does not read back is an error, not an
    /// absence. The runtime wrote these from the config, so a failure is a
    /// bug, and reading past it changes what the entry means: a `models` list
    /// that does not parse reads as "the config did not say", which lets the
    /// endpoint route any id where the list would have refused the rest, and a
    /// header with a bad position is a header the server never sees. The error
    /// names the entry and the option.
    pub fn from_creds(
        creds: &ProviderCreds,
    ) -> Result<Option<Self>, leviath_providers::ProviderError> {
        if creds.options.get(KIND_OPTION).map(String::as_str) != Some(OPENAI_COMPATIBLE) {
            return Ok(None);
        }
        let Some(base_url) = creds.base_url.clone() else {
            return Ok(None);
        };
        let malformed = |option: &str, what: &str| {
            leviath_providers::ProviderError::Other(format!(
                "endpoint '{}': the '{option}' option is not {what}; the runtime wrote it \
                 from the config, so this is a bug in leviath rather than in the config",
                creds.name
            ))
        };
        // Sorted by the position the encoder stamped, so the config's own
        // order is what the wire sees whatever order the map iterates in.
        let mut headers: Vec<(usize, String, String)> = Vec::new();
        for (key, value) in &creds.options {
            let Some(rest) = key.strip_prefix(HEADER_PREFIX) else {
                continue;
            };
            let position = rest
                .split_once(':')
                .and_then(|(position, name)| Some((position.parse().ok()?, name)));
            let Some((position, name)) = position else {
                return Err(malformed(key, "a numbered header (header:<n>:<name>)"));
            };
            headers.push((position, name.to_string(), value.clone()));
        }
        headers.sort();
        let list = |option: &str| -> Result<Option<Vec<String>>, _> {
            creds
                .options
                .get(option)
                .map(|json| serde_json::from_str(json))
                .transpose()
                .map_err(|_| malformed(option, "a JSON list of strings"))
        };
        Ok(Some(Self {
            base_url,
            headers: headers
                .into_iter()
                .map(|(_, name, value)| (name, value))
                .collect(),
            models: list(MODELS_OPTION)?,
            serves: list(SERVES_OPTION)?.unwrap_or_default(),
        }))
    }
}

/// Hand-written so the API key can never reach a log line.
///
/// A `#[derive(Debug)]` here meant a single `tracing::debug!(?creds)` - or an
/// error context that formats a struct holding one - would print the key.
/// Nothing did, which is when it is cheap to make impossible.
impl std::fmt::Debug for ProviderCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A header value is a credential as often as not (`X-Api-Key`), so
        // the options map is printed with those values replaced. Sorted so
        // two lines from one struct read the same.
        let mut options: Vec<(&str, &str)> = self
            .options
            .iter()
            .map(|(key, value)| match key.starts_with(HEADER_PREFIX) {
                true => (key.as_str(), "<set>"),
                false => (key.as_str(), value.as_str()),
            })
            .collect();
        options.sort_unstable();
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
            .field("options", &options)
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

    /// A cred entry for an OpenAI-compatible endpoint registered as `name`.
    ///
    /// Everything an [`leviath_providers::EndpointProvider`] needs travels in
    /// the fields every other provider already has, plus the options map:
    /// the kind marker, one `header:<n>:<name>` entry per header (numbered so
    /// the config's order survives a `HashMap`), and the model lists as JSON.
    /// [`EndpointSpec::from_creds`] reads it back.
    pub fn openai_compatible(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
        headers: Vec<(String, String)>,
        models: Option<Vec<String>>,
        serves: Vec<String>,
    ) -> Self {
        let mut options = std::collections::HashMap::new();
        options.insert(KIND_OPTION.to_string(), OPENAI_COMPATIBLE.to_string());
        for (position, (header, value)) in headers.into_iter().enumerate() {
            options.insert(format!("{HEADER_PREFIX}{position}:{header}"), value);
        }
        if let Some(models) = models {
            options.insert(
                MODELS_OPTION.to_string(),
                serde_json::to_string(&models).expect("a list of strings is JSON"),
            );
        }
        if !serves.is_empty() {
            options.insert(
                SERVES_OPTION.to_string(),
                serde_json::to_string(&serves).expect("a list of strings is JSON"),
            );
        }
        Self {
            name: name.into(),
            api_key,
            base_url: Some(base_url.into()),
            model_capabilities: std::collections::HashMap::new(),
            request_timeout_secs: None,
            rate_limit: None,
            options,
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
/// "the user configured this". Ollama needs no key, so registering it
/// unconditionally makes an install with no local server advertise a working
/// provider, and blueprint order then sends stages to a localhost port nothing
/// answers on.
///
/// A connect is the equivalent cheap stand-in: it does not prove Ollama is
/// there, only that the address is not dead, which is exactly the case that
/// misreports. The timeout is deliberately short - this runs while the
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
        // Decided by kind before the name is looked at: an endpoint is
        // registered under whatever name the config gave it, and that name is
        // the user's to choose.
        if let Some(endpoint) = EndpointSpec::from_creds(c)? {
            registry.register(
                c.name.clone(),
                Arc::new(
                    leviath_providers::EndpointProvider::new(
                        clients.get_or_build(timeout, build_client)?,
                        c.name.clone(),
                        endpoint.base_url,
                        c.api_key.clone(),
                        endpoint.headers,
                    )
                    .with_overrides(caps)
                    .with_rate_limit(c.rate_limit.as_ref())
                    .with_request_timeout(timeout)
                    .with_models(endpoint.models)
                    .with_serves(endpoint.serves),
                ),
            );
            continue;
        }
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

    /// The options map is the only place an endpoint's extras travel, so the
    /// encoder and the decoder have to agree on every key, including the
    /// order headers come back in.
    #[test]
    fn an_endpoint_cred_round_trips_through_the_options_map() {
        let creds = ProviderCreds::openai_compatible(
            "vllm",
            "http://localhost:8000/v1",
            Some("k".to_string()),
            vec![
                ("X-Org".to_string(), "research".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            Some(vec!["llama-3".to_string()]),
            vec!["llama".to_string()],
        );
        let spec = EndpointSpec::from_creds(&creds)
            .expect("decodes")
            .expect("an endpoint");
        assert_eq!(
            spec,
            EndpointSpec {
                base_url: "http://localhost:8000/v1".to_string(),
                headers: vec![
                    ("X-Org".to_string(), "research".to_string()),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                models: Some(vec!["llama-3".to_string()]),
                serves: vec!["llama".to_string()],
            }
        );

        // Nothing configured stays nothing rather than becoming an empty list.
        let bare = ProviderCreds::openai_compatible("x", "http://h", None, vec![], None, vec![]);
        let spec = EndpointSpec::from_creds(&bare)
            .expect("decodes")
            .expect("an endpoint");
        assert_eq!(spec.models, None);
        assert!(spec.serves.is_empty());
        assert!(spec.headers.is_empty());
    }

    /// Not an endpoint: a native provider, and an endpoint that lost its
    /// address.
    #[test]
    fn a_cred_that_is_not_an_endpoint_yields_no_spec() {
        assert_eq!(
            EndpointSpec::from_creds(&ProviderCreds::simple("anthropic")).expect("decodes"),
            None
        );

        let mut no_url =
            ProviderCreds::openai_compatible("x", "http://h", None, vec![], None, vec![]);
        no_url.base_url = None;
        assert_eq!(EndpointSpec::from_creds(&no_url).expect("decodes"), None);
    }

    /// An option the runtime wrote that does not read back is a bug, and it
    /// must not turn into a looser entry: `models` going from "this complete
    /// list" to "the config did not say" lets the endpoint route any id, and
    /// a header that lost its position is a header the server never sees.
    /// The registry refuses the entry and names it and the option.
    #[test]
    fn a_malformed_endpoint_option_fails_the_registry_rather_than_loosening_it() {
        for (option, value) in [
            ("models", "not json"),
            ("serves", "["),
            ("header:notanumber:X-Bad", "v"),
            ("header:X-No-Position", "v"),
        ] {
            let mut bad = ProviderCreds::openai_compatible(
                "gw",
                "http://127.0.0.1:1/v1",
                None,
                vec![],
                Some(vec!["m".to_string()]),
                vec![],
            );
            bad.options.insert(option.to_string(), value.to_string());
            let decoded = EndpointSpec::from_creds(&bad);
            assert!(decoded.is_err(), "{option}={value:?} must not decode");
            let text = decoded.expect_err("checked above").to_string();
            assert!(text.contains("'gw'"), "{option}: names the entry: {text}");
            assert!(text.contains(option), "names the option: {text}");
            let registry = build_provider_registry(&[bad]);
            assert!(
                registry.is_err(),
                "{option}={value:?} must fail the registry"
            );
            assert_eq!(
                registry.map(drop).expect_err("checked above").to_string(),
                text,
                "the registry passes it on as is"
            );
        }
    }

    /// A header value is a credential as often as not.
    #[test]
    fn debug_output_never_contains_a_header_value() {
        let creds = ProviderCreds::openai_compatible(
            "gw",
            "http://h/v1",
            None,
            vec![("X-Api-Key".to_string(), "hdr-SECRET-VALUE".to_string())],
            None,
            vec![],
        );
        let rendered = format!("{creds:?}");
        assert!(
            !rendered.contains("SECRET-VALUE"),
            "header leaked: {rendered}"
        );
        assert!(rendered.contains("header:0:X-Api-Key"), "{rendered}");
        assert!(rendered.contains("openai-compatible"), "{rendered}");
    }

    /// An endpoint registers under its own name, whatever that name is, and
    /// carries its rate limit and overrides.
    #[test]
    fn an_endpoint_cred_registers_a_native_provider_under_its_name() {
        let mut creds = ProviderCreds::openai_compatible(
            "mock",
            "http://127.0.0.1:1/v1",
            None,
            vec![],
            Some(vec!["gpt-mock".to_string()]),
            vec![],
        );
        creds.rate_limit = Some(leviath_providers::RateLimitConfig {
            requests_per_minute: 10,
            tokens_per_minute: 1000,
        });
        creds.model_capabilities.insert(
            "gpt-mock".to_string(),
            leviath_providers::ModelCapabilityOverride {
                max_context_tokens: Some(4096),
                ..Default::default()
            },
        );
        let registry = build_provider_registry(&[creds]).expect("an HTTPS client builds in tests");
        let provider = registry.get("mock").expect("registered");
        assert_eq!(provider.name(), "mock");
        assert_eq!(provider.max_context_tokens("gpt-mock"), 4096);
        assert_eq!(
            provider.served_catalog(),
            Some(vec!["gpt-mock".to_string()])
        );
        // Nothing else was registered under a built-in name by accident.
        assert!(!registry.has("openai"));
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
        let endpoint =
            ProviderCreds::openai_compatible("mock", "http://h/v1", None, vec![], None, vec![]);
        let keyed = ["anthropic", "openai", "google", "openrouter", "ollama"].map(|name| {
            let mut cred = ProviderCreds::simple(name);
            cred.api_key = Some("k".to_string());
            cred
        });
        for cred in keyed.into_iter().chain([endpoint]) {
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
