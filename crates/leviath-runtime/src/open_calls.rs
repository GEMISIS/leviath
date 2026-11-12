//! Accounting for every model call a run sends, however the call ends.
//!
//! A call that answers says what it used. One that fails, or that the run
//! stops waiting for, says nothing, and may have been billed all the same. So
//! each trip to a provider accounts for itself, through the [`Sent`] it is
//! made under:
//!
//! - the moment its request is handed to the provider it is listed in the
//!   run's [`OpenCalls`], with the provider's own id for it as soon as that
//!   arrives, and the run's file records the list;
//! - when it ends, by an answer, a failure, a deadline or a cancel, it
//!   reports one [`CallSpend`]: what the call used and cost, worked out by
//!   [`leviath_providers::settle`] where the call did not say;
//! - the world records each [`CallSpend`] against the run
//!   ([`collect_call_spend`]) and takes the call off the list. A report for
//!   a run that has left memory is written to that run's file by the
//!   persistence lane, which takes the call off the list the file holds.
//!
//! A finished run stays in memory while a call of its own is in flight or
//! finding out what it cost ([`OpenCalls::owed`]), so its report has the run
//! to land on.
//!
//! A call still listed when a run is read back was out when the daemon
//! stopped, or when its trip died. It is settled then, as a call of its own
//! ([`settle_orphaned`] for a run brought back, [`settle_recorded`] for one
//! that finished and stays on disk): the turn the run sends again is a
//! second call. Until it is settled, the run's cost is not exact: whoever
//! reads the run counts a listed call as one whose cost is not known yet.
//!
//! A call whose request never left this machine is never listed and reports
//! nothing: it cost nothing and is not a call.
//!
//! A provider that takes a job and is polled bills the job, and one job is
//! one cost. A call that stops waiting while the run still holds its job
//! reports nothing and stays listed, as the account of that job
//! ([`OpenCall::carries`]). The attempt that picks the job up and reports is
//! that account, and the call that stood for it comes off the list. A job
//! the run gives up on (another model answered in its place, the run ended,
//! the run was continued) is recorded once, under the call that stood for
//! it, as a call that was cut off ([`give_up_jobs`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bevy_ecs::prelude::*;
use leviath_providers::settle::{self, Ended, Unanswered};
use leviath_providers::{CallCost, CallTrace, Provider, ProviderError, TokenUsage};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::persistence::{RunMetadata, TokenTotals};
use crate::runfile::record::InferenceKind;
use crate::state::journal::CallEnding;

/// How long a finished run stays in memory for one call that is finding out
/// what it cost, from the moment that call began to, in seconds. Longer than
/// [`settle::LOOKUP_PATIENCE`], which bounds the lookup itself, so a call
/// reports while its run is still here; one that somehow does not is written
/// to the run's file when it does.
pub const SETTLE_HOLD_SECS: i64 = 30;

/// One model call that has been sent and is not yet accounted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenCall {
    /// The attempt's id, minted before the request went out.
    pub attempt: String,
    /// Which kind of call it is.
    pub kind: InferenceKind,
    /// The stage it was made for; empty for a call no stage owns.
    pub stage: String,
    /// The visit of that stage it was made in, by its id.
    pub visit: String,
    /// The iteration it was made in.
    pub iteration: usize,
    /// The provider it was sent to, as the run's configuration names it.
    pub provider: String,
    /// The model it asked for.
    pub model: String,
    /// The provider's own id for it, once it has given one.
    pub provider_call_id: Option<String>,
    /// When it was sent, in unix seconds.
    pub opened_at: i64,
    /// When it stopped waiting for an answer and began finding out what it
    /// cost, in unix seconds. `None` while its trip still waits on the
    /// provider. Not part of what a run's file lists.
    pub settling_since: Option<i64>,
    /// The remote job the call submitted or picked up, for a provider that
    /// takes a job and is polled. A run's file lists it as the call's id.
    pub job: Option<String>,
    /// Whether the call's trip is over and the call stays listed as the
    /// account of its job, which the provider goes on running.
    pub carries: bool,
}

impl OpenCall {
    /// Whether the call's job is one of `jobs`.
    fn job_among<'a>(&self, mut jobs: impl Iterator<Item = &'a String>) -> bool {
        jobs.any(|job| Some(job.as_str()) == self.job.as_deref())
    }
}

/// The calls a run gives up waiting on the jobs of, and the jobs a call of
/// its is still on.
pub(crate) struct GivenUp {
    /// Each call that stood for a job the run no longer waits on.
    pub calls: Vec<OpenCall>,
    /// The jobs a call is still on: one in flight, or one whose own trip is
    /// working out what it cost.
    pub busy: Vec<String>,
}

/// The calls one run has out, shared between the trips that make them and
/// the run that records them. Every run placed from a spec carries one.
#[derive(Component, Clone, Default)]
pub(crate) struct OpenCalls(Arc<Log>);

#[derive(Default)]
struct Log {
    /// The run these are the calls of. Empty for a run with no file.
    run_id: String,
    calls: Mutex<Vec<OpenCall>>,
    /// Bumped on every change, so the run can tell it has something new to
    /// record without comparing the whole list.
    version: AtomicU64,
    /// Woken on every change, so the run records a call at once.
    wake: Mutex<Option<Arc<Notify>>>,
}

impl std::fmt::Debug for OpenCalls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OpenCalls").field(&self.calls()).finish()
    }
}

impl OpenCalls {
    /// The calls of the run `run_id`, holding `calls`.
    pub(crate) fn new(run_id: &str, calls: Vec<OpenCall>) -> Self {
        Self(Arc::new(Log {
            run_id: run_id.to_string(),
            calls: Mutex::new(calls),
            ..Log::default()
        }))
    }

    /// The calls of the run `run_id`, holding the ones its state lists. A
    /// call a state lists has no trip waiting on it any more: it is one that
    /// is being settled, from `now`, or, where it names one of `jobs`, the
    /// jobs the state holds, the account of that job.
    pub(crate) fn for_run(
        run_id: &str,
        calls: &[crate::state::OpenCallState],
        jobs: &Jobs,
        now: i64,
    ) -> Self {
        Self::new(run_id, read_back(calls, jobs, now))
    }

    /// The run these are the calls of. Empty for a run with no file.
    pub(crate) fn run_id(&self) -> &str {
        &self.0.run_id
    }

    /// The calls it holds, as a run's state lists them. A model name no id
    /// can hold is spelled the nearest way one can, as the record of what
    /// the call cost spells it, so every call that is out is listed.
    pub(crate) fn to_state(&self) -> Vec<crate::state::OpenCallState> {
        use crate::spec::names::StageName;
        self.calls()
            .into_iter()
            .map(|call| crate::state::OpenCallState {
                attempt: call.attempt,
                kind: call.kind.into(),
                stage: StageName::new(call.stage).ok(),
                visit: call.visit,
                iteration: u32::try_from(call.iteration).unwrap_or(u32::MAX),
                model: crate::runfile::events::billed_model(&call.provider, &call.model),
                // A job names its call: that is how the call is known to
                // stand for the job when the list is read back.
                provider_call_id: call.job.or(call.provider_call_id),
                opened_at: call.opened_at,
            })
            .collect()
    }

    /// The calls it holds, oldest first.
    pub(crate) fn calls(&self) -> Vec<OpenCall> {
        self.held().clone()
    }

    /// How many calls it holds.
    pub(crate) fn count(&self) -> usize {
        self.held().len()
    }

    /// How many times it has changed.
    pub(crate) fn version(&self) -> u64 {
        self.0.version.load(Ordering::Acquire)
    }

    /// Wake `wake` whenever it changes.
    pub(crate) fn wake_with(&self, wake: Arc<Notify>) {
        *leviath_core::sync::lock(&self.0.wake) = Some(wake);
    }

    /// List `call` as sent.
    fn open(&self, call: OpenCall) {
        self.held().push(call);
        self.changed();
    }

    /// Record the provider's own id for the call made as `attempt`.
    fn note_id(&self, attempt: &str, id: &str) {
        let noted = self
            .held()
            .iter_mut()
            .find(|call| call.attempt == attempt)
            .map(|call| call.provider_call_id = Some(id.to_string()))
            .is_some();
        if noted {
            self.changed();
        }
    }

    /// Record `job` as the job the call made as `attempt` is on.
    fn note_job(&self, attempt: &str, job: &str) {
        let noted = self
            .held()
            .iter_mut()
            .find(|call| call.attempt == attempt)
            .map(|call| call.job = Some(job.to_string()))
            .is_some();
        if noted {
            self.changed();
        }
    }

    /// The call made as `attempt` stopped waiting while the run still holds
    /// `job`: it stays listed as the account of that job. Where another call
    /// already stands for the job, this one comes off the list instead: one
    /// job is one cost.
    fn carry(&self, attempt: &str, job: &str) {
        {
            let mut calls = self.held();
            let stood_for = calls
                .iter()
                .any(|c| c.carries && c.attempt != attempt && c.job.as_deref() == Some(job));
            match stood_for {
                true => calls.retain(|call| call.attempt != attempt),
                false => calls
                    .iter_mut()
                    .filter(|call| call.attempt == attempt)
                    .for_each(|call| {
                        call.job = Some(job.to_string());
                        call.carries = true;
                    }),
            }
        }
        self.changed();
    }

    /// Take off the list each call that stood for `job`: the call that
    /// reports now is the job's account.
    fn drop_carriers(&self, job: &str) {
        let before = self.held().len();
        self.held()
            .retain(|call| !(call.carries && call.job.as_deref() == Some(job)));
        if self.held().len() != before {
            self.changed();
        }
    }

    /// The calls that stood for a job the run no longer waits on: every one
    /// when the run has `ended`, and otherwise each whose job is not among
    /// `held`, the jobs the run holds. A job a call is still on is left to
    /// that call. Each call returned stands for its job no longer: it is
    /// settled once, by whoever asked, and stays listed until then as a call
    /// named by the job's id, being settled from `now`.
    pub(crate) fn give_up(&self, ended: bool, held: &Jobs, now: i64) -> GivenUp {
        let mut calls = self.held();
        let busy: Vec<String> = calls
            .iter()
            .filter(|call| !call.carries)
            .filter_map(|call| call.job.clone())
            .collect();
        let given = calls
            .iter_mut()
            .filter(|call| call.carries && !call.job_among(busy.iter()))
            .filter(|call| ended || !call.job_among(held.values()))
            .map(|call| {
                let given = OpenCall {
                    carries: false,
                    ..call.clone()
                };
                call.carries = false;
                call.provider_call_id = call.job.take();
                call.settling_since = Some(now);
                given
            })
            .collect();
        GivenUp { calls: given, busy }
    }

    /// The call made as `attempt` has stopped waiting for an answer and is
    /// finding out what it cost, from `now`. The first time stands.
    fn settling(&self, attempt: &str, now: i64) {
        if let Some(call) = self.held().iter_mut().find(|c| c.attempt == attempt) {
            call.settling_since.get_or_insert(now);
        }
    }

    /// Take the call made as `attempt` off the list: it is accounted for.
    /// False when the list held no such call, which is one that was already
    /// accounted for.
    pub(crate) fn close(&self, attempt: &str) -> bool {
        let before = self.held().len();
        self.held().retain(|call| call.attempt != attempt);
        let closed = self.held().len() != before;
        if closed {
            self.changed();
        }
        closed
    }

    /// Whether the run is still owed an account of a call, at `now`: one of
    /// its calls is in flight, whose trip ends by its own deadline, or began
    /// finding out what it cost less than [`SETTLE_HOLD_SECS`] ago. How old
    /// the run is, and how long ago any other call was out, has no say.
    pub(crate) fn owed(&self, now: i64) -> bool {
        self.held().iter().any(|call| match call.settling_since {
            None => true,
            Some(since) => now < since.saturating_add(SETTLE_HOLD_SECS),
        })
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Vec<OpenCall>> {
        leviath_core::sync::lock(&self.0.calls)
    }

    /// Say it changed: count it, and wake whoever records it.
    fn changed(&self) {
        self.0.version.fetch_add(1, Ordering::AcqRel);
        if let Some(wake) = leviath_core::sync::lock(&self.0.wake).as_ref() {
            wake.notify_one();
        }
    }
}

/// The jobs a run holds: the id of each, by the step that submitted it.
pub(crate) type Jobs = std::collections::BTreeMap<String, String>;

/// A call a run's state lists, as the list of calls out holds it, having
/// begun to settle at `now`. One whose id is a job among `jobs` stands for
/// that job.
fn listed(call: &crate::state::OpenCallState, now: i64, jobs: &Jobs) -> OpenCall {
    let job = call
        .provider_call_id
        .clone()
        .filter(|id| jobs.values().any(|held| held == id));
    OpenCall {
        attempt: call.attempt.clone(),
        kind: call.kind.into(),
        stage: call
            .stage
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
        visit: call.visit.clone(),
        iteration: call.iteration as usize,
        provider: call.model.provider_or_empty().to_string(),
        model: call.model.model.to_string(),
        provider_call_id: call.provider_call_id.clone(),
        opened_at: call.opened_at,
        settling_since: Some(now),
        carries: job.is_some(),
        job,
    }
}

/// The calls a run's state lists, as the list of calls out holds them, with
/// one account for each job. A daemon that died while one call stood for a
/// job and another was in flight picking it up saved both naming the job:
/// the first stands for it, and the other is not a second account.
fn read_back(calls: &[crate::state::OpenCallState], jobs: &Jobs, now: i64) -> Vec<OpenCall> {
    let mut back: Vec<OpenCall> = Vec::with_capacity(calls.len());
    for call in calls.iter().map(|call| listed(call, now, jobs)) {
        let stood_for = call.carries && back.iter().any(|held| held.job == call.job);
        if !stood_for {
            back.push(call);
        }
    }
    back
}

/// What one call that was sent used and cost, reported by the trip that
/// made it when it ended.
#[derive(Debug, Clone)]
pub(crate) struct CallSpend {
    /// The run's entity.
    pub entity: Entity,
    /// The run's id. Empty for a run with no file.
    pub run_id: String,
    /// The attempt's id.
    pub attempt: String,
    /// Which kind of call it was.
    pub kind: InferenceKind,
    /// The stage it was made for; empty for a call no stage owns.
    pub stage: String,
    /// The visit of that stage it was made in, by its id.
    pub visit: String,
    /// The iteration it was made in.
    pub iteration: usize,
    /// The provider, as the run's configuration names it.
    pub provider: String,
    /// The model it asked for.
    pub model: String,
    /// What it used, as far as anything said.
    pub usage: TokenUsage,
    /// What it cost, and how that is known.
    pub cost: CallCost,
    /// How it ended.
    pub ending: CallEnding,
    /// Why the model stopped, for a call that answered, in this build's
    /// word for it (`FinishReason::label`).
    pub finish_reason: Option<String>,
    /// The provider's own id for it, when it gave one.
    pub provider_call_id: Option<String>,
}

/// Where trips report what their calls cost, and the wake that tells the
/// world one has.
#[derive(Clone)]
pub(crate) struct SpendSink {
    /// The channel [`collect_call_spend`] reads.
    pub tx: UnboundedSender<CallSpend>,
    /// The tick loop's wake.
    pub wake: Arc<Notify>,
}

impl SpendSink {
    /// A sink and the receiving end the world keeps.
    pub(crate) fn channel(wake: Arc<Notify>) -> (Self, CallSpendResults) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx, wake }, CallSpendResults(rx))
    }

    /// Report one call, and wake the world to record it. A world that is
    /// gone takes no report: the call stays on its run's list, and is
    /// settled when the run is next read.
    fn report(&self, spend: CallSpend) {
        let _ = self.tx.send(spend);
        self.wake.notify_one();
    }
}

/// The receiving end of the spend channel, as a world resource.
#[derive(Resource)]
pub(crate) struct CallSpendResults(pub UnboundedReceiver<CallSpend>);

/// What a lane's calls for one run are accounted under: the run, the kind of
/// call, where in the run it is made, and where the account goes.
#[derive(Clone)]
pub(crate) struct CallTicket {
    /// The run's entity.
    pub entity: Entity,
    /// Which kind of call the lane makes.
    pub kind: InferenceKind,
    /// The stage the call is made for; empty for a call no stage owns.
    pub stage: String,
    /// The visit of that stage the call is made in, by its id.
    pub visit: String,
    /// The iteration the call is made in.
    pub iteration: usize,
    /// The run's list of calls out.
    pub open: OpenCalls,
    /// Where the account goes.
    pub sink: SpendSink,
}

impl CallTicket {
    /// A ticket for the calls `entity`'s run makes on one lane: `kind`, in
    /// `stage` (the visit of it named `visit`) at `iteration`, listed in
    /// `open` (the run's own list, or one of its own for a run that carries
    /// none) and reported through the lane's sink.
    pub(crate) fn new(
        lane: &crate::pipeline::InferenceStage,
        entity: Entity,
        open: Option<&OpenCalls>,
        kind: InferenceKind,
        (stage, visit): (&str, &str),
        iteration: usize,
    ) -> Self {
        let open = open.cloned().unwrap_or_default();
        // A call going out, or the provider naming it, is recorded at once.
        open.wake_with(lane.wake.clone());
        Self {
            entity,
            kind,
            stage: stage.to_string(),
            visit: visit.to_string(),
            iteration,
            open,
            sink: lane.spend.clone(),
        }
    }

    /// The trip that made this ticket's calls died without reporting: each
    /// call of its that is still listed is settled here, on `runtime`, as a
    /// call that was cut off, asked after at `provider` where it gave an id.
    pub(crate) fn settle_lost(
        &self,
        runtime: &tokio::runtime::Handle,
        provider: Option<Arc<dyn Provider>>,
    ) {
        let now = chrono::Utc::now().timestamp();
        let mine = |call: &OpenCall| {
            call.kind == self.kind
                && call.stage == self.stage
                && call.visit == self.visit
                && call.iteration == self.iteration
                && call.settling_since.is_none()
        };
        for call in self.open.calls().into_iter().filter(mine) {
            self.open.settling(&call.attempt, now);
            runtime.spawn(settle_orphan(
                self.entity,
                self.open.run_id().to_string(),
                call,
                provider.clone(),
                self.sink.clone(),
            ));
        }
    }
}

/// What a lane keeps to settle the calls of a trip that dies without
/// reporting: the ticket its calls were made under, the provider they went
/// to, and the runtime to settle them on.
pub(crate) struct LostCall {
    /// The ticket, or `None` in a world with nowhere to account to.
    pub ticket: Option<CallTicket>,
    /// The provider the trip called.
    pub provider: Arc<dyn Provider>,
    /// Where the settling runs.
    pub runtime: tokio::runtime::Handle,
}

impl LostCall {
    /// The trip died: settle what it left on its run's list of calls out.
    pub(crate) fn settle(self) {
        if let Some(ticket) = &self.ticket {
            ticket.settle_lost(&self.runtime, Some(self.provider));
        }
    }
}

/// One call to a provider, accounted for however it ends.
///
/// Made before the call, told when the request is about to go out
/// ([`Self::begin`]), wrapped round the call itself ([`Self::scope`]), and
/// told how it ended: [`Self::answered`], [`Self::failed`] or
/// [`Self::cut_off`]. With no ticket (a world with nowhere to account to)
/// every one of those does nothing.
pub(crate) struct Sent<'a> {
    ticket: Option<&'a CallTicket>,
    provider: &'a dyn Provider,
    /// The provider's name as the run's configuration has it.
    provider_name: &'a str,
    model: &'a str,
    trace: CallTrace,
    /// The jobs the run's call has submitted, for a provider that takes a
    /// job and is polled.
    jobs: Option<leviath_providers::jobs::JobLog>,
    /// The attempt's id, once the request is about to go out.
    attempt: Mutex<Option<String>>,
    /// Whether the account has been given: a call ends once.
    done: AtomicBool,
}

impl<'a> Sent<'a> {
    /// A call about to be made to `model` on `provider`.
    pub(crate) fn new(
        ticket: Option<&'a CallTicket>,
        provider: &'a dyn Provider,
        provider_name: &'a str,
        model: &'a str,
    ) -> Self {
        Self {
            ticket,
            provider,
            provider_name,
            model,
            trace: CallTrace::new(),
            jobs: None,
            attempt: Mutex::new(None),
            done: AtomicBool::new(false),
        }
    }

    /// The same call, made under `jobs`, the run's log of the jobs its call
    /// has submitted.
    pub(crate) fn with_jobs(mut self, jobs: Option<&leviath_providers::jobs::JobLog>) -> Self {
        self.jobs = jobs.cloned();
        self
    }

    /// The request is about to be handed to the provider, as `attempt`: from
    /// here the call is one the provider may bill, so it goes on the run's
    /// list, and the provider's id for it joins it there when it arrives.
    pub(crate) fn begin(&self, attempt: &str) {
        *leviath_core::sync::lock(&self.attempt) = Some(attempt.to_string());
        let Some(ticket) = self.ticket else {
            return;
        };
        ticket.open.open(OpenCall {
            attempt: attempt.to_string(),
            kind: ticket.kind,
            stage: ticket.stage.clone(),
            visit: ticket.visit.clone(),
            iteration: ticket.iteration,
            provider: self.provider_name.to_string(),
            model: self.model.to_string(),
            provider_call_id: None,
            opened_at: chrono::Utc::now().timestamp(),
            settling_since: None,
            job: None,
            carries: false,
        });
        let (open, made_as) = (ticket.open.clone(), attempt.to_string());
        self.trace.on_call_id(move |id| open.note_id(&made_as, id));
        let (open, made_as) = (ticket.open.clone(), attempt.to_string());
        self.trace.on_job(move |job| open.note_job(&made_as, job));
    }

    /// Run `call`, the provider call itself, where what it learns about
    /// itself outlives it.
    pub(crate) fn scope<F: std::future::Future>(
        &self,
        call: F,
    ) -> impl std::future::Future<Output = F::Output> {
        self.trace.scope(call)
    }

    /// The call answered with `usage`, stopping for `finish`: report it,
    /// priced by the reply's own figure or else by the answering model's
    /// rates, which are returned for whoever else prices the same reply.
    pub(crate) fn answered(
        &self,
        usage: &TokenUsage,
        finish: &leviath_providers::FinishReason,
    ) -> Option<leviath_providers::ModelPricing> {
        let (cost, pricing) =
            settle::answered(self.provider, self.model, usage, &self.trace.seen());
        self.report(
            usage.clone(),
            cost,
            CallEnding::Answered,
            Some(finish.label().to_string()),
        );
        pricing
    }

    /// The call failed with `error`: work out what it cost, which may take a
    /// short lookup, and report it. A call that was never sent reports
    /// nothing.
    pub(crate) async fn failed(&self, error: &ProviderError) {
        self.unanswered(Ended::Failed(error), CallEnding::Failed)
            .await;
    }

    /// The run stopped waiting for the call: work out what it cost and
    /// report it. A call cut off before its request was handed over reports
    /// nothing.
    pub(crate) async fn cut_off(&self) {
        self.unanswered(Ended::CutOff, CallEnding::CutOff).await;
    }

    async fn unanswered(&self, ended: Ended<'_>, ending: CallEnding) {
        // Nowhere to account to, a request that was never handed over, or a
        // call that has already ended: nothing to work out.
        let attempt = leviath_core::sync::lock(&self.attempt).clone();
        let (Some(ticket), Some(attempt), false) =
            (self.ticket, attempt, self.done.load(Ordering::Acquire))
        else {
            return;
        };
        // From here the run is owed this call's account, for a bounded while.
        ticket
            .open
            .settling(&attempt, chrono::Utc::now().timestamp());
        // A job the call submitted, or picked up, that the run still holds
        // carries the cost: the provider goes on running it, and the call
        // made again picks it up and records what it came to. This attempt
        // reports nothing, and stays listed as the account of the job until
        // that happens or the run gives the job up.
        if let Some(job) = self.held_job() {
            self.done.store(true, Ordering::Release);
            ticket.open.carry(&attempt, &job);
            return;
        }
        let settled = settle::unanswered(
            self.provider,
            self.model,
            ended,
            &self.trace.seen(),
            settle::LOOKUP_PATIENCE,
            settle::LOOKUP_INTERVAL,
        )
        .await;
        match settled {
            Some(Unanswered { usage, cost, .. }) => self.report(usage, cost, ending, None),
            // Never sent: not a call. It comes off the list unreported.
            None => {
                self.done.store(true, Ordering::Release);
                ticket.open.close(&attempt);
            }
        }
    }

    /// The job this call submitted or picked up, when it is still on the
    /// run's list of jobs: one the provider failed is taken off it, and one
    /// a different call left there is not this call's.
    fn held_job(&self) -> Option<String> {
        let held = self.jobs.as_ref()?.jobs();
        self.trace
            .seen()
            .job
            .filter(|job| held.values().any(|held| held == job))
    }

    /// Send the call's account to the world, once.
    fn report(
        &self,
        usage: TokenUsage,
        cost: CallCost,
        ending: CallEnding,
        finish_reason: Option<String>,
    ) {
        let attempt = leviath_core::sync::lock(&self.attempt).clone();
        let (Some(ticket), Some(attempt)) = (self.ticket, attempt) else {
            return;
        };
        if self.done.swap(true, Ordering::AcqRel) {
            return;
        }
        let seen = self.trace.seen();
        ticket.sink.report(CallSpend {
            entity: ticket.entity,
            run_id: ticket.open.run_id().to_string(),
            attempt,
            kind: ticket.kind,
            stage: ticket.stage.clone(),
            visit: ticket.visit.clone(),
            iteration: ticket.iteration,
            provider: self.provider_name.to_string(),
            model: self.model.to_string(),
            usage,
            cost,
            ending,
            finish_reason,
            // A job's id names the call where the provider gave no other.
            provider_call_id: seen.call_id.or(seen.job.clone()),
        });
        // This is the account of the job the call ran: a call that stood
        // for the same job, from an attempt that stopped waiting on it, is
        // not a second one.
        if let Some(job) = &seen.job {
            ticket.open.drop_carriers(job);
        }
    }
}

/// What [`collect_call_spend`] reads of the run a call belongs to.
type SpendQuery = (
    Option<&'static RunMetadata>,
    Option<&'static mut TokenTotals>,
    Option<&'static mut crate::pipeline::StageLedger>,
    Option<&'static OpenCalls>,
);

/// Record every call a trip has reported: fold it into its run's totals and
/// stage ledger, journal it, and take it off the run's list of calls out.
///
/// Whatever the run is doing by now. A call is billed whether the run that
/// made it is still working, paused, finished or cancelled, so nothing here
/// looks at where the run stands. A call whose run is not in memory (it was
/// unloaded, or the call is one a finished run's file still listed) goes to
/// the persistence lane, which writes it to that run's file.
///
/// A call is recorded once. One a run's list no longer holds was accounted
/// for already, by another trip that settled the same call, and is dropped.
/// One written to a run's file is remembered, for the run that was read
/// before the write and is placed after it.
pub(crate) fn collect_call_spend(
    mut results: ResMut<CallSpendResults>,
    mut runs: Query<SpendQuery>,
    ids: Query<(Entity, &'static RunMetadata)>,
    persist: Option<Res<crate::pipeline::JournalSender>>,
    lane: Option<Res<crate::pipeline::PersistenceStage>>,
    mut late: Option<ResMut<LateSettled>>,
) {
    crate::tick_scope::clear();
    while let Ok(spend) = results.0.try_recv() {
        // The run the call belongs to: the one it was made on, or, when
        // that is gone, the same run brought back since.
        let entity = match runs.contains(spend.entity) {
            true => Some(spend.entity),
            false => ids
                .iter()
                .find(|(_, md)| !spend.run_id.is_empty() && md.run_id == spend.run_id)
                .map(|(entity, _)| entity),
        };
        let Some(entity) = entity else {
            write_late(lane.as_deref(), late.as_deref_mut(), &spend);
            continue;
        };
        let (metadata, mut totals, mut ledger, open) = runs
            .get_mut(entity)
            .expect("a run found in the world is still in it");
        if open.is_some_and(|open| !open.close(&spend.attempt)) {
            continue;
        }
        crate::tick_scope::enter(entity);
        crate::inference_usage::record_call(
            totals.as_deref_mut(),
            ledger.as_deref_mut(),
            persist.as_deref(),
            metadata,
            &usage_of(&spend),
        );
    }
}

/// How long a call written to the file of a run that was out of memory is
/// remembered for, in seconds: longer than any read of a run file takes to
/// be placed.
const LATE_KEPT_SECS: i64 = 600;

/// The calls written to the files of runs that were out of memory, by run.
///
/// A run is read off disk first and placed in the world a moment later. A
/// call written to its file in between is listed as out in what was read
/// and settled in the file. Left alone, the run would settle it a second
/// time and record it twice. So each such write is remembered here, and a
/// run placed with the call still on its list takes what the call cost
/// from here instead ([`take_late`]).
#[derive(Resource, Default)]
pub(crate) struct LateSettled(std::collections::HashMap<String, Vec<(i64, CallSpend)>>);

impl LateSettled {
    /// Remember `spend`, written at `at`, and forget what is too old for
    /// any run still being placed to have missed.
    fn wrote(&mut self, spend: &CallSpend, at: i64) {
        self.0.retain(|_, calls| {
            calls.retain(|(written, _)| at < written.saturating_add(LATE_KEPT_SECS));
            !calls.is_empty()
        });
        self.0
            .entry(spend.run_id.clone())
            .or_default()
            .push((at, spend.clone()));
    }
}

/// Give `entity`'s run, just placed from what its file held when it was
/// read, each call that was written to that file since: a call the run still
/// lists as out is taken off the list and added to its totals and its stage.
/// Nothing is journaled: the file already holds the call's record.
pub(crate) fn take_late(world: &mut World, entity: Entity) {
    let run_id = world
        .get::<OpenCalls>(entity)
        .map(|open| open.run_id().to_string())
        .unwrap_or_default();
    let written = world
        .get_resource_mut::<LateSettled>()
        .and_then(|mut late| late.0.remove(&run_id))
        .unwrap_or_default();
    let mut runs = world.query::<SpendQuery>();
    for (_, spend) in written {
        let (metadata, mut totals, mut ledger, open) = runs
            .get_mut(world, entity)
            .expect("the run just placed is in the world");
        if open.is_some_and(|open| open.close(&spend.attempt)) {
            crate::inference_usage::record_call(
                totals.as_deref_mut(),
                ledger.as_deref_mut(),
                None,
                metadata,
                &usage_of(&spend),
            );
        }
    }
}

/// Hand the persistence lane a call whose run is not in memory, to write to
/// that run's file, and remember it in `late`. A call of a run with no file,
/// or in a world that keeps none, has nowhere to go, and that is said out
/// loud with the figure.
fn write_late(
    lane: Option<&crate::pipeline::PersistenceStage>,
    late: Option<&mut LateSettled>,
    spend: &CallSpend,
) {
    let at = chrono::Utc::now().timestamp();
    let landed = match (lane, spend.run_id.is_empty()) {
        (Some(lane), false) => {
            let record = crate::inference_usage::usage_record(&usage_of(spend), at);
            let mut step = crate::runfile::lane::RunFileStep::events(
                &spend.run_id,
                at,
                crate::runfile::events::journal_events(&record),
            );
            step.settles = vec![spend.attempt.clone()];
            lane.0
                .send(crate::persistence_bridge::PersistMsg::Step(Box::new(step)))
                .is_ok()
        }
        _ => false,
    };
    if let (true, Some(late)) = (landed, late) {
        late.wrote(spend, at);
    }
    if !landed {
        tracing::warn!(
            provider = %spend.provider,
            model = %spend.model,
            cost_usd = ?spend.cost.usd(),
            ending = spend.ending.label(),
            "a model call reported its cost with no run file to write it to"
        );
    }
}

/// `spend` as the record of one call.
fn usage_of(spend: &CallSpend) -> crate::inference_usage::CallUsage<'_> {
    crate::inference_usage::CallUsage {
        kind: spend.kind,
        stage: &spend.stage,
        visit: &spend.visit,
        iteration: spend.iteration,
        provider: &spend.provider,
        model: &spend.model,
        usage: &spend.usage,
        cost: spend.cost,
        ending: spend.ending,
        attempt: &spend.attempt,
        finish_reason: spend.finish_reason.as_deref(),
        provider_call_id: spend.provider_call_id.as_deref(),
    }
}

/// How many calls `entity`'s run lists as out.
pub(crate) fn calls_out(world: &World, entity: Entity) -> usize {
    world.get::<OpenCalls>(entity).map_or(0, OpenCalls::count)
}

/// Whether the calls `entity`'s run has out still owe it an account, at
/// `now`: the host asks before unloading a finished run, so their reports
/// find the run where it was.
pub(crate) fn calls_owed(world: &World, entity: Entity, now: i64) -> bool {
    world
        .get::<OpenCalls>(entity)
        .is_some_and(|open| open.owed(now))
}

/// Settle the calls `entity`'s run had out when it was last recorded: each
/// was sent by a daemon that stopped before the call ended, so nothing is
/// waiting on it any more. Each is asked after at its provider, where the
/// provider gave it an id and keeps a record, and reported as a call that
/// was cut off.
///
/// A call whose provider is no longer configured is reported with no lookup.
/// In a world with no inference lane there is nowhere to report to, and the
/// calls stay listed.
///
/// A call that stands for a job the run still holds is not one of these:
/// the run picks the job up, or gives it up, and it is accounted for then.
pub(crate) fn settle_orphaned(world: &World, entity: Entity) {
    let Some(open) = world.get::<OpenCalls>(entity) else {
        return;
    };
    let orphans = open.calls().into_iter().filter(|c| !c.carries).collect();
    settle_listed(world, entity, open.run_id(), orphans);
}

/// Settle the calls the file of the finished run `run_id` still lists: each
/// was out when a daemon stopped, or when the run was unloaded, and nothing
/// brings a finished run back to settle them. Each is asked after at its
/// provider and written to the run's file, which takes it off the list
/// there. One that stands for a job among `jobs`, the jobs the file holds,
/// is given up with its job: a finished run picks nothing up.
pub fn settle_recorded(
    world: &World,
    run_id: &str,
    calls: &[crate::state::OpenCallState],
    jobs: &Jobs,
) {
    let now = chrono::Utc::now().timestamp();
    settle_listed(
        world,
        Entity::PLACEHOLDER,
        run_id,
        read_back(calls, jobs, now),
    );
}

/// Give up the jobs each run no longer waits on, and record each once, as a
/// call that was cut off.
///
/// A job is given up when the run that holds it has ended, and when the run
/// stopped holding it with nothing picking it up: another model answered in
/// its place, or the run was continued. The provider billed the job when it
/// took it, so the call that stood for it is settled here: at nothing on
/// rates that charge nothing, and unknown otherwise, since a job is not a
/// call a provider keeps a bill for.
pub(crate) fn give_up_jobs(
    runs: Query<(
        Entity,
        &crate::components::AgentState,
        &OpenCalls,
        &crate::inference_call::RemoteJobs,
    )>,
    lane: Option<Res<crate::pipeline::InferenceStage>>,
    registry: Option<Res<crate::pipeline::Providers>>,
) {
    crate::tick_scope::clear();
    let Some(lane) = lane else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    for (entity, state, open, jobs) in &runs {
        let ended = crate::pipeline::is_terminal_status(&state.status);
        let given = open.give_up(ended, &jobs.0.jobs(), now);
        for call in given.calls {
            let provider = registry.as_ref().and_then(|r| r.0.get(&call.provider));
            lane.runtime.spawn(settle_orphan(
                entity,
                open.run_id().to_string(),
                call,
                provider,
                lane.spend.clone(),
            ));
        }
        // A run that has ended waits on no job. One a call is still on is
        // left to that call, which stops waiting with the run.
        if ended {
            jobs.0
                .keep(&|job| given.busy.iter().any(|busy| busy == job));
        }
    }
}

/// Spawn the settling of each of `calls`, the calls of `run_id`, on the
/// world's inference lane.
fn settle_listed(world: &World, entity: Entity, run_id: &str, calls: Vec<OpenCall>) {
    let Some(lane) = world.get_resource::<crate::pipeline::InferenceStage>() else {
        return;
    };
    let registry = world.get_resource::<crate::pipeline::Providers>();
    for call in calls {
        let provider = registry.and_then(|r| r.0.get(&call.provider));
        lane.runtime.spawn(settle_orphan(
            entity,
            run_id.to_string(),
            call,
            provider,
            lane.spend.clone(),
        ));
    }
}

/// Find out what one call with no trip left to end it cost, and report it.
async fn settle_orphan(
    entity: Entity,
    run_id: String,
    call: OpenCall,
    provider: Option<Arc<dyn Provider>>,
    sink: SpendSink,
) {
    // A job's id is never looked up: a job is not a call a provider keeps a
    // bill for.
    let seen = leviath_providers::call_trace::Traced {
        call_id: match call.job {
            Some(_) => None,
            None => call.provider_call_id.clone(),
        },
        job: call.job.clone(),
        ..Default::default()
    };
    let settled = match &provider {
        Some(provider) => settle::unanswered(
            provider.as_ref(),
            &call.model,
            Ended::CutOff,
            &seen,
            settle::LOOKUP_PATIENCE,
            settle::LOOKUP_INTERVAL,
        )
        .await
        .map(|settled| (settled.usage, settled.cost)),
        None => None,
    };
    let (usage, cost) = settled.unwrap_or_else(|| {
        (
            TokenUsage::new(0, 0, 0, 0),
            CallCost::Unknown(leviath_providers::CostUnknown::NoLookup),
        )
    });
    sink.report(CallSpend {
        entity,
        run_id,
        attempt: call.attempt,
        kind: call.kind,
        stage: call.stage,
        visit: call.visit,
        iteration: call.iteration,
        provider: call.provider,
        model: call.model,
        usage,
        cost,
        ending: CallEnding::CutOff,
        finish_reason: None,
        provider_call_id: call.job.or(call.provider_call_id),
    });
}

#[cfg(test)]
#[path = "open_calls_tests.rs"]
pub(crate) mod tests;
