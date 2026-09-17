//! Tests for the quota reading `GET /api/providers?quota=true` answers from.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::test_fixtures::{QuotaAnswer, Subscription, quota_report, subscriptions};

/// How many times the accounts were actually asked, so a test can tell a
/// reading served from memory from one that went and looked.
#[derive(Clone, Default)]
struct Reads(Arc<AtomicUsize>);

impl Reads {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// A config with both subscriptions switched on. A fresh `Arc` every call, so
/// two configs are two keys.
fn enabled() -> Arc<Config> {
    let mut config = Config::default();
    config.providers.codex_enabled = true;
    config.providers.grok_enabled = true;
    Arc::new(config)
}

/// The accounts to ask, by id, with no address on the grant.
fn asking(config: Arc<Config>, ids: &[&str]) -> Accounts {
    Accounts::new(
        config,
        std::path::PathBuf::from("provider-auth.json"),
        ids.iter()
            .map(|id| Asked {
                id: (*id).to_string(),
                account: None,
            })
            .collect(),
    )
}

/// Clients for exactly the accounts a reading was asked for, answering as
/// `answers` says. The filter matters: the live builder makes one client per
/// signed-in account and none for the rest, so a fake that answered for
/// everybody would hide the very thing the account set is in the key for.
fn registry_of(
    answers: &[(&'static str, QuotaAnswer)],
    accounts: &Accounts,
) -> Option<ProviderRegistry> {
    Some(subscriptions(
        answers
            .iter()
            .filter(|(name, _)| accounts.asked.iter().any(|a| a.id == *name))
            .map(|(name, answer)| Subscription {
                name,
                answer: answer.clone(),
            })
            .collect(),
    ))
}

/// A cache whose accounts answer as `answers` says, and the read counter.
fn cache_of(answers: Vec<(&'static str, QuotaAnswer)>) -> (QuotaCache, Reads) {
    let reads = Reads::default();
    let counted = reads.clone();
    let cache = QuotaCache::with_builder(Arc::new(move |accounts| {
        counted.0.fetch_add(1, Ordering::SeqCst);
        registry_of(&answers, accounts)
    }));
    (cache, reads)
}

#[tokio::test(start_paused = true)]
async fn the_first_read_asks_the_accounts_and_the_next_is_served_from_memory() {
    let (cache, reads) = cache_of(vec![("codex", QuotaAnswer::Report(quota_report(false)))]);
    let config = enabled();
    let (first, how) = cache
        .report(asking(Arc::clone(&config), &["codex"]), false)
        .await;
    assert_eq!(how, Freshness::Fresh);
    assert!(first.complete);
    assert_eq!(first.value.len(), 1);
    assert_eq!(first.value[0].provider, "codex");
    assert_eq!(reads.count(), 1);

    let (second, how) = cache.report(asking(config, &["codex"]), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(Arc::ptr_eq(&first, &second), "read again inside the window");
    assert_eq!(reads.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn past_the_window_the_reading_in_hand_is_served_and_taken_again_behind_it() {
    let (cache, reads) = cache_of(vec![("codex", QuotaAnswer::Report(quota_report(false)))]);
    let config = enabled();
    let (first, _) = cache
        .report(asking(Arc::clone(&config), &["codex"]), false)
        .await;
    tokio::time::advance(FRESH_FOR + Duration::from_secs(1)).await;

    let (served, how) = cache
        .report(asking(Arc::clone(&config), &["codex"]), false)
        .await;
    assert_eq!(how, Freshness::Stale);
    assert!(
        Arc::ptr_eq(&first, &served),
        "the reading in hand is served"
    );
    assert_eq!(served.age_secs(), FRESH_FOR.as_secs() + 1);

    tokio::task::yield_now().await;
    let (fresh, how) = cache.report(asking(config, &["codex"]), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert_eq!(fresh.age_secs(), 0);
    assert_eq!(reads.count(), 2);
}

#[tokio::test(start_paused = true)]
async fn an_account_that_could_not_be_read_is_tried_again_sooner() {
    let (cache, _) = cache_of(vec![
        ("codex", QuotaAnswer::Report(quota_report(false))),
        ("grok", QuotaAnswer::Fails("HTTP 401".into())),
    ]);
    let config = enabled();
    let (reading, how) = cache
        .report(asking(Arc::clone(&config), &["codex", "grok"]), false)
        .await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!reading.complete, "one account could not be read");
    assert_eq!(reading.value[1].report.as_ref().unwrap_err(), "HTTP 401");

    tokio::time::advance(RETRY_AFTER / 2).await;
    assert_eq!(
        cache
            .report(asking(Arc::clone(&config), &["codex", "grok"]), false)
            .await
            .1,
        Freshness::Fresh
    );
    tokio::time::advance(RETRY_AFTER).await;
    assert_eq!(
        cache
            .report(asking(config, &["codex", "grok"]), false)
            .await
            .1,
        Freshness::Stale,
        "a failed account is retried sooner than a whole reading is refreshed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_silent_account_is_given_up_on_and_the_rest_of_the_reading_stands() {
    let (cache, _) = cache_of(vec![
        ("codex", QuotaAnswer::Report(quota_report(false))),
        ("grok", QuotaAnswer::Hangs),
    ]);
    let started = tokio::time::Instant::now();
    let (reading, how) = cache
        .report(asking(enabled(), &["codex", "grok"]), false)
        .await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!reading.complete);
    assert!(reading.value[0].report.is_ok(), "the account that spoke");
    assert_eq!(
        reading.value[1].report.as_ref().unwrap_err(),
        "the account did not answer within 5s"
    );
    assert!(started.elapsed() <= ACCOUNT_TIMEOUT + Duration::from_secs(1));
}

/// The one a config-keyed cache would get wrong: the grant file is not the
/// config, so a sign-in has to rotate the key on its own.
#[tokio::test(start_paused = true)]
async fn a_sign_in_is_not_served_the_reading_taken_before_it() {
    let (cache, reads) = cache_of(vec![("codex", QuotaAnswer::Report(quota_report(false)))]);
    let config = enabled();
    let (none, _) = cache.report(asking(Arc::clone(&config), &[]), false).await;
    assert!(none.value.is_empty());

    let (after, _) = cache
        .report(asking(Arc::clone(&config), &["codex"]), false)
        .await;
    assert!(!Arc::ptr_eq(&none, &after), "a sign-in is a new reading");
    assert_eq!(reads.count(), 2);

    // And so is signing out and back in as somebody else.
    let mut elsewhere = asking(config, &["codex"]);
    elsewhere.asked[0].account = Some("somebody@example.com".into());
    let (renamed, _) = cache.report(elsewhere, false).await;
    assert!(!Arc::ptr_eq(&after, &renamed));
    assert_eq!(reads.count(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_forced_read_asks_the_accounts_again() {
    let (cache, reads) = cache_of(vec![("codex", QuotaAnswer::Report(quota_report(false)))]);
    let config = enabled();
    let (first, _) = cache
        .report(asking(Arc::clone(&config), &["codex"]), false)
        .await;
    let (forced, how) = cache.report(asking(config, &["codex"]), true).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!Arc::ptr_eq(&first, &forced), "asked again");
    assert_eq!(reads.count(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_reading_with_nothing_to_hand_answers_empty_when_the_accounts_are_too_slow() {
    // The wait is shorter than the account's own bound, so nothing has landed
    // by the time the request has to answer.
    let cache = QuotaCache::with_timings(
        Arc::new(|accounts| registry_of(&[("codex", QuotaAnswer::Hangs)], accounts)),
        FRESH_FOR,
        RETRY_AFTER,
        ACCOUNT_TIMEOUT,
        Duration::from_secs(1),
    );
    let config = enabled();
    let mut readings = cache.subscribe();
    let (empty, how) = cache
        .report(asking(Arc::clone(&config), &["codex"]), false)
        .await;
    assert_eq!(how, Freshness::Cold);
    assert!(empty.value.is_empty());
    assert!(!empty.complete);

    // The read it started still lands, and the next request has it.
    readings.changed().await.unwrap();
    let (reading, how) = cache.report(asking(config, &["codex"]), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(!reading.complete);
}

#[tokio::test(start_paused = true)]
async fn a_registry_that_cannot_be_built_reads_nothing_and_says_so() {
    let cache = QuotaCache::with_builder(Arc::new(|_| None));
    let (reading, how) = cache.report(asking(enabled(), &["codex"]), false).await;
    assert_eq!(how, Freshness::Fresh);
    assert!(reading.value.is_empty());
    assert!(
        !reading.complete,
        "no clients is not the same as nothing to say"
    );
}

/// The production builder, against a grant store that has no grant in it: it
/// builds clients and they report an error, without a network and without
/// reaching the developer's own home.
#[tokio::test]
async fn the_live_clients_are_built_from_the_grants_the_sign_in_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let accounts = Accounts::new(
        enabled(),
        dir.path().join("provider-auth.json"),
        vec![Asked {
            id: "grok".into(),
            account: None,
        }],
    );
    let (reading, _) = QuotaCache::default().report(accounts, false).await;
    assert!(
        reading.value[0].report.is_err(),
        "nothing is signed in there"
    );
    assert!(!reading.complete);
}
