//! Per-model inference concurrency pools.
//!
//! A single [`InferencePools`] belongs to the world and bounds how many
//! inference requests are in flight to each model at once - e.g. "at most 3
//! concurrent requests to `anthropic:claude-opus-4-8`", "at most 1 to a local
//! `ollama:gemma`". This is the world-level control the ECS inference-dispatch
//! system consults before issuing a request: an agent only leaves `ReadyToInfer`
//! once a permit for its model is available; otherwise it stays ready and is
//! retried on a later tick (so "waiting for a slot" costs nothing but data).
//!
//! Why this matters: a single inference can take up to an hour for very large
//! requests, so a permit may legitimately be held for a very long time - the
//! pool is what keeps us from opening an unbounded number of simultaneous
//! long-lived requests to a provider.
//!
//! Distinct from a blueprint's fan-out `max_workers` (which bounds a stage's
//! sub-agent fan *width*); these pools bound total in-flight inferences *per
//! model* across every agent in the world.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

/// How `tokio::sync::Semaphore` represents "effectively unbounded" - its own
/// maximum permit count. A model with no configured limit gets this many
/// permits, so `acquire` never actually waits for it.
const UNBOUNDED_PERMITS: usize = Semaphore::MAX_PERMITS;

/// Configuration for the world's per-model inference concurrency limits.
///
/// A model listed in `per_model` uses that limit; any other model uses
/// `default_limit` (or is unbounded when that is `None`). With a single world
/// today this is just a global config table.
#[derive(Debug, Clone, Default)]
pub struct InferencePoolConfig {
    per_model: HashMap<String, usize>,
    default_limit: Option<usize>,
}

impl InferencePoolConfig {
    /// An empty config: every model unbounded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the fallback limit applied to models with no explicit entry.
    /// `None` leaves unlisted models unbounded.
    pub fn with_default(mut self, limit: Option<usize>) -> Self {
        self.default_limit = limit;
        self
    }

    /// Set the concurrency limit for a specific model key.
    pub fn set_limit(&mut self, model: impl Into<String>, limit: usize) {
        self.per_model.insert(model.into(), limit);
    }

    /// The configured limit for `model`: its explicit entry if present, else the
    /// default. `None` means unbounded.
    pub fn limit_for(&self, model: &str) -> Option<usize> {
        self.per_model.get(model).copied().or(self.default_limit)
    }
}

/// The world's live per-model inference pools. Cheap to clone-share behind an
/// `Arc`; semaphores are created lazily the first time a model is seen.
#[derive(Debug)]
pub struct InferencePools {
    config: InferencePoolConfig,
    semaphores: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl InferencePools {
    /// Build the pools from a configuration.
    pub fn new(config: InferencePoolConfig) -> Self {
        Self {
            config,
            semaphores: Mutex::new(HashMap::new()),
        }
    }

    /// Acquire a permit for `model`, waiting for a free slot if the pool is
    /// full. The returned [`InferencePermit`] releases the slot when dropped -
    /// so the caller holds it for exactly the duration of the inference request.
    pub async fn acquire(&self, model: &str) -> InferencePermit {
        let semaphore = self.semaphore_for(model);
        // The semaphore is never closed (we never call `.close()`), so
        // `acquire_owned` only ever returns `Ok`; `expect_permit` documents and
        // enforces that invariant.
        let permit = expect_permit(semaphore.acquire_owned().await);
        InferencePermit { _permit: permit }
    }

    /// Try to take a permit for `model` **without waiting**. Returns `None` if
    /// the pool is currently full.
    ///
    /// This is what the synchronous ECS inference-dispatch system calls: a
    /// system can't `.await`, so instead of blocking on a full pool it leaves
    /// the agent `ReadyToInfer` and retries on a later tick.
    pub fn try_acquire(&self, model: &str) -> Option<InferencePermit> {
        let semaphore = self.semaphore_for(model);
        // `try_acquire_owned` errors only on "no permits" (pool full) or
        // "closed" (never, since we never close) - both mean "no slot now".
        match semaphore.try_acquire_owned() {
            Ok(permit) => Some(InferencePermit { _permit: permit }),
            Err(_) => None,
        }
    }

    /// Fetch (or lazily create) the semaphore for `model`.
    fn semaphore_for(&self, model: &str) -> Arc<Semaphore> {
        let mut map = self
            .semaphores
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = map.get(model) {
            return existing.clone();
        }
        let permits = self.config.limit_for(model).unwrap_or(UNBOUNDED_PERMITS);
        let semaphore = Arc::new(Semaphore::new(permits));
        map.insert(model.to_string(), semaphore.clone());
        semaphore
    }
}

/// Unwrap an `acquire_owned` result, panicking with a clear message if the
/// semaphore was closed. Extracted as a free function (rather than an inline
/// `.expect(...)`) so both arms - the ordinary `Ok` and the never-in-practice
/// `Err` - are exercised directly by unit tests, keeping the region covered.
fn expect_permit(result: Result<OwnedSemaphorePermit, AcquireError>) -> OwnedSemaphorePermit {
    result.expect("inference pool semaphore is never closed")
}

/// An RAII permit occupying one slot of a model's inference pool. Dropping it
/// frees the slot for the next waiting agent.
#[derive(Debug)]
pub struct InferencePermit {
    _permit: OwnedSemaphorePermit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_for_prefers_explicit_over_default() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(5));
        cfg.set_limit("anthropic:x", 3);
        assert_eq!(cfg.limit_for("anthropic:x"), Some(3)); // explicit entry
        assert_eq!(cfg.limit_for("ollama:gemma"), Some(5)); // falls back to default
    }

    #[test]
    fn limit_for_unbounded_when_no_entry_and_no_default() {
        let cfg = InferencePoolConfig::new();
        assert_eq!(cfg.limit_for("anything"), None);
    }

    #[test]
    fn semaphore_for_is_cached_per_model() {
        let pools = InferencePools::new(InferencePoolConfig::new());
        let first = pools.semaphore_for("m");
        let second = pools.semaphore_for("m"); // cache hit - same Arc
        assert!(Arc::ptr_eq(&first, &second));
        let other = pools.semaphore_for("n"); // cache miss - distinct Arc
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[tokio::test]
    async fn acquire_bounds_concurrency_and_releases_on_drop() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = Arc::new(InferencePools::new(cfg));

        let permit = pools.acquire("m").await; // takes the only slot

        // A second acquire cannot complete while the permit is held.
        let pools2 = pools.clone();
        let waiting = tokio::spawn(async move { pools2.acquire("m").await });
        // Give the task a chance to run and block on the full pool.
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "second acquire must wait for a slot"
        );

        drop(permit); // free the slot
        // Now the waiter can obtain the permit.
        let _second = waiting.await.expect("waiter task should not panic");
    }

    #[test]
    fn try_acquire_returns_none_when_full() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = InferencePools::new(cfg);

        let permit = pools.try_acquire("m").expect("first slot is free"); // Ok arm
        assert!(pools.try_acquire("m").is_none()); // Err arm: pool full
        drop(permit);
        assert!(pools.try_acquire("m").is_some()); // slot freed
    }

    #[tokio::test]
    async fn acquire_unbounded_model_never_blocks() {
        let pools = InferencePools::new(InferencePoolConfig::new()); // no limits
        // Hold many permits for an unlisted (unbounded) model at once; each
        // acquire returns immediately without ever waiting for a slot.
        let mut permits = Vec::new();
        for _ in 0..64 {
            permits.push(pools.acquire("free").await);
        }
        assert_eq!(permits.len(), 64);
    }

    #[test]
    fn expect_permit_returns_ok_permit() {
        let sem = Arc::new(Semaphore::new(1));
        let ok = sem.clone().try_acquire_owned().unwrap();
        // Wrap and unwrap through the same boundary the async path uses.
        let permit = expect_permit(Ok(ok));
        drop(permit);
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    #[should_panic(expected = "never closed")]
    async fn expect_permit_panics_on_closed_semaphore() {
        let sem = Arc::new(Semaphore::new(0));
        sem.close();
        // Acquiring on a closed semaphore yields the `Err` arm.
        let _ = expect_permit(sem.acquire_owned().await);
    }
}
