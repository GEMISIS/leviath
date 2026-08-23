//! The async worker side of run-title generation - the sync-ECS to async-I/O
//! bridge for the titling call.
//!
//! `dispatch_title` (in [`crate::title`]) builds the request and hands it off
//! as a [`TitleJob`] with a pool permit; [`run_title_job`] makes the provider
//! call - retrying a transient refusal on the dispatch lane's own schedule -
//! reports a [`TitleOutcome`], and wakes the tick loop so the collect system
//! can store the title.
//!
//! Titling is still best-effort in that it never fails the agent: the outcome
//! carries a `Result`, and a run whose name could not be generated keeps
//! showing its task text. What it is no longer is *silent* - the outcome's
//! error reaches `collect_title`, which either moves the run to the next
//! candidate provider or records why the run has no name.

use std::sync::Arc;

use bevy_ecs::entity::Entity;
use leviath_providers::{InferenceRequest, Provider, ProviderError};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::inference_pool::InferencePermit;

/// One agent's title-generation call.
pub struct TitleJob {
    /// The agent whose run is being titled.
    pub entity: Entity,
    /// The provider resolved for the title model.
    pub provider: Arc<dyn Provider>,
    /// The name of that provider, for the usage record. The trait object above
    /// cannot answer for itself which registry entry it came from.
    pub provider_name: String,
    /// The model the request targets, for the usage record.
    pub model: String,
    /// The titling request.
    pub request: InferenceRequest,
    /// The per-model pool permit, held for the call.
    pub permit: InferencePermit,
}

/// The completed result of a [`TitleJob`]: the model's raw reply, or the
/// provider error the call failed with.
pub struct TitleOutcome {
    /// The agent the title belongs to.
    pub entity: Entity,
    /// The raw model reply (sanitized by the collect system), or the error.
    pub result: Result<String, ProviderError>,
    /// How the provider says the reply ended. `None` for a call that never
    /// completed. [`crate::title::collect_title`] refuses a reply that stopped
    /// at the token limit: a title cut off mid-sentence is not a title, and
    /// that is exactly the shape a reasoning model returns when it spends the
    /// budget thinking.
    pub finish_reason: Option<leviath_providers::FinishReason>,
    /// What the call billed, when it completed. `None` for a call that failed
    /// or timed out, which is the only case where nothing was served.
    pub usage: Option<leviath_providers::TokenUsage>,
    /// The provider that served the call.
    pub provider_name: String,
    /// The model the call targeted.
    pub model: String,
    /// That model's rates, resolved while the provider handle was in scope.
    pub pricing: Option<leviath_providers::ModelPricing>,
}

/// Run one title job: make the call with the permit held, release the slot,
/// report the outcome, and wake the tick loop.
///
/// `retry` is the same policy the dispatch lane uses, and for the same two
/// reasons. Its schedule retries a transient refusal - a reset connection, a
/// 429, a 5xx - instead of surrendering the run's name to one unlucky moment;
/// this call used to be a single naked `infer`, so a provider having a bad
/// minute left the run permanently untitled and said so only to a debug log.
/// Its `job_timeout` is the outer bound: the permit must be released within a
/// fixed time even when the provider's own timer is missing (script providers)
/// or defeated. A hung title call once held its pool slot forever.
pub async fn run_title_job(
    job: TitleJob,
    retry: crate::inference_bridge::RetryPolicy,
    results: UnboundedSender<TitleOutcome>,
    wake: Arc<Notify>,
) {
    let TitleJob {
        entity,
        provider,
        provider_name,
        model,
        request,
        permit,
    } = job;

    // Mirrors `run_inference_job`'s loop, down to sharing `backoff_after`: a
    // capacity refusal gets the slow schedule or the provider's own
    // `Retry-After`, an ordinary blip the fast one, and a permanent error stops
    // at once. `infer` borrows the request, so every attempt reuses the one
    // assembled copy.
    let attempts = async {
        let mut attempt = 1u32;
        let mut spent = std::time::Duration::ZERO;
        loop {
            match provider.infer(&request).await {
                Ok(response) => break Ok(response),
                Err(e) => {
                    match crate::inference_bridge::backoff_after(&retry, &e, attempt, spent) {
                        Some(delay) => {
                            tokio::time::sleep(delay).await;
                            spent = spent.saturating_add(delay);
                            attempt += 1;
                        }
                        None => break Err(e),
                    }
                }
            }
        }
    };
    // The usage travels beside the reply rather than being folded into it: the
    // collect system wants the title, the run's accounting wants the tokens,
    // and dropping the half this channel had no use for is how the title call
    // came to be billed and counted nowhere.
    let call = tokio::time::timeout(retry.job_timeout, attempts).await;
    let (result, usage, finish_reason) = match call {
        Ok(Ok(r)) => (Ok(r.content), Some(r.tokens_used), Some(r.finish_reason)),
        Ok(Err(e)) => (Err(e), None, None),
        Err(_) => (
            Err(leviath_providers::ProviderError::Other(format!(
                "title generation exceeded the {}s deadline and was aborted to free the pool slot",
                retry.job_timeout.as_secs()
            ))),
            None,
            None,
        ),
    };
    drop(permit); // free the pool slot before the collect system runs

    let _ = results.send(TitleOutcome {
        entity,
        result,
        finish_reason,
        usage,
        pricing: provider.pricing(&model),
        provider_name,
        model,
    });
    wake.notify_one();
}
