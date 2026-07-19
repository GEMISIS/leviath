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

use bevy_ecs::entity::Entity;
use leviath_providers::{InferenceRequest, InferenceResponse, Provider, ProviderError};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::inference_pool::InferencePermit;

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
) {
    let InferenceJob {
        entity,
        provider,
        request,
        permit,
    } = job;
    let result = provider.infer(request).await;
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
    async fn run_job_reports_ok_and_wakes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        run_inference_job(job(Arc::new(Fixed::Ok(response("hi")))), tx, wake.clone()).await;

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
        run_inference_job(job(err), tx, wake).await;

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
        run_inference_job(job(Arc::new(Fixed::Ok(response("x")))), tx, wake).await;
    }
}
