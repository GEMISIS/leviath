//! The async worker side of run-title generation - the sync-ECS to async-I/O
//! bridge for the titling call.
//!
//! `dispatch_title` (in [`crate::title`]) builds the request and hands it off
//! as a [`TitleJob`] with a pool permit. The call is held on the run as a
//! [`TitleCall`], and each trip to the provider is its own task
//! ([`run_title_attempt`]), which reports a [`TitleOutcome`] and wakes the tick
//! loop. A failed trip is retried on the dispatch lane's own schedule, decided
//! in the world: `collect_title` settles each outcome against the call, and
//! [`fire_due_titles`] sends the next trip once its backoff is over.
//!
//! Titling is still best-effort in that it never fails the agent: the outcome
//! carries a `Result`, and a run whose name could not be generated keeps
//! showing its task text. It is not *silent*, though: the outcome's error
//! reaches `collect_title`, which either moves the run to the next candidate
//! provider or records why the run has no name.

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::prelude::*;
use leviath_providers::{InferenceRequest, Provider, ProviderError};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::inference_call::{Due, Next, RetryClock, wake_at};
use crate::inference_pool::InferencePermit;
use crate::pipeline::InferenceStage;

/// One agent's title-generation call.
pub(crate) struct TitleJob {
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

/// The completed result of a title trip: the model's raw reply, or the
/// provider error the trip failed with.
pub(crate) struct TitleOutcome {
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

/// A title call held on the run from its first trip until `collect_title`
/// applies what it came to, its pool permit with it.
///
/// `policy` is the same schedule the dispatch lane uses, and for the same two
/// reasons. It retries a transient refusal - a reset connection, a 429, a
/// 5xx - instead of surrendering the run's name to one unlucky moment. And its
/// `job_timeout` is the outer bound: the permit must be released within a
/// fixed time even when the provider's own timer is missing (script
/// providers) or defeated, or a hung title call holds its pool slot forever.
#[derive(Component)]
pub(crate) struct TitleCall {
    /// The provider being asked.
    provider: Arc<dyn Provider>,
    /// Its name, for the outcome.
    provider_name: String,
    /// The model asked, for the outcome.
    model: String,
    /// The request every trip sends.
    request: Arc<InferenceRequest>,
    /// The pool slot the call holds until it settles.
    _permit: InferencePermit,
    /// The schedule a failed trip is retried on.
    policy: crate::inference_bridge::RetryPolicy,
    /// Where the schedule stands.
    clock: RetryClock,
}

/// One title trip: what its task needs and nothing it decides.
struct TitleAttempt {
    entity: Entity,
    provider: Arc<dyn Provider>,
    provider_name: String,
    model: String,
    request: Arc<InferenceRequest>,
    /// The first trip measures the request against the window first.
    guard: bool,
    deadline: tokio::time::Instant,
    job_timeout: Duration,
}

impl TitleCall {
    /// See [`RetryClock::make_due`].
    #[cfg(test)]
    pub(crate) fn make_due(&mut self, expired: bool) {
        self.clock.make_due(expired);
    }

    /// Hold `job` as a call retried under `policy`, and send its first trip.
    pub(crate) fn start(
        job: TitleJob,
        policy: crate::inference_bridge::RetryPolicy,
        stage: &InferenceStage,
        sink: &UnboundedSender<TitleOutcome>,
    ) -> Self {
        let TitleJob {
            entity,
            provider,
            provider_name,
            model,
            request,
            permit,
        } = job;
        let call = Self {
            provider,
            provider_name,
            model,
            request: Arc::new(request),
            _permit: permit,
            clock: RetryClock::start(&policy),
            policy,
        };
        call.send(entity, true, stage, sink);
        call
    }

    /// The next trip for `entity`.
    fn attempt(&self, entity: Entity, guard: bool) -> TitleAttempt {
        TitleAttempt {
            entity,
            provider: self.provider.clone(),
            provider_name: self.provider_name.clone(),
            model: self.model.clone(),
            request: self.request.clone(),
            guard,
            deadline: self.clock.deadline(),
            job_timeout: self.policy.job_timeout,
        }
    }

    /// Spawn a trip for `entity`, supervised: the run is held `AwaitingTitle`
    /// until an outcome lands, so a trip that died without one would hold a
    /// finished run in memory until its deadline and then blame the clock.
    /// The synthesized error takes the collect system's failure path, which
    /// moves on down the chain exactly as a refused call does.
    fn send(
        &self,
        entity: Entity,
        guard: bool,
        stage: &InferenceStage,
        sink: &UnboundedSender<TitleOutcome>,
    ) {
        let lost = (sink.clone(), stage.wake.clone());
        let (lost_provider, lost_model) = (self.provider_name.clone(), self.model.clone());
        crate::lane_supervisor::spawn_supervised(
            &stage.runtime,
            "title",
            run_title_attempt(
                self.attempt(entity, guard),
                sink.clone(),
                stage.wake.clone(),
            ),
            move |message| {
                let _ = lost.0.send(TitleOutcome {
                    entity,
                    result: Err(ProviderError::Other(message)),
                    finish_reason: None,
                    // Nothing was served, so nothing was billed.
                    usage: None,
                    provider_name: lost_provider,
                    model: lost_model,
                    pricing: None,
                });
                lost.1.notify_one();
            },
        );
    }

    /// Settle what one trip came back with: a failure the schedule retries
    /// leaves the call waiting on its due time, and anything else finishes
    /// it. Mirrors the dispatch lane, down to sharing `backoff_after`: a
    /// capacity refusal gets the slow schedule or the provider's own
    /// `Retry-After`, an ordinary blip the fast one, and a permanent error
    /// stops at once.
    pub(crate) fn settle(&mut self, outcome: &TitleOutcome) -> Next {
        let Err(error) = &outcome.result else {
            return Next::Done;
        };
        match self.clock.after_failure(&self.policy, error) {
            Some(delay) => Next::Waiting(self.clock.wait(delay)),
            None => Next::Done,
        }
    }

    /// The outcome a call reports when its whole allowance ran out.
    fn timed_out(&self, entity: Entity) -> TitleOutcome {
        TitleOutcome {
            entity,
            result: Err(title_timed_out(self.policy.job_timeout)),
            finish_reason: None,
            usage: None,
            provider_name: self.provider_name.clone(),
            model: self.model.clone(),
            pricing: None,
        }
    }
}

/// The error a title call reports when it runs out of its whole allowance.
fn title_timed_out(job_timeout: Duration) -> ProviderError {
    ProviderError::Other(format!(
        "title generation exceeded the {}s deadline and was aborted to free the pool slot",
        job_timeout.as_secs()
    ))
}

/// Make one title trip, report it, and wake the tick loop.
async fn run_title_attempt(
    attempt: TitleAttempt,
    results: UnboundedSender<TitleOutcome>,
    wake: Arc<Notify>,
) {
    let TitleAttempt {
        entity,
        provider,
        provider_name,
        model,
        request,
        guard,
        deadline,
        job_timeout,
    } = attempt;
    let trip = async {
        // The same pre-flight guard as the stage lane, inside the same
        // deadline. A title request is a few hundred tokens and almost never
        // reaches the counting line, but "almost" is not a property a lane gets
        // to rely on: the model is whatever `[title]` names, and its window is
        // its own.
        if guard {
            crate::inference_bridge::guard_context_window(provider.as_ref(), &request, None)
                .await?;
        }
        provider.infer(&request).await
    };
    // The usage travels beside the reply rather than being folded into it: the
    // collect system wants the title, the run's accounting wants the tokens,
    // and dropping the half this channel had no use for is how the title call
    // came to be billed and counted nowhere.
    let (result, usage, finish_reason) = match tokio::time::timeout_at(deadline, trip).await {
        Ok(Ok(r)) => (Ok(r.content), Some(r.tokens_used), Some(r.finish_reason)),
        Ok(Err(e)) => (Err(e), None, None),
        Err(_) => (Err(title_timed_out(job_timeout)), None, None),
    };
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

/// Send each title call's next trip once its backoff is over. A call whose
/// whole allowance ran out while it waited reports that, without another trip.
pub(crate) fn fire_due_titles(
    mut calls: Query<(Entity, &mut TitleCall)>,
    stage: Res<InferenceStage>,
    sink: Res<crate::title::TitleSink>,
) {
    crate::tick_scope::clear();
    let now = tokio::time::Instant::now();
    for (entity, mut call) in calls.iter_mut() {
        crate::tick_scope::enter(entity);
        match call.clock.take_due(now) {
            None => {}
            Some(Due::Expired) => {
                let _ = sink.0.send(call.timed_out(entity));
                stage.wake.notify_one();
            }
            Some(Due::Send) => call.send(entity, false, &stage, &sink.0),
        }
    }
}

/// After a title trip came back: settle it against the run's call, and when
/// the call waits for another trip, wake the tick loop when that one is due.
/// A finished call is taken off the run, releasing its permit.
pub(crate) fn settle_title(
    call: Option<Mut<TitleCall>>,
    outcome: &TitleOutcome,
    stage: Option<&InferenceStage>,
    commands: &mut Commands,
) -> Next {
    let (Some(mut call), Some(stage)) = (call, stage) else {
        return Next::Done;
    };
    let next = call.settle(outcome);
    match next {
        Next::Done => {
            commands.entity(outcome.entity).remove::<TitleCall>();
        }
        Next::Waiting(due) => wake_at(&stage.runtime, &stage.wake, due),
    }
    next
}

#[cfg(test)]
#[path = "title_bridge_tests.rs"]
pub(crate) mod tests;
