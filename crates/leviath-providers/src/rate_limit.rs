//! Rate limiting for LLM provider API calls.
//!
//! Prevents hammering provider APIs with configurable RPM and TPM limits,
//! automatic backoff, and Retry-After header support.

use crate::provider::{ProviderError, RateLimitConfig};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Rate limiter for controlling API request frequency.
///
/// Tracks requests per minute (RPM) and tokens per minute (TPM),
/// sleeping when limits are approached and applying exponential
/// backoff on 429 responses.
pub struct RateLimiter {
    /// Requests per minute limit
    rpm_limit: u32,

    /// Tokens per minute limit
    tpm_limit: u32,

    /// Current window state
    state: Arc<Mutex<RateLimiterState>>,
}

struct RateLimiterState {
    request_timestamps: VecDeque<Instant>,
    token_counts: VecDeque<(Instant, usize)>,
    consecutive_429s: u32,
}

impl RateLimiter {
    /// Create a new rate limiter with the given configuration.
    pub fn new(config: &RateLimitConfig) -> Self {
        Self {
            rpm_limit: config.requests_per_minute,
            tpm_limit: config.tokens_per_minute,
            state: Arc::new(Mutex::new(RateLimiterState {
                request_timestamps: VecDeque::new(),
                token_counts: VecDeque::new(),
                consecutive_429s: 0,
            })),
        }
    }

    /// Create a rate limiter with default limits.
    pub fn with_defaults() -> Self {
        Self {
            rpm_limit: 60,
            tpm_limit: 100_000,
            state: Arc::new(Mutex::new(RateLimiterState {
                request_timestamps: VecDeque::new(),
                token_counts: VecDeque::new(),
                consecutive_429s: 0,
            })),
        }
    }

    /// Wait until we are allowed to make a request, then record it.
    ///
    /// This may sleep if we are at or near the rate limit.
    pub async fn acquire(&self) -> Result<(), ProviderError> {
        let window = Duration::from_secs(60);

        loop {
            let now = Instant::now();
            let mut state = self.state.lock().await;

            // Prune old entries outside the 1-minute window
            let cutoff = now - window;
            while state
                .request_timestamps
                .front()
                .is_some_and(|t| *t < cutoff)
            {
                state.request_timestamps.pop_front();
            }
            while state.token_counts.front().is_some_and(|(t, _)| *t < cutoff) {
                state.token_counts.pop_front();
            }

            let current_rpm = state.request_timestamps.len() as u32;

            if current_rpm < self.rpm_limit {
                state.request_timestamps.push_back(now);
                return Ok(());
            }

            // Calculate how long to wait
            let oldest = state.request_timestamps.front().copied().unwrap_or(now);
            let wait = (oldest + window).saturating_duration_since(now) + Duration::from_millis(50);

            drop(state);
            tracing::debug!(
                wait_ms = wait.as_millis(),
                "Rate limiter: waiting for RPM capacity"
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// Record token usage for TPM tracking.
    pub async fn record_tokens(&self, tokens: usize) {
        let mut state = self.state.lock().await;
        state.token_counts.push_back((Instant::now(), tokens));
    }

    /// Check current TPM usage against limit.
    pub async fn check_tpm(&self) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut state = self.state.lock().await;

        // Prune old entries
        let cutoff = now - window;
        while state.token_counts.front().is_some_and(|(t, _)| *t < cutoff) {
            state.token_counts.pop_front();
        }

        let current_tpm: usize = state.token_counts.iter().map(|(_, t)| t).sum();
        current_tpm < self.tpm_limit as usize
    }

    /// Handle a 429 rate limit response.
    ///
    /// If a Retry-After header value is provided (in seconds), sleep for that
    /// duration. Otherwise, apply exponential backoff.
    pub async fn handle_rate_limit(&self, retry_after_secs: Option<u64>) {
        let mut state = self.state.lock().await;
        state.consecutive_429s += 1;

        let wait = if let Some(secs) = retry_after_secs {
            Duration::from_secs(secs)
        } else {
            // Exponential backoff: 1s, 2s, 4s, 8s, 16s, max 60s
            let base = Duration::from_secs(1);
            let multiplier = 2u64.pow(state.consecutive_429s.min(6) - 1);
            (base * multiplier as u32).min(Duration::from_secs(60))
        };

        drop(state);
        tracing::warn!(
            wait_secs = wait.as_secs(),
            "Rate limited (429), backing off"
        );
        tokio::time::sleep(wait).await;
    }

    /// Reset the consecutive 429 counter (call after a successful request).
    pub async fn reset_backoff(&self) {
        let mut state = self.state.lock().await;
        state.consecutive_429s = 0;
    }
}

impl Clone for RateLimiter {
    fn clone(&self) -> Self {
        Self {
            rpm_limit: self.rpm_limit,
            tpm_limit: self.tpm_limit,
            state: Arc::clone(&self.state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::always_on_tracing_guard;

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
    async fn test_rate_limiter_tpm_check() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 100,
            tokens_per_minute: 1000,
        });

        assert!(limiter.check_tpm().await);
        limiter.record_tokens(500).await;
        assert!(limiter.check_tpm().await);
        limiter.record_tokens(600).await;
        assert!(!limiter.check_tpm().await);
    }

    #[tokio::test]
    async fn with_defaults_has_sensible_values() {
        let limiter = RateLimiter::with_defaults();
        // Should be able to acquire at least once
        limiter.acquire().await.unwrap();
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
            let state = limiter.state.lock().await;
            assert_eq!(state.consecutive_429s, 1);
        }
        limiter.reset_backoff().await;
        {
            let state = limiter.state.lock().await;
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
        let state = limiter.state.lock().await;
        assert_eq!(state.consecutive_429s, 2);
    }

    #[tokio::test]
    async fn clone_shares_state() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        let clone = limiter.clone();
        limiter.record_tokens(500).await;
        // Clone should see the same tokens
        let state = clone.state.lock().await;
        assert_eq!(state.token_counts.len(), 1);
    }

    #[tokio::test]
    async fn tpm_check_empty_is_under_limit() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100,
        });
        assert!(limiter.check_tpm().await);
    }

    #[tokio::test]
    async fn record_tokens_accumulates() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 1000,
        });
        limiter.record_tokens(300).await;
        limiter.record_tokens(400).await;
        limiter.record_tokens(200).await;
        assert!(limiter.check_tpm().await); // 900 < 1000
        limiter.record_tokens(200).await;
        assert!(!limiter.check_tpm().await); // 1100 >= 1000
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
            let state = limiter.state.lock().await;
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
    async fn record_tokens_zero() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100,
        });
        limiter.record_tokens(0).await;
        assert!(limiter.check_tpm().await);
    }

    #[tokio::test]
    async fn with_defaults_rpm_and_tpm() {
        let limiter = RateLimiter::with_defaults();
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
            let state = limiter.state.lock().await;
            assert_eq!(state.request_timestamps.len(), 1);
        }
        limiter.acquire().await.unwrap();
        {
            let state = limiter.state.lock().await;
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
            let mut state = limiter.state.lock().await;
            state
                .request_timestamps
                .push_back(Instant::now() - Duration::from_millis(59_950));
        }
        limiter.acquire().await.unwrap();
        let state = limiter.state.lock().await;
        assert_eq!(state.request_timestamps.len(), 1);
    }

    #[tokio::test]
    async fn acquire_prunes_expired_token_count_entries() {
        // acquire()'s pruning loop also prunes `token_counts` (shared state
        // with check_tpm()'s own, separate pruning loop) -- no other test
        // seeds an expired token_counts entry before calling acquire()
        // itself, so that pop_front() call is otherwise unexercised from this
        // function.
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        {
            let mut state = limiter.state.lock().await;
            state
                .token_counts
                .push_back((Instant::now() - Duration::from_secs(61), 500));
        }
        limiter.acquire().await.unwrap();
        let state = limiter.state.lock().await;
        assert!(state.token_counts.is_empty());
    }

    #[tokio::test]
    async fn check_tpm_prunes_expired_token_entries() {
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100,
        });
        {
            let mut state = limiter.state.lock().await;
            state
                .token_counts
                .push_back((Instant::now() - Duration::from_secs(61), 90));
        }
        // The stale entry is outside the 60s window and should be pruned,
        // leaving the limiter under its TPM limit.
        assert!(limiter.check_tpm().await);
        let state = limiter.state.lock().await;
        assert!(state.token_counts.is_empty());
    }
}
