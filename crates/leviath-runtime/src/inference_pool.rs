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

use tokio::sync::{AcquireError, Notify, OwnedSemaphorePermit, Semaphore};

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
    /// The tick-loop wake handle, handed to every permit so that releasing one
    /// re-drives dispatch. See [`InferencePools::with_wake`].
    wake: Option<Arc<Notify>>,
}

impl InferencePools {
    /// Build the pools from a configuration.
    pub fn new(config: InferencePoolConfig) -> Self {
        Self {
            config,
            semaphores: Mutex::new(HashMap::new()),
            wake: None,
        }
    }

    /// Attach the tick-loop wake handle, so that **releasing** a permit wakes
    /// the driver.
    ///
    /// This is load-bearing, not a nicety. `dispatch_inference` leaves a
    /// slot-starved agent `ReadyToInfer` to be retried "on a later tick", and
    /// the loop is event-driven: a later tick only happens when something wakes
    /// it. So every path that frees a permit owes the loop a wake, or the freed
    /// slot is invisible and everything queued behind it stays parked (issue
    /// #189).
    ///
    /// Hanging the wake off the permit's `Drop` rather than off each release
    /// site is the point: the obligation can't be forgotten by a new call site,
    /// and it covers the paths that don't report an outcome at all - notably a
    /// cancelled job, which frees its permit and returns with nothing to send.
    pub fn with_wake(mut self, wake: Arc<Notify>) -> Self {
        self.wake = Some(wake);
        self
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
        self.issue(model, permit)
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
            Ok(permit) => Some(self.issue(model, permit)),
            Err(_) => None,
        }
    }

    /// Wrap a raw semaphore permit as an [`InferencePermit`] carrying this
    /// pool's wake handle, and trace the acquisition. Acquire and release are
    /// traced as a pair so a leaked or long-held slot can be read straight off
    /// the log.
    fn issue(&self, model: &str, permit: OwnedSemaphorePermit) -> InferencePermit {
        tracing::trace!(model = %model, "inference slot acquired");
        InferencePermit {
            permit: Some(permit),
            model: model.to_string(),
            wake: self.wake.clone(),
        }
    }

    /// How many slots are in use per model, against each model's cap. `None` as
    /// the cap means the model is unbounded. Only models that have actually been
    /// used appear - the semaphores are created lazily.
    ///
    /// This is what makes "the pool is full and has been for hours" observable
    /// rather than inferred.
    pub fn occupancy(&self) -> Vec<PoolOccupancy> {
        let map = self
            .semaphores
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<PoolOccupancy> = map
            .iter()
            .map(|(model, semaphore)| {
                let cap = self.config.limit_for(model);
                let free = semaphore.available_permits();
                PoolOccupancy {
                    model: model.clone(),
                    // An unbounded pool starts at `UNBOUNDED_PERMITS`, so its
                    // in-use count is the shortfall from that, not from a cap.
                    in_use: cap.unwrap_or(UNBOUNDED_PERMITS).saturating_sub(free),
                    cap,
                }
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model)); // stable output for logs and tests
        out
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

/// How busy one model's pool is. `cap: None` means the model is unbounded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PoolOccupancy {
    /// The model key the pool is scoped to.
    pub model: String,
    /// Slots currently held.
    pub in_use: usize,
    /// The configured limit, or `None` when unbounded.
    pub cap: Option<usize>,
}

impl PoolOccupancy {
    /// Whether every slot in this pool is taken. An unbounded pool never is.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.cap.is_some_and(|cap| self.in_use >= cap)
    }
}

impl std::fmt::Display for PoolOccupancy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.cap {
            Some(cap) => write!(f, "{}={}/{}", self.model, self.in_use, cap),
            None => write!(f, "{}={}/unbounded", self.model, self.in_use),
        }
    }
}

/// Unwrap an `acquire_owned` result, panicking with a clear message if the
/// semaphore was closed. Extracted as a free function (rather than an inline
/// `.expect(...)`) so both arms - the ordinary `Ok` and the never-in-practice
/// `Err` - are exercised directly by unit tests, keeping the region covered.
///
/// Shared with the tool lane, whose semaphore is never closed either.
pub(crate) fn expect_permit(
    result: Result<OwnedSemaphorePermit, AcquireError>,
) -> OwnedSemaphorePermit {
    result.expect("a lane semaphore is never closed")
}

/// An RAII permit occupying one slot of a model's inference pool. Dropping it
/// frees the slot for the next waiting agent **and wakes the tick loop**, so the
/// agents parked on a full pool are re-driven and can take it.
#[derive(Debug)]
pub struct InferencePermit {
    /// `Option` purely so `Drop` can hand the slot back *before* it wakes the
    /// loop; a field would otherwise be dropped after the `Drop` body, and the
    /// woken tick could re-check the pool while this slot was still held.
    permit: Option<OwnedSemaphorePermit>,
    /// The model whose pool this slot belongs to; carried for the release trace.
    model: String,
    /// Present when the pools were built with [`InferencePools::with_wake`].
    wake: Option<Arc<Notify>>,
}

impl Drop for InferencePermit {
    fn drop(&mut self) {
        // Release first, wake second. The other order is a race: the woken tick
        // could run `dispatch_inference` before this slot was actually handed
        // back, find the pool still full, and park again - with nothing left to
        // wake it. That is the exact failure this whole mechanism exists to stop.
        drop(self.permit.take());
        tracing::trace!(model = %self.model, "inference slot released");
        // `notify_one` stores a permit when nobody is parked yet, so a wake that
        // lands mid-tick is remembered rather than lost.
        if let Some(wake) = &self.wake {
            wake.notify_one();
        }
    }
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

    /// The issue #189 contract, at its narrowest: handing a slot back has to wake
    /// the driver. `dispatch_inference` parks a slot-starved agent to be retried
    /// "on a later tick", and the loop is event-driven - so a silent release
    /// leaves the freed capacity invisible.
    #[tokio::test]
    async fn dropping_a_permit_frees_the_slot_and_wakes_the_driver() {
        leviath_testkit::with_tracing(|| async {
            let mut cfg = InferencePoolConfig::new();
            cfg.set_limit("m", 1);
            let wake = Arc::new(Notify::new());
            let pools = InferencePools::new(cfg).with_wake(wake.clone());

            let permit = pools.try_acquire("m").expect("the only slot");
            assert!(pools.try_acquire("m").is_none(), "pool full");
            // Nothing has released yet, so there is no wake to collect.
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), wake.notified())
                    .await
                    .is_err(),
                "holding a permit must not wake the driver"
            );

            drop(permit);
            // The slot is back...
            assert!(pools.try_acquire("m").is_some(), "slot freed");
            // ...and the driver was told, so a parked loop re-drives dispatch.
            tokio::time::timeout(std::time::Duration::from_millis(20), wake.notified())
                .await
                .expect("releasing a permit must wake the driver");
        })
        .await;
    }

    /// Pools built without a wake (the embedding case, and every pre-existing
    /// caller) still release cleanly - the handle is optional, not required.
    #[tokio::test]
    async fn a_permit_without_a_wake_handle_still_releases() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = InferencePools::new(cfg); // no `with_wake`
        let permit = pools.try_acquire("m").expect("the only slot");
        assert!(pools.try_acquire("m").is_none());
        drop(permit);
        assert!(pools.try_acquire("m").is_some());
    }

    /// The async `acquire` path carries the wake too, not just `try_acquire`.
    #[tokio::test]
    async fn the_awaiting_acquire_path_also_wakes_on_release() {
        let wake = Arc::new(Notify::new());
        let pools = InferencePools::new(InferencePoolConfig::new()).with_wake(wake.clone());
        drop(pools.acquire("m").await);
        tokio::time::timeout(std::time::Duration::from_millis(20), wake.notified())
            .await
            .expect("an awaited permit wakes on release as well");
    }

    #[tokio::test]
    async fn occupancy_reports_in_use_against_each_models_cap() {
        let mut cfg = InferencePoolConfig::new().with_default(None); // unlisted = unbounded
        cfg.set_limit("capped", 2);
        let pools = InferencePools::new(cfg);

        // Untouched models don't appear: the semaphores are lazy.
        assert!(pools.occupancy().is_empty());

        let held = pools.try_acquire("capped").expect("free");
        let _unbounded = pools.try_acquire("free").expect("unbounded is always free");
        let occ = pools.occupancy();
        assert_eq!(
            occ,
            vec![
                PoolOccupancy {
                    model: "capped".to_string(),
                    in_use: 1,
                    cap: Some(2)
                },
                PoolOccupancy {
                    model: "free".to_string(),
                    in_use: 1,
                    cap: None
                },
            ],
            "sorted by model, in-use counted against the cap where there is one"
        );
        assert!(!occ[0].is_full(), "1 of 2 is not full");
        assert!(!occ[1].is_full(), "an unbounded pool is never full");
        assert_eq!(occ[0].to_string(), "capped=1/2");
        assert_eq!(occ[1].to_string(), "free=1/unbounded");

        // Fill the capped pool and it reads as full.
        let _second = pools.try_acquire("capped").expect("second of two");
        assert!(pools.occupancy()[0].is_full(), "2 of 2 is full");
        drop(held);
        assert!(!pools.occupancy()[0].is_full(), "and not full once freed");
    }
}
