//! A one-shot "stop what you're doing" signal for work already in flight.
//!
//! Cancelling an agent sets its status, which stops the dispatch systems from
//! starting *new* work — but an inference request or tool batch handed to the
//! async lanes before that keeps running to completion. For a stalled provider
//! call that is up to the job timeout; for a tool batch there was no bound at
//! all, and the batch holds one of the tool lane's fixed number of workers the
//! whole time.
//!
//! A [`CancelToken`] is handed to each async job when it is dispatched and kept
//! on the agent, so cancelling the agent drops the in-flight future at its next
//! await point: the HTTP request is aborted, the pool permit and lane worker are
//! released, and the result is never applied.
//!
//! This is deliberately a few lines rather than a dependency: the whole contract
//! is "set a flag once, wake anyone waiting".

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// A cloneable cancellation signal. Every clone observes the same state, and
/// cancelling is idempotent — a token that fires twice (say, a cancel racing a
/// reap) is not an error.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl CancelToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fire the signal, waking every waiter. Idempotent.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Whether this token has already fired.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolve once the token fires, immediately if it already has.
    ///
    /// The waiter is armed *before* the flag is re-checked. `notify_waiters`
    /// only wakes waiters registered at the time it runs, so checking first and
    /// waiting second would lose a cancel that landed in between — and the job
    /// would then run to completion despite having been cancelled.
    pub async fn cancelled(&self) {
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_resolves_when_the_token_fires() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());

        let waiter = tokio::spawn({
            let token = token.clone();
            async move { token.cancelled().await }
        });
        // Let the waiter arm itself, then fire.
        tokio::task::yield_now().await;
        token.cancel();

        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("a fired token wakes its waiter")
            .unwrap();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_returns_immediately_for_an_already_fired_token() {
        let token = CancelToken::new();
        token.cancel();
        tokio::time::timeout(Duration::from_secs(5), token.cancelled())
            .await
            .expect("no wait for a token that already fired");
    }

    /// A cancel concurrent with the wait is still observed. This exercises the
    /// path but cannot *prove* the arm-then-check ordering: the window it guards
    /// is a few instructions with no await point in it, so a test cannot land in
    /// it reliably. The ordering is enforced by construction (see `cancelled`),
    /// not by this test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_concurrent_with_the_wait_is_observed() {
        for _ in 0..500 {
            let token = CancelToken::new();
            let firing = tokio::spawn({
                let token = token.clone();
                async move { token.cancel() }
            });
            tokio::time::timeout(Duration::from_secs(5), token.cancelled())
                .await
                .expect("the cancel was observed");
            firing.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancel_is_idempotent_and_wakes_every_clone() {
        let token = CancelToken::new();
        let waiters: Vec<_> = (0..3)
            .map(|_| {
                let token = token.clone();
                tokio::spawn(async move { token.cancelled().await })
            })
            .collect();
        tokio::task::yield_now().await;
        token.cancel();
        token.cancel(); // twice is fine

        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("every clone observes the cancel")
                .unwrap();
        }
    }

    #[test]
    fn debug_reports_the_state() {
        let token = CancelToken::new();
        assert!(format!("{token:?}").contains("cancelled: false"));
        token.cancel();
        assert!(format!("{token:?}").contains("cancelled: true"));
    }
}
