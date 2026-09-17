use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use leviath_providers::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider, ProviderError,
};
use leviath_runtime::ProviderRegistry;

use super::*;
use crate::config::Config;

/// How a stub provider answers `list_models`.
#[derive(Clone, Copy)]
enum Answer {
    Lists,
    Fails,
    Hangs,
    Panics,
}

/// A provider that never touches a network, so a listing's shape can be
/// asserted on rather than whatever the machine's providers say today.
struct Stub {
    name: &'static str,
    answer: Answer,
    primes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for Stub {
    async fn infer(&self, _r: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        Err(ProviderError::Other("stub".to_string()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        1000
    }
    fn name(&self) -> &str {
        self.name
    }
    fn capabilities(&self, _m: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn prime_capabilities(&self) -> leviath_providers::Result<()> {
        self.primes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn list_models(&self) -> leviath_providers::Result<Vec<ModelInfo>> {
        match self.answer {
            Answer::Lists => Ok(vec![
                ModelInfo::new(
                    format!("{}-small", self.name),
                    self.name,
                    ModelCapabilities::default(),
                ),
                ModelInfo::new(
                    format!("{}-large", self.name),
                    self.name,
                    ModelCapabilities::default(),
                ),
            ]),
            Answer::Fails => Err(ProviderError::Other("down".to_string())),
            Answer::Hangs => std::future::pending().await,
            Answer::Panics => panic!("this provider's listing panicked"),
        }
    }
}

/// What a test watches: how often the registry was built and how often the
/// stubs were asked to prime.
struct Counters {
    builds: Arc<AtomicUsize>,
    primes: Arc<AtomicUsize>,
}

impl Counters {
    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
    fn primes(&self) -> usize {
        self.primes.load(Ordering::SeqCst)
    }
}

fn builder_of(stubs: Vec<(&'static str, Answer)>) -> (RegistryBuilder, Counters) {
    let builds = Arc::new(AtomicUsize::new(0));
    let primes = Arc::new(AtomicUsize::new(0));
    let counters = Counters {
        builds: Arc::clone(&builds),
        primes: Arc::clone(&primes),
    };
    let builder: RegistryBuilder = Arc::new(move |_config| {
        builds.fetch_add(1, Ordering::SeqCst);
        let mut registry = ProviderRegistry::new();
        for (name, answer) in &stubs {
            registry.register(
                name.to_string(),
                Arc::new(Stub {
                    name,
                    answer: *answer,
                    primes: Arc::clone(&primes),
                }),
            );
        }
        Ok(registry)
    });
    (builder, counters)
}

fn catalog_of(stubs: Vec<(&'static str, Answer)>) -> (ModelCatalog, Counters) {
    let (builder, counters) = builder_of(stubs);
    (ModelCatalog::with_builder(builder), counters)
}

fn config() -> Arc<Config> {
    Arc::new(Config::default())
}

fn ids(snapshot: &Listing) -> Vec<String> {
    snapshot
        .value
        .iter()
        .map(|m| format!("{}/{}", m.provider, m.id))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_first_request_waits_for_the_providers_and_later_ones_are_served_from_memory() {
    let (catalog, counters) = catalog_of(vec![("beta", Answer::Lists), ("alpha", Answer::Lists)]);
    let config = config();

    let (first, how) = catalog.models(Arc::clone(&config), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(first.complete);
    assert_eq!(
        ids(&first),
        [
            "alpha/alpha-large",
            "alpha/alpha-small",
            "beta/beta-large",
            "beta/beta-small"
        ],
        "sorted by provider then id"
    );
    assert_eq!(counters.builds(), 1);
    assert_eq!(
        counters.primes(),
        0,
        "a first listing asks, it does not prime"
    );

    let (second, how) = catalog.models(config, false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(Arc::ptr_eq(&first, &second), "served from memory");
    assert_eq!(counters.builds(), 1);
}

#[tokio::test(start_paused = true)]
async fn past_the_window_the_listing_in_hand_is_served_and_refreshed_behind_it() {
    let (catalog, counters) = catalog_of(vec![("alpha", Answer::Lists), ("beta", Answer::Lists)]);
    let config = config();
    let (first, _) = catalog.models(Arc::clone(&config), false).await;

    tokio::time::advance(FRESH_FOR + Duration::from_secs(1)).await;
    let mut listings = catalog.subscribe();
    let (served, how) = catalog.models(Arc::clone(&config), false).await;
    assert_eq!(how, Freshness::Stale);
    assert!(
        Arc::ptr_eq(&first, &served),
        "the old listing answers at once"
    );
    assert_eq!(served.age_secs(), FRESH_FOR.as_secs() + 1);

    listings.changed().await.unwrap();
    let (fresh, how) = catalog.models(config, false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!Arc::ptr_eq(&first, &fresh));
    assert_eq!(fresh.age_secs(), 0);
    assert_eq!(
        counters.primes(),
        2,
        "a refresh behind an answer primes each provider"
    );
    assert_eq!(
        counters.builds(),
        1,
        "the registry is kept for an unchanged config"
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_that_fails_or_panics_leaves_a_listing_that_says_so() {
    let (catalog, _) = catalog_of(vec![
        ("ok", Answer::Lists),
        ("down", Answer::Fails),
        ("boom", Answer::Panics),
    ]);
    let config = config();
    let (listing, how) = catalog.models(Arc::clone(&config), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!listing.complete);
    assert_eq!(ids(&listing), ["ok/ok-large", "ok/ok-small"]);

    // Served for the shorter window, then refreshed.
    tokio::time::advance(RETRY_AFTER / 2).await;
    assert_eq!(
        catalog.models(Arc::clone(&config), false).await.1,
        Freshness::Fresh
    );
    tokio::time::advance(RETRY_AFTER).await;
    assert_eq!(catalog.models(config, false).await.1, Freshness::Stale);
}

#[tokio::test(start_paused = true)]
async fn a_provider_that_never_answers_is_given_up_on_within_the_bound() {
    let (catalog, _) = catalog_of(vec![("ok", Answer::Lists), ("slow", Answer::Hangs)]);
    let started = tokio::time::Instant::now();
    let (listing, how) = catalog.models(config(), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!listing.complete);
    assert_eq!(ids(&listing), ["ok/ok-large", "ok/ok-small"]);
    assert!(started.elapsed() <= PROVIDER_TIMEOUT + Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn a_request_with_nothing_to_hand_answers_empty_when_the_providers_are_too_slow() {
    let (builder, _) = builder_of(vec![("slow", Answer::Hangs)]);
    let catalog = ModelCatalog::with_timings(
        builder,
        FRESH_FOR,
        RETRY_AFTER,
        PROVIDER_TIMEOUT,
        Duration::from_secs(1),
    );
    let config = config();
    let mut listings = catalog.subscribe();
    let (empty, how) = catalog.models(Arc::clone(&config), false).await;
    assert_eq!(how, Freshness::Cold);
    assert!(empty.value.is_empty());
    assert!(!empty.complete);

    // The refresh it started still lands, and the next request has it.
    listings.changed().await.unwrap();
    let (listing, how) = catalog.models(config, false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!listing.complete);
}

#[tokio::test(start_paused = true)]
async fn a_registry_that_cannot_be_built_lists_nothing_and_says_so() {
    let catalog = ModelCatalog::with_builder(Arc::new(|_config| {
        Err(ProviderError::Other(
            "no https client on this machine".to_string(),
        ))
    }));
    let (listing, how) = catalog.models(config(), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(listing.value.is_empty());
    assert!(!listing.complete);
}

#[tokio::test(start_paused = true)]
async fn a_new_config_gets_its_own_listing_and_registry() {
    let (catalog, counters) = catalog_of(vec![("alpha", Answer::Lists)]);
    let (config_a, config_b) = (config(), config());

    let (for_a, _) = catalog.models(Arc::clone(&config_a), false).await;
    assert_eq!(counters.builds(), 1);
    let (for_b, how) = catalog.models(Arc::clone(&config_b), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!Arc::ptr_eq(&for_a, &for_b));
    assert!(for_b.is_for(&config_b));
    assert_eq!(counters.builds(), 2, "a registry per config");
}

#[tokio::test(start_paused = true)]
async fn a_forced_refresh_asks_the_providers_again_and_waits_for_them() {
    let (catalog, counters) = catalog_of(vec![("alpha", Answer::Lists)]);
    let config = config();
    let (first, _) = catalog.models(Arc::clone(&config), false).await;
    assert_eq!(counters.primes(), 0);

    let (forced, how) = catalog.models(config, true).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(
        !Arc::ptr_eq(&first, &forced),
        "a new listing, not the remembered one"
    );
    assert_eq!(counters.primes(), 1, "forcing primes");
}

#[tokio::test(start_paused = true)]
async fn a_refresh_asked_for_during_another_runs_after_it_for_the_config_asked_last() {
    let (catalog, counters) = catalog_of(vec![("slow", Answer::Hangs)]);
    let (config_a, config_b, config_c) = (config(), config(), config());
    let mut listings = catalog.subscribe();

    catalog.request_refresh(Arc::clone(&config_a), false);
    catalog.request_refresh(Arc::clone(&config_b), true);
    catalog.request_refresh(Arc::clone(&config_c), false);

    listings.changed().await.unwrap();
    assert!(listings.borrow().as_ref().unwrap().is_for(&config_a));
    listings.changed().await.unwrap();
    assert!(
        listings.borrow().as_ref().unwrap().is_for(&config_c),
        "the last config asked for wins; the one before it was superseded"
    );
    assert_eq!(
        counters.primes(),
        1,
        "the queued prime is kept when a later ask replaces it"
    );
    assert_eq!(
        counters.builds(),
        2,
        "one registry per config that was listed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refresh_that_panics_does_not_wedge_the_catalogue() {
    let builds = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&builds);
    let catalog = ModelCatalog::with_timings(
        Arc::new(move |_config| {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("the registry builder blew up")
        }),
        FRESH_FOR,
        RETRY_AFTER,
        PROVIDER_TIMEOUT,
        Duration::from_secs(1),
    );
    let config = config();
    let (_, how) = catalog.models(Arc::clone(&config), false).await;
    assert_eq!(how, Freshness::Cold, "nothing landed");

    // Had `in_flight` stayed set, this ask would be queued behind a refresh
    // that is never coming and the builder would not run again.
    let (_, how) = catalog.models(config, false).await;
    assert_eq!(how, Freshness::Cold);
    assert_eq!(builds.load(Ordering::SeqCst), 2);
}

/// The daemon primes every provider at start-up and writes what it learned to
/// a shared cache. A server that reads it lists those models without a
/// network call of its own, which is what makes a first listing quick on a
/// machine whose daemon is up.
#[tokio::test]
async fn a_listing_is_seeded_from_the_capability_cache_the_daemon_wrote() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let path = leviath_core::paths::capability_cache_path().expect("a home");
        let mut cache = leviath_providers::CapabilityCache::new(1);
        let mut learned = std::collections::BTreeMap::new();
        learned.insert(
            "claude-seeded".to_string(),
            leviath_providers::LearnedModel {
                max_context_tokens: Some(123_456),
                ..Default::default()
            },
        );
        cache.set("anthropic", learned);
        cache.save(&path).unwrap();

        // Keyed, so it registers; a dead address, so nothing else could have
        // answered.
        let config = Arc::new(Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("test-key".to_string()),
                anthropic_base_url: Some("http://127.0.0.1:1".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        });
        let (listing, _) = ModelCatalog::default().models(config, false).await;
        let seeded = listing
            .value
            .iter()
            .find(|m| m.provider == "anthropic" && m.id == "claude-seeded")
            .expect("the seeded model is listed");
        assert!(seeded.learned);
        assert_eq!(seeded.max_context_tokens, 123_456);
    })
    .await;
}

// ─── the route ──────────────────────────────────────────────────────────────

fn state_with(catalog: ModelCatalog) -> super::super::types::AppState {
    let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    state.caches = super::super::caches::ServeCaches {
        model_catalog: catalog,
        ..Default::default()
    };
    state
}

#[tokio::test(start_paused = true)]
async fn the_route_says_how_old_and_how_complete_its_answer_is() {
    use axum::response::Json;

    let (catalog, counters) = catalog_of(vec![("ok", Answer::Lists), ("down", Answer::Fails)]);
    let state = state_with(catalog);
    let (headers, Json(models)) =
        super::super::config::models_with(&state, &Default::default()).await;
    assert_eq!(models.len(), 2);
    assert_eq!(headers.get(CATALOG_AGE).unwrap(), "0");
    assert_eq!(headers.get(CATALOG_COMPLETE).unwrap(), "false");

    tokio::time::advance(Duration::from_secs(7)).await;
    let (headers, _) = super::super::config::models_with(&state, &Default::default()).await;
    assert_eq!(headers.get(CATALOG_AGE).unwrap(), "7");

    // `?refresh=1` asks again and answers with the new listing.
    let (headers, _) = super::super::config::models_with(
        &state,
        &super::super::types::ModelsQuery {
            provider: None,
            refresh: true,
        },
    )
    .await;
    assert_eq!(headers.get(CATALOG_AGE).unwrap(), "0");
    assert_eq!(counters.primes(), 2);
}
