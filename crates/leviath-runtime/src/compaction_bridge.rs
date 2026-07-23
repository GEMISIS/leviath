//! The async worker side of the ECS compaction stage — the sync-ECS ↔ async-I/O
//! bridge for LLM context compaction.
//!
//! When an agent's context crosses the eviction threshold and a region needs
//! LLM summarization, the compaction-dispatch system does the synchronous
//! eviction inline and hands the summarization off as a [`CompactionJob`] (one
//! summarize request per region, plus the pool permit it acquired).
//! [`run_compaction_job`] runs the requests sequentially, reports the summaries
//! (or the first error) as a [`CompactionOutcome`], and wakes the tick loop; the
//! collect system stores each summary into its paired history region on a later
//! tick.
//!
//! Compaction is **best-effort**: a provider error just means the context isn't
//! compacted this round (the agent proceeds), so the outcome carries a `Result`
//! the collect system logs-and-continues on rather than failing the agent.

use std::sync::Arc;

use bevy_ecs::entity::Entity;
use leviath_providers::{InferenceRequest, Provider, ProviderError};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::inference_pool::InferencePermit;

/// A batch of per-region summarize requests for one agent.
pub struct CompactionJob {
    /// The agent whose context is being compacted.
    pub entity: Entity,
    /// The compaction provider (resolved for the compaction model).
    pub provider: Arc<dyn Provider>,
    /// One `(region_name, request)` per region to summarize.
    pub requests: Vec<(String, InferenceRequest)>,
    /// The per-model pool permit, held for the whole batch.
    pub permit: InferencePermit,
}

/// The completed result of a [`CompactionJob`]: the `(region_name, summary)`
/// pairs, or the first provider error encountered.
pub struct CompactionOutcome {
    /// The agent the summaries belong to.
    pub entity: Entity,
    /// Per-region summaries, or the error compaction failed with.
    pub result: Result<Vec<(String, String)>, ProviderError>,
}

/// Run one compaction job: summarize each region sequentially with the permit
/// held, release the slot, report the outcome, and wake the tick loop. Stops at
/// the first error (best-effort — the collect system leaves the context as-is).
pub async fn run_compaction_job(
    job: CompactionJob,
    results: UnboundedSender<CompactionOutcome>,
    wake: Arc<Notify>,
) {
    let CompactionJob {
        entity,
        provider,
        requests,
        permit,
    } = job;

    let mut summaries = Vec::new();
    let mut result = Ok(());
    for (region, request) in requests {
        match provider.infer(request).await {
            Ok(response) => summaries.push((region, response.content)),
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    drop(permit); // free the pool slot before the collect system runs

    let outcome = CompactionOutcome {
        entity,
        result: result.map(|()| summaries),
    };
    let _ = results.send(outcome);
    wake.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_pool::{InferencePoolConfig, InferencePools};
    use leviath_providers::{FinishReason, InferenceResponse, ModelCapabilities, TokenUsage};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    fn request() -> InferenceRequest {
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

    struct Script {
        out: Mutex<std::collections::VecDeque<Result<String, String>>>,
    }

    #[async_trait::async_trait]
    impl Provider for Script {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            match self.out.lock().unwrap().pop_front() {
                Some(Ok(text)) => Ok(InferenceResponse {
                    content: text,
                    tool_calls: vec![],
                    tokens_used: TokenUsage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                        cached_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    finish_reason: FinishReason::Complete,
                }),
                Some(Err(m)) => Err(ProviderError::Other(m)),
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
            "script"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn job(provider: Arc<dyn Provider>, regions: Vec<&str>) -> CompactionJob {
        let pools = InferencePools::new(InferencePoolConfig::new());
        CompactionJob {
            entity: Entity::from_raw(3),
            provider,
            requests: regions
                .into_iter()
                .map(|r| (r.to_string(), request()))
                .collect(),
            permit: pools.try_acquire("m").expect("free"),
        }
    }

    #[tokio::test]
    async fn runs_all_regions_and_reports_summaries() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let provider = Arc::new(Script {
            out: Mutex::new(vec![Ok("s1".to_string()), Ok("s2".to_string())].into()),
        });
        run_compaction_job(job(provider, vec!["a", "b"]), tx, wake.clone()).await;

        let outcome = rx.try_recv().unwrap();
        assert_eq!(outcome.entity, Entity::from_raw(3));
        let summaries = outcome.result.unwrap();
        assert_eq!(
            summaries,
            vec![
                ("a".to_string(), "s1".to_string()),
                ("b".to_string(), "s2".to_string()),
            ]
        );
        wake.notified().await; // was signalled
    }

    #[tokio::test]
    async fn stops_at_first_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let provider = Arc::new(Script {
            out: Mutex::new(vec![Err("boom".to_string())].into()),
        });
        run_compaction_job(job(provider, vec!["a", "b"]), tx, wake).await;

        let outcome = rx.try_recv().unwrap();
        assert!(outcome.result.is_err());
    }

    #[tokio::test]
    async fn survives_dropped_receiver() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let wake = Arc::new(Notify::new());
        let provider = Arc::new(Script {
            out: Mutex::new(vec![Ok("s".to_string())].into()),
        });
        run_compaction_job(job(provider, vec!["a"]), tx, wake).await;
    }

    #[tokio::test]
    async fn script_metadata_is_exercised() {
        let p = Script {
            out: Mutex::new(std::collections::VecDeque::new()),
        };
        assert_eq!(p.name(), "script");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }
}
