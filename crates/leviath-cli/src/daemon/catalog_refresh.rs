//! Keeping the daemon's provider model lists live.
//!
//! A gateway's model list is what a bare model name in a blueprint is judged
//! against. The daemon reads each list at start and writes it to the
//! capability cache. A list that cannot be read then (the gateway, or a proxy
//! in front of it, is down) is filled from the cache instead when there is a
//! copy, so runs resolve straight away against the last list that was read;
//! with no copy the list stays unread, and a new run whose stage needs it is
//! refused. A run resumed from its run file does not need it: it chose its
//! models when it started.
//!
//! Either way the live list is still owed, and this asks for it: soon and
//! often while it is owed, backing off while the gateway stays down, and then
//! every few hours for every provider, so a model a gateway added since the
//! daemon started is served without a restart.

use std::sync::Arc;
use std::time::Duration;

use crate::daemon::config_reload::ConfigReloader;
use crate::daemon::provider_reload::ProviderReload;

/// How often the lists are asked for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pacing {
    /// The first wait after a list could not be read, and the wait again
    /// once one answers.
    pub retry_min: Duration,
    /// The longest wait between two asks while a list is still owed.
    pub retry_max: Duration,
    /// How long a daemon with every list in hand waits before asking every
    /// provider again.
    pub refresh_every: Duration,
}

impl Pacing {
    /// The daemon's pacing. Five seconds first, because a proxy that is
    /// restarting is back within seconds and the runs refused meanwhile
    /// should not wait long; never longer than a minute between asks while a
    /// list is owed; every
    /// six hours otherwise, which is often enough to pick up a new model the
    /// day it appears and costs one small `GET` per provider.
    pub(crate) const DAEMON: Pacing = Pacing {
        retry_min: Duration::from_secs(5),
        retry_max: Duration::from_secs(60),
        refresh_every: Duration::from_secs(6 * 60 * 60),
    };

    /// The wait before the next ask: `backoff` while a list is owed,
    /// otherwise what is left of the refresh interval.
    fn next_wait(&self, awaiting: bool, backoff: Duration, since_full: Duration) -> Duration {
        match awaiting {
            true => backoff,
            false => self.refresh_every.saturating_sub(since_full),
        }
    }

    /// The backoff after an ask that read `read`: back to the start when
    /// something answered, doubled (up to the ceiling) when nothing did.
    fn next_backoff(&self, read: &[String], backoff: Duration) -> Duration {
        match read.is_empty() {
            true => (backoff * 2).min(self.retry_max),
            false => self.retry_min,
        }
    }
}

/// Fill every unread list the capability cache holds a copy of, and note every
/// list not read live, so the refresher asks for it.
///
/// Called once at start, after the prime and after the cache was written from
/// it: a copy restored here must not be written back as though the provider
/// had answered. Answers the providers still unread afterwards: the ones with
/// no copy at all.
pub(crate) fn settle_at_start(reload: &ProviderReload) -> Vec<String> {
    let registry = reload.registry();
    let unread = registry.unread_catalogs();
    if unread.is_empty() {
        return unread;
    }
    reload.await_live(&unread);
    let restored = reload
        .cache_path()
        .map(|path| registry.restore_from_cache(path, &unread))
        .unwrap_or_default();
    if !restored.is_empty() {
        let names = restored.join(", ");
        tracing::warn!(
            providers = %names,
            "could not read these providers' model lists at start, so they answer from the \
             copy in the capability cache until the live list comes back; it is asked for \
             again in the background"
        );
    }
    let still = registry.unread_catalogs();
    if !still.is_empty() {
        let names = still.join(", ");
        tracing::error!(
            providers = %names,
            "started without these providers' model lists and with no cached copy; a new \
             run whose stage names a model by bare name that only they could serve is \
             refused until the list is read; it is asked for again in the background and \
             before each spawn"
        );
    }
    still
}

/// Ask for the owed lists, and every list now and then, for as long as the
/// daemon runs.
pub(crate) fn spawn(
    runtime: &tokio::runtime::Handle,
    reload: Arc<ProviderReload>,
    reloader: Arc<ConfigReloader>,
    pacing: Pacing,
) -> tokio::task::JoinHandle<()> {
    runtime.spawn(async move {
        let mut backoff = pacing.retry_min;
        let mut since_full = tokio::time::Instant::now();
        loop {
            let awaiting = !reload.awaiting_live().is_empty();
            tokio::time::sleep(pacing.next_wait(awaiting, backoff, since_full.elapsed())).await;
            let config = reloader.current();
            let read = match awaiting {
                true => reload.read_awaiting(&config).await,
                false => {
                    since_full = tokio::time::Instant::now();
                    reload.read_all(&config).await
                }
            };
            backoff = pacing.next_backoff(&read, backoff);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use leviath_runtime::ProviderRegistry;

    /// An OpenRouter at `url`, which nothing has asked for its list yet.
    fn gateway(url: &str) -> Arc<leviath_providers::OpenRouterProvider> {
        Arc::new(
            leviath_providers::OpenRouterProvider::new(
                leviath_providers::provider::build_http_client(None).unwrap(),
                "sk-or-test".to_string(),
            )
            .with_base_url(Some(url.to_string())),
        )
    }

    /// A cache holding a copy of one gateway's list, written where the
    /// daemon would look for it.
    fn cache_with(provider: &str, model: &str) {
        let path = leviath_core::paths::capability_cache_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut cache = leviath_providers::CapabilityCache::new(1);
        cache.set(
            provider,
            [(
                model.to_string(),
                leviath_providers::LearnedModel::default(),
            )]
            .into_iter()
            .collect(),
        );
        cache.save(&path).unwrap();
    }

    /// At start, an unread list with a cached copy answers from the copy; one
    /// without stays unread; both are owed a live read. A daemon with every
    /// list in hand has nothing to settle.
    #[tokio::test]
    async fn settling_fills_unread_lists_from_the_cache_and_owes_a_live_read() {
        crate::config::with_isolated_config_path_async("catalog_settle", |_| async move {
            cache_with("openrouter", "anthropic/claude-opus-5");
            let mut registry = ProviderRegistry::new();
            registry.register("openrouter".to_string(), gateway("http://127.0.0.1:9"));
            registry.register("uncached".to_string(), gateway("http://127.0.0.1:9"));
            let reload = crate::daemon::provider_reload::for_daemon(&Config::default(), registry);

            assert_eq!(settle_at_start(&reload), ["uncached"]);
            assert_eq!(reload.awaiting_live(), ["openrouter", "uncached"]);
            assert_eq!(reload.registry().unread_catalogs(), ["uncached"]);

            let settled = crate::daemon::provider_reload::for_daemon(
                &Config::default(),
                ProviderRegistry::new(),
            );
            assert!(settle_at_start(&settled).is_empty());
            assert!(settled.awaiting_live().is_empty());
        })
        .await;
    }

    /// The refresher asks for an owed list until it answers, and writes it to
    /// the cache as a list the gateway gave.
    /// Once nothing is owed it asks every provider again on the refresh
    /// interval, and one that does not answer then is owed again.
    #[tokio::test]
    async fn the_refresher_reads_an_owed_list_and_then_keeps_every_list_fresh() {
        crate::config::with_isolated_config_path_async("catalog_refresher", |_| async move {
            let listing = br#"{"data":[{"id":"anthropic/claude-opus-5","context_length":200000}]}"#;
            let (url, _) = leviath_testkit::spawn_mock_sequence(vec![
                (503, "Service Unavailable", b"down".to_vec()),
                (200, "OK", listing.to_vec()),
            ])
            .await;
            let mut registry = ProviderRegistry::new();
            registry.register("openrouter".to_string(), gateway(&url));
            let reload = crate::daemon::provider_reload::for_daemon(&Config::default(), registry);
            assert!(
                reload.read_awaiting(&Config::default()).await.is_empty(),
                "nothing owed"
            );
            reload.await_live(&["openrouter".to_string()]);
            reload.await_live(&["openrouter".to_string()]);
            assert_eq!(reload.awaiting_live(), ["openrouter"]);
            let reloader = Arc::new(ConfigReloader::new(
                crate::config::Config::config_path(),
                Config::default(),
            ));
            let pacing = Pacing {
                retry_min: Duration::from_millis(10),
                retry_max: Duration::from_millis(20),
                refresh_every: Duration::from_millis(50),
            };
            let task = spawn(
                &tokio::runtime::Handle::current(),
                reload.clone(),
                reloader,
                pacing,
            );

            // Waits on the cache, which is written just after the list is
            // installed: a slow runner can see the list before the file.
            let path = reload.cache_path().unwrap();
            leviath_testkit::wait_until("the cache was written", || {
                leviath_providers::CapabilityCache::load(path)
                    .is_some_and(|cache| cache.check("openrouter").is_some())
            })
            .await;
            assert!(
                reload.registry().unread_catalogs().is_empty(),
                "the list came back"
            );
            let cache =
                leviath_providers::CapabilityCache::load(path).expect("the cache was written");
            assert_eq!(
                cache.check("openrouter").map(|c| &c.outcome),
                Some(&leviath_providers::CheckOutcome::Reachable { models: 1 })
            );

            // The full refresh finds the gateway gone again: the list it has
            // is kept, and the live one is owed once more.
            leviath_testkit::wait_until("the gateway is owed again", || {
                !reload.awaiting_live().is_empty()
            })
            .await;
            task.abort();
            assert_eq!(reload.awaiting_live(), ["openrouter"]);
            assert!(
                reload.registry().unread_catalogs().is_empty(),
                "the list read is kept"
            );
        })
        .await;
    }

    #[test]
    fn the_wait_backs_off_while_a_list_is_owed_and_refreshes_otherwise() {
        let pacing = Pacing::DAEMON;
        let min = pacing.retry_min;
        assert_eq!(pacing.next_wait(true, min, Duration::ZERO), min);
        assert_eq!(
            pacing.next_wait(false, min, Duration::from_secs(60)),
            pacing.refresh_every - Duration::from_secs(60)
        );
        assert_eq!(
            pacing.next_wait(false, min, pacing.refresh_every * 2),
            Duration::ZERO,
            "an overdue refresh goes at once"
        );
        assert_eq!(pacing.next_backoff(&[], min), min * 2);
        assert_eq!(
            pacing.next_backoff(&[], pacing.retry_max),
            pacing.retry_max,
            "never longer than the ceiling"
        );
        assert_eq!(
            pacing.next_backoff(&["openrouter".to_string()], pacing.retry_max),
            min
        );
    }
}
