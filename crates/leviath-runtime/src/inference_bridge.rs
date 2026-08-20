//! The async worker side of the ECS inference stage - the sync-ECS ↔ async-I/O
//! bridge for inference.
//!
//! Systems can't `.await`, and a single inference can take up to an hour, so the
//! inference-dispatch system never runs the network call itself. Instead it
//! builds an [`InferenceJob`] (an agent's request plus the per-model pool permit
//! it acquired) and `tokio::spawn`s [`run_inference_job`]. That short-lived task
//! performs the call with the permit held, reports an [`InferenceOutcome`] on the
//! results channel, and wakes the tick loop; the inference-collect system drains
//! outcomes on a later tick and applies them back to the agents.
//!
//! One task exists per *in-flight request* (bounded by the per-model
//! [`InferencePools`](crate::inference_pool::InferencePools) permits), **never**
//! one per agent - that is what keeps CPU bounded by work, not by agent count.

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::entity::Entity;
use leviath_providers::{InferenceRequest, InferenceResponse, Provider, ProviderError};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::inference_pool::InferencePermit;

/// Total attempts, including the first, for a transient inference failure.
///
/// Served from `[limits] inference_retry_attempts`, which defaults to this.
pub const DEFAULT_RETRY_ATTEMPTS: u32 = 4;

/// The first backoff, in milliseconds, after an ordinary transient failure.
///
/// Served from `[limits] inference_retry_base_ms`, which defaults to this. Each
/// further retry doubles it, so the default schedule is 1s, 2s, 4s.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 1_000;

/// The first backoff after a *capacity* failure (429, or a 529 "overloaded")
/// the provider gave no `Retry-After` for, in seconds.
///
/// Deliberately much larger than [`DEFAULT_RETRY_BASE_DELAY_MS`]. An overload
/// window lasts minutes, so a second of waiting only buys another refusal: the
/// reported run (issue #417) spent all three of its retries inside one 529
/// window and was failed with 44 iterations of finished work in hand.
pub const CAPACITY_BASE_DELAY_SECS: u64 = 15;

/// The longest one capacity backoff may last, in seconds, and the ceiling on a
/// `Retry-After` the provider asks for.
///
/// A minute is long enough to leave most overload windows and short enough that
/// a run still notices when the provider comes back. A server asking for longer
/// than this is waited out a minute at a time instead, which costs an extra
/// refusal but keeps one header from parking a run for an hour.
pub const CAPACITY_MAX_DELAY_SECS: u64 = 60;

/// The ceiling on the *cumulative* backoff of a single inference job, in
/// seconds, across every retry it makes.
///
/// The overall bound on waiting, and the answer to "how long can a retrying job
/// hold its pool slot": five minutes of sleeping, however the attempts,
/// backoffs and `Retry-After` hints add up. Network time is on top of it and is
/// bounded separately by [`RetryPolicy::job_timeout`].
pub const MAX_TOTAL_BACKOFF_SECS: u64 = 300;

/// How a transient inference failure is retried before the agent is failed.
///
/// Transient errors (see [`ProviderError::is_transient`]) are retried with
/// exponential backoff; a permanent error (auth, invalid request, token limit)
/// fails immediately. This keeps a passing network blip from marking a stage
/// `error` and carrying its half-finished work forward.
///
/// A *capacity* failure - a 429, or Anthropic's 529 "overloaded" - is retried on
/// its own, much slower schedule (see [`Self::capacity_base_delay`]), because it
/// describes a window that lasts minutes rather than a blip that clears in a
/// second. When the provider said how long to wait, that answer is used instead
/// of any of these numbers.
///
/// Every schedule is bounded twice over: by [`Self::max_attempts`], and by
/// [`Self::max_total_backoff`] on the sum of the waits. A job can never retry
/// forever.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first (e.g. `4` = one try + three retries).
    pub max_attempts: u32,
    /// Base backoff; the retry after attempt `n` waits `base_delay * 2^(n-1)`
    /// (so 1s, 2s, 4s, … for a 1s base).
    pub base_delay: Duration,
    /// Base backoff for a capacity failure the provider gave no `Retry-After`
    /// for, doubling per attempt exactly as [`Self::base_delay`] does and capped
    /// at [`Self::capacity_max_delay`]. Defaults to
    /// [`CAPACITY_BASE_DELAY_SECS`], so the default schedule is 15s, 30s, 60s.
    pub capacity_base_delay: Duration,
    /// Ceiling on one capacity backoff, and on an honored `Retry-After`.
    /// Defaults to [`CAPACITY_MAX_DELAY_SECS`].
    pub capacity_max_delay: Duration,
    /// Ceiling on the sum of every backoff this job sleeps. Once it is spent the
    /// job stops retrying and reports the last error, whatever
    /// [`Self::max_attempts`] still allowed. Defaults to
    /// [`MAX_TOTAL_BACKOFF_SECS`].
    pub max_total_backoff: Duration,
    /// Hard ceiling on the total wall-clock time one job (all attempts +
    /// backoffs) may run before it is aborted and its pool slot freed.
    ///
    /// Providers apply this same deadline as their own per-call timeout, so a
    /// call normally ends there. This outer bound is the backstop: a provider
    /// timer can be defeated (e.g. a connection that keeps trickling keepalive
    /// bytes without ever completing resets a read timer forever), which would
    /// leak the permit until the model's pool fills and new agents never get a
    /// slot. Wrapping the whole job in a wall-clock timeout guarantees the slot
    /// is released within a fixed time regardless. Defaults to
    /// [`leviath_providers::DEFAULT_INFERENCE_TIMEOUT_SECS`]; a stage's
    /// `request_timeout_secs` overrides it per stage.
    pub job_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RETRY_ATTEMPTS,
            base_delay: Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS),
            capacity_base_delay: Duration::from_secs(CAPACITY_BASE_DELAY_SECS),
            capacity_max_delay: Duration::from_secs(CAPACITY_MAX_DELAY_SECS),
            max_total_backoff: Duration::from_secs(MAX_TOTAL_BACKOFF_SECS),
            // The unified default inference deadline. A stage's
            // `request_timeout_secs` overrides this per stage (see
            // `pipeline::retry_policy_for`); the providers apply the same value
            // as their own per-call timeout, so this is the single outer bound
            // that also frees the pool slot if a provider's timer is defeated.
            job_timeout: Duration::from_secs(leviath_providers::DEFAULT_INFERENCE_TIMEOUT_SECS),
        }
    }
}

/// A unit of inference work the dispatch system hands to the worker pool.
pub struct InferenceJob {
    /// The agent this inference is for.
    pub entity: Entity,
    /// The provider to call (already resolved for the agent's model).
    pub provider: Arc<dyn Provider>,
    /// The assembled request.
    pub request: InferenceRequest,
    /// The per-model pool permit, held for the whole request and released when
    /// the job finishes.
    pub permit: InferencePermit,
    /// When set, count the assembled request's tokens exactly (via the
    /// provider's `count_tokens`, which uses a remote endpoint where available)
    /// before calling `infer`, and fail early if it would exceed the model's
    /// context window. Off by default - the runtime's cheap `len/4` estimates
    /// drive normal budgeting; this is the opt-in accurate guard.
    pub exact_token_counting: bool,
}

/// Flatten a request into the text whose tokens we count for the budget guard:
/// system blocks, every message's textual content, and each tool's name +
/// description + JSON schema. This mirrors what the provider sends closely enough
/// for a context-window check (exact per-message/role overhead is the provider's
/// to add; the bulk is this text).
fn flatten_request_text(request: &InferenceRequest) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in &request.system {
        parts.push(block.text.clone());
    }
    for msg in &request.messages {
        parts.push(msg.content.as_text());
    }
    for tool in &request.tools {
        parts.push(tool.name.clone());
        parts.push(tool.description.clone());
        parts.push(tool.parameters.to_string());
    }
    parts.join("\n")
}

/// `base` doubled once per attempt already made: `base * 2^(attempt - 1)`.
///
/// Saturating throughout, and the exponent is clamped, so a policy with a large
/// base or a job that somehow retried thousands of times yields a very long
/// duration rather than an overflow panic.
fn exponential(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1).min(16)))
}

/// How long to wait before retrying `error`, or `None` to stop and report it.
///
/// `attempt` is the attempt that just failed, counting from 1, and `spent` is
/// the backoff this job has already slept. Pure, so the whole schedule - the
/// ordinary one, the capacity one, an honored `Retry-After`, and both ceilings -
/// is asserted in tests without a second of real waiting.
///
/// The order of the checks is the policy: a permanent error is never retried, an
/// exhausted attempt count stops, an exhausted backoff budget stops, and only
/// then does the kind of failure decide how long to wait.
fn backoff_after(
    policy: &RetryPolicy,
    error: &ProviderError,
    attempt: u32,
    spent: Duration,
) -> Option<Duration> {
    if !error.is_transient() || attempt >= policy.max_attempts {
        return None;
    }
    // The overall ceiling: once the job has slept its whole budget it stops,
    // however many attempts were left. This is what guarantees a retrying run
    // cannot wait forever, whatever a provider's `Retry-After` asks for.
    let remaining = policy
        .max_total_backoff
        .checked_sub(spent)
        .filter(|left| !left.is_zero())?;
    let advice = error.retry_advice();
    let delay = match (advice.capacity, advice.retry_after_secs) {
        // The provider said when to come back, so come back then.
        (true, Some(secs)) => Duration::from_secs(secs).min(policy.capacity_max_delay),
        // At capacity with no hint: the slow schedule, since the window this
        // failure describes outlasts a blip-sized wait (issue #417).
        (true, None) => {
            exponential(policy.capacity_base_delay, attempt).min(policy.capacity_max_delay)
        }
        // An ordinary blip - a reset connection, a 500 - keeps the fast
        // schedule it has always had.
        (false, _) => exponential(policy.base_delay, attempt),
    };
    Some(delay.min(remaining))
}

/// The completed result of an [`InferenceJob`], applied on a later tick by the
/// inference-collect system.
pub struct InferenceOutcome {
    /// The agent the result belongs to.
    pub entity: Entity,
    /// The provider's response, or the error it failed with.
    pub result: Result<InferenceResponse, ProviderError>,
    /// Wall-clock time the job took, retries and backoff included. Measured
    /// here because the ECS only sees the outcome land on a later tick; this
    /// is the only place the call's real duration exists.
    pub latency: std::time::Duration,
}

/// Run one inference job to completion: perform the (possibly hour-long) network
/// call with the pool permit held, release the slot, report the outcome, and
/// wake the tick loop.
///
/// Meant to be `tokio::spawn`ed by the dispatch system. If the results receiver
/// has been dropped (the world is shutting down) the send is a harmless no-op.
pub async fn run_inference_job(
    job: InferenceJob,
    results: UnboundedSender<InferenceOutcome>,
    wake: Arc<Notify>,
    retry: RetryPolicy,
    cancel: crate::cancel::CancelToken,
) {
    let InferenceJob {
        entity,
        provider,
        request,
        permit,
        exact_token_counting,
    } = job;
    let started = std::time::Instant::now();
    // Opt-in accurate pre-flight budget guard: count the assembled request
    // exactly (remote endpoint where the provider has one, heuristic otherwise)
    // and refuse a request that would overflow the model's context window,
    // rather than sending it and letting the provider reject it after the fact.
    if exact_token_counting {
        let text = flatten_request_text(&request);
        let used = provider.count_tokens(&text, &request.model).await;
        let max = provider.max_context_tokens(&request.model);
        if used.saturating_add(request.max_tokens) > max {
            drop(permit);
            let _ = results.send(InferenceOutcome {
                entity,
                result: Err(ProviderError::TokenLimitExceeded { used, max }),
                latency: started.elapsed(),
            });
            wake.notify_one();
            return;
        }
    }
    // Retry transient failures (connection reset, timeout, 429, 5xx) with
    // exponential backoff, holding the permit across the backoff; a permanent
    // error fails immediately. `backoff_after` decides each wait: a capacity
    // refusal gets the slow schedule or the provider's own `Retry-After`, an
    // ordinary blip the fast one. The whole thing is bounded by
    // `max_total_backoff` on the sleeping and by `job_timeout` on the job, so a
    // never-completing (stalled-stream) call cannot hold the pool slot forever.
    //
    // `infer` borrows the request, so every attempt reuses the one assembled
    // copy. It used to be cloned per attempt, which doubled the live footprint
    // of every in-flight request for the whole (possibly minutes-long) call.
    let attempts = async {
        let mut attempt = 1u32;
        let mut spent = Duration::ZERO;
        loop {
            match provider.infer(&request).await {
                Ok(response) => break Ok(response),
                Err(e) => match backoff_after(&retry, &e, attempt, spent) {
                    Some(delay) => {
                        tokio::time::sleep(delay).await;
                        spent = spent.saturating_add(delay);
                        attempt += 1;
                    }
                    None => break Err(e),
                },
            }
        }
    };
    // A cancel drops the whole retry-and-backoff future - aborting the in-flight
    // HTTP request rather than waiting out the job timeout (up to 15 minutes) -
    // and reports nothing: the agent is already terminal, so there is no outcome
    // to apply. Releasing the permit here is the point; a cancelled run used to
    // hold its model's pool slot for as long as the provider took to answer.
    //
    // Note this arm sends no outcome and so never reaches the `wake` below: the
    // tick loop learns the slot is free from the permit's own `Drop` (see
    // `InferencePools::with_wake`). Without that, this return frees a slot in
    // silence and every agent queued on this model stays parked (issue #189).
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            drop(permit);
            return;
        }
        outcome = tokio::time::timeout(retry.job_timeout, attempts) => match outcome {
            Ok(result) => result,
            Err(_elapsed) => Err(leviath_providers::ProviderError::Other(format!(
                "inference exceeded the {}s job timeout and was aborted to free the \
                 pool slot (a stalled or never-completing response)",
                retry.job_timeout.as_secs()
            ))),
        },
    };
    drop(permit); // free the pool slot before the collect system runs
    let _ = results.send(InferenceOutcome {
        entity,
        result,
        latency: started.elapsed(),
    });
    wake.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_pool::{InferencePoolConfig, InferencePools};
    use tokio::sync::mpsc;

    fn test_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "m".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    fn response(text: &str) -> InferenceResponse {
        InferenceResponse {
            content: text.to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::Complete,
        }
    }

    /// A provider that returns a fixed success or error for `infer` (avoids
    /// cloning `ProviderError`, which isn't `Clone`).
    enum Fixed {
        Ok(InferenceResponse),
        Err(String),
    }

    #[async_trait::async_trait]
    impl Provider for Fixed {
        async fn infer(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            match self {
                Fixed::Ok(r) => Ok(r.clone()),
                Fixed::Err(m) => Err(ProviderError::Other(m.clone())),
            }
        }
        async fn count_tokens(&self, _text: &str, _model: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "fixed"
        }
        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn job(provider: Arc<dyn Provider>) -> InferenceJob {
        let pools = InferencePools::new(InferencePoolConfig::new());
        InferenceJob {
            entity: Entity::from_raw_u32(7)
                .expect("a small literal index is always a valid entity id"),
            provider,
            request: test_request(),
            permit: pools.try_acquire("p", "m").expect("free pool"),
            exact_token_counting: false,
        }
    }

    /// Cancelling releases the pool slot immediately instead of waiting out the
    /// job timeout (15 minutes by default), and reports no outcome - the agent is
    /// already terminal, so there is nothing to apply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_job_frees_its_pool_slot_without_reporting() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = InferencePools::new(cfg);
        let permit = pools.try_acquire("p", "m").expect("free pool");
        assert!(pools.try_acquire("p", "m").is_none(), "pool should be full");

        // A provider that never answers - the stalled-call case a cancel exists
        // to escape.
        let provider = Arc::new(Scripted {
            steps: std::sync::Mutex::new(vec![Step::Hang].into()),
            calls: std::sync::Mutex::new(0),
        });
        let job = InferenceJob {
            entity: Entity::from_raw_u32(7)
                .expect("a small literal index is always a valid entity id"),
            provider,
            request: test_request(),
            permit,
            exact_token_counting: false,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = crate::cancel::CancelToken::new();
        let running = tokio::spawn(run_inference_job(
            job,
            tx,
            Arc::new(Notify::new()),
            // A job timeout far longer than the test: only the cancel can end it.
            RetryPolicy {
                max_attempts: 1,
                job_timeout: Duration::from_secs(3600),
                ..instant()
            },
            cancel.clone(),
        ));
        tokio::task::yield_now().await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the cancel ended the job")
            .unwrap();
        assert!(
            pools.try_acquire("p", "m").is_some(),
            "the pool slot is free for the next agent"
        );
        assert!(
            rx.try_recv().is_err(),
            "and no outcome is reported for a cancelled run"
        );
    }

    #[tokio::test]
    async fn run_job_aborts_a_hung_call_and_frees_the_pool_slot() {
        // A model pool of one slot, taken by the (hung) job under test.
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = InferencePools::new(cfg);
        let permit = pools.try_acquire("p", "m").expect("free pool");
        assert!(pools.try_acquire("p", "m").is_none(), "pool should be full");

        let provider = Arc::new(Scripted {
            steps: std::sync::Mutex::new(vec![Step::Hang].into()),
            calls: std::sync::Mutex::new(0),
        });
        let job = InferenceJob {
            entity: Entity::from_raw_u32(7)
                .expect("a small literal index is always a valid entity id"),
            provider,
            request: test_request(),
            permit,
            exact_token_counting: false,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let policy = RetryPolicy {
            max_attempts: 1,
            job_timeout: Duration::from_millis(50),
            ..instant()
        };
        run_inference_job(
            job,
            tx,
            Arc::new(Notify::new()),
            policy,
            crate::cancel::CancelToken::new(),
        )
        .await;

        // The hung call was aborted with a timeout error…
        let outcome = rx.try_recv().expect("outcome sent");
        let err = outcome.result.expect_err("hung call should error");
        assert!(err.to_string().contains("job timeout"), "got: {err}");
        // …and its pool slot is free again for the next agent.
        assert!(
            pools.try_acquire("p", "m").is_some(),
            "the slot must be released after the timeout"
        );
    }

    #[tokio::test]
    async fn run_job_reports_ok_and_wakes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        run_inference_job(
            job(Arc::new(Fixed::Ok(response("hi")))),
            tx,
            wake.clone(),
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;

        let outcome = rx.try_recv().expect("outcome sent");
        assert_eq!(
            outcome.entity,
            Entity::from_raw_u32(7).expect("a small literal index is always a valid entity id")
        );
        assert_eq!(outcome.result.unwrap().content, "hi");
        // The wake was signalled (a subsequent notified() returns immediately).
        wake.notified().await;
    }

    #[tokio::test]
    async fn run_job_reports_provider_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let err = Arc::new(Fixed::Err("boom".to_string()));
        run_inference_job(
            job(err),
            tx,
            wake,
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;

        let outcome = rx.try_recv().expect("outcome sent");
        assert!(outcome.result.is_err());
    }

    /// A provider with a fixed `count_tokens` result and context window, used to
    /// drive the opt-in pre-inference budget guard. `infer` always succeeds.
    struct Counter {
        count: usize,
        max: usize,
    }

    #[async_trait::async_trait]
    impl Provider for Counter {
        async fn infer(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            Ok(response("ok"))
        }
        async fn count_tokens(&self, _text: &str, _model: &str) -> usize {
            self.count
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            self.max
        }
        fn name(&self) -> &str {
            "counter"
        }
        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn counting_job(provider: Arc<dyn Provider>, exact: bool) -> InferenceJob {
        let pools = InferencePools::new(InferencePoolConfig::new());
        InferenceJob {
            entity: Entity::from_raw_u32(7)
                .expect("a small literal index is always a valid entity id"),
            provider,
            request: test_request(), // max_tokens: 100
            permit: pools.try_acquire("p", "m").expect("free pool"),
            exact_token_counting: exact,
        }
    }

    #[test]
    fn flatten_request_text_includes_system_messages_and_tools() {
        use leviath_providers::{SystemBlock, Tool};
        let req = InferenceRequest {
            system: vec![SystemBlock {
                text: "sys".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![leviath_providers::Message {
                role: "user".to_string(),
                content: "hello".into(),
                cache_breakpoint: false,
            }],
            model: "m".to_string(),
            max_tokens: 10,
            temperature: 0.0,
            tools: vec![Tool {
                name: "search".to_string(),
                description: "find things".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let text = flatten_request_text(&req);
        assert!(text.contains("sys"));
        assert!(text.contains("hello"));
        assert!(text.contains("search"));
        assert!(text.contains("find things"));
        assert!(text.contains("object"));
    }

    #[tokio::test]
    async fn guard_rejects_request_over_context_window() {
        // count(950) + max_tokens(100) = 1050 > context(1000) ⇒ rejected pre-flight.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(Counter {
            count: 950,
            max: 1000,
        });
        run_inference_job(
            counting_job(provider, true),
            tx,
            Arc::new(Notify::new()),
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        let err = outcome.result.expect_err("should be rejected");
        // Assert on the Display string (branch-free) rather than `matches!`,
        // whose non-matching arm would be an uncovered region.
        assert_eq!(err.to_string(), "Token limit exceeded: 950 > 1000");
    }

    #[tokio::test]
    async fn guard_allows_request_within_context_window() {
        // count(800) + 100 = 900 ≤ 1000 ⇒ proceeds to infer.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(Counter {
            count: 800,
            max: 1000,
        });
        run_inference_job(
            counting_job(provider, true),
            tx,
            Arc::new(Notify::new()),
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert_eq!(outcome.result.expect("should succeed").content, "ok");
    }

    #[tokio::test]
    async fn counter_provider_metadata_is_exercised() {
        // Keep the Counter mock's non-`infer` trait methods measured.
        let p = Counter { count: 5, max: 10 };
        assert_eq!(p.name(), "counter");
        assert_eq!(p.max_context_tokens("m"), 10);
        assert_eq!(p.count_tokens("t", "m").await, 5);
        assert!(p.capabilities("m").supports_streaming);
    }

    #[tokio::test]
    async fn guard_off_skips_the_count_and_proceeds() {
        // Even wildly over budget, with the flag off the guard never runs.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(Counter {
            count: 1_000_000,
            max: 1000,
        });
        run_inference_job(
            counting_job(provider, false),
            tx,
            Arc::new(Notify::new()),
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert_eq!(outcome.result.expect("should succeed").content, "ok");
    }

    #[tokio::test]
    async fn fixed_provider_metadata_is_exercised() {
        // Covers the mock's non-`infer` trait methods (the pipeline resolves
        // these off the provider elsewhere; here we just keep them measured).
        let p = Fixed::Ok(response("x"));
        assert_eq!(p.name(), "fixed");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }

    #[tokio::test]
    async fn run_job_survives_dropped_receiver() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // world shutting down: nobody to receive
        let wake = Arc::new(Notify::new());
        // Must not panic even though the send fails.
        run_inference_job(
            job(Arc::new(Fixed::Ok(response("x")))),
            tx,
            wake,
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;
    }

    // ── retry behavior ──

    enum Step {
        Ok(String),
        Transient,
        /// The reported failure: a 529 the provider reports as a plain API
        /// error, which is a capacity refusal rather than a blip (issue #417).
        Overloaded,
        Permanent,
        /// Never returns - a stalled/hung call, for the job-timeout test.
        Hang,
    }

    /// A provider that plays a scripted sequence of results and counts calls.
    struct Scripted {
        steps: std::sync::Mutex<std::collections::VecDeque<Step>>,
        calls: std::sync::Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        async fn infer(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            *self.calls.lock().unwrap() += 1;
            // Pop before matching so the mutex guard is not held across the
            // `Hang` arm's `.await` (which would make this future non-`Send`).
            let step = self.steps.lock().unwrap().pop_front();
            match step {
                Some(Step::Ok(t)) => Ok(response(&t)),
                Some(Step::Transient) => Err(ProviderError::RateLimitExceeded {
                    retry_after_secs: None,
                }),
                Some(Step::Overloaded) => {
                    Err(ProviderError::ApiError("HTTP 529 Overloaded".to_string()))
                }
                Some(Step::Permanent) => Err(ProviderError::Other("permanent".to_string())),
                Some(Step::Hang) => std::future::pending().await,
                None => Err(ProviderError::Other("exhausted".to_string())),
            }
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "scripted"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// A policy whose every backoff is zero, so a job-level test drives the
    /// retry *loop* without sleeping. The schedule those durations would have
    /// been is asserted against `backoff_after` directly, where no sleeping is
    /// involved at all.
    fn instant() -> RetryPolicy {
        RetryPolicy {
            base_delay: Duration::ZERO,
            capacity_base_delay: Duration::ZERO,
            capacity_max_delay: Duration::ZERO,
            ..RetryPolicy::default()
        }
    }

    fn no_delay(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            job_timeout: Duration::from_secs(30),
            ..instant()
        }
    }

    #[tokio::test]
    async fn run_job_retries_transient_then_succeeds() {
        let provider = Arc::new(Scripted {
            steps: std::sync::Mutex::new(
                vec![
                    Step::Transient,
                    Step::Transient,
                    Step::Ok("done".to_string()),
                ]
                .into(),
            ),
            calls: std::sync::Mutex::new(0),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            job(provider.clone()),
            tx,
            Arc::new(Notify::new()),
            no_delay(4),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert_eq!(outcome.result.unwrap().content, "done");
        assert_eq!(*provider.calls.lock().unwrap(), 3); // two retries then success
    }

    #[tokio::test]
    async fn run_job_gives_up_after_max_attempts() {
        let provider = Arc::new(Scripted {
            steps: std::sync::Mutex::new(
                vec![
                    Step::Transient,
                    Step::Transient,
                    Step::Transient,
                    Step::Transient,
                ]
                .into(),
            ),
            calls: std::sync::Mutex::new(0),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            job(provider.clone()),
            tx,
            Arc::new(Notify::new()),
            no_delay(3),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert!(outcome.result.is_err());
        assert_eq!(*provider.calls.lock().unwrap(), 3); // exhausted the 3 attempts
    }

    #[tokio::test]
    async fn run_job_does_not_retry_a_permanent_error() {
        let provider = Arc::new(Scripted {
            steps: std::sync::Mutex::new(vec![Step::Permanent, Step::Ok("x".to_string())].into()),
            calls: std::sync::Mutex::new(0),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            job(provider.clone()),
            tx,
            Arc::new(Notify::new()),
            no_delay(4),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert!(outcome.result.is_err());
        assert_eq!(*provider.calls.lock().unwrap(), 1); // no retry on a permanent error
    }

    // ── the retry schedule (issue #417) ──
    //
    // Asserted against `backoff_after` rather than by running a job and timing
    // it: the schedule is minutes long, and a test that slept it would be a test
    // nobody runs. The job-level tests above drive the same loop with every
    // delay set to zero, which proves the loop and the schedule separately.

    fn blip() -> ProviderError {
        ProviderError::RequestFailed("connection reset by peer".to_string())
    }

    fn overloaded() -> ProviderError {
        ProviderError::ApiError("HTTP 529 Overloaded".to_string())
    }

    #[test]
    fn an_ordinary_blip_keeps_the_fast_schedule() {
        // The common case must not get slower: 1s, 2s, 4s, then give up on the
        // fourth attempt.
        let policy = RetryPolicy::default();
        let spent = Duration::ZERO;
        assert_eq!(
            backoff_after(&policy, &blip(), 1, spent),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            backoff_after(&policy, &blip(), 2, spent),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            backoff_after(&policy, &blip(), 3, spent),
            Some(Duration::from_secs(4))
        );
        assert_eq!(backoff_after(&policy, &blip(), 4, spent), None);
    }

    #[test]
    fn an_overload_waits_long_enough_to_leave_the_window() {
        // The reported bug: three retries of 1s, 2s and 4s all landed inside the
        // same 529 window, so a run with 44 iterations of finished work was
        // failed after seven seconds of trying. The capacity schedule is 15s,
        // 30s, 60s instead - 105 seconds of waiting on the same four attempts.
        let policy = RetryPolicy::default();
        let spent = Duration::ZERO;
        assert_eq!(
            backoff_after(&policy, &overloaded(), 1, spent),
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            backoff_after(&policy, &overloaded(), 2, spent),
            Some(Duration::from_secs(30))
        );
        // Capped, rather than the 60s the doubling would reach on its own; the
        // next attempt would ask for 120s and gets the same minute.
        assert_eq!(
            backoff_after(&policy, &overloaded(), 3, spent),
            Some(Duration::from_secs(60))
        );
        // A rate limit with no hint is the same kind of failure and waits the
        // same way.
        assert_eq!(
            backoff_after(
                &policy,
                &ProviderError::RateLimitExceeded {
                    retry_after_secs: None
                },
                1,
                spent
            ),
            Some(Duration::from_secs(15))
        );
    }

    #[test]
    fn the_servers_own_answer_wins_and_is_capped() {
        let policy = RetryPolicy::default();
        let hint = |secs| ProviderError::RateLimitExceeded {
            retry_after_secs: Some(secs),
        };
        // Told to come back in three seconds, come back in three - not the 15
        // the schedule would have picked. This is what keeps a provider that
        // answers precisely fast.
        assert_eq!(
            backoff_after(&policy, &hint(3), 1, Duration::ZERO),
            Some(Duration::from_secs(3))
        );
        // An hour is not honored: one header may not park a run indefinitely.
        assert_eq!(
            backoff_after(&policy, &hint(3600), 1, Duration::ZERO),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn a_permanent_error_is_never_retried() {
        assert_eq!(
            backoff_after(
                &RetryPolicy::default(),
                &ProviderError::TokenLimitExceeded { used: 9, max: 8 },
                1,
                Duration::ZERO
            ),
            None
        );
    }

    #[test]
    fn the_total_backoff_ceiling_bounds_however_long_a_provider_asks_for() {
        // The promise the config docs make: whatever the attempts, the schedule
        // and the provider's hints add up to, one request's retries sleep at
        // most `max_total_backoff`.
        let policy = RetryPolicy {
            max_attempts: 100,
            ..RetryPolicy::default()
        };
        // Most of the budget already slept: the next wait is trimmed to what is
        // left rather than the full minute it would have been.
        assert_eq!(
            backoff_after(
                &policy,
                &overloaded(),
                5,
                policy.max_total_backoff - Duration::from_secs(2)
            ),
            Some(Duration::from_secs(2))
        );
        // Budget spent: stop, with attempts still on the clock.
        assert_eq!(
            backoff_after(&policy, &overloaded(), 5, policy.max_total_backoff),
            None
        );
        assert_eq!(
            backoff_after(
                &policy,
                &overloaded(),
                5,
                policy.max_total_backoff + Duration::from_secs(1)
            ),
            None
        );
    }

    #[test]
    fn a_long_schedule_saturates_rather_than_overflowing() {
        // A large base and a high attempt count must not panic in a release
        // build's arithmetic or wrap in a debug one; the ceiling catches the
        // result either way.
        let policy = RetryPolicy {
            max_attempts: u32::MAX,
            base_delay: Duration::from_secs(u64::MAX / 2),
            ..RetryPolicy::default()
        };
        assert_eq!(
            backoff_after(&policy, &blip(), u32::MAX - 1, Duration::ZERO),
            Some(policy.max_total_backoff)
        );
    }

    #[tokio::test]
    async fn run_job_retries_an_overloaded_provider() {
        // End to end through the loop: a 529 is retried, not reported. The
        // policy's waits are zero here so the test costs nothing; how long they
        // would have been is asserted above.
        let provider = Arc::new(Scripted {
            steps: std::sync::Mutex::new(
                vec![
                    Step::Overloaded,
                    Step::Overloaded,
                    Step::Ok("survived the overload".to_string()),
                ]
                .into(),
            ),
            calls: std::sync::Mutex::new(0),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            job(provider.clone()),
            tx,
            Arc::new(Notify::new()),
            no_delay(4),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert_eq!(outcome.result.unwrap().content, "survived the overload");
        assert_eq!(*provider.calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn scripted_provider_metadata_is_exercised() {
        let p = Scripted {
            steps: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::Mutex::new(0),
        };
        assert_eq!(p.name(), "scripted");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }
}
