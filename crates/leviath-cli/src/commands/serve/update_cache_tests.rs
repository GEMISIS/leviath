use super::*;

use crate::commands::update::Channel;

/// A fetcher that answers one release body every time.
fn answering(body: &'static str) -> ReleaseFetcher {
    Arc::new(move |_: &str| Ok(body.to_string()))
}

/// A fresh cache knows nothing, which is an answer a console can render.
#[test]
fn a_new_cache_has_no_answer_yet() {
    let cache = UpdateCheckCache::with_fetcher(answering(r#"{"name": "9.9.9"}"#));
    assert_eq!(cache.peek(), LatestCheck::default());
}

/// The refresh runs off the caller's thread and its answer lands in the cache.
#[tokio::test]
async fn a_stale_cache_is_refreshed_in_the_background() {
    let cache = UpdateCheckCache::with_fetcher(answering(r#"{"name": "9.9.9"}"#));
    cache.read_and_maybe_refresh(Some(Channel::Stable), "0.4.0");

    // The lookup is on a blocking thread, so the answer arrives after this call
    // has already returned - which is the point of it.
    leviath_testkit::wait_until("the refresh stores its answer", || {
        cache.peek().latest.is_some()
    })
    .await;

    let found = cache.peek();
    assert_eq!(found.latest.as_deref(), Some("9.9.9"));
    assert_eq!(found.update_available, Some(true));
    assert!(found.checked_at.is_some());
}

/// A cache holding a current answer does not go and get another one.
#[tokio::test]
async fn a_fresh_answer_is_not_looked_up_again() {
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counting = {
        let asked = Arc::clone(&asked);
        Arc::new(move |_: &str| {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(r#"{"name": "9.9.9"}"#.to_string())
        }) as ReleaseFetcher
    };
    let cache = UpdateCheckCache::with_fetcher(counting);

    cache.read_and_maybe_refresh(Some(Channel::Stable), "0.4.0");
    leviath_testkit::wait_until("the first refresh stores its answer", || {
        cache.peek().latest.is_some()
    })
    .await;

    for _ in 0..5 {
        cache.read_and_maybe_refresh(Some(Channel::Stable), "0.4.0");
    }
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a console asking on every page load must not become a lookup per load"
    );
}

/// A copy whose channel is unknown is not asked about at all, rather than being
/// compared against a channel it may not be on.
#[tokio::test]
async fn an_unknown_channel_is_not_looked_up() {
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counting = {
        let asked = Arc::clone(&asked);
        Arc::new(move |_: &str| {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(r#"{"name": "9.9.9"}"#.to_string())
        }) as ReleaseFetcher
    };
    let cache = UpdateCheckCache::with_fetcher(counting);

    cache.read_and_maybe_refresh(None, "0.4.0");
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(cache.peek(), LatestCheck::default());
}

/// A lookup that could not answer leaves the cache alone, so the next request
/// tries again instead of being told the failure is a fresh answer.
#[test]
fn a_failed_lookup_is_not_stored_as_an_answer() {
    let last = Mutex::new(LatestCheck::default());
    store(&last, LatestCheck::default());
    assert_eq!(
        *last.lock().expect("not poisoned"),
        LatestCheck::default(),
        "nothing was found, so nothing is remembered"
    );

    let real = LatestCheck {
        latest: Some("1.2.3".to_string()),
        update_available: Some(true),
        checked_at: Some(42),
    };
    store(&last, real.clone());
    assert_eq!(*last.lock().expect("not poisoned"), real);
}

/// The `Debug` is hand-written because a closure has none; it has to show the
/// answer rather than fail to compile or print nothing useful.
#[test]
fn the_cache_prints_the_answer_it_holds() {
    let cache = UpdateCheckCache::with_fetcher(answering(r#"{"name": "9.9.9"}"#));
    let shown = format!("{cache:?}");
    assert!(shown.contains("UpdateCheckCache"), "{shown}");
    assert!(shown.contains("last"), "{shown}");
}
