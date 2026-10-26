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

use crate::inference_pool::{InferencePermit, InferencePools};

/// One place a summary may be sent: a registered provider, the model on it,
/// and the request fields that provider's retention settings add.
#[derive(Clone)]
pub(crate) struct SummaryRoute {
    /// The provider.
    pub provider: Arc<dyn Provider>,
    /// Its name in the registry, for the usage records and the pools. The
    /// trait object cannot answer for itself which entry it came from.
    pub provider_name: String,
    /// The model the requests name on it.
    pub model: String,
    /// The extra request fields it is sent with.
    pub extra: serde_json::Value,
}

/// A batch of per-region summarize requests for one agent.
pub(crate) struct CompactionJob {
    /// The agent whose context is being compacted.
    pub entity: Entity,
    /// The route the batch starts on.
    pub route: SummaryRoute,
    /// The pool permit for that route, held for the whole batch.
    pub permit: InferencePermit,
    /// Where the batch goes, best first, when a route turns out to be
    /// unusable before it summarized anything.
    pub fallbacks: Vec<SummaryRoute>,
    /// The pools a fallback takes its slot from.
    pub pools: Arc<InferencePools>,
    /// One `(region_name, request)` per region to summarize. Each request is
    /// sent with its route's model and extra fields.
    pub requests: Vec<(String, InferenceRequest)>,
}

/// The completed result of a [`CompactionJob`]: the `(region_name, summary)`
/// pairs, or the first provider error encountered.
pub(crate) struct CompactionOutcome {
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
    /// What the provider charges for that model, resolved while the
    /// `Arc<dyn Provider>` was still in scope. Only the name survives past
    /// here, and a name cannot be asked its rates.
    pub pricing: Option<leviath_providers::ModelPricing>,
}

/// What one route made of the batch.
struct Batch {
    summaries: Vec<(String, String)>,
    usage: Vec<leviath_providers::TokenUsage>,
    result: Result<(), ProviderError>,
}

impl Batch {
    /// Whether the batch should move to the next route: the provider said it
    /// cannot serve anyone (out of credits, unreachable) before a single
    /// summary was billed, so nothing is lost by sending the batch elsewhere.
    fn fails_over(&self) -> bool {
        self.usage.is_empty()
            && self
                .result
                .as_ref()
                .is_err_and(|e| e.unavailable_reason().is_some())
    }
}

/// Summarize each region on `route`, in order, stopping at the first error.
async fn summarize(
    route: &SummaryRoute,
    requests: &[(String, InferenceRequest)],
    deadline: std::time::Duration,
) -> Batch {
    let mut batch = Batch {
        summaries: Vec::new(),
        usage: Vec::new(),
        result: Ok(()),
    };
    for (region, request) in requests {
        let request = InferenceRequest {
            model: route.model.clone(),
            extra: route.extra.clone(),
            ..request.clone()
        };
        // Guarded before it is sent, like every other lane's request, and
        // inside the same deadline so a count that hangs cannot hold the slot
        // past it. No calibration: that corrects the agent's own window on
        // the agent's own model, and this request goes to the compaction
        // model, whose framing it has never measured.
        let call = async {
            crate::inference_bridge::guard_context_window(route.provider.as_ref(), &request, None)
                .await?;
            route.provider.infer(&request).await
        };
        match tokio::time::timeout(deadline, call).await {
            Ok(Ok(response)) => {
                crate::listed_404s::answered(route.provider.name(), &route.model);
                batch.usage.push(response.tokens_used);
                batch.summaries.push((region.clone(), response.content));
            }
            Ok(Err(e)) => {
                // A 404 for a model the route's provider lists is an outage,
                // and moves the batch on as an unusable route does.
                let listed = crate::inference_bridge::listed_outage(
                    route.provider.as_ref(),
                    &route.model,
                    &e,
                );
                batch.result = Err(listed.unwrap_or(e));
                break;
            }
            Err(_) => {
                batch.result = Err(leviath_providers::ProviderError::Other(format!(
                    "compaction exceeded the {}s deadline and was aborted to free \
                     the pool slot",
                    deadline.as_secs()
                )));
                break;
            }
        }
    }
    batch
}

/// Run one compaction job: summarize each region sequentially with the permit
/// held, release the slot, report the outcome, and wake the tick loop. Stops at
/// the first error (best-effort - the collect system leaves the context as-is),
/// except that a route whose provider turns out to be unusable before it wrote
/// anything hands the batch to the next fallback with a free slot.
/// `deadline` bounds each summarization call the same way
/// `RetryPolicy.job_timeout` bounds a dispatch-lane inference: the permit must
/// be released within a fixed time even when the provider's own timer is
/// missing (script providers) or defeated.
pub(crate) async fn run_compaction_job(
    job: CompactionJob,
    deadline: std::time::Duration,
    results: UnboundedSender<CompactionOutcome>,
    wake: Arc<Notify>,
) {
    let CompactionJob {
        entity,
        route,
        permit,
        fallbacks,
        pools,
        requests,
    } = job;
    let mut rest = fallbacks.into_iter();
    let mut at = (route, permit);
    let (route, batch) = loop {
        let (route, permit) = at;
        let batch = summarize(&route, &requests, deadline).await;
        drop(permit); // free the pool slot before anything else runs
        let next = match batch.fails_over() {
            true => rest.by_ref().find_map(|r| {
                pools
                    .try_acquire(&r.provider_name, &r.model)
                    .map(|p| (r, p))
            }),
            false => None,
        };
        let Some(next) = next else {
            break (route, batch);
        };
        tracing::warn!(
            from_provider = %route.provider_name,
            from_model = %route.model,
            to_provider = %next.0.provider_name,
            to_model = %next.0.model,
            "compaction provider is unusable; summarizing on the next route"
        );
        at = next;
    };

    let outcome = CompactionOutcome {
        entity,
        result: batch.result.map(|()| batch.summaries),
        usage: batch.usage,
        pricing: route.provider.pricing(&route.model),
        provider_name: route.provider_name,
        model: route.model,
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
                    parts: Vec::new(),
                    content: text,
                    tool_calls: vec![],
                    tokens_used: TokenUsage {
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                        cached_tokens: 0,
                        cache_write_tokens: 0,
                        reported_cost_usd: None,
                    },
                    finish_reason: FinishReason::Complete,
                    reasoning: None,
                    content_is_reasoning: false,
                }),
                Some(Err(m)) if m == "unavailable" => Err(ProviderError::Unavailable {
                    reason: leviath_providers::UnavailableReason::CreditsExhausted,
                    detail: m,
                }),
                Some(Err(m)) if m == "not-found" => Err(ProviderError::ApiError(
                    "[not-found] HTTP 404 Not Found: no such model".to_string(),
                )),
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
        /// Lists only the model a `listed` route names.
        fn lists_model(&self, model_key: &str) -> Option<String> {
            (model_key == "listed-model").then(|| model_key.to_string())
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

    fn route(name: &str, provider: Arc<dyn Provider>) -> SummaryRoute {
        SummaryRoute {
            provider,
            provider_name: name.to_string(),
            model: format!("{name}-model"),
            extra: serde_json::Value::Null,
        }
    }

    fn job(provider: Arc<dyn Provider>, regions: Vec<&str>) -> CompactionJob {
        job_over(
            route("p", provider),
            Vec::new(),
            InferencePools::new(InferencePoolConfig::new()),
            regions,
        )
    }

    fn job_over(
        first: SummaryRoute,
        fallbacks: Vec<SummaryRoute>,
        pools: InferencePools,
        regions: Vec<&str>,
    ) -> CompactionJob {
        let pools = Arc::new(pools);
        CompactionJob {
            entity: Entity::from_raw_u32(3)
                .expect("a small literal index is always a valid entity id"),
            permit: pools
                .try_acquire(&first.provider_name, &first.model)
                .expect("free"),
            route: first,
            fallbacks,
            pools,
            requests: regions
                .into_iter()
                .map(|r| (r.to_string(), request()))
                .collect(),
        }
    }

    fn script(out: Vec<Result<&str, &str>>) -> Arc<dyn Provider> {
        Arc::new(Script {
            out: Mutex::new(
                out.into_iter()
                    .map(|r| r.map(str::to_string).map_err(str::to_string))
                    .collect(),
            ),
        })
    }

    async fn outcome_of(job: CompactionJob) -> CompactionOutcome {
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_compaction_job(
            job,
            std::time::Duration::from_secs(60),
            tx,
            Arc::new(Notify::new()),
        )
        .await;
        rx.try_recv().expect("an outcome is always reported")
    }

    /// A provider out of credits before it wrote anything hands the batch to
    /// the next route, which summarizes it and is billed for it.
    #[tokio::test]
    async fn an_unusable_route_fails_over_to_the_next() {
        let outcome = outcome_of(job_over(
            route("a", script(vec![Err("unavailable")])),
            vec![route("b", script(vec![Ok("s1"), Ok("s2")]))],
            InferencePools::new(InferencePoolConfig::new()),
            vec!["x", "y"],
        ))
        .await;
        assert_eq!(
            (outcome.provider_name.as_str(), outcome.model.as_str()),
            ("b", "b-model")
        );
        assert_eq!(outcome.usage.len(), 2);
        assert_eq!(outcome.result.unwrap().len(), 2);
    }

    /// A 404 for a model the route's provider lists is an outage and moves
    /// the batch on; a 404 for one it does not list is that route's answer.
    #[tokio::test]
    async fn a_not_found_moves_on_only_for_a_listed_model() {
        let outcome = outcome_of(job_over(
            route("listed", script(vec![Err("not-found")])),
            vec![route("b", script(vec![Ok("s1")]))],
            InferencePools::new(InferencePoolConfig::new()),
            vec!["x"],
        ))
        .await;
        assert_eq!(outcome.provider_name, "b");
        assert_eq!(outcome.result.unwrap().len(), 1);

        let outcome = outcome_of(job_over(
            route("a", script(vec![Err("not-found")])),
            vec![route("b", script(vec![Ok("never")]))],
            InferencePools::new(InferencePoolConfig::new()),
            vec!["x"],
        ))
        .await;
        assert_eq!(outcome.provider_name, "a");
        assert!(outcome.result.is_err());
    }

    /// A fallback whose pool is full is passed over; with none left, the
    /// first route's error is the outcome.
    #[tokio::test]
    async fn a_busy_fallback_is_passed_over() {
        let mut busy = InferencePoolConfig::new();
        busy.set_limit("b-model", 0);
        let outcome = outcome_of(job_over(
            route("a", script(vec![Err("unavailable")])),
            vec![route("b", script(vec![Ok("never")]))],
            InferencePools::new(busy),
            vec!["x"],
        ))
        .await;
        assert_eq!(outcome.provider_name, "a");
        assert!(outcome.result.is_err());
    }

    /// An ordinary error, or one after a summary was already billed, ends
    /// the batch where it is: the next route would get the same request, or
    /// the bill would split across two providers.
    #[tokio::test]
    async fn only_an_unusable_provider_with_nothing_billed_fails_over() {
        for (first, billed) in [
            (vec![Err("boom")], 0),
            (vec![Ok("s"), Err("unavailable")], 1),
        ] {
            let outcome = outcome_of(job_over(
                route("a", script(first)),
                vec![route("b", script(vec![Ok("never"), Ok("never")]))],
                InferencePools::new(InferencePoolConfig::new()),
                vec!["x", "y"],
            ))
            .await;
            assert_eq!(outcome.provider_name, "a");
            assert_eq!(outcome.usage.len(), billed);
            assert!(outcome.result.is_err());
        }
    }

    /// Each request goes out with its route's model and extra fields.
    #[tokio::test]
    async fn each_request_names_its_routes_model() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = outcome_of(job_over(
            SummaryRoute {
                extra: serde_json::json!({"store": false}),
                ..route("a", Arc::new(Recording { seen: seen.clone() }))
            },
            Vec::new(),
            InferencePools::new(InferencePoolConfig::new()),
            vec!["x"],
        ))
        .await;
        assert!(outcome.result.is_err(), "the recorder answers nothing");
        assert_eq!(
            *seen.lock().unwrap(),
            [("a-model".to_string(), serde_json::json!({"store": false}))]
        );
        let r = Recording {
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        assert_eq!(r.count_tokens("t", "m").await, 1);
        assert_eq!(r.max_context_tokens("m"), 100_000);
        assert_eq!(r.name(), "recording");
        let _ = r.capabilities("m");
    }

    /// A provider that records the model and extra fields of each request.
    struct Recording {
        seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }

    #[async_trait::async_trait]
    impl Provider for Recording {
        async fn infer(
            &self,
            req: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            self.seen
                .lock()
                .unwrap()
                .push((req.model.clone(), req.extra.clone()));
            Err(ProviderError::Other("recorded".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "recording"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
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

    /// A provider with a 1,000-token window that counts every request at 950,
    /// and records whether `infer` was ever reached.
    struct Narrow {
        inferred: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl Provider for Narrow {
        async fn infer(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            self.inferred
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(InferenceResponse {
                content: "summary".to_string(),
                tool_calls: vec![],
                tokens_used: TokenUsage::new(1, 0, 0, 1),
                finish_reason: FinishReason::Complete,
                reasoning: None,
                parts: Vec::new(),
                content_is_reasoning: false,
            })
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            950
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            1_000
        }
        fn name(&self) -> &str {
            "narrow"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    /// The compaction lane is guarded like the stage lane: a summarize request
    /// that would overflow the compaction model's window is refused before it
    /// is sent, and the batch fails with the refusal rather than the provider's
    /// own rejection after the round trip.
    #[tokio::test]
    async fn the_compaction_lane_refuses_an_overflowing_request() {
        let provider = Arc::new(Narrow {
            inferred: std::sync::atomic::AtomicBool::new(false),
        });
        assert_eq!(provider.name(), "narrow");
        let _ = provider.capabilities("m");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut job = job(provider.clone(), vec!["a"]);
        // 2,000 bytes of region text: estimated at 500, plus the 100-token
        // reply budget, is over half the 1,000 window, so it is measured.
        job.requests[0].1.messages.push(leviath_providers::Message {
            role: "user".to_string(),
            content: "x".repeat(2_000).into(),
            cache_breakpoint: false,
            reasoning: None,
        });
        run_compaction_job(
            job,
            std::time::Duration::from_secs(60),
            tx,
            Arc::new(Notify::new()),
        )
        .await;

        let outcome = rx.try_recv().unwrap();
        let err = outcome.result.expect_err("950 + 100 does not fit in 1,000");
        assert_eq!(
            err.to_string(),
            "Token limit exceeded: the prompt's 950 tokens plus the 100-token \
             reply budget (max_output_tokens) exceed the model's 1000-token \
             context window. Lower the region budgets that fill the prompt, or, if \
             that window is wrong for this model, set the real one under \
             [model_capabilities.<model>]"
        );
        assert!(
            !provider.inferred.load(std::sync::atomic::Ordering::SeqCst),
            "refused before the call, not after it"
        );
        assert!(
            outcome.usage.is_empty(),
            "nothing ran, so nothing was billed"
        );
        // The provider would have answered had the request reached it, so the
        // refusal above is the guard's doing and not the mock's.
        let answered = provider.infer(&request()).await.expect("the mock answers");
        assert_eq!(answered.content, "summary");
        assert!(provider.inferred.load(std::sync::atomic::Ordering::SeqCst));
    }
}
