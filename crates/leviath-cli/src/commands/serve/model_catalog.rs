//! The model catalogue `GET /api/models` answers from.
//!
//! Listing models means asking every configured provider over the network,
//! and a provider that is slow or down is asked for up to five seconds. That
//! is a fine cost to pay once; it was being paid on every request, on the
//! console's front page, with a fresh provider registry (and a blocking probe
//! for a local Ollama) built each time. The catalogue here is built once per
//! config, kept for a while, and served from memory: a request inside the
//! window gets the list at once, a request after it gets the list it has and
//! starts a refresh behind the answer, and only the very first request for a
//! config waits, bounded, for the providers to speak.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderName;
use leviath_runtime::ProviderRegistry;
use tokio::sync::watch;
use tokio::time::Instant;

use super::types::ModelEntry;
use crate::config::Config;

/// How a registry is built for a config. Injectable so a test can hand in
/// providers that answer, fail, hang or panic without a network.
type RegistryBuilder = Arc<
    dyn Fn(&Config) -> Result<ProviderRegistry, leviath_providers::ProviderError> + Send + Sync,
>;

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

/// The catalogue, shared by every handler through `AppState`.
#[derive(Clone)]
pub(super) struct ModelCatalog {
    inner: Arc<Inner>,
}

struct Inner {
    /// Bookkeeping only; never held across an await.
    state: Mutex<CatalogState>,
    /// The latest listing. A `watch` so a request that has to wait for one
    /// can do so without polling, and so a refresh publishes to every waiter
    /// at once.
    published: watch::Sender<Option<Arc<Snapshot>>>,
    build_registry: RegistryBuilder,
    fresh_for: Duration,
    retry_after: Duration,
    provider_timeout: Duration,
    /// How long a request with no listing to hand waits for one before
    /// answering with nothing.
    cold_wait: Duration,
}

#[derive(Default)]
struct CatalogState {
    /// The registry built for one config, kept so a refresh under the same
    /// config does not rebuild it (and recompile every script provider).
    registry: Option<(Arc<Config>, Arc<ProviderRegistry>)>,
    /// Whether a refresh is running. One at a time: a burst of requests past
    /// the window starts one refresh, not one per request.
    in_flight: bool,
    /// A refresh asked for while one was running, to run after it. The
    /// config may differ from the running one, which is exactly the case
    /// that must not be dropped: `PUT /api/config` while a listing is in
    /// flight.
    queued: Option<Refresh>,
}

/// One refresh's inputs.
struct Refresh {
    config: Arc<Config>,
    /// Whether to ask each provider afresh even where it has an answer.
    prime: bool,
}

/// One listing, as of one refresh.
pub(super) struct Snapshot {
    pub(super) models: Vec<ModelEntry>,
    /// Whether every provider answered. `false` when one timed out, errored
    /// or could not be built; those models are simply absent.
    pub(super) complete: bool,
    built_at: Instant,
    /// The config this was built for; compared by identity, because the
    /// reloader hands out the same `Arc` until the file changes.
    config: Arc<Config>,
}

impl Snapshot {
    /// Seconds since this listing was built.
    pub(super) fn age_secs(&self) -> u64 {
        self.built_at.elapsed().as_secs()
    }

    fn is_for(&self, config: &Arc<Config>) -> bool {
        Arc::ptr_eq(&self.config, config)
    }
}

/// How a request got its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Freshness {
    /// Built within the window.
    Fresh,
    /// Past the window; a refresh is running behind this answer.
    Stale,
    /// Nothing to hand: the providers did not answer inside the wait.
    Cold,
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
        let (published, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(CatalogState::default()),
                published,
                build_registry,
                fresh_for,
                retry_after,
                provider_timeout,
                cold_wait,
            }),
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
    ) -> (Arc<Snapshot>, Freshness) {
        let current = self.inner.published.borrow().clone();
        if let Some(snapshot) = &current
            && snapshot.is_for(&config)
            && !force
        {
            if snapshot.built_at.elapsed() < self.inner.window(snapshot) {
                return (Arc::clone(snapshot), Freshness::Fresh);
            }
            self.request_refresh(config, true);
            return (Arc::clone(snapshot), Freshness::Stale);
        }
        // Nothing for this config yet, or a forced re-read: start one and wait
        // for it, bounded. Subscribing before asking, so a refresh that lands
        // in between is still seen. "New" is by identity rather than by time,
        // so a forced re-read is satisfied by any listing but the one in hand.
        let mut listings = self.inner.published.subscribe();
        self.request_refresh(Arc::clone(&config), force);
        let landed = tokio::time::timeout(
            self.inner.cold_wait,
            listings.wait_for(|listing| {
                listing.as_ref().is_some_and(|s| {
                    s.is_for(&config) && current.as_ref().is_none_or(|had| !Arc::ptr_eq(had, s))
                })
            }),
        )
        .await;
        // The predicate only passes on `Some`, and the channel cannot close
        // while `self.inner` holds the sender, so falling through here means
        // the providers did not answer in time.
        if let Ok(Ok(listing)) = landed
            && let Some(snapshot) = listing.as_ref()
        {
            return (Arc::clone(snapshot), Freshness::Fresh);
        }
        (Arc::new(Snapshot::empty(config)), Freshness::Cold)
    }

    /// Start a refresh for `config` unless one is running, in which case it
    /// runs next. Returns at once; the listing lands on the watch.
    pub(super) fn request_refresh(&self, config: Arc<Config>, prime: bool) {
        let refresh = Refresh { config, prime };
        {
            let mut state = leviath_core::sync::lock(&self.inner.state);
            if state.in_flight {
                // A queued refresh that wanted a prime keeps wanting one.
                let prime = prime || state.queued.as_ref().is_some_and(|q| q.prime);
                state.queued = Some(Refresh {
                    config: refresh.config,
                    prime,
                });
                return;
            }
            state.in_flight = true;
        }
        let inner = Arc::clone(&self.inner);
        // Detached rather than tied to the request: a client that gives up
        // must not cancel the listing every other client is waiting on.
        tokio::spawn(async move {
            inner.run_refreshes(refresh).await;
        });
    }

    /// The latest listing as it lands, for a caller that wants to be told.
    #[cfg(test)]
    pub(super) fn subscribe(&self) -> watch::Receiver<Option<Arc<Snapshot>>> {
        self.inner.published.subscribe()
    }
}

impl Snapshot {
    fn empty(config: Arc<Config>) -> Self {
        Self {
            models: Vec::new(),
            complete: false,
            built_at: Instant::now(),
            config,
        }
    }
}

/// Clears `in_flight` when the refresh task ends, however it ends, so a
/// panic inside a provider cannot leave the catalogue believing a refresh is
/// still running and never start another.
struct InFlight(Arc<Inner>);

impl Drop for InFlight {
    fn drop(&mut self) {
        leviath_core::sync::lock(&self.0.state).in_flight = false;
    }
}

impl Inner {
    fn window(&self, snapshot: &Snapshot) -> Duration {
        if snapshot.complete {
            self.fresh_for
        } else {
            self.retry_after
        }
    }

    /// Run `first`, then whatever was queued while it ran, until nothing is.
    async fn run_refreshes(self: Arc<Self>, first: Refresh) {
        let _in_flight = InFlight(Arc::clone(&self));
        let mut next = Some(first);
        while let Some(refresh) = next.take() {
            self.refresh(refresh).await;
            next = leviath_core::sync::lock(&self.state).queued.take();
        }
    }

    async fn refresh(&self, refresh: Refresh) {
        let (models, complete) = match self.registry_for(&refresh.config).await {
            Some(registry) => collect_models(&registry, self.provider_timeout, refresh.prime).await,
            None => (Vec::new(), false),
        };
        self.published.send_replace(Some(Arc::new(Snapshot {
            models,
            complete,
            built_at: Instant::now(),
            config: refresh.config,
        })));
    }

    /// The registry for `config`: the one already built for it, or a new one
    /// built off the runtime (constructing it probes for a local Ollama with
    /// a blocking connect) and seeded from the capability cache the daemon
    /// writes, so a provider the daemon has already asked lists without a
    /// network call of its own.
    async fn registry_for(&self, config: &Arc<Config>) -> Option<Arc<ProviderRegistry>> {
        if let Some((built_for, registry)) = &leviath_core::sync::lock(&self.state).registry
            && Arc::ptr_eq(built_for, config)
        {
            return Some(Arc::clone(registry));
        }
        let build = Arc::clone(&self.build_registry);
        let for_config = Arc::clone(config);
        let built = super::blocking::blocking(move || build(&for_config)).await;
        let registry = match built {
            Ok(registry) => registry,
            Err(e) => {
                tracing::warn!(error = %e, "could not build the provider registry to list models");
                leviath_core::sync::lock(&self.state).registry = None;
                return None;
            }
        };
        if let Some(path) = leviath_core::paths::capability_cache_path() {
            registry.load_capability_cache(&path);
        }
        let registry = Arc::new(registry);
        leviath_core::sync::lock(&self.state).registry =
            Some((Arc::clone(config), Arc::clone(&registry)));
        Some(registry)
    }
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
                tracing::warn!(
                    provider = %name,
                    timeout_secs = timeout.as_secs(),
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
