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
//! A second, coarser pool sits in front of the per-model one: an optional cap
//! *per provider*, across every model that provider serves. It exists for the
//! metered third-party API where the point of a small pool is bounding spend,
//! and where capping one experimental provider at 1 must not also throttle
//! Anthropic and OpenAI on the same machine. A provider has no pool unless one
//! is configured for it, so nothing is bounded twice by accident.
//!
//! Distinct from a blueprint's fan-out `max_workers` (which bounds a stage's
//! sub-agent fan *width*); these pools bound total in-flight inferences *per
//! model* across every agent in the world.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{AcquireError, Notify, OwnedSemaphorePermit, Semaphore};

/// How `tokio::sync::Semaphore` represents "effectively unbounded" - its own
/// maximum permit count. A model with no configured limit gets this many
/// permits, so `acquire` never actually waits for it.
const UNBOUNDED_PERMITS: usize = Semaphore::MAX_PERMITS;

/// A model id with the vendor path a gateway prepends removed, so
/// `anthropic/claude-sonnet-5` reads as `claude-sonnet-5`.
///
/// Only the path is dropped. A `:` is left alone because it does not mean the
/// same thing everywhere: on OpenRouter it introduces a variant of one model,
/// but Ollama uses it for the size tag, where `qwen3.5:9b` and `qwen3.5:70b` are
/// different models that should never share a pool - a 70b wants a far smaller
/// one. Treating the tag as a variant would hand the larger model the smaller
/// one's limit, which is the failure this whole change is about, inverted.
///
/// Used only to look up a pool limit, never to call anything: the id sent to a
/// provider stays exactly as the resolver produced it.
fn bare_model_name(model: &str) -> &str {
    match model.rsplit_once('/') {
        Some((_, name)) => name,
        None => model,
    }
}

/// Configuration for the world's inference concurrency limits.
///
/// A model listed in `per_model` uses that limit; any other model uses
/// `default_limit` (or is unbounded when that is `None`). A provider listed in
/// `per_provider` additionally bounds every one of its models together.
/// With a single world today this is just a global config table.
#[derive(Debug, Clone, Default)]
pub struct InferencePoolConfig {
    per_model: HashMap<String, usize>,
    per_provider: HashMap<String, usize>,
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

    /// Set the concurrency limit for every model one provider serves, together.
    pub fn set_provider_limit(&mut self, provider: impl Into<String>, limit: usize) {
        self.per_provider.insert(provider.into(), limit);
    }

    /// The configured limit for `model`: its explicit entry if present, else an
    /// entry under its bare name, else the default. `None` means unbounded.
    ///
    /// The bare-name step is what makes one line of config cover a model reached
    /// by more than one route. The same model carries a different id per route -
    /// `claude-sonnet-5` direct from Anthropic, `anthropic/claude-sonnet-5`
    /// through OpenRouter - so an exact-match table asked the operator to know
    /// which spelling the resolver would land on, and quietly did nothing when
    /// they guessed the other one. Nothing said so: an unmatched key looks
    /// exactly like a matched one, and the pool stays at the global default.
    ///
    /// Exact first, so a route that genuinely needs its own number can still say
    /// so - `anthropic/claude-sonnet-5 = 4` beside a bare `claude-sonnet-5` - and
    /// the more specific key wins.
    ///
    /// Matching does not merge the pools: each route keeps its own semaphore at
    /// the same size. Two routes to one model are two upstream endpoints with
    /// their own limits, and sharing one semaphore between them would throttle
    /// a run below what either endpoint allows.
    pub fn limit_for(&self, model: &str) -> Option<usize> {
        self.per_model
            .get(model)
            .or_else(|| self.per_model.get(bare_model_name(model)))
            .copied()
            .or(self.default_limit)
    }

    /// The configured limit for `provider` as a whole. `None` means the
    /// provider has no pool of its own - deliberately *not* falling back to
    /// `default_limit`, which is a per-model number: applying it per provider
    /// as well would silently tighten every install that never asked for a
    /// provider cap.
    pub fn provider_limit_for(&self, provider: &str) -> Option<usize> {
        self.per_provider.get(provider).copied()
    }
}

/// The world's live per-model inference pools. Cheap to clone-share behind an
/// `Arc`; semaphores are created lazily the first time a model is seen.
#[derive(Debug)]
pub struct InferencePools {
    /// The limits in force. Behind a lock rather than owned outright because
    /// `[limits]` is re-read whenever `config.toml` changes, and a pool that
    /// went on serving the number the daemon booted with would make the edit
    /// look like it did nothing.
    config: Mutex<InferencePoolConfig>,
    semaphores: Mutex<HashMap<String, Pool>>,
    /// Per-provider pools, created lazily and only for a provider that has a
    /// configured cap. A provider absent from here is unbounded, and its
    /// models are bounded by their own pools alone.
    provider_semaphores: Mutex<HashMap<String, Pool>>,
    /// The tick-loop wake handle, handed to every permit so that releasing one
    /// re-drives dispatch. See [`InferencePools::with_wake`].
    wake: Option<Arc<Notify>>,
}

/// One pool: its semaphore, the ceiling it is meant to have, and how many
/// permits it actually has right now.
///
/// The last two stop being the same number the moment an operator lowers a
/// limit on a busy daemon. A semaphore can only give back permits nobody is
/// holding, so a shrink takes what is idle now and leaves the rest to be taken
/// as the requests in flight finish. Nothing in flight is cancelled and no
/// slot is pulled out from under it: the pool narrows as it drains. The
/// leftover is collected the next time this pool is touched, which is the next
/// acquire against it.
#[derive(Debug)]
struct Pool {
    semaphore: Arc<Semaphore>,
    /// The ceiling asked for. `None` is unbounded.
    cap: Option<usize>,
    /// Permits the semaphore holds, held and free together.
    granted: usize,
}

impl Pool {
    /// A pool sized to `cap`, unbounded when that is `None`.
    fn new(cap: Option<usize>) -> Self {
        let granted = cap.unwrap_or(UNBOUNDED_PERMITS);
        Self {
            semaphore: Arc::new(Semaphore::new(granted)),
            cap,
            granted,
        }
    }

    /// Aim the pool at `cap` and move it as far that way as it can go without
    /// disturbing anything in flight.
    fn retarget(&mut self, cap: Option<usize>) {
        self.cap = cap;
        let target = cap.unwrap_or(UNBOUNDED_PERMITS);
        match target.cmp(&self.granted) {
            std::cmp::Ordering::Greater => {
                let extra = target - self.granted;
                self.semaphore.add_permits(extra);
                self.granted = target;
            }
            // `forget_permits` only ever takes *available* permits, so what it
            // returns is how much of the shrink actually landed.
            std::cmp::Ordering::Less => {
                self.granted -= self.semaphore.forget_permits(self.granted - target);
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    /// Slots handed out and not yet returned.
    fn in_use(&self) -> usize {
        self.granted
            .saturating_sub(self.semaphore.available_permits())
    }
}

impl InferencePools {
    /// Build the pools from a configuration.
    pub(crate) fn new(config: InferencePoolConfig) -> Self {
        Self {
            config: Mutex::new(config),
            semaphores: Mutex::new(HashMap::new()),
            provider_semaphores: Mutex::new(HashMap::new()),
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
    /// slot is invisible and everything queued behind it stays parked.
    ///
    /// Hanging the wake off the permit's `Drop` rather than off each release
    /// site is the point: the obligation can't be forgotten by a new call site,
    /// and it covers the paths that don't report an outcome at all - notably a
    /// cancelled job, which frees its permit and returns with nothing to send.
    pub(crate) fn with_wake(mut self, wake: Arc<Notify>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Acquire a permit for `provider`'s `model`, waiting for a free slot if
    /// either pool is full. The returned [`InferencePermit`] releases both
    /// slots when dropped - so the caller holds it for exactly the duration of
    /// the inference request.
    ///
    /// The provider's slot is taken first and the model's second, here and in
    /// [`try_acquire`](Self::try_acquire), so two callers can never each hold
    /// half of what the other is waiting for.
    #[cfg(test)]
    pub(crate) async fn acquire(&self, provider: &str, model: &str) -> InferencePermit {
        // The semaphores are never closed (we never call `.close()`), so
        // `acquire_owned` only ever returns `Ok`; `expect_permit` documents and
        // enforces that invariant.
        let provider_permit = match self.provider_semaphore_for(provider) {
            Some(semaphore) => Some(expect_permit(semaphore.acquire_owned().await)),
            None => None,
        };
        let semaphore = self.semaphore_for(model);
        let permit = expect_permit(semaphore.acquire_owned().await);
        self.issue(provider, model, permit, provider_permit)
    }

    /// Take a permit for `provider`'s `model` **without waiting**. Returns
    /// `None` if either pool is currently full.
    ///
    /// This is what the synchronous ECS inference-dispatch system calls: a
    /// system can't `.await`, so instead of blocking on a full pool it leaves
    /// the agent `ReadyToInfer` and retries on a later tick.
    pub(crate) fn try_acquire(&self, provider: &str, model: &str) -> Option<InferencePermit> {
        // `try_acquire_owned` errors only on "no permits" (pool full) or
        // "closed" (never, since we never close) - both mean "no slot now".
        let provider_permit = match self.provider_semaphore_for(provider) {
            Some(semaphore) => match semaphore.try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => return None,
            },
            None => None,
        };
        let semaphore = self.semaphore_for(model);
        match semaphore.try_acquire_owned() {
            Ok(permit) => Some(self.issue(provider, model, permit, provider_permit)),
            // `provider_permit` is dropped here, handing that slot straight
            // back: nothing was started, so nothing may stay held.
            Err(_) => None,
        }
    }

    /// Wrap a raw semaphore permit as an [`InferencePermit`] carrying this
    /// pool's wake handle, and trace the acquisition. Acquire and release are
    /// traced as a pair so a leaked or long-held slot can be read straight off
    /// the log.
    fn issue(
        &self,
        provider: &str,
        model: &str,
        permit: OwnedSemaphorePermit,
        provider_permit: Option<OwnedSemaphorePermit>,
    ) -> InferencePermit {
        tracing::trace!(provider = %provider, model = %model, "inference slot acquired");
        InferencePermit {
            permit: Some(permit),
            provider_permit,
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
    pub(crate) fn occupancy(&self) -> Vec<PoolOccupancy> {
        let map = leviath_core::sync::lock(&self.semaphores);
        let mut out: Vec<PoolOccupancy> = map
            .iter()
            .map(|(model, pool)| PoolOccupancy {
                model: model.clone(),
                // Counted against what the pool actually holds rather than
                // against the cap: mid-shrink those differ, and the shortfall
                // from the cap would read as slots nobody ever took.
                in_use: pool.in_use(),
                cap: pool.cap,
            })
            .collect();
        out.sort_by(|a, b| a.model.cmp(&b.model)); // stable output for logs and tests
        out
    }

    /// How many slots are in use per *provider*, against each provider's cap.
    /// Only providers with a configured cap have a pool, so a provider absent
    /// from this list is unbounded rather than idle.
    ///
    /// Reported separately from [`occupancy`](Self::occupancy) because a run
    /// parked on a full provider pool sees every model pool with room in it,
    /// and "waiting on nothing" is the one shape this reporting exists to
    /// prevent.
    pub(crate) fn provider_occupancy(&self) -> Vec<ProviderPoolOccupancy> {
        let map = leviath_core::sync::lock(&self.provider_semaphores);
        let mut out: Vec<ProviderPoolOccupancy> = map
            .iter()
            .map(|(provider, pool)| ProviderPoolOccupancy {
                provider: provider.clone(),
                in_use: pool.in_use(),
                // A provider is only in this map because it has a cap.
                cap: pool.cap.unwrap_or(UNBOUNDED_PERMITS),
            })
            .collect();
        out.sort_by(|a, b| a.provider.cmp(&b.provider)); // stable output
        out
    }

    /// Fetch (or lazily create) the semaphore bounding `provider` as a whole,
    /// or `None` when that provider has no configured cap.
    fn provider_semaphore_for(&self, provider: &str) -> Option<Arc<Semaphore>> {
        // Config first, then the map, in every path here: one lock order, so
        // two of these running at once can never wait on each other.
        // A provider whose cap has been taken out of the config has already had
        // its pool released and dropped by `reconfigure`, so there is nothing
        // to look up here.
        let cap = leviath_core::sync::lock(&self.config).provider_limit_for(provider)?;
        let mut map = leviath_core::sync::lock(&self.provider_semaphores);
        let pool = map
            .entry(provider.to_string())
            .or_insert_with(|| Pool::new(Some(cap)));
        pool.retarget(Some(cap));
        Some(pool.semaphore.clone())
    }

    /// Fetch (or lazily create) the semaphore for `model`, sized to whatever
    /// the config says now.
    fn semaphore_for(&self, model: &str) -> Arc<Semaphore> {
        let cap = leviath_core::sync::lock(&self.config).limit_for(model);
        let mut map = leviath_core::sync::lock(&self.semaphores);
        let pool = map
            .entry(model.to_string())
            .or_insert_with(|| Pool::new(cap));
        pool.retarget(cap);
        pool.semaphore.clone()
    }

    /// The limits in force, as a copy. The reader beside
    /// [`reconfigure`](Self::reconfigure), so a caller can ask what a model or
    /// a provider is capped at without reaching into the pools.
    pub(crate) fn config(&self) -> InferencePoolConfig {
        leviath_core::sync::lock(&self.config).clone()
    }

    /// Serve `config` from here on: every pool already created moves to its new
    /// ceiling, and every one created later starts at it.
    ///
    /// This is what lets `[limits] max_concurrent_inferences` and its per-model
    /// and per-provider tables take effect on a running daemon. A raised limit
    /// is available at once. A lowered one takes back the slots nobody is using
    /// and then narrows as the rest come back, so an inference already in
    /// flight runs to its end.
    pub(crate) fn reconfigure(&self, config: InferencePoolConfig) {
        let mut current = leviath_core::sync::lock(&self.config);
        *current = config;
        let mut models = leviath_core::sync::lock(&self.semaphores);
        for (model, pool) in models.iter_mut() {
            pool.retarget(current.limit_for(model));
        }
        let mut providers = leviath_core::sync::lock(&self.provider_semaphores);
        providers.retain(
            |provider, pool| match current.provider_limit_for(provider) {
                Some(cap) => {
                    pool.retarget(Some(cap));
                    true
                }
                None => {
                    pool.retarget(None);
                    false
                }
            },
        );
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
    pub(crate) fn is_full(&self) -> bool {
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

/// How busy one provider's pool is. Only a provider with a configured cap has
/// one, so unlike [`PoolOccupancy`] the cap is never absent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderPoolOccupancy {
    /// The provider the pool is scoped to.
    pub provider: String,
    /// Slots currently held, across every model this provider serves.
    pub in_use: usize,
    /// The configured limit.
    pub cap: usize,
}

impl ProviderPoolOccupancy {
    /// Whether every slot in this pool is taken.
    #[must_use]
    pub(crate) fn is_full(&self) -> bool {
        self.in_use >= self.cap
    }
}

impl std::fmt::Display for ProviderPoolOccupancy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}/{}", self.provider, self.in_use, self.cap)
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
pub(crate) struct InferencePermit {
    /// `Option` purely so `Drop` can hand the slot back *before* it wakes the
    /// loop; a field would otherwise be dropped after the `Drop` body, and the
    /// woken tick could re-check the pool while this slot was still held.
    permit: Option<OwnedSemaphorePermit>,
    /// The provider-wide slot, when that provider has a pool. Released with the
    /// model's, in the reverse of the order they were taken.
    provider_permit: Option<OwnedSemaphorePermit>,
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
        drop(self.provider_permit.take());
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

    /// The case that sent this run at the global default: the operator wrote the
    /// model as the vendor names it, the resolver landed on a gateway route that
    /// spells the same model with a vendor path, and the exact-match table
    /// matched neither. Nothing reported it - an unmatched key and a matched one
    /// look identical from outside, and the pool silently stayed at 8.
    #[test]
    fn one_line_of_config_covers_a_model_reached_by_either_route() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(8));
        cfg.set_limit("claude-sonnet-5", 24);

        assert_eq!(cfg.limit_for("claude-sonnet-5"), Some(24), "direct");
        assert_eq!(
            cfg.limit_for("anthropic/claude-sonnet-5"),
            Some(24),
            "the same model through a gateway takes the same number"
        );
        assert_eq!(
            cfg.limit_for("anthropic/claude-opus-5"),
            Some(8),
            "a different model on that vendor is untouched"
        );
    }

    /// Written the other way round, a gateway id covers itself and nothing else:
    /// the bare name is what generalises, so a key that names one route stays
    /// specific to it.
    #[test]
    fn a_key_that_names_a_route_stays_specific_to_that_route() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(8));
        cfg.set_limit("x-ai/grok-4.6", 12);

        assert_eq!(cfg.limit_for("x-ai/grok-4.6"), Some(12));
        assert_eq!(
            cfg.limit_for("grok-4.6"),
            Some(8),
            "the bare id is not the key that was written"
        );
    }

    /// A route that needs its own number can still say so beside the bare name,
    /// because exact is tried first.
    #[test]
    fn an_exact_route_entry_beats_the_bare_name() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(8));
        cfg.set_limit("claude-sonnet-5", 24);
        cfg.set_limit("anthropic/claude-sonnet-5", 4);

        assert_eq!(cfg.limit_for("anthropic/claude-sonnet-5"), Some(4));
        assert_eq!(cfg.limit_for("claude-sonnet-5"), Some(24));
    }

    /// Ollama spells the parameter count after a colon, so two sizes of one
    /// model are two ids. They must not collapse into one pool: the pool a 9b
    /// can afford is not the one a 70b can.
    #[test]
    fn an_ollama_size_tag_is_not_treated_as_a_variant() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(8));
        cfg.set_limit("qwen3.5:9b", 16);

        assert_eq!(cfg.limit_for("qwen3.5:9b"), Some(16));
        assert_eq!(
            cfg.limit_for("qwen3.5:70b"),
            Some(8),
            "the larger model keeps the default, not the 9b's number"
        );
        assert_eq!(cfg.limit_for("qwen3.5"), Some(8));
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

        let permit = pools.acquire("p", "m").await; // takes the only slot

        // A second acquire cannot complete while the permit is held.
        let pools2 = pools.clone();
        let waiting = tokio::spawn(async move { pools2.acquire("p", "m").await });
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

        let permit = pools.try_acquire("p", "m").expect("first slot is free"); // Ok arm
        assert!(pools.try_acquire("p", "m").is_none()); // Err arm: pool full
        drop(permit);
        assert!(pools.try_acquire("p", "m").is_some()); // slot freed
    }

    #[tokio::test]
    async fn acquire_unbounded_model_never_blocks() {
        let pools = InferencePools::new(InferencePoolConfig::new()); // no limits
        // Hold many permits for an unlisted (unbounded) model at once; each
        // acquire returns immediately without ever waiting for a slot.
        let mut permits = Vec::new();
        for _ in 0..64 {
            permits.push(pools.acquire("p", "free").await);
        }
        assert_eq!(permits.len(), 64);
    }

    #[test]
    fn provider_limit_is_only_what_was_configured() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(8));
        cfg.set_provider_limit("cerebras", 1);
        assert_eq!(cfg.provider_limit_for("cerebras"), Some(1));
        // The global fallback is a per-model number and is deliberately not
        // applied a second time per provider - an install that caps nothing
        // must keep the concurrency it had.
        assert_eq!(cfg.provider_limit_for("anthropic"), None);
    }

    /// The motivating case: one provider capped at 1 while every other provider
    /// keeps the global cap.
    #[test]
    fn a_provider_cap_bounds_its_models_together_and_nobody_else() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(8));
        cfg.set_provider_limit("cerebras", 1);
        let pools = InferencePools::new(cfg);

        // Two *different* models of the capped provider: the second waits, even
        // though its own model pool is untouched.
        let held = pools.try_acquire("cerebras", "gpt-oss-120b").expect("free");
        assert!(
            pools.try_acquire("cerebras", "llama-3.3-70b").is_none(),
            "the provider's one slot is taken, whatever the model"
        );
        // An uncapped provider is unaffected by any of that.
        let _elsewhere = pools
            .try_acquire("anthropic", "claude-sonnet-5")
            .expect("another provider has no pool of its own");

        drop(held);
        assert!(
            pools.try_acquire("cerebras", "llama-3.3-70b").is_some(),
            "the slot comes back when the request finishes"
        );
    }

    /// A model pool that refuses must hand the provider slot straight back -
    /// nothing was started, so nothing may stay held.
    #[test]
    fn a_full_model_pool_releases_the_provider_slot_it_just_took() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        cfg.set_provider_limit("p", 4);
        let pools = InferencePools::new(cfg);

        let held = pools.try_acquire("p", "m").expect("the only model slot");
        assert!(pools.try_acquire("p", "m").is_none(), "model pool is full");
        // Three provider slots free, not two: the refused attempt kept none.
        assert_eq!(
            pools.provider_occupancy(),
            vec![ProviderPoolOccupancy {
                provider: "p".to_string(),
                in_use: 1,
                cap: 4,
            }]
        );
        drop(held);
    }

    #[tokio::test]
    async fn acquire_waits_for_a_provider_slot_and_takes_it_when_freed() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_provider_limit("p", 1);
        let pools = Arc::new(InferencePools::new(cfg));

        let held = pools.acquire("p", "one-model").await;
        let pools2 = pools.clone();
        // A different model, so only the provider pool can be what it waits on.
        let waiting = tokio::spawn(async move { pools2.acquire("p", "other-model").await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "the provider's only slot is held");
        drop(held);
        let taken = waiting.await.expect("the waiter is handed the freed slot");
        drop(taken);
    }

    #[test]
    fn provider_occupancy_lists_only_capped_providers_that_have_run() {
        let mut cfg = InferencePoolConfig::new().with_default(Some(4));
        cfg.set_provider_limit("b", 2);
        cfg.set_provider_limit("a", 1);
        let pools = InferencePools::new(cfg);
        assert!(
            pools.provider_occupancy().is_empty(),
            "pools are created lazily, so an unused cap has no entry"
        );

        let _a = pools.try_acquire("a", "m").expect("free");
        let _b = pools.try_acquire("b", "m2").expect("free");
        let _uncapped = pools.try_acquire("c", "m3").expect("free");
        let occupancy = pools.provider_occupancy();
        assert_eq!(
            occupancy,
            vec![
                ProviderPoolOccupancy {
                    provider: "a".to_string(),
                    in_use: 1,
                    cap: 1,
                },
                ProviderPoolOccupancy {
                    provider: "b".to_string(),
                    in_use: 1,
                    cap: 2,
                },
            ],
            "sorted, and the uncapped provider has no pool to report"
        );
        assert!(occupancy[0].is_full());
        assert!(!occupancy[1].is_full());
        assert_eq!(occupancy[0].to_string(), "a=1/1");
        // The daemon's health payload carries these, so assert the wire form
        // rather than assuming it.
        let json = serde_json::to_string(&occupancy).expect("occupancy serializes");
        assert_eq!(
            serde_json::from_str::<Vec<ProviderPoolOccupancy>>(&json).expect("and reads back"),
            occupancy
        );
    }

    #[test]
    fn provider_semaphore_is_cached_per_provider() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_provider_limit("p", 2);
        let pools = InferencePools::new(cfg);
        let first = pools.provider_semaphore_for("p").expect("configured");
        let second = pools.provider_semaphore_for("p").expect("cache hit");
        assert!(Arc::ptr_eq(&first, &second));
        assert!(pools.provider_semaphore_for("other").is_none());
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

    /// The contract, at its narrowest: handing a slot back has to wake
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

            let permit = pools.try_acquire("p", "m").expect("the only slot");
            assert!(pools.try_acquire("p", "m").is_none(), "pool full");
            // Nothing has released yet, so there is no wake to collect.
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), wake.notified())
                    .await
                    .is_err(),
                "holding a permit must not wake the driver"
            );

            drop(permit);
            // The slot is back...
            assert!(pools.try_acquire("p", "m").is_some(), "slot freed");
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
        let permit = pools.try_acquire("p", "m").expect("the only slot");
        assert!(pools.try_acquire("p", "m").is_none());
        drop(permit);
        assert!(pools.try_acquire("p", "m").is_some());
    }

    /// The async `acquire` path carries the wake too, not just `try_acquire`.
    #[tokio::test]
    async fn the_awaiting_acquire_path_also_wakes_on_release() {
        let wake = Arc::new(Notify::new());
        let pools = InferencePools::new(InferencePoolConfig::new()).with_wake(wake.clone());
        drop(pools.acquire("p", "m").await);
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

        let held = pools.try_acquire("p", "capped").expect("free");
        let _unbounded = pools
            .try_acquire("p", "free")
            .expect("unbounded is always free");
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
        let _second = pools.try_acquire("p", "capped").expect("second of two");
        assert!(pools.occupancy()[0].is_full(), "2 of 2 is full");
        drop(held);
        assert!(!pools.occupancy()[0].is_full(), "and not full once freed");
    }

    /// Raising `[limits] max_concurrent_inferences` under a running daemon
    /// widens a pool that is already in use, rather than waiting for a restart.
    #[tokio::test]
    async fn a_raised_limit_widens_a_pool_already_in_use() {
        let pools = InferencePools::new(InferencePoolConfig::new().with_default(Some(1)));
        let held = pools.try_acquire("p", "m").expect("the one slot");
        assert!(pools.try_acquire("p", "m").is_none(), "one wide");

        pools.reconfigure(InferencePoolConfig::new().with_default(Some(3)));
        let _b = pools
            .try_acquire("p", "m")
            .expect("the widened pool has room");
        let _c = pools.try_acquire("p", "m").expect("and again");
        assert!(pools.try_acquire("p", "m").is_none(), "three wide now");
        drop(held);
    }

    /// Lowering it takes back the slots nobody is using and then narrows as the
    /// requests in flight finish. Nothing is cancelled and no request loses the
    /// slot it is holding.
    #[tokio::test]
    async fn a_lowered_limit_drains_rather_than_cancelling() {
        let pools = InferencePools::new(InferencePoolConfig::new().with_default(Some(3)));
        let a = pools.try_acquire("p", "m").expect("a slot");
        let b = pools.try_acquire("p", "m").expect("another");

        pools.reconfigure(InferencePoolConfig::new().with_default(Some(1)));
        assert!(
            pools.try_acquire("p", "m").is_none(),
            "the third, idle slot is taken back at once"
        );
        drop(a);
        assert!(
            pools.try_acquire("p", "m").is_none(),
            "the slot the first request gave back goes to the shrink, not to a new request"
        );
        drop(b);
        let _last = pools.try_acquire("p", "m").expect("one slot is left");
        assert!(pools.try_acquire("p", "m").is_none(), "and one is all");
    }

    /// A pool with no limit at all can be given one, and have it taken away.
    #[tokio::test]
    async fn an_unbounded_pool_can_be_capped_and_uncapped() {
        let pools = InferencePools::new(InferencePoolConfig::new());
        let _a = pools.try_acquire("p", "m").expect("unbounded");
        let _b = pools.try_acquire("p", "m").expect("still unbounded");

        pools.reconfigure(InferencePoolConfig::new().with_default(Some(1)));
        assert!(
            pools.try_acquire("p", "m").is_none(),
            "the new cap is already exceeded, so nothing more gets in"
        );

        pools.reconfigure(InferencePoolConfig::new());
        assert!(
            pools.try_acquire("p", "m").is_some(),
            "and removing the cap makes room again"
        );
    }

    /// The same for a per-provider cap, which is the pool that only exists
    /// while the config names it.
    #[tokio::test]
    async fn a_provider_cap_can_be_added_and_taken_away() {
        let pools = InferencePools::new(InferencePoolConfig::new().with_default(Some(4)));
        let _warm = pools.try_acquire("slow", "m").expect("no provider cap yet");

        let mut capped = InferencePoolConfig::new().with_default(Some(4));
        capped.set_provider_limit("slow", 1);
        pools.reconfigure(capped);
        let held = pools
            .try_acquire("slow", "m")
            .expect("the provider's one slot");
        assert!(
            pools.try_acquire("slow", "other").is_none(),
            "the provider cap bounds every model it serves"
        );

        // Raised, with the pool now in existence and one slot of it held: the
        // arm that keeps a pool it already has rather than making a new one.
        let mut wider = InferencePoolConfig::new().with_default(Some(4));
        wider.set_provider_limit("slow", 2);
        pools.reconfigure(wider);
        let second = pools
            .try_acquire("slow", "other")
            .expect("the raised provider cap has room");
        assert!(
            pools.try_acquire("slow", "third").is_none(),
            "and two is all it has"
        );
        drop(held);
        drop(second);

        pools.reconfigure(InferencePoolConfig::new().with_default(Some(4)));
        assert!(
            pools.try_acquire("slow", "other").is_some(),
            "with the cap deleted the provider is unbounded again"
        );
        assert!(
            pools.provider_occupancy().is_empty(),
            "and it is no longer reported as a pool"
        );
    }

    /// Reconfiguring with the same numbers is a no-op, so a config save that
    /// changed something else entirely cannot disturb a pool.
    #[tokio::test]
    async fn reconfiguring_with_the_same_limits_changes_nothing() {
        let pools = InferencePools::new(InferencePoolConfig::new().with_default(Some(2)));
        let _held = pools.try_acquire("p", "m").expect("a slot");
        pools.reconfigure(InferencePoolConfig::new().with_default(Some(2)));
        assert_eq!(
            pools.occupancy(),
            vec![PoolOccupancy {
                model: "m".to_string(),
                in_use: 1,
                cap: Some(2),
            }]
        );
        assert!(
            pools.try_acquire("p", "m").is_some(),
            "the free slot is untouched"
        );
    }

    /// Occupancy reports the cap the operator set most recently, not the one
    /// the daemon started with.
    #[tokio::test]
    async fn occupancy_reports_the_cap_in_force_now() {
        let pools = InferencePools::new(InferencePoolConfig::new().with_default(Some(2)));
        let _held = pools.try_acquire("p", "m").expect("a slot");
        pools.reconfigure(InferencePoolConfig::new().with_default(Some(5)));
        assert_eq!(
            pools.occupancy(),
            vec![PoolOccupancy {
                model: "m".to_string(),
                in_use: 1,
                cap: Some(5),
            }]
        );
        assert_eq!(pools.config().limit_for("m"), Some(5));
    }
}
