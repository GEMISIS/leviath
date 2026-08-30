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
/// window lasts minutes, so a second of waiting only buys another refusal: a
/// run can otherwise spend all three of its retries inside one 529 window and
/// be failed with dozens of iterations of finished work in hand.
pub const CAPACITY_BASE_DELAY_SECS: u64 = 15;

/// The longest one capacity backoff may last, in seconds, and the ceiling on a
/// `Retry-After` the provider asks for.
///
/// A minute is long enough to leave most overload windows and short enough that
/// a run still notices when the provider comes back. A server asking for longer
/// than this is waited out a minute at a time instead, which costs an extra
/// refusal but keeps one header from parking a run for an hour.
pub const CAPACITY_MAX_DELAY_SECS: u64 = 60;

/// The first backoff, in seconds, after a failure that *reached* the provider -
/// a timeout, or an answer that stopped part-way.
///
/// Bigger than the ordinary blip delay because the two describe different
/// events. A reset connection or a 500 is usually gone by the next attempt, so a
/// second of waiting is the right price. A connection that was established and
/// then went quiet is the network changing underneath the run - a wifi handover,
/// a VPN reconnecting, a laptop coming back from sleep - and those take tens of
/// seconds, not one.
///
/// The default four attempts spent 1s, 2s and 4s: seven seconds of tolerance,
/// after which the run is parked and needs a person to type `lev resume`. Almost
/// no real network interruption is over in seven seconds, so the run was
/// reliably parked for a condition that would have cleared on its own. At five
/// seconds the same four attempts cover about thirty-five, which most of them
/// do outlast.
///
/// Not a separate config key. `inference_retry_attempts` already sets how long a
/// run rides out a bad patch, and [`MAX_TOTAL_BACKOFF_SECS`] still caps the lot,
/// so no run waits longer than it could before.
pub const REACHED_BASE_DELAY_SECS: u64 = 5;

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
    /// Base backoff for a failure that reached the provider and then stopped -
    /// a timeout, or an answer that died part-way - doubling per attempt as
    /// [`Self::base_delay`] does. Defaults to [`REACHED_BASE_DELAY_SECS`].
    pub reached_base_delay: Duration,
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
            reached_base_delay: Duration::from_secs(REACHED_BASE_DELAY_SECS),
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
pub(crate) struct InferenceJob {
    /// The agent this inference is for.
    pub entity: Entity,
    /// The provider to call (already resolved for the agent's model).
    pub provider: Arc<dyn Provider>,
    /// The assembled request.
    pub request: InferenceRequest,
    /// The per-model pool permit, held for the whole request and released when
    /// the job finishes.
    pub permit: InferencePermit,
    /// What earlier calls taught the window about its own estimate, so the
    /// pre-flight guard ([`guard_context_window`]) decides whether to measure
    /// this request from the corrected figure rather than the raw one. `None`
    /// before anything was measured, or for a lane that has no window of its
    /// own to correct.
    pub calibration: Option<crate::pipeline::PromptCalibration>,
    /// Ask the provider to stream this answer and fold the chunks back into one
    /// response, rather than waiting for the whole thing at once.
    ///
    /// The agent sees no difference - it gets a finished turn either way, which
    /// is all it can act on. What differs is the socket. A buffered call sends
    /// nothing back until the model has finished thinking, so a long turn is a
    /// connection that has been silent for minutes, and anything on the path
    /// that reaps idle connections - a NAT, a VPN, a corporate proxy - takes it
    /// as dead and closes it. The run then fails on a request that was going
    /// perfectly well. A streamed call has bytes moving the whole time.
    ///
    /// Set from `[limits] stream_inference` and off for a model whose provider
    /// does not advertise streaming.
    pub stream: bool,
}

/// The share of the model's window below which a request goes out unmeasured.
///
/// The guard costs a provider round trip on Anthropic and Gemini, so it is only
/// paid where it can change the answer. A request whose corrected estimate plus
/// its reply budget sits under half the window cannot overflow it however far
/// the estimate is off - the estimator has never been measured drifting by
/// anything like a factor of two - and is sent as it is. Everything above the
/// line is counted with the provider's own tokenizer before it is sent.
pub const COUNT_ABOVE_WINDOW_FRACTION: usize = 2;

/// Measure a request against the model's context window before it is sent.
///
/// Returns `Ok(None)` when the request was small enough to skip the count,
/// `Ok(Some(used))` with the provider's exact prompt count when it was measured
/// and fits, and `Err(TokenLimitExceeded)` when it was measured and would
/// overflow. Every inference lane - the stage's own call, the routing call,
/// compaction and titling - goes through this one function, so a window is
/// guarded the same way whichever lane assembled the request.
///
/// A provider that reports no window for the model (`max_context_tokens` of
/// zero) cannot be guarded, and is not: refusing everything on the strength of
/// a number nobody supplied would be worse than sending it.
///
/// The count is the provider's (`count_tokens`): a remote endpoint on
/// Anthropic and Gemini, tiktoken on OpenAI, a script's own `count_tokens` on
/// a Rhai provider, and the byte heuristic elsewhere - so on a heuristic-only
/// provider the "exact" figure is the same estimate, and the guard reduces to
/// the overflow check.
pub async fn guard_context_window(
    provider: &dyn Provider,
    request: &InferenceRequest,
    calibration: Option<&crate::pipeline::PromptCalibration>,
) -> Result<Option<usize>, ProviderError> {
    let max = provider.max_context_tokens(&request.model);
    if max == 0 {
        return Ok(None);
    }
    let text = flatten_request_text(request);
    let estimate =
        crate::pipeline::calibrated_tokens(leviath_core::estimate_tokens(&text), calibration);
    if estimate.saturating_add(request.max_tokens) < max / COUNT_ABOVE_WINDOW_FRACTION {
        return Ok(None);
    }
    let used = provider.count_tokens(&text, &request.model).await;
    if used.saturating_add(request.max_tokens) > max {
        return Err(ProviderError::TokenLimitExceeded { used, max });
    }
    tracing::debug!(
        model = %request.model,
        estimated = estimate,
        counted = used,
        window = max,
        "request measured before sending"
    );
    Ok(Some(used))
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
pub(crate) fn backoff_after(
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
        // failure describes outlasts a blip-sized wait.
        (true, None) => {
            exponential(policy.capacity_base_delay, attempt).min(policy.capacity_max_delay)
        }
        // Not at capacity. Which schedule depends on whether the provider was
        // ever reached: a name that did not resolve or a port that refused is
        // answered instantly and identically, so waiting longer buys nothing,
        // while a call that connected and then went quiet is the network moving
        // underneath the run and needs tens of seconds, not one.
        (false, _) => match error
            .failure_kind()
            .is_some_and(|k| k.provider_was_reached())
        {
            true => exponential(policy.reached_base_delay, attempt),
            // An ordinary blip - a reset connection, a 500 - keeps the fast
            // schedule it has always had.
            false => exponential(policy.base_delay, attempt),
        },
    };
    Some(delay.min(remaining))
}

/// The completed result of an [`InferenceJob`], applied on a later tick by the
/// inference-collect system.
pub(crate) struct InferenceOutcome {
    /// The agent the result belongs to.
    pub entity: Entity,
    /// The provider's response, or the error it failed with.
    pub result: Result<InferenceResponse, ProviderError>,
    /// Wall-clock time the job took, retries and backoff included. Measured
    /// here because the ECS only sees the outcome land on a later tick; this
    /// is the only place the call's real duration exists.
    pub latency: std::time::Duration,
    /// What the provider charges for the model this job called.
    ///
    /// Resolved here for the same reason `latency` is: this is the last place
    /// the `Arc<dyn Provider>` exists. By the time the outcome reaches the ECS
    /// only the provider's *name* survives, and a name cannot be asked its
    /// rates. Used only when the response carried no cost of its own.
    pub pricing: Option<leviath_providers::ModelPricing>,
}

/// Run one inference job to completion: perform the (possibly hour-long) network
/// call with the pool permit held, release the slot, report the outcome, and
/// wake the tick loop.
///
/// Meant to be `tokio::spawn`ed by the dispatch system. If the results receiver
/// has been dropped (the world is shutting down) the send is a harmless no-op.
pub(crate) async fn run_inference_job(
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
        calibration,
        stream,
    } = job;
    let started = std::time::Instant::now();
    // Retry transient failures (connection reset, timeout, 429, 5xx) with
    // exponential backoff, holding the permit across the backoff; a permanent
    // error fails immediately. `backoff_after` decides each wait: a capacity
    // refusal gets the slow schedule or the provider's own `Retry-After`, an
    // ordinary blip the fast one. The whole thing is bounded by
    // `max_total_backoff` on the sleeping and by `job_timeout` on the job, so a
    // never-completing (stalled-stream) call cannot hold the pool slot forever.
    //
    // `infer` borrows the request, so every attempt reuses the one assembled
    // copy. Cloning it per attempt doubles the live footprint of every
    // in-flight request for the whole (possibly minutes-long) call.
    let attempts = async {
        // The pre-flight guard, inside the cancel and the job timeout with the
        // call it protects: a count that hangs is bounded by the same deadline
        // the request is, and a cancelled run does not wait for one. Before the
        // loop rather than in it, because a refusal here is a fact about the
        // request and a retry would only restate it.
        guard_context_window(provider.as_ref(), &request, calibration.as_ref()).await?;
        let mut attempt = 1u32;
        let mut spent = Duration::ZERO;
        loop {
            // Both arms produce the same finished `InferenceResponse`; the
            // difference is entirely in how the bytes crossed the wire. A
            // stream that dies part-way through reports a dropped connection,
            // which is transient, so it retries here exactly as a failed send
            // does rather than costing the run its turn.
            let call = async {
                match stream {
                    true => {
                        let chunks = provider.infer_stream(&request).await?;
                        leviath_providers::collect_stream(chunks).await
                    }
                    false => provider.infer(&request).await,
                }
            };
            match call.await {
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
    // silence and every agent queued on this model stays parked.
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            drop(permit);
            return;
        }
        outcome = tokio::time::timeout(retry.job_timeout, attempts) => match outcome {
            Ok(result) => result,
            // A timeout, said the same way the provider's own would have said
            // it. `ProviderError::Other` is neither transient nor
            // `Unreachable`, so labelling it that way sends the two ways a call
            // can run out of time to opposite places: a provider-side timeout
            // fails over and then parks the run for a resume, while sitting out
            // the whole job deadline - the *worse* of the two - kills it
            // outright and throws away every stage it finished. The wall is the
            // same wall; only which timer noticed differs.
            Err(_elapsed) => Err(leviath_providers::ProviderError::labelled(
                leviath_providers::FailureKind::Timeout,
                "waiting for the provider",
                &format!(
                    "the call was aborted after the {}s job timeout to free the pool slot \
                     (a stalled or never-completing response)",
                    retry.job_timeout.as_secs()
                ),
            )),
        },
    };
    drop(permit); // free the pool slot before the collect system runs
    let _ = results.send(InferenceOutcome {
        entity,
        result,
        latency: started.elapsed(),
        pricing: provider.pricing(&request.model),
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
                reported_cost_usd: None,
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
            calibration: None,
            stream: false,
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
            calibration: None,
            stream: false,
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
            calibration: None,
            stream: false,
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
        // …said the way the provider's own timeout would have said it. This was
        // a `ProviderError::Other`, which is neither transient nor
        // `Unreachable`, so the two ways a call can run out of time ended in
        // opposite places: a provider-side timeout failed over and then parked
        // the run for a resume, while sitting out the whole job deadline - the
        // worse of the two - killed the run and threw away every finished
        // stage.
        assert_eq!(
            err.failure_kind(),
            Some(leviath_providers::FailureKind::Timeout)
        );
        assert_eq!(
            err.unavailable_reason(),
            Some(leviath_providers::UnavailableReason::Unreachable),
            "so it fails over, and parks the run when there is nowhere left to go"
        );
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
    /// drive the pre-flight window guard. `infer` always succeeds, and every
    /// count call is tallied so a test can say whether the guard paid for one.
    struct Counter {
        count: usize,
        max: usize,
        counts: std::sync::atomic::AtomicUsize,
    }

    impl Counter {
        fn new(count: usize, max: usize) -> Self {
            Self {
                count,
                max,
                counts: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn count_calls(&self) -> usize {
            self.counts.load(std::sync::atomic::Ordering::SeqCst)
        }
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
            self.counts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    /// A job whose request carries `prompt_bytes` of user text, so the guard's
    /// own estimate (`bytes / 4`) is under the test's control.
    fn counting_job(
        provider: Arc<dyn Provider>,
        prompt_bytes: usize,
        calibration: Option<crate::pipeline::PromptCalibration>,
    ) -> InferenceJob {
        let pools = InferencePools::new(InferencePoolConfig::new());
        InferenceJob {
            entity: Entity::from_raw_u32(7)
                .expect("a small literal index is always a valid entity id"),
            provider,
            request: sized_request(prompt_bytes), // max_tokens: 100
            permit: pools.try_acquire("p", "m").expect("free pool"),
            calibration,
            stream: false,
        }
    }

    /// [`test_request`] with one user message of `bytes` ASCII bytes.
    fn sized_request(bytes: usize) -> InferenceRequest {
        let mut request = test_request();
        request.messages.push(leviath_providers::Message {
            role: "user".to_string(),
            content: "x".repeat(bytes).into(),
            cache_breakpoint: false,
        });
        request
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

    /// The guard is always on now, with one cheap escape: a request whose
    /// estimate plus its reply budget is under half the window is sent without
    /// asking the provider, and one at or above the line is measured first.
    /// Both against a window of 1,000 and a reply budget of 100: 400 bytes
    /// estimate at 100 tokens (200 all told, under the line), 1,600 bytes at
    /// 400 (500, on it).
    #[tokio::test]
    async fn a_large_prompt_is_counted_and_a_small_one_is_not() {
        let small = Arc::new(Counter::new(10, 1000));
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            counting_job(small.clone(), 400, None),
            tx,
            Arc::new(Notify::new()),
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert_eq!(outcome.result.expect("sent unmeasured").content, "ok");
        assert_eq!(small.count_calls(), 0, "a small turn pays nothing");

        let large = Arc::new(Counter::new(800, 1000));
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            counting_job(large.clone(), 1_600, None),
            tx,
            Arc::new(Notify::new()),
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        // count(800) + max_tokens(100) = 900 <= 1000: measured, and it fits.
        assert_eq!(outcome.result.expect("measured and sent").content, "ok");
        assert_eq!(large.count_calls(), 1, "one count call, no more");
    }

    /// The guard's estimate is the calibrated one. A request the raw estimate
    /// would wave through (200 of a 1,000 window) is measured once earlier
    /// calls have shown the provider charging 400 more than the window
    /// believes - which is exactly the run that is about to overflow.
    #[tokio::test]
    async fn the_calibration_can_push_a_request_over_the_counting_line() {
        let mut calibration = crate::pipeline::PromptCalibration::default();
        calibration.observe(1_000, 1_400);
        let provider = Arc::new(Counter::new(10, 1000));
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            counting_job(provider.clone(), 400, Some(calibration)),
            tx,
            Arc::new(Notify::new()),
            RetryPolicy::default(),
            crate::cancel::CancelToken::new(),
        )
        .await;
        assert!(rx.try_recv().expect("outcome sent").result.is_ok());
        assert_eq!(provider.count_calls(), 1);
    }

    #[tokio::test]
    async fn guard_rejects_request_over_context_window() {
        // count(950) + max_tokens(100) = 1050 > context(1000) ⇒ rejected pre-flight.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let provider = Arc::new(Counter::new(950, 1000));
        run_inference_job(
            counting_job(provider, 1_600, None),
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

    /// A refusal is a fact about the request, not the network: it is reported
    /// once, and the retry schedule never sees it.
    #[tokio::test]
    async fn a_refused_request_is_not_retried() {
        let provider = Arc::new(Counter::new(950, 1000));
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            counting_job(provider.clone(), 1_600, None),
            tx,
            Arc::new(Notify::new()),
            no_delay(5),
            crate::cancel::CancelToken::new(),
        )
        .await;
        assert!(rx.try_recv().expect("outcome sent").result.is_err());
        assert_eq!(provider.count_calls(), 1, "measured once, refused once");
    }

    /// A provider that reports no window for the model cannot be guarded and
    /// is not: the request goes out unmeasured rather than being refused on a
    /// number nobody supplied.
    #[tokio::test]
    async fn an_unknown_window_is_never_guarded() {
        let provider = Arc::new(Counter::new(1_000_000, 0));
        let verdict = guard_context_window(provider.as_ref(), &sized_request(1_600), None)
            .await
            .expect("nothing to measure against");
        assert_eq!(verdict, None);
        assert_eq!(provider.count_calls(), 0);
    }

    /// What the guard hands back when it did measure: the provider's count, so
    /// a caller can feed it into the calibration.
    #[tokio::test]
    async fn a_measured_request_reports_its_count() {
        let provider = Arc::new(Counter::new(800, 1000));
        let verdict = guard_context_window(provider.as_ref(), &sized_request(1_600), None)
            .await
            .expect("fits");
        assert_eq!(verdict, Some(800));
    }

    #[tokio::test]
    async fn counter_provider_metadata_is_exercised() {
        // Keep the Counter mock's non-`infer` trait methods measured.
        let p = Counter::new(5, 10);
        assert_eq!(p.name(), "counter");
        assert_eq!(p.max_context_tokens("m"), 10);
        assert_eq!(p.count_tokens("t", "m").await, 5);
        assert!(p.capabilities("m").supports_streaming);
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
        /// error, which is a capacity refusal rather than a blip.
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
        /// Deliberately independent of `steps`: a test scripts `infer` to fail
        /// and then asks for a stream, so a success here can only mean the
        /// streaming door was used. A recorded flag could not prove that - the
        /// trait's default `infer_stream` is built on `infer`, so "streamed"
        /// and "buffered then wrapped" would look identical.
        async fn infer_stream(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<
            std::pin::Pin<
                Box<
                    dyn futures_core::Stream<
                            Item = leviath_providers::Result<
                                leviath_providers::provider::StreamChunk,
                            >,
                        > + Send,
                >,
            >,
        > {
            Ok(Box::pin(tokio_stream::iter(vec![
                Ok(leviath_providers::provider::StreamChunk {
                    delta: "streamed".to_string(),
                    tool_calls: Vec::new(),
                    tokens: None,
                    finish_reason: None,
                }),
                Ok(leviath_providers::provider::StreamChunk {
                    delta: String::new(),
                    tool_calls: Vec::new(),
                    tokens: Some(leviath_providers::TokenUsage::new(7, 0, 0, 3)),
                    finish_reason: Some(leviath_providers::FinishReason::Complete),
                }),
            ])))
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

    /// A job marked to stream is streamed, and the caller gets back the same
    /// finished turn it would have got from a buffered call.
    ///
    /// The flag was the whole point of the change: `infer_stream` had been
    /// implemented on every provider and called by nothing in production, so
    /// every real inference held a socket that went silent for as long as the
    /// model took to think.
    #[tokio::test]
    async fn a_job_marked_to_stream_takes_the_streaming_path() {
        let pools = InferencePools::new(InferencePoolConfig::new());
        let job = InferenceJob {
            entity: Entity::from_raw_u32(7)
                .expect("a small literal index is always a valid entity id"),
            // `infer` is scripted to fail outright, so an `Ok` below can only
            // have come through `infer_stream`.
            provider: Arc::new(Scripted {
                steps: std::sync::Mutex::new(vec![Step::Permanent].into()),
                calls: std::sync::Mutex::new(0),
            }),
            request: test_request(),
            permit: pools.try_acquire("p", "m").expect("free pool"),
            calibration: None,
            stream: true,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            job,
            tx,
            Arc::new(Notify::new()),
            no_delay(1),
            crate::cancel::CancelToken::new(),
        )
        .await;

        let outcome = rx.try_recv().expect("outcome sent");
        let response = outcome.result.expect("the streamed call succeeded");
        assert_eq!(response.content, "streamed");
        // Folded, not merely forwarded: the usage chunk arrives separately from
        // the text and has to survive the trip.
        assert_eq!(response.tokens_used.completion_tokens, 3);
    }

    /// And a job that is not marked to stream still buffers, so turning the
    /// switch off is a real escape hatch rather than a no-op.
    #[tokio::test]
    async fn a_job_not_marked_to_stream_calls_infer() {
        let pools = InferencePools::new(InferencePoolConfig::new());
        let job = InferenceJob {
            entity: Entity::from_raw_u32(7)
                .expect("a small literal index is always a valid entity id"),
            provider: Arc::new(Scripted {
                steps: std::sync::Mutex::new(vec![Step::Permanent].into()),
                calls: std::sync::Mutex::new(0),
            }),
            request: test_request(),
            permit: pools.try_acquire("p", "m").expect("free pool"),
            calibration: None,
            stream: false,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_inference_job(
            job,
            tx,
            Arc::new(Notify::new()),
            no_delay(1),
            crate::cancel::CancelToken::new(),
        )
        .await;

        let outcome = rx.try_recv().expect("outcome sent");
        let err = outcome.result.expect_err("`infer` was scripted to fail");
        assert!(err.to_string().contains("permanent"));
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
            reached_base_delay: Duration::ZERO,
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

    /// A transient failure carrying no kind - a Rhai provider's own error, or
    /// anything `reqwest` could not place. Unclassified takes the fast
    /// schedule, which is the conservative side: it is the behaviour every
    /// transient failure had before kinds existed.
    fn blip() -> ProviderError {
        ProviderError::RequestFailed("connection reset by peer".to_string())
    }

    /// A failure that reached the provider and then stopped.
    fn reached(kind: leviath_providers::FailureKind) -> ProviderError {
        ProviderError::labelled(kind, "sending the request", "no answer came")
    }

    fn overloaded() -> ProviderError {
        ProviderError::ApiError("HTTP 529 Overloaded".to_string())
    }

    /// A call that connected and then went quiet waits long enough for the
    /// network to come back.
    ///
    /// The fast schedule gives four attempts seven seconds, and then the run is
    /// parked until a person types `lev resume`. Almost nothing that interrupts
    /// a network is over in seven seconds - a wifi handover, a VPN reconnecting,
    /// a laptop waking - so a run was reliably parked for a condition that would
    /// have cleared on its own. Five seconds doubling covers about thirty-five.
    ///
    /// A provider that was never reached keeps the fast schedule: a name that
    /// does not resolve or a port that refuses answers instantly and identically
    /// however long you wait, so waiting buys nothing.
    #[test]
    fn a_provider_that_went_quiet_is_waited_out_longer_than_one_that_was_never_there() {
        let policy = RetryPolicy::default();
        let spent = Duration::ZERO;

        for kind in [
            leviath_providers::FailureKind::Timeout,
            leviath_providers::FailureKind::ConnectionDropped,
        ] {
            assert_eq!(
                backoff_after(&policy, &reached(kind), 1, spent),
                Some(Duration::from_secs(5))
            );
            assert_eq!(
                backoff_after(&policy, &reached(kind), 3, spent),
                Some(Duration::from_secs(20))
            );
        }

        for kind in [
            leviath_providers::FailureKind::DnsFailure,
            leviath_providers::FailureKind::ConnectionRefused,
            leviath_providers::FailureKind::TlsFailure,
        ] {
            assert_eq!(
                backoff_after(&policy, &reached(kind), 1, spent),
                Some(Duration::from_secs(1)),
                "nothing was there to answer, so waiting changes nothing"
            );
        }

        // And the ceiling still governs: whatever the schedule, a job stops
        // once its whole backoff budget is spent.
        assert_eq!(
            backoff_after(
                &policy,
                &reached(leviath_providers::FailureKind::Timeout),
                1,
                policy.max_total_backoff
            ),
            None
        );
    }

    #[test]
    fn an_ordinary_blip_keeps_the_fast_schedule() {
        // An unclassified transient must not get slower: 1s, 2s, 4s, then give
        // up on the fourth attempt.
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
