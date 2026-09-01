//! Rate limiting for LLM provider API calls.
//!
//! Prevents hammering provider APIs with configurable RPM and TPM limits,
//! automatic backoff, and Retry-After header support.

use crate::provider::{ProviderError, RateLimitConfig, StreamChunk};
use futures_core::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// Rate limiter for controlling API request frequency.
///
/// Tracks requests per minute (RPM) and tokens per minute (TPM),
/// sleeping when limits are approached and applying exponential
/// backoff on 429 responses.
#[derive(Clone)]
pub struct RateLimiter {
    /// Requests per minute limit
    rpm_limit: u32,

    /// Tokens per minute limit
    tpm_limit: u32,
    /// The sliding window both limits count over. A minute in production;
    /// tests inject a short one so a wait can be observed in real time.
    window: Duration,

    /// Current window state. A plain mutex, never held across an await: the
    /// waits below release it first, and [`MeteredStream`] has to book its
    /// tokens from a `Drop`, where there is nothing to await on.
    state: Arc<Mutex<RateLimiterState>>,
}

/// What one pass of [`RateLimiter::acquire`] decided.
enum Admission {
    /// Under both limits; the request has been counted.
    Now,
    /// A window is full: sleep `wait`, then look again. The counts are for
    /// the log line.
    Wait {
        wait: Duration,
        requests: u32,
        tokens: usize,
        /// The token window is (part of) why this call is waiting, and it is
        /// the first time this limiter has throttled on it. Surfaced once at
        /// `warn` so a `tokens_per_minute` that was inert before it was
        /// enforced does not slow a run invisibly; later waits stay at
        /// `debug`.
        first_tpm_wait: bool,
    },
}

struct RateLimiterState {
    request_timestamps: VecDeque<Instant>,
    token_counts: VecDeque<(Instant, usize)>,
    consecutive_429s: u32,
    /// Whether the one-time `tokens_per_minute` throttle notice has fired.
    tpm_wait_announced: bool,
}

impl RateLimiter {
    /// Create a new rate limiter with the given configuration.
    pub fn new(config: &RateLimitConfig) -> Self {
        Self::with_window(config, Duration::from_secs(60))
    }

    /// A limiter counting over `window` instead of a minute; the tests use
    /// a few hundred milliseconds so the wait paths run in real time.
    fn with_window(config: &RateLimitConfig, window: Duration) -> Self {
        Self {
            rpm_limit: config.requests_per_minute,
            tpm_limit: config.tokens_per_minute,
            window,
            state: Arc::new(Mutex::new(RateLimiterState {
                request_timestamps: VecDeque::new(),
                token_counts: VecDeque::new(),
                consecutive_429s: 0,
                tpm_wait_announced: false,
            })),
        }
    }

    /// The window state, recovered from a poisoned lock: a panic while the
    /// state was held cannot have left a count that is worse than stale.
    fn state(&self) -> MutexGuard<'_, RateLimiterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wait until we are allowed to make a request, then record it.
    ///
    /// This may sleep if we are at or near the rate limit.
    pub async fn acquire(&self) -> Result<(), ProviderError> {
        loop {
            // The lock lives in this block and is gone before the sleep: a
            // guard still in scope across the await would make the future
            // `!Send`, even after an explicit drop.
            let (wait, current_rpm, current_tpm, first_tpm_wait) = {
                let mut state = self.state();
                match self.admit(&mut state) {
                    Admission::Now => return Ok(()),
                    Admission::Wait {
                        wait,
                        requests,
                        tokens,
                        first_tpm_wait,
                    } => (wait, requests, tokens, first_tpm_wait),
                }
            };
            // The first time a run waits on the token window, say so at `warn`.
            // The limit was documented as inert before it was enforced, so a
            // stale or placeholder `tokens_per_minute` throttles silently
            // otherwise, indistinguishable from a slow model. Once per limiter;
            // the debug line still carries every wait.
            if first_tpm_wait {
                tracing::warn!(
                    tokens_per_minute = self.tpm_limit,
                    tokens_in_window = current_tpm,
                    "Throttling on a configured tokens_per_minute limit: calls are waiting for the \
                     token window to clear. This is a client-side rate limit; raise it or set \
                     tokens_per_minute = 0 to disable it."
                );
            }
            tracing::debug!(
                wait_ms = wait.as_millis(),
                requests = current_rpm,
                tokens = current_tpm,
                "Rate limiter: waiting for capacity"
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// One pass over the windows: prune, then admit or say how long to wait.
    fn admit(&self, state: &mut RateLimiterState) -> Admission {
        let now = Instant::now();
        let window = self.window;

        // Prune old entries outside the 1-minute window. `duration_since`
        // rather than `now - window`: an `Instant` cannot go before the
        // platform's epoch, and `Instant - Duration` panics when it would -
        // on Linux the epoch is boot, so a daemon started under a service
        // manager within 60s of boot hit that on its first request.
        while state
            .request_timestamps
            .front()
            .is_some_and(|t| now.duration_since(*t) > window)
        {
            state.request_timestamps.pop_front();
        }
        while state
            .token_counts
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > window)
        {
            state.token_counts.pop_front();
        }

        let current_rpm = state.request_timestamps.len() as u32;
        let current_tpm: usize = state.token_counts.iter().map(|(_, t)| t).sum();
        // A zero limit on either key is no limit at all, the way a
        // provider without a `[rate_limits]` table has none. Without the
        // guard a zero `requests_per_minute` could never be satisfied and
        // `acquire` never returned.
        let rpm_ok = self.rpm_limit == 0 || current_rpm < self.rpm_limit;
        let tpm_ok = self.tpm_limit == 0 || current_tpm < self.tpm_limit as usize;

        if rpm_ok && tpm_ok {
            state.request_timestamps.push_back(now);
            return Admission::Now;
        }

        // Wait until whichever window is full has let its oldest entry out.
        // The tokens a request spent are recorded after it answers, so the
        // token window lags the request window by one call; that is the
        // right side to err on, since the provider counts them the same way.
        let mut until = now;
        if !rpm_ok {
            let oldest = state.request_timestamps.front().copied().unwrap_or(now);
            until = until.max(oldest + window);
        }
        let mut first_tpm_wait = false;
        if !tpm_ok {
            let oldest = state.token_counts.front().map(|(t, _)| *t).unwrap_or(now);
            until = until.max(oldest + window);
            // Latch the one-time notice here, while the lock is held, so it
            // fires exactly once however many callers are waiting at once.
            if !state.tpm_wait_announced {
                state.tpm_wait_announced = true;
                first_tpm_wait = true;
            }
        }
        Admission::Wait {
            wait: until.saturating_duration_since(now) + Duration::from_millis(50),
            requests: current_rpm,
            tokens: current_tpm,
            first_tpm_wait,
        }
    }

    /// Record token usage for TPM tracking.
    ///
    /// The buffered path calls this with the response in hand; a streamed
    /// call goes through [`meter_stream`], which calls it once the stream is
    /// done and the usage frame has named the total.
    pub fn record_tokens(&self, tokens: usize) {
        self.state()
            .token_counts
            .push_back((Instant::now(), tokens));
    }

    /// Handle a 429 rate limit response.
    ///
    /// If a Retry-After header value is provided (in seconds), sleep for that
    /// duration. Otherwise, apply exponential backoff.
    pub async fn handle_rate_limit(&self, retry_after_secs: Option<u64>) {
        // The guard is scoped away before the sleep; see `acquire`.
        let wait = {
            let mut state = self.state();
            state.consecutive_429s += 1;
            if let Some(secs) = retry_after_secs {
                Duration::from_secs(secs)
            } else {
                // Exponential backoff: 1s, 2s, 4s, 8s, 16s, max 60s
                let base = Duration::from_secs(1);
                let multiplier = 2u64.pow(state.consecutive_429s.min(6) - 1);
                (base * multiplier as u32).min(Duration::from_secs(60))
            }
        };
        tracing::warn!(
            wait_secs = wait.as_secs(),
            "Rate limited (429), backing off"
        );
        tokio::time::sleep(wait).await;
    }

    /// Reset the consecutive 429 counter (call after a successful request).
    pub async fn reset_backoff(&self) {
        let mut state = self.state();
        state.consecutive_429s = 0;
    }
}

/// The chunk stream every `infer_stream` hands back.
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamChunk, ProviderError>> + Send>>;

/// Book `stream`'s tokens on `limiter` once the stream is done.
///
/// A provider without a limiter gets its stream back untouched. With one, the
/// usage a streamed call reports, which only arrives on its last frames, lands
/// in the token window the same way a buffered call's does; without this the
/// daemon's default path never fed `tokens_per_minute` at all.
pub fn meter_stream(limiter: Option<&RateLimiter>, stream: ChunkStream) -> ChunkStream {
    match limiter {
        Some(limiter) => Box::pin(MeteredStream {
            inner: stream,
            limiter: limiter.clone(),
            tokens: 0,
        }),
        None => stream,
    }
}

/// A chunk stream that spends what its usage frames name on a limiter.
///
/// The total is booked when the stream is dropped rather than on its final
/// poll, so a stream that fails or is abandoned midway still books whatever
/// the provider had counted by then, which is what the provider bills.
pub struct MeteredStream {
    inner: ChunkStream,
    limiter: RateLimiter,
    /// The running total across the usage frames seen so far.
    tokens: usize,
}

impl Stream for MeteredStream {
    type Item = std::result::Result<StreamChunk, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let item = std::task::ready!(this.inner.as_mut().poll_next(cx));
        if let Some(Ok(chunk)) = &item
            && let Some(usage) = &chunk.tokens
        {
            this.tokens = this.tokens.saturating_add(usage.total_tokens);
        }
        Poll::Ready(item)
    }
}

impl Drop for MeteredStream {
    fn drop(&mut self) {
        // A stream that named no usage (a refusal before the first frame, a
        // server that does not report it) has nothing to book.
        if self.tokens > 0 {
            self.limiter.record_tokens(self.tokens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::TokenUsage;
    use crate::test_support::always_on_tracing_guard;
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn test_rate_limiter_basic() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 10,
            tokens_per_minute: 100_000,
        });

        // Should be able to acquire without blocking
        limiter.acquire().await.unwrap();
        limiter.acquire().await.unwrap();
    }

    #[tokio::test]
    async fn a_fresh_limiter_admits_the_first_request() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        // Should be able to acquire at least once
        limiter.acquire().await.unwrap();
    }

    /// The first time a call waits on the token window, the one-time notice
    /// latches; it does not before any wait, and a second wait does not
    /// re-arm it. The `warn!` fires under a real subscriber so its fields run.
    #[tokio::test]
    async fn the_first_tpm_wait_announces_once() {
        let _guard = always_on_tracing_guard();
        let limiter = short_window(60, 100);
        assert!(
            !limiter.state().tpm_wait_announced,
            "nothing waited yet, so nothing announced"
        );
        limiter.record_tokens(100);
        // First throttled acquire: waits, then announces as it goes.
        tokio::time::timeout(Duration::from_secs(5), limiter.acquire())
            .await
            .expect("released once the window turned over")
            .expect("acquire succeeds");
        assert!(
            limiter.state().tpm_wait_announced,
            "the first token-window wait announced"
        );
        // A second throttle does not re-arm the latch.
        limiter.record_tokens(100);
        tokio::time::timeout(Duration::from_secs(5), limiter.acquire())
            .await
            .expect("released again")
            .expect("acquire succeeds");
        assert!(limiter.state().tpm_wait_announced, "still latched, once");
    }

    /// A wait driven only by the request window never arms the token notice:
    /// the two limits are separate, and the notice is about the one that was
    /// documented inert.
    #[tokio::test]
    async fn an_rpm_only_wait_does_not_announce_a_tpm_throttle() {
        let limiter = short_window(1, 0);
        limiter.acquire().await.expect("first request admitted");
        tokio::time::timeout(Duration::from_secs(5), limiter.acquire())
            .await
            .expect("released once the request window turned over")
            .expect("acquire succeeds");
        assert!(
            !limiter.state().tpm_wait_announced,
            "an rpm wait is not a tpm throttle"
        );
    }

    /// A limiter over a short window, so a wait is observable in real time.
    fn short_window(rpm: u32, tpm: u32) -> Arc<RateLimiter> {
        Arc::new(RateLimiter::with_window(
            &RateLimitConfig {
                requests_per_minute: rpm,
                tokens_per_minute: tpm,
            },
            Duration::from_millis(400),
        ))
    }

    /// `tokens_per_minute` throttles: with the window's token budget spent, the
    /// next `acquire` waits for the oldest spend to leave the window.
    #[tokio::test]
    async fn acquire_waits_for_tpm_capacity() {
        let limiter = short_window(60, 100);
        limiter.record_tokens(100);
        let started = Instant::now();
        let waiting = {
            let limiter = limiter.clone();
            tokio::spawn(async move { limiter.acquire().await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !waiting.is_finished(),
            "held while the window's tokens are spent"
        );
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("released once the window turned over")
            .expect("the task completes")
            .expect("acquire succeeds");
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "waited for the window"
        );
    }

    /// A zero `requests_per_minute` means no request limit; only tokens
    /// count. Without the guard `acquire` loops on a limit no count can stay
    /// under, so the test is bounded rather than left to hang.
    #[tokio::test]
    async fn a_zero_requests_per_minute_is_no_request_limit() {
        let limiter = short_window(0, 0);
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), limiter.acquire())
                .await
                .expect("a zero request limit admits the call at once")
                .expect("acquire succeeds");
        }
    }

    /// A zero `tokens_per_minute` means no token limit; only requests count.
    #[tokio::test]
    async fn a_zero_tokens_per_minute_is_no_token_limit() {
        let limiter = short_window(60, 0);
        limiter.record_tokens(1_000_000);
        let started = Instant::now();
        limiter.acquire().await.expect("nothing to wait for");
        assert!(started.elapsed() < Duration::from_millis(300), "no wait");
    }

    /// Both windows full: the wait is for the later of the two to open.
    #[tokio::test]
    async fn acquire_waits_for_the_later_of_both_windows() {
        let limiter = short_window(1, 10);
        limiter
            .acquire()
            .await
            .expect("the first request goes through");
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The token spend lands 200 ms after the request, so its window opens
        // 200 ms later than the request window does.
        limiter.record_tokens(10);
        let started = Instant::now();
        let waiting = {
            let limiter = limiter.clone();
            tokio::spawn(async move { limiter.acquire().await })
        };
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("released once both windows turned over")
            .expect("the task completes")
            .expect("acquire succeeds");
        // Request window alone would have opened ~200 ms in; the token window
        // holds it to ~400 ms.
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "held by the token window"
        );
    }

    #[tokio::test]
    async fn acquire_up_to_limit() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 5,
            tokens_per_minute: 100_000,
        });
        // Should be able to acquire exactly rpm_limit times without blocking
        for _ in 0..5 {
            limiter.acquire().await.unwrap();
        }
    }

    #[tokio::test]
    async fn reset_backoff_clears_counter() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        // Simulate a 429
        limiter.handle_rate_limit(Some(0)).await;
        {
            let state = limiter.state();
            assert_eq!(state.consecutive_429s, 1);
        }
        limiter.reset_backoff().await;
        {
            let state = limiter.state();
            assert_eq!(state.consecutive_429s, 0);
        }
    }

    #[tokio::test]
    async fn handle_rate_limit_increments_429_counter() {
        // Registers a real Subscriber so the tracing::warn! call's field
        // arguments in handle_rate_limit are actually exercised.
        let _guard = always_on_tracing_guard();
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        limiter.handle_rate_limit(Some(0)).await;
        limiter.handle_rate_limit(Some(0)).await;
        let state = limiter.state();
        assert_eq!(state.consecutive_429s, 2);
    }

    #[tokio::test]
    async fn clone_shares_state() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        let clone = limiter.clone();
        limiter.record_tokens(500);
        // Clone should see the same tokens
        let state = clone.state();
        assert_eq!(state.token_counts.len(), 1);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[tokio::test]
    async fn handle_rate_limit_with_retry_after() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        // retry_after = 0 should return almost immediately
        let start = std::time::Instant::now();
        limiter.handle_rate_limit(Some(0)).await;
        assert!(start.elapsed().as_secs() < 2);
    }

    #[tokio::test]
    async fn handle_rate_limit_exponential_backoff_increments() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        // Calling handle_rate_limit multiple times with None increments the counter
        // Use retry_after=0 to avoid actual sleep
        limiter.handle_rate_limit(Some(0)).await;
        limiter.handle_rate_limit(Some(0)).await;
        limiter.handle_rate_limit(Some(0)).await;
        {
            let state = limiter.state();
            assert_eq!(state.consecutive_429s, 3);
        }
    }

    #[tokio::test]
    async fn clone_shares_rpm_and_tpm() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 42,
            tokens_per_minute: 12345,
        });
        let clone = limiter.clone();
        assert_eq!(clone.rpm_limit, 42);
        assert_eq!(clone.tpm_limit, 12345);
    }

    #[tokio::test]
    async fn new_copies_rpm_and_tpm_from_the_config() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        assert_eq!(limiter.rpm_limit, 60);
        assert_eq!(limiter.tpm_limit, 100_000);
    }

    #[tokio::test]
    async fn acquire_records_timestamp() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
        });
        limiter.acquire().await.unwrap();
        {
            let state = limiter.state();
            assert_eq!(state.request_timestamps.len(), 1);
        }
        limiter.acquire().await.unwrap();
        {
            let state = limiter.state();
            assert_eq!(state.request_timestamps.len(), 2);
        }
    }

    #[tokio::test]
    async fn acquire_waits_then_prunes_expired_timestamp() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments in acquire()'s "wait for RPM capacity" branch are
        // actually exercised.
        let _guard = always_on_tracing_guard();
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 1,
            tokens_per_minute: 100_000,
        });
        // Seed a timestamp that's 59.95s old, so the very first `acquire()`
        // call is already at the RPM limit and must take the "wait" branch
        // before the entry ages out of the 60s window and gets pruned.
        {
            let mut state = limiter.state();
            state
                .request_timestamps
                .push_back(Instant::now() - Duration::from_millis(59_950));
        }
        limiter.acquire().await.unwrap();
        let state = limiter.state();
        assert_eq!(state.request_timestamps.len(), 1);
    }

    #[tokio::test]
    async fn acquire_prunes_expired_token_count_entries() {
        // acquire()'s pruning loop also prunes `token_counts`; no other test
        // seeds an expired token_counts entry before calling acquire(), so
        // that pop_front() call is otherwise unexercised.
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        {
            let mut state = limiter.state();
            state
                .token_counts
                .push_back((Instant::now() - Duration::from_secs(61), 500));
        }
        limiter.acquire().await.unwrap();
        let state = limiter.state();
        assert!(state.token_counts.is_empty());
    }

    fn chunks(items: Vec<StreamChunk>) -> ChunkStream {
        Box::pin(tokio_stream::iter(items.into_iter().map(Ok)))
    }

    fn usage_chunk(total: usize) -> StreamChunk {
        StreamChunk {
            delta: String::new(),
            tool_calls: vec![],
            tokens: Some(TokenUsage::new(total, 0, 0, 0)),
            finish_reason: None,
            reasoning: None,
        }
    }

    fn text_chunk() -> StreamChunk {
        StreamChunk {
            delta: "hi".to_string(),
            tool_calls: vec![],
            tokens: None,
            finish_reason: None,
            reasoning: None,
        }
    }

    /// Usage split across frames is summed, and booked once the stream is
    /// dropped: nothing lands while it is still being read.
    #[tokio::test]
    async fn a_metered_stream_books_its_usage_when_dropped() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 1000,
        });
        let mut stream = meter_stream(
            Some(&limiter),
            chunks(vec![text_chunk(), usage_chunk(100), usage_chunk(50)]),
        );
        while stream.next().await.is_some() {}
        assert!(limiter.state().token_counts.is_empty(), "not yet booked");
        drop(stream);
        let state = limiter.state();
        assert_eq!(state.token_counts.len(), 1);
        assert_eq!(state.token_counts[0].1, 150);
    }

    /// A stream abandoned after its usage frame still books what it saw.
    #[tokio::test]
    async fn an_abandoned_metered_stream_books_what_it_saw() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 1000,
        });
        let mut stream = meter_stream(Some(&limiter), chunks(vec![usage_chunk(70), text_chunk()]));
        stream.next().await.expect("the usage frame").expect("ok");
        drop(stream);
        assert_eq!(limiter.state().token_counts[0].1, 70);
    }

    /// A stream that fails midway passes the error through and books the
    /// usage seen before it.
    #[tokio::test]
    async fn a_metered_stream_passes_errors_through() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 1000,
        });
        let inner: ChunkStream = Box::pin(tokio_stream::iter(vec![
            Ok(usage_chunk(30)),
            Err(ProviderError::InvalidResponse("torn".to_string())),
        ]));
        let mut stream = meter_stream(Some(&limiter), inner);
        stream.next().await.expect("the usage frame").expect("ok");
        assert!(stream.next().await.expect("the error").is_err());
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(limiter.state().token_counts[0].1, 30);
    }

    /// A frame that has not arrived yet leaves the poll pending, and the
    /// usage is still summed once it does. The sender runs on a task the
    /// current-thread runtime has not scheduled at the first poll, so that
    /// poll is pending by construction.
    #[tokio::test]
    async fn a_metered_stream_waits_for_a_frame_that_is_not_there_yet() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 1000,
        });
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let inner: ChunkStream = Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(rx));
        let mut stream = meter_stream(Some(&limiter), inner);
        tokio::spawn(async move {
            tx.send(Ok(usage_chunk(40))).expect("the receiver is alive");
        });
        stream.next().await.expect("the usage frame").expect("ok");
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(limiter.state().token_counts[0].1, 40);
    }

    /// No usage frame, nothing booked; no limiter, the stream is untouched.
    #[tokio::test]
    async fn a_stream_without_usage_or_limiter_books_nothing() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 1000,
        });
        let stream = meter_stream(Some(&limiter), chunks(vec![text_chunk()]));
        let collected = crate::collect_stream(stream).await;
        assert!(collected.is_err(), "no finish reason");
        assert!(limiter.state().token_counts.is_empty());

        let mut bare = meter_stream(None, chunks(vec![usage_chunk(5)]));
        assert!(bare.next().await.is_some());
        assert!(bare.next().await.is_none());
    }
}
