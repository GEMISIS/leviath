//! Building a provider from what a config says about it.
//!
//! The one place a provider kind becomes a provider. It lives here, beside
//! the providers, because a build can leave any of them out: this match is
//! where that shows, and nothing above this crate has to know which ones a
//! build has.

use std::collections::HashMap;
use std::sync::Arc;

use crate::compiled;
use crate::provider::{HttpClient, ModelCapabilityOverride, Provider, RateLimitConfig, Result};

/// What a config says about one provider, read into plain values.
pub struct Spec {
    /// The name the provider registers under: its kind for a built-in, the
    /// config's own name for a second OpenAI host.
    pub name: String,
    /// Which provider this is: `anthropic`, `openai`, `codex`, and so on.
    pub kind: String,
    /// The API key, for a provider that takes one.
    pub api_key: Option<String>,
    /// Where to send requests instead of the provider's own host.
    pub base_url: Option<String>,
    /// Extra headers on every request, in the order the config listed them.
    pub headers: Vec<(String, String)>,
    /// The header a key goes in instead of `Authorization: Bearer`.
    pub auth_header: Option<String>,
    /// Ids a bare model name may route here on.
    pub serves: Vec<String>,
    /// Per-model capability overrides.
    pub caps: HashMap<String, ModelCapabilityOverride>,
    /// A client-side rate limit, when one is configured.
    pub rate_limit: Option<RateLimitConfig>,
    /// The per-request timeout in seconds, when one is configured.
    pub request_timeout_secs: Option<u64>,
    /// Settings particular to one provider: `cache_ttl`, `region`, `effort`,
    /// `auth_store_path` and the like.
    pub options: HashMap<String, String>,
    /// The OS credential store a sign-in provider's grant lives in; `None`
    /// is the grant file.
    pub credential_store: Option<Arc<dyn leviath_core::CredentialStore>>,
}

impl Spec {
    /// A spec for the built-in provider `kind`, registered under its own
    /// name, with nothing configured yet.
    pub fn new(kind: impl Into<String>) -> Self {
        let kind = kind.into();
        Self {
            name: kind.clone(),
            kind,
            api_key: None,
            base_url: None,
            headers: Vec::new(),
            auth_header: None,
            serves: Vec::new(),
            caps: HashMap::new(),
            rate_limit: None,
            request_timeout_secs: None,
            options: HashMap::new(),
            credential_store: None,
        }
    }

    /// One provider-specific setting.
    #[cfg(any(
        feature = "bedrock",
        feature = "xai",
        feature = "xai-subscription",
        feature = "meta",
        feature = "openai-subscription"
    ))]
    fn option(&self, key: &str) -> Option<String> {
        self.options.get(key).cloned()
    }
}

/// The provider `spec` describes, or `None` when there is nothing to
/// register: a keyed provider with no key, an Ollama nothing answers at, a
/// kind this crate does not know, or one this build left out (which is
/// logged, since the config asked for it).
///
/// `client` builds the HTTP client on first use, so a provider that
/// registers nothing never reads the certificate store. `reachable` is the
/// check Ollama registers on in place of a key.
pub fn build(
    spec: Spec,
    client: &mut dyn FnMut() -> Result<HttpClient>,
    reachable: &dyn Fn(&str) -> bool,
) -> Result<Option<Arc<dyn Provider>>> {
    build_in(spec, client, reachable, compiled::COMPILED)
}

/// [`build`] against a given set of built-in kinds, so a test can ask about
/// a build it is not.
fn build_in(
    spec: Spec,
    client: &mut dyn FnMut() -> Result<HttpClient>,
    reachable: &dyn Fn(&str) -> bool,
    built: &[&str],
) -> Result<Option<Arc<dyn Provider>>> {
    if let Some(why) = compiled::missing_in(&spec.kind, built) {
        tracing::warn!(provider = %spec.name, "{why}; skipping it");
        return Ok(None);
    }
    // Every arm but these two needs a key, and no key means nothing to build.
    let keyless = matches!(spec.kind.as_str(), "ollama" | "codex" | "grok");
    if !keyless && spec.api_key.is_none() {
        return Ok(None);
    }
    let _ = (&client, reachable);
    match spec.kind.as_str() {
        #[cfg(feature = "anthropic")]
        "anthropic" => anthropic(spec, client()?).map(Some),
        #[cfg(feature = "openai")]
        "openai" => openai(spec, client()?).map(Some),
        #[cfg(feature = "google")]
        "google" => google(spec, client()?).map(Some),
        #[cfg(feature = "openrouter")]
        "openrouter" => openrouter(spec, client()?).map(Some),
        #[cfg(feature = "meshy")]
        "meshy" => meshy(spec, client()?).map(Some),
        #[cfg(feature = "bedrock")]
        "bedrock" => bedrock(spec, client()?).map(Some),
        #[cfg(feature = "xai")]
        "xai" => xai(spec, client()?).map(Some),
        #[cfg(feature = "meta")]
        "meta" => meta(spec, client()?).map(Some),
        #[cfg(feature = "xai-subscription")]
        "grok" => grok(spec, client),
        #[cfg(feature = "ollama")]
        "ollama" => ollama(spec, client, reachable),
        #[cfg(feature = "openai-subscription")]
        "codex" => codex(spec, client),
        _ => Ok(None),
    }
}

/// The key, which [`build_in`] has already checked is there.
#[cfg(any(
    feature = "anthropic",
    feature = "openai",
    feature = "google",
    feature = "openrouter",
    feature = "meshy",
    feature = "bedrock",
    feature = "xai",
    feature = "meta"
))]
fn key(spec: &Spec) -> String {
    spec.api_key.clone().unwrap_or_default()
}

#[cfg(feature = "anthropic")]
fn anthropic(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    use crate::anthropic::CacheTtl;
    // An unrecognised value keeps the default rather than failing the
    // daemon's boot over a cache setting; the config layer is what validates
    // it.
    let ttl = match spec.options.get("cache_ttl").map(String::as_str) {
        Some("1h") => CacheTtl::Ephemeral1h,
        _ => CacheTtl::Ephemeral5m,
    };
    Ok(Arc::new(
        crate::AnthropicProvider::with_overrides(
            client,
            key(&spec),
            spec.caps,
            spec.rate_limit.as_ref(),
        )
        .with_base_url(spec.base_url)
        .with_headers(spec.headers)
        .with_cache_ttl(ttl),
    ))
}

/// OpenAI's own API, at its own host or at another one (an Azure resource, a
/// gateway) registered under the config's name.
#[cfg(feature = "openai")]
fn openai(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(
        crate::OpenAIProvider::with_overrides(
            client,
            key(&spec),
            spec.caps,
            spec.rate_limit.as_ref(),
        )
        .named(spec.name)
        .with_base_url(spec.base_url)
        .with_headers(spec.headers)
        .with_auth_header(spec.auth_header)
        .with_serves(spec.serves),
    ))
}

#[cfg(feature = "google")]
fn google(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(
        crate::GeminiProvider::with_overrides(
            client,
            key(&spec),
            spec.caps,
            spec.rate_limit.as_ref(),
        )
        .with_base_url(spec.base_url)
        .with_headers(spec.headers),
    ))
}

#[cfg(feature = "openrouter")]
fn openrouter(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(
        crate::OpenRouterProvider::with_overrides(
            client,
            key(&spec),
            spec.caps,
            spec.rate_limit.as_ref(),
        )
        .with_base_url(spec.base_url)
        .with_headers(spec.headers),
    ))
}

#[cfg(feature = "meshy")]
fn meshy(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(
        crate::MeshyProvider::with_overrides(
            client,
            key(&spec),
            spec.caps,
            spec.rate_limit.as_ref(),
        )
        .with_base_url(spec.base_url)
        .with_headers(spec.headers),
    ))
}

#[cfg(feature = "bedrock")]
fn bedrock(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    // Absent means the provider's default region; the config layer writes it
    // only when one was set.
    let region = spec.option("region");
    Ok(Arc::new(
        crate::BedrockProvider::with_overrides(
            client,
            key(&spec),
            spec.caps,
            spec.rate_limit.as_ref(),
        )
        .with_base_url(spec.base_url)
        .with_headers(spec.headers)
        .with_region(region),
    ))
}

#[cfg(feature = "xai")]
fn xai(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    let auth = crate::xai::Auth::Key(key(&spec));
    xai_with(spec, client, auth)
}

/// An xAI client with `auth`: a key for the `xai` provider, a sign-in for
/// `grok`.
#[cfg(any(feature = "xai", feature = "xai-subscription"))]
fn xai_with(spec: Spec, client: HttpClient, auth: crate::xai::Auth) -> Result<Arc<dyn Provider>> {
    let effort = spec.option("effort");
    Ok(Arc::new(
        crate::xai::XaiProvider::new(client, auth)
            .with_overrides(spec.caps)
            .with_rate_limit(spec.rate_limit.as_ref())
            .with_request_timeout(spec.request_timeout_secs)
            .with_base_url(spec.base_url)
            .with_headers(spec.headers)
            .with_reasoning_effort(effort),
    ))
}

#[cfg(feature = "meta")]
fn meta(spec: Spec, client: HttpClient) -> Result<Arc<dyn Provider>> {
    let effort = spec.option("effort");
    Ok(Arc::new(
        crate::meta::MetaProvider::new(client, key(&spec))
            .with_overrides(spec.caps)
            .with_rate_limit(spec.rate_limit.as_ref())
            .with_request_timeout(spec.request_timeout_secs)
            .with_base_url(spec.base_url)
            .with_headers(spec.headers)
            .with_reasoning_effort(effort),
    ))
}

/// Where a sign-in provider's grant is kept, or `None` (with a warning) when
/// the config layer left it out.
///
/// The path comes from the caller rather than being resolved here: the CLI
/// owns where Leviath's files live, and a provider that guessed could look
/// somewhere the CLI never wrote.
#[cfg(any(feature = "openai-subscription", feature = "xai-subscription"))]
fn grant_path(spec: &Spec) -> Option<std::path::PathBuf> {
    let path = spec.option("auth_store_path").map(std::path::PathBuf::from);
    if path.is_none() {
        tracing::warn!(
            provider = %spec.kind,
            "this sign-in provider was configured without a grant location, so it is \
             skipped; this is a bug in leviath rather than in the config"
        );
    }
    path
}

/// Registered without reading the grant, for the reason [`codex`] gives.
#[cfg(feature = "xai-subscription")]
fn grok(
    spec: Spec,
    client: &mut dyn FnMut() -> Result<HttpClient>,
) -> Result<Option<Arc<dyn Provider>>> {
    let Some(store_path) = grant_path(&spec) else {
        return Ok(None);
    };
    let client = client()?;
    let tokens = crate::oauth::OAuthTokenSource::new(
        crate::grok::PROVIDER_NAME,
        store_path,
        Arc::new(crate::oauth::HttpRefresh::new(
            client.clone(),
            &crate::grok::PROFILE,
        )),
    )
    .with_credential_store(spec.credential_store.clone());
    let auth = crate::xai::Auth::Signin(Arc::new(tokens));
    xai_with(spec, client, auth).map(Some)
}

/// The only provider that registers on something other than a key, because
/// it has no key to register on: an address nothing answers on is not a
/// usable provider, and pretending otherwise put it ahead of providers that
/// were actually configured.
#[cfg(feature = "ollama")]
fn ollama(
    spec: Spec,
    client: &mut dyn FnMut() -> Result<HttpClient>,
    reachable: &dyn Fn(&str) -> bool,
) -> Result<Option<Arc<dyn Provider>>> {
    let url = spec
        .base_url
        .unwrap_or_else(|| "http://localhost:11434".to_string());
    if !reachable(&url) {
        tracing::info!(
            base_url = %url,
            "nothing is listening for ollama; not registering it. Start \
             ollama and reload the config to use it."
        );
        return Ok(None);
    }
    Ok(Some(Arc::new(
        crate::OllamaProvider::with_overrides(client()?, url, spec.caps)
            .with_rate_limit(spec.rate_limit.as_ref()),
    )))
}

/// Registered without probing for a grant. The alternative is a
/// synchronous credential-store read during daemon start, which on the
/// keychain backend can raise a GUI prompt. The cost is that a `codex/...`
/// model fails at its first inference with "run `lev auth login codex`"
/// rather than being skipped, and that is the better failure: a silently
/// skipped provider is how a run quietly uses a model nobody chose.
#[cfg(feature = "openai-subscription")]
fn codex(
    spec: Spec,
    client: &mut dyn FnMut() -> Result<HttpClient>,
) -> Result<Option<Arc<dyn Provider>>> {
    let Some(store_path) = grant_path(&spec) else {
        return Ok(None);
    };
    let client = client()?;
    let tokens = crate::oauth::OAuthTokenSource::new(
        crate::codex::PROVIDER_NAME,
        store_path,
        Arc::new(crate::oauth::HttpRefresh::new(
            client.clone(),
            &crate::codex::PROFILE,
        )),
    )
    .with_credential_store(spec.credential_store.clone());
    let originator = spec.option("originator");
    // A separate host from `base_url` in production, so it takes its own
    // option rather than a path under that one.
    let usage_url = spec.option("usage_url");
    let reasoning = (spec.option("effort"), spec.option("verbosity"));
    let replay = spec.options.get("replay_reasoning").map(String::as_str) != Some("false");
    Ok(Some(Arc::new(
        crate::CodexProvider::new(client, Arc::new(tokens))
            .with_overrides(Some(spec.caps))
            .with_rate_limit(spec.rate_limit.as_ref())
            .with_request_timeout(spec.request_timeout_secs)
            .with_base_url(spec.base_url)
            .with_originator(originator)
            .with_usage_url(usage_url)
            .with_reasoning(reasoning.0, reasoning.1)
            .with_reasoning_replay(replay),
    )))
}

#[cfg(all(test, feature = "providers"))]
mod tests;
