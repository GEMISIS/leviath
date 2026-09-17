//! One answer kept in memory, refreshed behind the request that finds it old.
//!
//! Two routes here have the same problem. `GET /api/models` has to ask every
//! configured provider what it serves, and `GET /api/providers?quota=true` has
//! to ask every signed-in subscription what it has left. Both are a network
//! read per provider, both are what a console asks for the moment a page
//! opens, and in both a provider that is slow or down must not be allowed to
//! hold the response.
//!
//! The shape that answers all of that is the same either way: build the answer
//! once per key, serve it from memory while it is young, and when a request
//! finds it old hand over the answer in hand and start the next build behind
//! it. Only a request that has nothing at all to be given waits, and that wait
//! is bounded. This module is that shape with the payload left out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio::time::Instant;

/// How two keys are judged to name the same answer.
///
/// Not `PartialEq`, because neither caller wants a field-by-field compare: a
/// config is compared by `Arc` identity, since the reloader hands out the same
/// `Arc` until the file changes and a deep compare of the whole thing on every
/// request would be the wrong price for the same answer. A key may also carry
/// inputs a build needs but that do not distinguish one answer from another.
pub(super) trait SameAnswer: Clone + Send + Sync + 'static {
    /// Whether an answer built for `other` is an answer to this key.
    fn same_answer(&self, other: &Self) -> bool;
}

impl SameAnswer for Arc<crate::config::Config> {
    fn same_answer(&self, other: &Self) -> bool {
        Arc::ptr_eq(self, other)
    }
}

/// What one build produced: the answer, and whether every source it asked
/// answered. An incomplete answer is still served - the missing source is
/// simply absent from it - but it is kept for a shorter time.
pub(super) type Built<T> = (T, bool);

/// How one answer is built. The `bool` is the force flag: a caller that asked
/// for a re-read rather than being handed what was in memory.
pub(super) type Build<K, T> = Arc<dyn Fn(K, bool) -> BoxFuture<'static, Built<T>> + Send + Sync>;

/// One answer, as of one build.
pub(super) struct Cached<K, T> {
    /// What was built.
    pub(super) value: T,
    /// Whether every source answered when it was. `false` when one timed out,
    /// errored or could not be reached; what it would have contributed is
    /// simply absent.
    pub(super) complete: bool,
    built_at: Instant,
    key: K,
}

impl<K: SameAnswer, T> Cached<K, T> {
    /// Seconds since this answer was built.
    pub(super) fn age_secs(&self) -> u64 {
        self.built_at.elapsed().as_secs()
    }

    pub(super) fn is_for(&self, key: &K) -> bool {
        self.key.same_answer(key)
    }
}

/// How a request got its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Freshness {
    /// Built within the window.
    Fresh,
    /// Past the window; a build is running behind this answer.
    Stale,
    /// Nothing to hand: the sources did not answer inside the wait.
    Cold,
}

/// The answer for one key at a time, and the machinery that keeps it current.
pub(super) struct Refreshing<K, T> {
    inner: Arc<Inner<K, T>>,
}

/// Hand-written rather than derived: a derive would demand `K: Clone` and
/// `T: Clone`, and nothing here clones either - the handle is an `Arc`.
impl<K, T> Clone for Refreshing<K, T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<K, T> {
    /// Bookkeeping only; never held across an await.
    state: Mutex<Bookkeeping<K>>,
    /// The latest answer. A `watch` so a request that has to wait for one can
    /// do so without polling, and so a build publishes to every waiter at once.
    published: watch::Sender<Option<Arc<Cached<K, T>>>>,
    build: Build<K, T>,
    fresh_for: Duration,
    retry_after: Duration,
    /// How long a request with no answer to hand waits for one before
    /// answering with nothing.
    cold_wait: Duration,
}

struct Bookkeeping<K> {
    /// Whether a build is running. One at a time: a burst of requests past the
    /// window starts one build, not one per request.
    in_flight: bool,
    /// A build asked for while one was running, to run after it. The key may
    /// differ from the running one, which is exactly the case that must not be
    /// dropped: a config write, or a sign-in, while a build is in flight.
    queued: Option<(K, bool)>,
}

/// Hand-written for the same reason [`Refreshing::clone`] is: a derive would
/// demand `K: Default`, and there is no default key.
impl<K> Default for Bookkeeping<K> {
    fn default() -> Self {
        Self {
            in_flight: false,
            queued: None,
        }
    }
}

impl<K: SameAnswer, T: Default + Send + Sync + 'static> Refreshing<K, T> {
    pub(super) fn new(
        build: Build<K, T>,
        fresh_for: Duration,
        retry_after: Duration,
        cold_wait: Duration,
    ) -> Self {
        let (published, _) = watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(Bookkeeping::default()),
                published,
                build,
                fresh_for,
                retry_after,
                cold_wait,
            }),
        }
    }

    /// The answer for `key`, and how it was got.
    ///
    /// `force` builds again and waits for the result, for a page that has just
    /// changed something and wants to show what it did rather than the memory
    /// of what was there before.
    pub(super) async fn get(&self, key: K, force: bool) -> (Arc<Cached<K, T>>, Freshness) {
        let current = self.inner.published.borrow().clone();
        if let Some(cached) = &current
            && cached.is_for(&key)
            && !force
        {
            if cached.built_at.elapsed() < self.inner.window(cached) {
                return (Arc::clone(cached), Freshness::Fresh);
            }
            self.request_refresh(key, true);
            return (Arc::clone(cached), Freshness::Stale);
        }
        // Nothing for this key yet, or a forced rebuild: start one and wait for
        // it, bounded. Subscribing before asking, so a build that lands in
        // between is still seen. "New" is by identity rather than by time, so a
        // forced rebuild is satisfied by any answer but the one in hand.
        let mut answers = self.inner.published.subscribe();
        self.request_refresh(key.clone(), force);
        let landed = tokio::time::timeout(
            self.inner.cold_wait,
            answers.wait_for(|answer| {
                answer.as_ref().is_some_and(|a| {
                    a.is_for(&key) && current.as_ref().is_none_or(|had| !Arc::ptr_eq(had, a))
                })
            }),
        )
        .await;
        // The predicate only passes on `Some`, and the channel cannot close
        // while `self.inner` holds the sender, so falling through here means
        // the sources did not answer in time.
        if let Ok(Ok(answer)) = landed
            && let Some(cached) = answer.as_ref()
        {
            return (Arc::clone(cached), Freshness::Fresh);
        }
        (
            Arc::new(Cached {
                value: T::default(),
                complete: false,
                built_at: Instant::now(),
                key,
            }),
            Freshness::Cold,
        )
    }

    /// Start a build for `key` unless one is running, in which case it runs
    /// next. Returns at once; the answer lands on the watch.
    pub(super) fn request_refresh(&self, key: K, force: bool) {
        {
            let mut state = leviath_core::sync::lock(&self.inner.state);
            if state.in_flight {
                // A queued build that wanted a re-read keeps wanting one.
                let force = force || state.queued.as_ref().is_some_and(|(_, f)| *f);
                state.queued = Some((key, force));
                return;
            }
            state.in_flight = true;
        }
        let inner = Arc::clone(&self.inner);
        // Detached rather than tied to the request: a client that gives up must
        // not cancel the build every other client is waiting on.
        tokio::spawn(async move {
            inner.run_builds(key, force).await;
        });
    }

    /// The latest answer as it lands, for a caller that wants to be told.
    #[cfg(test)]
    pub(super) fn subscribe(&self) -> watch::Receiver<Option<Arc<Cached<K, T>>>> {
        self.inner.published.subscribe()
    }
}

/// Clears `in_flight` when the build task ends, however it ends, so a panic
/// inside a source cannot leave this believing a build is still running and
/// never start another.
struct InFlight<K, T>(Arc<Inner<K, T>>);

impl<K, T> Drop for InFlight<K, T> {
    fn drop(&mut self) {
        leviath_core::sync::lock(&self.0.state).in_flight = false;
    }
}

impl<K: SameAnswer, T: Send + Sync + 'static> Inner<K, T> {
    fn window(&self, cached: &Cached<K, T>) -> Duration {
        if cached.complete {
            self.fresh_for
        } else {
            self.retry_after
        }
    }

    /// Build for this key, then for whatever was queued while that ran, until
    /// nothing is.
    async fn run_builds(self: Arc<Self>, key: K, force: bool) {
        let _in_flight = InFlight(Arc::clone(&self));
        let mut next = Some((key, force));
        while let Some((key, force)) = next.take() {
            let (value, complete) = (self.build)(key.clone(), force).await;
            self.published.send_replace(Some(Arc::new(Cached {
                value,
                complete,
                built_at: Instant::now(),
                key,
            })));
            next = leviath_core::sync::lock(&self.state).queued.take();
        }
    }
}
