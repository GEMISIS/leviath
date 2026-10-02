//! A model call held in the world for as long as it takes, retries included.
//!
//! A lane that asks a provider something (an agent's turn, a routing choice)
//! keeps the call on the agent as an [`InferenceCall`]: the request, the pool
//! permit it holds, its retry policy and a [`RetryClock`]. Each trip to the
//! provider is its own task ([`run_attempt`]), which makes the trip and reports
//! what came back. Everything after that is decided here, in the world: a
//! failed trip is journaled, and [`InferenceCall::settle`] says whether the
//! call is tried again at once (a file the vendor lost), after a backoff, or
//! not at all. A backoff is a due time on the clock, and [`fire_due_calls`]
//! sends the next trip when it comes round, so a pause, an inspection or a
//! cancel sees a call that is waiting to retry as exactly that.
//!
//! The permit stays with the call the whole time, backoffs included, as it
//! always has: a call sitting out a 429 still owns its slot. It is released
//! when the call settles or its agent ends.

use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::prelude::*;
use leviath_providers::{InferenceRequest, ProviderError};
use tokio::time::Instant;

use crate::inference_bridge::{
    AttemptJournal, Ending, InferenceAttempt, InferenceJob, InferenceOutcome, JobHydration,
    RetryPolicy, backoff_after, failed, job_timed_out, run_attempt,
};
use crate::inference_pool::InferencePermit;
use crate::pipeline::{InFlightWork, InferenceStage, is_terminal_status, track_in_flight};
use crate::runfile::record::Retry;

/// Where a call's backoff stands: how many attempts the retry budget has
/// spent, how long it has slept, and when the next trip is due.
///
/// Shared by every lane that retries (the agent's turn, the routing choice,
/// the title), so the three read a schedule the same way.
#[derive(Debug, Clone)]
pub(crate) struct RetryClock {
    /// The attempt the retry budget is on, counting from 1. A file renewal
    /// spends none of it.
    attempt: u32,
    /// The backoff slept so far, against the policy's ceiling.
    spent: Duration,
    /// When the call started.
    started: Instant,
    /// When the whole call runs out of time, trips and backoffs together.
    deadline: Instant,
    /// When the next trip goes out, while the call is waiting to send one.
    due: Option<Instant>,
}

/// What a due time found when it came round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Due {
    /// Send the next trip.
    Send,
    /// The call's whole allowance ran out while it waited.
    Expired,
}

impl RetryClock {
    /// Make the waiting trip due now, or (with `expired`) make the whole
    /// allowance run out now too, so a test need not sleep out a backoff.
    #[cfg(test)]
    pub(crate) fn make_due(&mut self, expired: bool) {
        let now = Instant::now();
        if expired {
            self.deadline = now;
        }
        self.due = Some(now);
    }

    /// A clock for a call starting now under `policy`.
    pub(crate) fn start(policy: &RetryPolicy) -> Self {
        let started = Instant::now();
        Self {
            attempt: 1,
            spent: Duration::ZERO,
            started,
            deadline: started + policy.job_timeout,
            due: None,
        }
    }

    /// When the whole call runs out of time.
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// When the next trip is due, while the call waits for one.
    #[cfg(test)]
    pub(crate) fn due_at(&self) -> Option<Instant> {
        self.due
    }

    /// How long the call has taken so far.
    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// After a failed trip: how long to wait before the next one, or `None`
    /// to stop and report `error`. Spends the wait from the budget.
    pub(crate) fn after_failure(
        &mut self,
        policy: &RetryPolicy,
        error: &ProviderError,
    ) -> Option<Duration> {
        if Instant::now() >= self.deadline {
            return None;
        }
        let delay = backoff_after(policy, error, self.attempt, self.spent)?;
        self.spent = self.spent.saturating_add(delay);
        self.attempt += 1;
        Some(delay)
    }

    /// Wait `delay` before the next trip, or less when the call's deadline
    /// comes first. Returns when the trip is due.
    pub(crate) fn wait(&mut self, delay: Duration) -> Instant {
        let due = (Instant::now() + delay).min(self.deadline);
        self.due = Some(due);
        due
    }

    /// Whether the wait is over at `now`, and if so what for. Clears the due
    /// time, so a due trip fires once.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<Due> {
        let due = self.due.filter(|due| now >= *due)?;
        self.due = None;
        Some(match due >= self.deadline {
            true => Due::Expired,
            false => Due::Send,
        })
    }
}

/// Wake the tick loop at `due`, so a call waiting on a backoff is looked at
/// when its time comes. The task only sleeps and wakes; whether anything is
/// sent then is decided by [`fire_due_calls`].
pub(crate) fn wake_at(
    runtime: &tokio::runtime::Handle,
    wake: &Arc<tokio::sync::Notify>,
    due: Instant,
) {
    let wake = wake.clone();
    runtime.spawn(async move {
        tokio::time::sleep_until(due).await;
        wake.notify_one();
    });
}

/// Which lane a call reports to: the two collect from separate channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallLane {
    /// An agent's turn, collected by `collect_inference`.
    Stage,
    /// A stage-boundary routing choice, collected by
    /// `collect_transition_choice`.
    Routing,
}

impl CallLane {
    /// The lane's name, for the supervisor's report of a lost trip.
    fn name(self) -> &'static str {
        match self {
            CallLane::Stage => "inference",
            CallLane::Routing => "transition-choice",
        }
    }

    /// The channel the lane's collect system reads.
    fn outcomes(
        self,
        stage: &InferenceStage,
    ) -> tokio::sync::mpsc::UnboundedSender<InferenceOutcome> {
        match self {
            CallLane::Stage => stage.outcomes.clone(),
            CallLane::Routing => stage.transition_outcomes.clone(),
        }
    }
}

/// What the world does with a call after one of its trips came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// The call is finished; apply the outcome.
    Done,
    /// Wait for the call's next trip, which is due then.
    Waiting(Instant),
}

/// One model call in progress for an agent, from the first trip until the
/// world applies what it came to.
#[derive(Component)]
pub(crate) struct InferenceCall {
    /// The provider being asked.
    provider: Arc<dyn leviath_providers::Provider>,
    /// The request as assembled. Shared with each trip, which hydrates a copy.
    request: Arc<InferenceRequest>,
    /// The pool slot the call holds until it settles.
    _permit: InferencePermit,
    /// The window's correction, for the first trip's guard.
    calibration: Option<crate::pipeline::PromptCalibration>,
    /// Stream the answer.
    stream: bool,
    /// How stored parts reach the model.
    hydration: Option<JobHydration>,
    /// Where each trip is journaled.
    journal: Option<AttemptJournal>,
    /// The schedule a failed trip is retried on.
    policy: RetryPolicy,
    /// Where the schedule stands.
    clock: RetryClock,
    /// Which lane the call reports to.
    lane: CallLane,
    /// Trips made so far, for the journal: a renewal is a trip the retry
    /// budget does not count.
    made: u32,
    /// The wait before the trip now out, for its record.
    waited: Duration,
    /// Whether a lost file was already uploaded again; that is done once.
    renewed_files: bool,
    /// Whether the next trip uploads the request's files again.
    renew_next: bool,
}

impl InferenceCall {
    /// Hold `job` as a call on `lane`, retried under `policy`. Returns the
    /// call with the refusal the job carried, which only the first trip
    /// reports.
    fn new(job: InferenceJob, policy: RetryPolicy, lane: CallLane) -> (Self, Option<String>) {
        let InferenceJob {
            entity: _,
            refused,
            provider,
            request,
            permit,
            calibration,
            stream,
            hydration,
            journal,
        } = job;
        let call = Self {
            provider,
            request: Arc::new(request),
            _permit: permit,
            calibration,
            stream,
            hydration,
            journal,
            clock: RetryClock::start(&policy),
            policy,
            lane,
            made: 0,
            waited: Duration::ZERO,
            renewed_files: false,
            renew_next: false,
        };
        (call, refused)
    }

    /// Whether the call is waiting out a backoff, for a test that watches it
    /// wait.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> bool {
        self.clock.due_at().is_some()
    }

    /// The next trip for `entity`.
    fn attempt(
        &self,
        entity: Entity,
        refused: Option<String>,
        renew_files: bool,
    ) -> InferenceAttempt {
        InferenceAttempt {
            entity,
            refused,
            provider: self.provider.clone(),
            request: self.request.clone(),
            calibration: self.calibration,
            guard: self.made == 0 && !renew_files,
            stream: self.stream,
            hydration: self.hydration.clone(),
            renew_files,
            plan: self.journal.as_ref().map(|j| j.model_input.clone()),
            deadline: self.clock.deadline(),
            job_timeout: self.policy.job_timeout,
        }
    }

    /// Spawn the next trip, supervised, and return its cancel handle.
    fn send(
        &self,
        stage: &InferenceStage,
        entity: Entity,
        refused: Option<String>,
        renew_files: bool,
    ) -> crate::cancel::CancelToken {
        let cancel = crate::cancel::CancelToken::new();
        let outcomes = self.lane.outcomes(stage);
        // Supervised: the agent waits on this lane's marker, which the driver
        // reads as "busy". A trip that died without reporting would leave it
        // waiting on a completion that can no longer come, so the supervisor
        // reports one in its place.
        let lost_outcomes = outcomes.clone();
        let lost_wake = stage.wake.clone();
        crate::lane_supervisor::spawn_supervised(
            &stage.runtime,
            self.lane.name(),
            run_attempt(
                self.attempt(entity, refused, renew_files),
                outcomes,
                stage.wake.clone(),
                cancel.clone(),
            ),
            move |message| {
                let _ = lost_outcomes.send(InferenceOutcome {
                    entity,
                    result: Err(ProviderError::Other(message)),
                    attempt_id: String::new(),
                    // The trip never got to measure itself.
                    latency: Duration::ZERO,
                    // ...and never reached a provider, so it billed nothing
                    // and needs no rates.
                    pricing: None,
                    attempt: None,
                });
                lost_wake.notify_one();
            },
        );
        cancel
    }

    /// Settle what one trip came back with: journal it, and say whether the
    /// call is finished or waits for another trip. A finished call's outcome
    /// carries the whole call's latency, retries and backoff included.
    pub(crate) fn settle(&mut self, outcome: &mut InferenceOutcome) -> Next {
        let next = match (outcome.attempt.take(), &outcome.result) {
            // No trip was made: a refusal, a guard, a deadline or a lost task.
            // None of those is anything a retry would change.
            (None, _) => Next::Done,
            (Some(report), Ok(response)) => {
                self.record(&report, Ending::Answered(&response.finish_reason));
                Next::Done
            }
            (Some(report), Err(e)) => self.after_failure(report, e),
        };
        if next == Next::Done {
            outcome.latency = self.clock.elapsed();
        }
        next
    }

    /// Decide on a failed trip, journal it with what was decided, and set the
    /// due time of the next one.
    fn after_failure(
        &mut self,
        report: crate::inference_bridge::AttemptReport,
        error: &ProviderError,
    ) -> Next {
        // A file the request named is gone (expired, deleted, or held by
        // another account): upload again and retry once, at once.
        if !self.renewed_files
            && report.named_files
            && self.hydration.is_some()
            && leviath_providers::files::names_a_missing_file(error)
        {
            self.record(&report, Ending::Failed(failed(error, Retry::RenewedFiles)));
            self.renewed_files = true;
            self.renew_next = true;
            // Taken at once, so the next attempt's record says it waited for
            // nothing.
            self.waited = Duration::ZERO;
            return Next::Waiting(self.clock.wait(Duration::ZERO));
        }
        match self.clock.after_failure(&self.policy, error) {
            Some(delay) => {
                self.record(&report, Ending::Failed(failed(error, Retry::SameModel)));
                self.waited = delay;
                Next::Waiting(self.clock.wait(delay))
            }
            None => {
                self.record(&report, Ending::Failed(failed(error, Retry::Reported)));
                Next::Done
            }
        }
    }

    /// Journal one trip, when the call keeps a journal.
    fn record(&mut self, report: &crate::inference_bridge::AttemptReport, ending: Ending<'_>) {
        self.made += 1;
        if let (Some(journal), Some(model_input)) = (&self.journal, report.model_input.clone()) {
            journal.record(
                &report.id,
                self.made,
                ending,
                report.took,
                self.waited,
                model_input,
            );
        }
    }
}

/// Start `job` as a call on `lane`: send its first trip and return the call
/// to keep on the agent, with the trip's cancel handle.
pub(crate) fn start_call(
    stage: &InferenceStage,
    job: InferenceJob,
    policy: RetryPolicy,
    lane: CallLane,
) -> (InferenceCall, crate::cancel::CancelToken) {
    let entity = job.entity;
    let (call, refused) = InferenceCall::new(job, policy, lane);
    let cancel = call.send(stage, entity, refused, false);
    (call, cancel)
}

/// After a call's trip came back to `entity`: settle it, and when it waits
/// for another trip, wake the tick loop when that one is due. Returns whether
/// the call is finished; a finished call is taken off the agent, releasing its
/// permit.
pub(crate) fn settle_call(
    call: Option<Mut<InferenceCall>>,
    outcome: &mut InferenceOutcome,
    stage: Option<&InferenceStage>,
    commands: &mut Commands,
) -> Next {
    // A replayed outcome (a pause held it) or one from a world that kept no
    // call: nothing to decide, apply it. A call only exists where a lane
    // started it, and a lane is a stage.
    let (Some(mut call), Some(stage)) = (call, stage) else {
        return Next::Done;
    };
    let next = call.settle(outcome);
    match next {
        Next::Done => {
            commands.entity(outcome.entity).remove::<InferenceCall>();
        }
        Next::Waiting(due) => wake_at(&stage.runtime, &stage.wake, due),
    }
    next
}

/// What a collect system needs to settle an outcome against its agent's call:
/// the calls, and the lane they wake the loop through.
#[derive(bevy_ecs::system::SystemParam)]
pub(crate) struct CallParams<'w, 's> {
    /// Every agent's call in progress.
    calls: Query<'w, 's, &'static mut InferenceCall>,
    /// The inference lane; absent in a world assembled by hand for a collect
    /// test, which then holds no calls.
    stage: Option<Res<'w, InferenceStage>>,
}

impl CallParams<'_, '_> {
    /// [`settle_call`] for `outcome`, against its agent's call.
    pub(crate) fn settle(
        &mut self,
        outcome: &mut InferenceOutcome,
        commands: &mut Commands,
    ) -> Next {
        settle_call(
            self.calls.get_mut(outcome.entity).ok(),
            outcome,
            self.stage.as_deref(),
            commands,
        )
    }
}

/// What `fire_due_calls` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type DueCallQuery = (
    Entity,
    &'static crate::components::AgentState,
    &'static mut InferenceCall,
    Option<&'static InFlightWork>,
);

/// Send each call's next trip once its backoff is over, and let go of the
/// calls whose agents have ended.
///
/// A call whose whole allowance ran out while it waited reports that as its
/// outcome, on its own lane, without another trip. A paused agent's call
/// keeps going: pause lets the step in flight finish, and a retry is part of
/// that step.
pub(crate) fn fire_due_calls(
    mut calls: Query<DueCallQuery>,
    stage: Res<InferenceStage>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let now = Instant::now();
    for (entity, state, mut call, in_flight) in calls.iter_mut() {
        crate::tick_scope::enter(entity);
        if is_terminal_status(&state.status) {
            // Ended mid-call: the permit goes back now, whatever the trip.
            commands.entity(entity).remove::<InferenceCall>();
            continue;
        }
        match call.clock.take_due(now) {
            None => {}
            Some(Due::Expired) => {
                let _ = call.lane.outcomes(&stage).send(InferenceOutcome {
                    entity,
                    result: Err(job_timed_out(call.policy.job_timeout)),
                    attempt_id: String::new(),
                    latency: Duration::ZERO,
                    pricing: None,
                    attempt: None,
                });
                stage.wake.notify_one();
            }
            Some(Due::Send) => {
                let renew = std::mem::take(&mut call.renew_next);
                let cancel = call.send(&stage, entity, None, renew);
                track_in_flight(&mut commands, entity, in_flight, cancel);
            }
        }
    }
}

/// Let go of every model call the world holds, for a world shutting down. A
/// call held in the world keeps its journal's sender, and its pool slot, for
/// as long as it waits on a backoff; letting the calls go is what lets both go.
pub(crate) fn drop_held_calls(world: &mut World) {
    drop_every::<InferenceCall>(world);
    drop_every::<crate::title_bridge::TitleCall>(world);
}

/// Take `C` off every entity that has one.
fn drop_every<C: Component>(world: &mut World) {
    let holders: Vec<Entity> = world
        .query_filtered::<Entity, With<C>>()
        .iter(world)
        .collect();
    for entity in holders {
        world.entity_mut(entity).remove::<C>();
    }
}

#[cfg(test)]
#[path = "inference_call_tests.rs"]
pub(crate) mod tests;
