//! The daemon's memory of "is there anything newer".
//!
//! `GET /api/update` is asked on every console page load and must never wait on
//! a network call, so the answer it gives is whatever the last lookup found. The
//! lookup itself is [`crate::commands::update::latest`], the same code
//! `lev update` runs, so the console and the terminal cannot come to different
//! conclusions about the same binary.

use std::sync::{Arc, Mutex, PoisonError};

use crate::commands::update::latest::{self, LatestCheck, ReleaseFetcher};

/// The last answer, and how to get a new one.
///
/// Carried on the app state rather than in a `static` so that two tests, or two
/// servers in one process, cannot write into each other's answer.
#[derive(Clone)]
pub(super) struct UpdateCheckCache {
    last: Arc<Mutex<LatestCheck>>,
    fetch: ReleaseFetcher,
}

impl Default for UpdateCheckCache {
    fn default() -> Self {
        Self::with_fetcher(Arc::new(latest::fetch_release))
    }
}

impl std::fmt::Debug for UpdateCheckCache {
    /// Hand-written because a closure has no `Debug`. Prints the answer, which
    /// is the part anyone reading a state dump wants.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateCheckCache")
            .field("last", &self.peek())
            .finish_non_exhaustive()
    }
}

impl UpdateCheckCache {
    /// A cache that looks answers up the given way. Tests pass a canned one.
    pub(super) fn with_fetcher(fetch: ReleaseFetcher) -> Self {
        Self {
            last: Arc::new(Mutex::new(LatestCheck::default())),
            fetch,
        }
    }

    /// The stored answer, without looking anything up.
    pub(super) fn peek(&self) -> LatestCheck {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The stored answer, and a refresh started if it has gone stale.
    ///
    /// Returns immediately either way: the caller gets what is known now, and a
    /// later request gets the benefit of the lookup this one started. That is
    /// the whole contract of the route - a console asking on every page load
    /// must never trigger a wait, only at most a background lookup.
    pub(super) fn read_and_maybe_refresh(
        &self,
        channel: Option<crate::commands::update::Channel>,
        running: &str,
    ) {
        let now = latest::now_secs();
        if !self.peek().is_stale(now, latest::CHECK_TTL_SECS) {
            return;
        }
        // A copy whose channel could not be worked out is not asked about. The
        // only answer available would be the stable release compared against a
        // build that may not be on that line, which is the wrong-in-both-
        // directions guess this whole change exists to stop making.
        let Some(channel) = channel else {
            return;
        };
        let (last, fetch, running) = (
            Arc::clone(&self.last),
            Arc::clone(&self.fetch),
            running.to_string(),
        );
        tokio::task::spawn_blocking(move || {
            store(&last, latest::check_with(channel, &running, &fetch, now));
        });
    }
}

/// Keep an answer, unless there was not one.
///
/// A failed lookup is dropped rather than stored. Storing it would stamp
/// `checked_at`, which makes the failure look like a fresh answer and holds off
/// the next attempt for the whole hour - so one flaky moment would cost a
/// console its update prompt for the rest of the hour.
fn store(last: &Mutex<LatestCheck>, found: LatestCheck) {
    if found.latest.is_some() {
        *last.lock().unwrap_or_else(PoisonError::into_inner) = found;
    }
}

#[cfg(test)]
#[path = "update_cache_tests.rs"]
mod tests;
