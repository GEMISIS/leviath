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

/// How a transient inference failure is retried before the agent is failed.
///
/// Transient errors (see [`ProviderError::is_transient`]) are retried with
/// exponential backoff; a permanent error (auth, invalid request, token limit)
/// fails immediately. This keeps a passing network blip from marking a stage
/// `error` and carrying its half-finished work forward.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first (e.g. `4` = one try + three retries).
    pub max_attempts: u32,
    /// Base backoff; the retry after attempt `n` waits `base_delay * 2^(n-1)`
    /// (so 1s, 2s, 4s, … for a 1s base).
    pub base_delay: Duration,
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
            max_attempts: 4,
            base_delay: Duration::from_secs(1),
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

/// The completed result of an [`InferenceJob`], applied on a later tick by the
/// inference-collect system.
pub struct InferenceOutcome {
    /// The agent the result belongs to.
    pub entity: Entity,
    /// The provider's response, or the error it failed with.
    pub result: Result<InferenceResponse, ProviderError>,
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
            });
            wake.notify_one();
            return;
        }
    }
    // Retry transient failures (connection reset, timeout, 429, 5xx) with
    // exponential backoff, holding the permit across the backoff; a permanent
    // error fails immediately. The whole thing is bounded by `job_timeout` so a
    // never-completing (stalled-stream) call cannot hold the pool slot forever.
    let attempts = async {
        let mut attempt = 1u32;
        loop {
            match provider.infer(request.clone()).await {
                Ok(response) => break Ok(response),
                Err(e) if e.is_transient() && attempt < retry.max_attempts => {
                    tokio::time::sleep(retry.base_delay * 2u32.pow(attempt - 1)).await;
                    attempt += 1;
                }
                Err(e) => break Err(e),
            }
        }
    };
    // A cancel drops the whole retry-and-backoff future - aborting the in-flight
    // HTTP request rather than waiting out the job timeout (up to 15 minutes) -
    // and reports nothing: the agent is already terminal, so there is no outcome
    // to apply. Releasing the permit here is the point; a cancelled run used to
    // hold its model's pool slot for as long as the provider took to answer.
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
    let _ = results.send(InferenceOutcome { entity, result });
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
            _req: InferenceRequest,
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
            permit: pools.try_acquire("m").expect("free pool"),
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
        let permit = pools.try_acquire("m").expect("free pool");
        assert!(pools.try_acquire("m").is_none(), "pool should be full");

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
                base_delay: Duration::ZERO,
                job_timeout: Duration::from_secs(3600),
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
            pools.try_acquire("m").is_some(),
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
        let permit = pools.try_acquire("m").expect("free pool");
        assert!(pools.try_acquire("m").is_none(), "pool should be full");

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
            base_delay: Duration::ZERO,
            job_timeout: Duration::from_millis(50),
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
            pools.try_acquire("m").is_some(),
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
            _req: InferenceRequest,
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
            permit: pools.try_acquire("m").expect("free pool"),
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
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            *self.calls.lock().unwrap() += 1;
            // Pop before matching so the mutex guard is not held across the
            // `Hang` arm's `.await` (which would make this future non-`Send`).
            let step = self.steps.lock().unwrap().pop_front();
            match step {
                Some(Step::Ok(t)) => Ok(response(&t)),
                Some(Step::Transient) => Err(ProviderError::RateLimitExceeded),
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

    fn no_delay(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::ZERO,
            job_timeout: Duration::from_secs(30),
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
