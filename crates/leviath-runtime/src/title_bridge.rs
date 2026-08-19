//! The async worker side of run-title generation - the sync-ECS to async-I/O
//! bridge for the one-shot titling call.
//!
//! `dispatch_title` (in [`crate::title`]) builds the request and hands it off
//! as a [`TitleJob`] with a pool permit; [`run_title_job`] makes the single
//! provider call, reports a [`TitleOutcome`], and wakes the tick loop so the
//! collect system can store the title.
//!
//! Titling is best-effort: a provider error just means the run keeps showing
//! its blueprint name in the dashboard, so the outcome carries a `Result` the
//! collect system drops on the floor rather than failing the agent.

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
    /// The one-shot titling request.
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
}

/// Run one title job: make the call with the permit held, release the slot,
/// report the outcome, and wake the tick loop.
///
/// `deadline` bounds the call the same way `RetryPolicy.job_timeout` bounds a
/// dispatch-lane inference: the permit must be released within a fixed time
/// even when the provider's own timer is missing (script providers) or
/// defeated. A hung title call once held its pool slot forever.
pub async fn run_title_job(
    job: TitleJob,
    deadline: std::time::Duration,
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

    // The usage travels beside the reply rather than being folded into it: the
    // collect system wants the title, the run's accounting wants the tokens,
    // and dropping the half this channel had no use for is how the title call
    // came to be billed and counted nowhere.
    let call = tokio::time::timeout(deadline, provider.infer(&request)).await;
    let (result, usage, finish_reason) = match call {
        Ok(Ok(r)) => (Ok(r.content), Some(r.tokens_used), Some(r.finish_reason)),
        Ok(Err(e)) => (Err(e), None, None),
        Err(_) => (
            Err(leviath_providers::ProviderError::Other(format!(
                "title generation exceeded the {}s deadline and was aborted to free the pool slot",
                deadline.as_secs()
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
        provider_name,
        model,
    });
    wake.notify_one();
}
