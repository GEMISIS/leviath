//! The async worker side of the ECS compaction stage - the sync-ECS ↔ async-I/O
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
    /// The name of that provider, for the usage records. The trait object above
    /// cannot answer for itself which registry entry it came from.
    pub provider_name: String,
    /// The model the requests target, for the usage records.
    pub model: String,
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
    /// What each completed call billed, one entry per region actually
    /// summarized.
    ///
    /// Carried separately from `result` because the two do not have the same
    /// length when a batch fails partway: the summaries are discarded whole
    /// (compaction is all-or-nothing for the window), but the calls that
    /// already ran were still billed and still have to be counted.
    pub usage: Vec<leviath_providers::TokenUsage>,
    /// The provider that served the batch.
    pub provider_name: String,
    /// The model the batch targeted.
    pub model: String,
}

/// Run one compaction job: summarize each region sequentially with the permit
/// held, release the slot, report the outcome, and wake the tick loop. Stops at
/// the first error (best-effort - the collect system leaves the context as-is).
/// `deadline` bounds each summarization call the same way
/// `RetryPolicy.job_timeout` bounds a dispatch-lane inference: the permit must
/// be released within a fixed time even when the provider's own timer is
/// missing (script providers) or defeated.
pub async fn run_compaction_job(
    job: CompactionJob,
    deadline: std::time::Duration,
    results: UnboundedSender<CompactionOutcome>,
    wake: Arc<Notify>,
) {
    let CompactionJob {
        entity,
        provider,
        provider_name,
        model,
        requests,
        permit,
    } = job;

    let mut summaries = Vec::new();
    let mut usage = Vec::new();
    let mut result = Ok(());
    for (region, request) in requests {
        match tokio::time::timeout(deadline, provider.infer(&request)).await {
            Ok(Ok(response)) => {
                usage.push(response.tokens_used);
                summaries.push((region, response.content));
            }
            Ok(Err(e)) => {
                result = Err(e);
                break;
            }
            Err(_) => {
                result = Err(leviath_providers::ProviderError::Other(format!(
                    "compaction exceeded the {}s deadline and was aborted to free \
                     the pool slot",
                    deadline.as_secs()
                )));
                break;
            }
        }
    }
    drop(permit); // free the pool slot before the collect system runs

    let outcome = CompactionOutcome {
        entity,
        result: result.map(|()| summaries),
        usage,
        provider_name,
        model,
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
            request_timeout_secs: None,
        }
    }

    struct Script {
        out: Mutex<std::collections::VecDeque<Result<String, String>>>,
    }

    #[async_trait::async_trait]
    impl Provider for Script {
        async fn infer(
            &self,
            _req: &InferenceRequest,
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

    /// A provider whose `infer` never returns - only the job deadline can end
    /// the call, which is exactly what these tests exercise.
    struct Hang;

    #[async_trait::async_trait]
    impl Provider for Hang {
        async fn infer(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            std::future::pending().await
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "hang"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    /// The permit must come back within the deadline even when the provider
    /// never answers - a hung compaction call once held its pool slot forever.
    #[tokio::test]
    async fn deadline_frees_the_slot_when_the_provider_hangs() {
        // Trait obligations on the mock; only `infer` matters to this test.
        assert_eq!(Hang.count_tokens("t", "m").await, 1);
        assert_eq!(Hang.max_context_tokens("m"), 100_000);
        assert_eq!(Hang.name(), "hang");
        let _ = Hang.capabilities("m");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let wake = Arc::new(tokio::sync::Notify::new());
        run_compaction_job(
            job(Arc::new(Hang), vec!["a"]),
            std::time::Duration::from_millis(5),
            tx,
            wake,
        )
        .await;
        let outcome = rx.recv().await.expect("an outcome is always reported");
        let err = outcome.result.expect_err("the deadline must surface");
        assert!(err.to_string().contains("deadline"), "{err}");
    }

    fn job(provider: Arc<dyn Provider>, regions: Vec<&str>) -> CompactionJob {
        let pools = InferencePools::new(InferencePoolConfig::new());
        CompactionJob {
            entity: Entity::from_raw_u32(3)
                .expect("a small literal index is always a valid entity id"),
            provider,
            provider_name: "p".to_string(),
            model: "m".to_string(),
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
        run_compaction_job(
            job(provider, vec!["a", "b"]),
            std::time::Duration::from_secs(60),
            tx,
            wake.clone(),
        )
        .await;

        let outcome = rx.try_recv().unwrap();
        assert_eq!(
            outcome.entity,
            Entity::from_raw_u32(3).expect("a small literal index is always a valid entity id")
        );
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
        run_compaction_job(
            job(provider, vec!["a", "b"]),
            std::time::Duration::from_secs(60),
            tx,
            wake,
        )
        .await;

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
        run_compaction_job(
            job(provider, vec!["a"]),
            std::time::Duration::from_secs(60),
            tx,
            wake,
        )
        .await;
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
