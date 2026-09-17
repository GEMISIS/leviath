//! The model catalogue `GET /api/models` answers from.
//!
//! Listing models means asking every configured provider over the network,
//! and a provider that is slow or down is asked for up to five seconds. That
//! is a fine cost to pay once; it was being paid on every request, on the
//! console's front page, with a fresh provider registry (and a blocking probe
//! for a local Ollama) built each time. The catalogue here is built once per
//! config, kept for a while, and served from memory: a request inside the
//! window gets the list at once, a request after it gets the list it has and
//! starts a refresh behind it, and only the very first request for a config
//! waits, bounded, for the providers to speak.
//!
//! The keeping and the refreshing are [`Refreshing`](super::refreshing); what
//! is here is what is the catalogue's own - the provider registry it asks, and
//! how a model is spelled on the wire.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderName;
use leviath_runtime::ProviderRegistry;

/// Re-exported so a caller that reads a listing does not also have to name
/// the module the keeping lives in.
pub(super) use super::refreshing::Freshness;
use super::refreshing::{Build, Cached, Refreshing};
use super::types::ModelEntry;
use crate::config::Config;

/// How a registry is built for a config. Injectable so a test can hand in
/// providers that answer, fail, hang or panic without a network.
type RegistryBuilder = Arc<
    dyn Fn(&Config) -> Result<ProviderRegistry, leviath_providers::ProviderError> + Send + Sync,
>;

/// The registry built for one config, kept so a refresh under the same config
/// does not rebuild it (and recompile every script provider).
type RegistryMemo = Arc<Mutex<Option<(Arc<Config>, Arc<ProviderRegistry>)>>>;

/// How long a complete listing is served without asking the providers again.
const FRESH_FOR: Duration = Duration::from_secs(15 * 60);
/// How long a listing missing a provider's answer is served before another
/// try: sooner than a complete one, because the gap is likely a blip.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// How long each provider gets to describe its models. Shorter than the
/// daemon's start-up prime: this bounds a page load.
pub(super) const PROVIDER_TIMEOUT: Duration = Duration::from_secs(5);

/// Response header: seconds since the listing was built.
pub(super) const CATALOG_AGE: HeaderName = HeaderName::from_static("x-leviath-catalog-age");
/// Response header: whether every provider answered when the listing was built.
pub(super) const CATALOG_COMPLETE: HeaderName =
    HeaderName::from_static("x-leviath-catalog-complete");

/// One listing, as of one refresh.
pub(super) type Listing = Cached<Arc<Config>, Vec<ModelEntry>>;

/// The catalogue, shared by every handler through `AppState`.
#[derive(Clone)]
pub(super) struct ModelCatalog {
    listings: Refreshing<Arc<Config>, Vec<ModelEntry>>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::with_builder(Arc::new(|config| {
            crate::commands::run::session::build_provider_registry_from_config_with(
                config,
                &leviath_providers::provider::build_http_client,
            )
        }))
    }
}

impl ModelCatalog {
    pub(super) fn with_builder(build_registry: RegistryBuilder) -> Self {
        Self::with_timings(
            build_registry,
            FRESH_FOR,
            RETRY_AFTER,
            PROVIDER_TIMEOUT,
            PROVIDER_TIMEOUT + Duration::from_secs(1),
        )
    }

    fn with_timings(
        build_registry: RegistryBuilder,
        fresh_for: Duration,
        retry_after: Duration,
        provider_timeout: Duration,
        cold_wait: Duration,
    ) -> Self {
        // The memo lives in the closure rather than beside the listings: it is
        // an input to building one, not part of the answer, and nothing outside
        // a refresh has any business reading it.
        let memo: RegistryMemo = Arc::new(Mutex::new(None));
        let build: Build<Arc<Config>, Vec<ModelEntry>> = Arc::new(move |config, prime| {
            let (build_registry, memo) = (Arc::clone(&build_registry), Arc::clone(&memo));
            Box::pin(async move {
                match registry_for(&memo, &build_registry, &config).await {
                    Some(registry) => collect_models(&registry, provider_timeout, prime).await,
                    None => (Vec::new(), false),
                }
            })
        });
        Self {
            listings: Refreshing::new(build, fresh_for, retry_after, cold_wait),
        }
    }

    /// The listing for `config`, and how it was got.
    ///
    /// `force` asks the providers again and waits for their answer, for a
    /// settings page that has just changed something and wants to show the
    /// result rather than the memory of the old one.
    pub(super) async fn models(
        &self,
        config: Arc<Config>,
        force: bool,
    ) -> (Arc<Listing>, Freshness) {
        self.listings.get(config, force).await
    }

    /// Start a refresh for `config` unless one is running, in which case it
    /// runs next. Returns at once; the listing lands behind the caller.
    pub(super) fn request_refresh(&self, config: Arc<Config>, prime: bool) {
        self.listings.request_refresh(config, prime);
    }

    /// The latest listing as it lands, for a caller that wants to be told.
    #[cfg(test)]
    pub(super) fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<Arc<Listing>>> {
        self.listings.subscribe()
    }
}

/// The registry for `config`: the one already built for it, or a new one built
/// off the runtime (constructing it probes for a local Ollama with a blocking
/// connect) and seeded from the capability cache the daemon writes, so a
/// provider the daemon has already asked lists without a network call of its
/// own.
async fn registry_for(
    memo: &RegistryMemo,
    build_registry: &RegistryBuilder,
    config: &Arc<Config>,
) -> Option<Arc<ProviderRegistry>> {
    if let Some((built_for, registry)) = &*leviath_core::sync::lock(memo)
        && Arc::ptr_eq(built_for, config)
    {
        return Some(Arc::clone(registry));
    }
    let build = Arc::clone(build_registry);
    let for_config = Arc::clone(config);
    let built = super::blocking::blocking(move || build(&for_config)).await;
    let registry = match built {
        Ok(registry) => registry,
        Err(e) => {
            tracing::warn!(error = %e, "could not build the provider registry to list models");
            *leviath_core::sync::lock(memo) = None;
            return None;
        }
    };
    // `is_some_and` rather than `if let`: a home that does not resolve is
    // `false` flowing through, not an else arm no test can reach.
    let seeded = leviath_core::paths::capability_cache_path()
        .is_some_and(|path| registry.load_capability_cache(&path));
    tracing::debug!(
        seeded,
        "built the provider registry for the model catalogue"
    );
    let registry = Arc::new(registry);
    *leviath_core::sync::lock(memo) = Some((Arc::clone(config), Arc::clone(&registry)));
    Some(registry)
}

/// Every model every provider in `registry` reports, as the API spells them,
/// sorted by provider then id so two listings of one machine read the same.
///
/// The providers are asked side by side, each within `timeout`; one that does
/// not answer is left out and the listing says so through the returned flag.
/// `prime` asks each provider afresh first; without it a provider answers
/// from what it already knows and asks only when it knows nothing.
pub(super) async fn collect_models(
    registry: &Arc<ProviderRegistry>,
    timeout: Duration,
    prime: bool,
) -> (Vec<ModelEntry>, bool) {
    if prime {
        registry.prime_capabilities(timeout, &[]).await;
    }
    let mut in_flight = tokio::task::JoinSet::new();
    for name in registry.resolvable_names() {
        // A script name is a candidate until it compiles; one that will not
        // load is skipped, with its own log line already written by the layer.
        let Some(provider) = registry.get(&name) else {
            continue;
        };
        in_flight.spawn(async move {
            let listed = tokio::time::timeout(timeout, provider.list_models()).await;
            (name, provider, listed)
        });
    }
    let mut models = Vec::new();
    let mut complete = true;
    while let Some(joined) = in_flight.join_next().await {
        let Ok((name, provider, listed)) = joined else {
            // A panic inside one provider's listing is that provider's alone.
            complete = false;
            continue;
        };
        match listed {
            Ok(Ok(list)) => {
                for m in list {
                    let mime = provider.mime(&m.id);
                    models.push(ModelEntry {
                        input_types: mime.input,
                        output_types: mime.output,
                        id: m.id,
                        provider: m.provider,
                        display_name: m.display_name,
                        max_context_tokens: m.capabilities.max_context_tokens,
                        max_output_tokens: m.capabilities.max_output_tokens,
                        limits_source: super::config::limits_source_label(
                            m.capabilities.limits_source,
                        ),
                        supports_tools: m.capabilities.supports_tools,
                        supports_temperature: m.capabilities.supports_temperature,
                        learned: m.learned,
                        released: m.released,
                        retires: m.retires,
                        pricing: m.pricing,
                    });
                }
            }
            Ok(Err(e)) => {
                complete = false;
                tracing::warn!(provider = %name, error = %e, "could not list this provider's models");
            }
            Err(_) => {
                complete = false;
                // Bound first rather than computed inside the field: a method
                // call in a structured field runs only when the callsite is
                // enabled, which a coverage run cannot count on.
                let timeout_secs = timeout.as_secs();
                tracing::warn!(
                    provider = %name,
                    timeout_secs,
                    "timed out listing this provider's models"
                );
            }
        }
    }
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    (models, complete)
}

#[cfg(test)]
#[path = "model_catalog_tests.rs"]
mod tests;
