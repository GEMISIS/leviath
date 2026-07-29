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
}

/// Run one title job: make the call with the permit held, release the slot,
/// report the outcome, and wake the tick loop.
pub async fn run_title_job(
    job: TitleJob,
    results: UnboundedSender<TitleOutcome>,
    wake: Arc<Notify>,
) {
    let TitleJob {
        entity,
        provider,
        request,
        permit,
    } = job;

    let result = provider.infer(request).await.map(|r| r.content);
    drop(permit); // free the pool slot before the collect system runs

    let _ = results.send(TitleOutcome { entity, result });
    wake.notify_one();
}
