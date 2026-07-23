//! The async worker side of the ECS inference stage — the sync-ECS ↔ async-I/O
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
//! one per agent — that is what keeps CPU bounded by work, not by agent count.

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
    /// The provider's HTTP client already has a read-*stall* timeout, but a
    /// connection that keeps trickling keepalive bytes without ever completing
    /// (a stalled stream) resets that timer forever, so the permit would leak
    /// and, once enough leak, the model's pool fills and new agents never get a
    /// slot. This bound guarantees a slot is released within a fixed time. The
    /// default is generous — far above any realistic single inference — so it
    /// only ever catches a genuine hang.
    pub job_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_secs(1),
            job_timeout: Duration::from_secs(1800),
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
) {
    let InferenceJob {
        entity,
        provider,
        request,
        permit,
    } = job;
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
    let result = match tokio::time::timeout(retry.job_timeout, attempts).await {
        Ok(result) => result,
        Err(_elapsed) => Err(leviath_providers::ProviderError::Other(format!(
            "inference exceeded the {}s job timeout and was aborted to free the \
             pool slot (a stalled or never-completing response)",
            retry.job_timeout.as_secs()
        ))),
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
        fn count_tokens(&self, _text: &str, _model: &str) -> usize {
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
            entity: Entity::from_raw(7),
            provider,
            request: test_request(),
            permit: pools.try_acquire("m").expect("free pool"),
        }
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
            entity: Entity::from_raw(7),
            provider,
            request: test_request(),
            permit,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let policy = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            job_timeout: Duration::from_millis(50),
        };
        run_inference_job(job, tx, Arc::new(Notify::new()), policy).await;

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
        )
        .await;

        let outcome = rx.try_recv().expect("outcome sent");
        assert_eq!(outcome.entity, Entity::from_raw(7));
        assert_eq!(outcome.result.unwrap().content, "hi");
        // The wake was signalled (a subsequent notified() returns immediately).
        wake.notified().await;
    }

    #[tokio::test]
    async fn run_job_reports_provider_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let err = Arc::new(Fixed::Err("boom".to_string()));
        run_inference_job(job(err), tx, wake, RetryPolicy::default()).await;

        let outcome = rx.try_recv().expect("outcome sent");
        assert!(outcome.result.is_err());
    }

    #[test]
    fn fixed_provider_metadata_is_exercised() {
        // Covers the mock's non-`infer` trait methods (the pipeline resolves
        // these off the provider elsewhere; here we just keep them measured).
        let p = Fixed::Ok(response("x"));
        assert_eq!(p.name(), "fixed");
        assert_eq!(p.count_tokens("t", "m"), 1);
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
        )
        .await;
    }

    // ── retry behavior ──

    enum Step {
        Ok(String),
        Transient,
        Permanent,
        /// Never returns — a stalled/hung call, for the job-timeout test.
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
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
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
        )
        .await;
        let outcome = rx.try_recv().expect("outcome sent");
        assert!(outcome.result.is_err());
        assert_eq!(*provider.calls.lock().unwrap(), 1); // no retry on a permanent error
    }

    #[test]
    fn scripted_provider_metadata_is_exercised() {
        let p = Scripted {
            steps: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::Mutex::new(0),
        };
        assert_eq!(p.name(), "scripted");
        assert_eq!(p.count_tokens("t", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }
}
