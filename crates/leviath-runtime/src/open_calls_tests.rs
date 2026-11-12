use super::*;
use leviath_providers::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelPricing, ProviderError,
};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

/// A provider whose record of a call a test sets.
pub(crate) struct Shop {
    /// Its rates for every model.
    pub(crate) rates: Option<ModelPricing>,
    /// What its record of a call answers. `None` keeps no record.
    pub(crate) record: Option<TokenUsage>,
    /// How many times its record was asked for.
    pub(crate) asked: AtomicUsize,
    /// How long its record takes to answer.
    pub(crate) slow: Duration,
}

impl Shop {
    pub(crate) fn priced() -> Self {
        Self {
            rates: Some(ModelPricing::flat(1.0, 2.0)),
            record: None,
            asked: AtomicUsize::new(0),
            slow: Duration::ZERO,
        }
    }
}

#[async_trait::async_trait]
impl Provider for Shop {
    async fn infer(
        &self,
        _request: &InferenceRequest,
    ) -> leviath_providers::Result<InferenceResponse> {
        Err(ProviderError::unsent("not called"))
    }
    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        text.len()
    }
    fn max_context_tokens(&self, _model: &str) -> usize {
        0
    }
    fn name(&self) -> &str {
        "shop"
    }
    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        self.rates
    }
    async fn call_usage(
        &self,
        _call_id: &str,
        _model: &str,
    ) -> Option<leviath_providers::Result<TokenUsage>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        // No timer at all for a record that answers at once.
        if !self.slow.is_zero() {
            tokio::time::sleep(self.slow).await;
        }
        self.record.clone().map(Ok)
    }
}

/// A ticket for `entity`, the log its calls are listed in, and the receiving
/// end its reports land on.
pub(crate) fn ticket(
    entity: Entity,
    kind: InferenceKind,
) -> (CallTicket, OpenCalls, CallSpendResults) {
    let wake = Arc::new(Notify::new());
    let (sink, results) = SpendSink::channel(wake);
    let open = OpenCalls::new("run-1", Vec::new());
    let ticket = CallTicket {
        entity,
        kind,
        stage: "plan".to_string(),
        iteration: 3,
        open: open.clone(),
        sink,
        visit: String::new(),
    };
    (ticket, open, results)
}

/// A sink whose reports go nowhere, for a world assembled by hand that
/// reads none of them.
pub(crate) fn sink() -> SpendSink {
    SpendSink::channel(Arc::new(Notify::new())).0
}

fn entity() -> Entity {
    World::new().spawn_empty().id()
}

/// A stage call in flight, made as `attempt`.
fn call(attempt: &str) -> OpenCall {
    OpenCall {
        attempt: attempt.to_string(),
        kind: InferenceKind::Stage,
        stage: "plan".into(),
        iteration: 3,
        provider: "shop".into(),
        model: "m".into(),
        provider_call_id: None,
        opened_at: 7,
        visit: String::new(),
        settling_since: None,
        job: None,
        carries: false,
    }
}

/// No jobs held.
fn no_jobs() -> Jobs {
    Jobs::new()
}

/// The jobs a run holds, each under the step that submitted it.
fn jobs(held: &[(&str, &str)]) -> Jobs {
    held.iter()
        .map(|(step, id)| (step.to_string(), id.to_string()))
        .collect()
}

#[tokio::test]
async fn an_answered_call_is_listed_then_reported_with_its_cost() {
    let shop = Shop::priced();
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    let sent = Sent::new(Some(&ticket), &shop, "shop-as-configured", "m");
    assert!(open.calls().is_empty(), "nothing is out before it is sent");

    sent.begin("att-1");
    let listed = open.calls();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].attempt, "att-1");
    assert_eq!(listed[0].provider, "shop-as-configured");
    assert_eq!((listed[0].stage.as_str(), listed[0].iteration), ("plan", 3));
    assert_eq!(listed[0].provider_call_id, None);
    assert_eq!(listed[0].settling_since, None, "it is in flight");

    // The provider names the call while it is in flight.
    sent.scope(async { leviath_providers::call_trace::note_call_id("gen-9") })
        .await;
    assert_eq!(open.calls()[0].provider_call_id.as_deref(), Some("gen-9"));

    let usage = TokenUsage::new(1_000_000, 0, 0, 1_000_000);
    let pricing = sent.answered(&usage, &leviath_providers::FinishReason::Complete);
    assert_eq!(pricing, shop.rates);
    let spend = results.0.try_recv().expect("one report");
    assert_eq!(spend.cost, CallCost::Computed(3.0));
    assert_eq!(spend.ending, CallEnding::Answered);
    assert_eq!(spend.finish_reason.as_deref(), Some("complete"));
    assert_eq!(spend.attempt, "att-1");
    assert_eq!(spend.run_id, "run-1");
    assert_eq!(spend.provider_call_id.as_deref(), Some("gen-9"));
    assert_eq!((spend.stage.as_str(), spend.iteration), ("plan", 3));

    // A call ends once: nothing more is reported for it.
    sent.answered(&usage, &leviath_providers::FinishReason::Complete);
    sent.cut_off().await;
    sent.failed(&ProviderError::ApiError("HTTP 500".into()))
        .await;
    assert!(results.0.try_recv().is_err());
}

#[tokio::test]
async fn a_failed_call_reports_what_it_cost_and_a_refusal_costs_nothing() {
    let shop = Shop::priced();
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Routing);
    let sent = Sent::new(Some(&ticket), &shop, "shop", "m");
    sent.begin("att-2");
    sent.failed(&ProviderError::RateLimitExceeded {
        retry_after_secs: None,
    })
    .await;
    let spend = results.0.try_recv().expect("reported");
    assert_eq!(spend.cost, CallCost::Computed(0.0));
    assert_eq!(spend.ending, CallEnding::Failed);
    assert_eq!(spend.kind, InferenceKind::Routing);
    // Listed until the world records it, and marked as settling from the
    // moment it stopped waiting for an answer.
    assert!(open.calls()[0].settling_since.is_some());
}

#[tokio::test]
async fn a_call_that_was_never_sent_comes_off_the_list_unreported() {
    let shop = Shop::priced();
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    let sent = Sent::new(Some(&ticket), &shop, "shop", "m");
    sent.begin("att-3");
    assert_eq!(open.calls().len(), 1);
    sent.failed(&ProviderError::unsent("refused on this machine"))
        .await;
    assert!(
        results.0.try_recv().is_err(),
        "it cost nothing and is no call"
    );
    assert!(open.calls().is_empty());
    // And only once.
    let version = open.version();
    sent.failed(&ProviderError::unsent("again")).await;
    assert_eq!(open.version(), version);
}

#[tokio::test]
async fn a_call_cut_off_is_looked_up_where_the_provider_keeps_a_record() {
    let shop = Shop {
        record: Some(TokenUsage::new(14, 0, 0, 9).with_reported_cost(Some(0.000_005))),
        ..Shop::priced()
    };
    let (ticket, _open, mut results) = ticket(entity(), InferenceKind::Stage);
    let sent = Sent::new(Some(&ticket), &shop, "shop", "m");
    sent.begin("att-4");
    sent.scope(async { leviath_providers::call_trace::note_call_id("gen-4") })
        .await;
    sent.cut_off().await;
    let spend = results.0.try_recv().expect("reported");
    assert_eq!(spend.cost, CallCost::Reported(0.000_005));
    assert_eq!(spend.ending, CallEnding::CutOff);
    assert_eq!(spend.usage.prompt_tokens, 14);
    assert_eq!(shop.asked.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_call_cut_off_before_it_was_sent_or_with_no_ticket_reports_nothing() {
    let shop = Shop::priced();
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    // Never begun: a deadline that fired while the request was being built.
    let unsent = Sent::new(Some(&ticket), &shop, "shop", "m");
    unsent.cut_off().await;
    assert!(results.0.try_recv().is_err());
    assert!(open.calls().is_empty());

    // No ticket: a world with nowhere to account to.
    let untracked = Sent::new(None, &shop, "shop", "m");
    untracked.begin("att-5");
    untracked.answered(
        &TokenUsage::new(1, 0, 0, 1),
        &leviath_providers::FinishReason::Complete,
    );
    untracked.cut_off().await;
    untracked
        .failed(&ProviderError::unsent("refused on this machine"))
        .await;
    assert!(results.0.try_recv().is_err());
}

/// A daemon that stops while a call is finding out what it cost drops the
/// lookup where it stands. The call is still on its run's list, with the
/// provider's id, and nothing was reported: the next start settles it.
#[tokio::test(start_paused = true)]
async fn a_lookup_dropped_part_way_leaves_the_call_listed_for_the_next_start() {
    let shop = Shop {
        record: Some(TokenUsage::new(3, 0, 0, 2).with_reported_cost(Some(0.01))),
        slow: Duration::from_secs(5),
        ..Shop::priced()
    };
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    let sent = Sent::new(Some(&ticket), &shop, "shop", "m");
    sent.begin("att-6");
    sent.scope(async { leviath_providers::call_trace::note_call_id("gen-6") })
        .await;
    // The runtime going away drops the trip's future mid-lookup.
    let stopped = tokio::time::timeout(Duration::from_secs(1), sent.cut_off()).await;
    assert!(stopped.is_err(), "the lookup was still out");
    assert!(results.0.try_recv().is_err(), "nothing was reported");
    let listed = open.to_state();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provider_call_id.as_deref(), Some("gen-6"));

    // The next start reads the list back and settles the call.
    let back = OpenCalls::for_run("run-1", &listed, &no_jobs(), 50);
    let (sink, mut next) = SpendSink::channel(Arc::new(Notify::new()));
    let shop: Arc<dyn Provider> = Arc::new(shop);
    let left = back.calls().remove(0);
    settle_orphan(entity(), "run-1".into(), left, Some(shop), sink).await;
    let spend = next.0.try_recv().expect("settled at the next start");
    assert_eq!(spend.cost, CallCost::Reported(0.01));
    assert_eq!(spend.attempt, "att-6");
}

/// A job step whose poll drops with the job still running.
async fn poll_drops(job: &str) -> ProviderError {
    let job = job.to_string();
    leviath_providers::jobs::submit_or_resume(
        "shop/m/video",
        || async move { Ok(job) },
        |_id| async { Err::<(), _>(ProviderError::RequestFailed("connection reset".into())) },
    )
    .await
    .unwrap_err()
}

/// A call whose job the run still holds reports nothing: the provider goes
/// on running the job, and the call made again picks it up and records what
/// it came to. Until then the call stays on the list, as the account of the
/// job, named by the job's id. A job the provider failed, and a job some
/// other call left on the list, do not carry this call's cost.
#[tokio::test]
async fn a_call_whose_job_the_run_still_holds_stands_for_the_job() {
    use leviath_providers::jobs::{JobLog, submit_or_resume};
    let shop = Shop::priced();
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    let log = JobLog::default();

    // The poll drops, so the job stays the run's to pick up.
    let sent = Sent::new(Some(&ticket), &shop, "shop", "m").with_jobs(Some(&log));
    sent.begin("att-7");
    let dropped = sent.scope(log.scope(poll_drops("job-7"))).await;
    assert_eq!(
        open.calls()[0].job.as_deref(),
        Some("job-7"),
        "the call is known to be on the job while it is out"
    );
    assert!(!open.calls()[0].carries);
    sent.failed(&dropped).await;
    assert!(results.0.try_recv().is_err(), "the job carries the cost");
    let stood = open.calls();
    assert_eq!(stood.len(), 1, "and the call stays listed, standing for it");
    assert!(stood[0].carries);
    assert!(stood[0].settling_since.is_some(), "its trip is over");
    assert_eq!(
        open.to_state()[0].provider_call_id.as_deref(),
        Some("job-7"),
        "the run's file names the call by its job"
    );
    assert_eq!(log.jobs().len(), 1, "the job is still the run's");
    // Only once.
    sent.cut_off().await;
    assert!(results.0.try_recv().is_err());
    assert_eq!(open.calls().len(), 1);

    // Made again, it picks the job up and stops waiting a second time: one
    // call stands for one job, so this one comes off the list.
    let second = Sent::new(Some(&ticket), &shop, "shop", "m").with_jobs(Some(&log));
    second.begin("att-7b");
    let dropped = second.scope(log.scope(poll_drops("unused"))).await;
    assert_eq!(open.calls().len(), 2);
    second.failed(&dropped).await;
    assert!(results.0.try_recv().is_err());
    let stood = open.calls();
    assert_eq!(stood.len(), 1);
    assert_eq!(stood[0].attempt, "att-7", "the first one stands");

    // Made again, it picks the job up, and this time the provider says the
    // job failed: it is forgotten, and the call is a call like any other.
    // Its report is the job's account, so the call that stood for the job
    // comes off the list with nothing recorded.
    let again = Sent::new(Some(&ticket), &shop, "shop", "m").with_jobs(Some(&log));
    again.begin("att-8");
    let failed = again
        .scope(log.scope(submit_or_resume(
            "shop/m/video",
            || async { Ok("never submitted".to_string()) },
            |_id| async { Err::<(), _>(ProviderError::Other("the job failed".into())) },
        )))
        .await
        .unwrap_err();
    assert!(log.jobs().is_empty(), "a failed job is forgotten");
    again.failed(&failed).await;
    let spend = results.0.try_recv().expect("reported");
    assert_eq!(
        spend.cost,
        CallCost::Unknown(leviath_providers::CostUnknown::NoLookup)
    );
    assert_eq!(
        spend.provider_call_id.as_deref(),
        Some("job-7"),
        "the job names the call in its record"
    );
    let left = open.calls();
    assert_eq!(left.len(), 1, "one job, one account");
    assert_eq!(left[0].attempt, "att-8");
    assert!(results.0.try_recv().is_err());

    // A job another call left on the list is not this call's.
    let other = JobLog::new([("shop/m/other".to_string(), "job-0".to_string())].into());
    let text = Sent::new(Some(&ticket), &shop, "shop", "m").with_jobs(Some(&other));
    text.begin("att-9");
    text.cut_off().await;
    assert!(results.0.try_recv().is_ok(), "a call of its own");
}

/// A daemon that dies while one call stands for a job and a second is in
/// flight picking the job up saves both naming the job. Read back, one job
/// has one account: the first call stands for it, and the second is not
/// listed. Given up, the job is recorded once, for a run brought back and
/// for a finished one settled where it is.
#[tokio::test]
async fn two_calls_saved_on_one_job_come_back_as_one_account() {
    let open = OpenCalls::new(
        "run-1",
        vec![
            OpenCall {
                job: Some("job-1".into()),
                carries: true,
                settling_since: Some(1),
                ..call("stood")
            },
            call("picking-up"),
            call("text"),
        ],
    );
    open.note_job("picking-up", "job-1");
    let saved = open.to_state();
    assert_eq!(saved[0].provider_call_id.as_deref(), Some("job-1"));
    assert_eq!(saved[1].provider_call_id.as_deref(), Some("job-1"));
    let held = jobs(&[("shop/m/video", "job-1")]);

    let back = OpenCalls::for_run("run-1", &saved, &held, 9);
    let attempts: Vec<String> = back.calls().into_iter().map(|c| c.attempt).collect();
    assert_eq!(attempts, ["stood", "text"], "one job, one account");
    let given = back.give_up(true, &held, 10);
    assert_eq!(given.calls.len(), 1, "given up, it is settled once");
    assert_eq!(given.calls[0].attempt, "stood");

    // The same file, of a run that finished: settled where it is, once.
    let (world, mut results) = lane_world(Shop::priced());
    settle_recorded(&world, "run-1", &saved, &held);
    let mut settled = [reported(&mut results).await, reported(&mut results).await];
    settled.sort_by(|a, b| a.attempt.cmp(&b.attempt));
    assert_eq!(settled[0].attempt, "stood");
    assert_eq!(settled[1].attempt, "text");
    tokio::task::yield_now().await;
    assert!(results.0.try_recv().is_err(), "and no second account");
}

/// The attempt that picks a job up and is answered is the job's account:
/// the call that stood for the job comes off the list, and one cost is
/// recorded for the one job.
#[tokio::test]
async fn the_attempt_that_picks_a_job_up_is_its_one_account() {
    use leviath_providers::jobs::{JobLog, submit_or_resume};
    let shop = Shop::priced();
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    let log = JobLog::default();
    let first = Sent::new(Some(&ticket), &shop, "shop", "m").with_jobs(Some(&log));
    first.begin("att-1");
    let dropped = first.scope(log.scope(poll_drops("job-1"))).await;
    first.failed(&dropped).await;
    assert!(open.calls()[0].carries);

    let again = Sent::new(Some(&ticket), &shop, "shop", "m").with_jobs(Some(&log));
    again.begin("att-2");
    let ran = again
        .scope(log.scope(submit_or_resume(
            "shop/m/video",
            || async { Ok("never submitted".to_string()) },
            |_id| async { Ok(()) },
        )))
        .await
        .expect("the job finished");
    assert_eq!(ran.id, "job-1", "picked up, not submitted again");
    again.answered(
        &TokenUsage::new(0, 0, 0, 0).with_reported_cost(Some(0.4)),
        &leviath_providers::FinishReason::Complete,
    );
    let spend = results.0.try_recv().expect("one report");
    assert_eq!(spend.cost, CallCost::Reported(0.4));
    assert_eq!(spend.attempt, "att-2");
    assert!(results.0.try_recv().is_err(), "and no second one");
    let left = open.calls();
    assert_eq!(left.len(), 1);
    assert_eq!(
        left[0].attempt, "att-2",
        "the call that stood for it is gone"
    );
}

/// A run gives a job up when it has ended, and when it no longer holds the
/// job with nothing picking it up. The call that stood for the job is handed
/// back once, to be settled; a job a call is still on is left to that call.
#[test]
fn a_run_gives_up_the_jobs_it_no_longer_waits_on_once_each() {
    let stood = |attempt: &str, job: &str| OpenCall {
        job: Some(job.to_string()),
        carries: true,
        settling_since: Some(1),
        ..call(attempt)
    };
    let open = OpenCalls::new("run-1", vec![stood("a", "job-a"), stood("b", "job-b")]);
    open.open(call("text"));
    // A call still out on job-b: a retry that picked it up.
    open.open(call("b2"));
    open.note_job("b2", "job-b");
    open.note_job("nobody", "job-z");
    let held = jobs(&[("shop/a/video", "job-a"), ("shop/b/video", "job-b")]);

    // A report for a job no call stands for takes nothing off the list.
    let version = open.version();
    open.drop_carriers("job-z");
    assert_eq!(open.version(), version);

    // Going, and both jobs held: nothing is given up.
    let given = open.give_up(false, &held, 50);
    assert!(given.calls.is_empty());
    assert_eq!(given.busy, ["job-b"]);

    // Another model answered and the run holds no job: job-a is given up,
    // and job-b is left to the call that is on it.
    let version = open.version();
    let given = open.give_up(false, &no_jobs(), 50);
    assert_eq!(given.calls.len(), 1);
    assert_eq!(given.calls[0].attempt, "a");
    assert_eq!(
        open.calls()[0].settling_since,
        Some(50),
        "the run is owed its account from now"
    );
    assert_eq!(given.calls[0].job.as_deref(), Some("job-a"));
    assert!(!given.calls[0].carries);
    assert_eq!(open.version(), version, "the list holds the same calls");
    // Asked again, the same call is not handed back a second time.
    assert!(open.give_up(false, &no_jobs(), 51).calls.is_empty());
    assert!(open.give_up(true, &no_jobs(), 51).calls.is_empty());

    // The call on job-b stops waiting with the run still holding the job,
    // and comes off: one call stands for it already. The run has ended, so
    // the job is given up though it is still held.
    open.carry("b2", "job-b");
    let given = open.give_up(true, &held, 52);
    assert_eq!(given.calls.len(), 1);
    assert_eq!(given.calls[0].attempt, "b");
    assert!(given.busy.is_empty(), "no call is on a job any more");
    assert_eq!(open.count(), 3, "listed until each is recorded");
    assert_eq!(
        open.to_state()[0].provider_call_id.as_deref(),
        Some("job-a"),
        "and named by its job while it is"
    );
}

/// A world with a run in it that holds `held` and lists `calls`, standing
/// as `status`.
fn job_world(
    shop: Shop,
    status: crate::components::AgentStatus,
    calls: Vec<OpenCall>,
    held: &Jobs,
) -> (World, Entity, CallSpendResults) {
    let (mut world, results) = lane_world(shop);
    let entity = world
        .spawn((
            crate::components::AgentState {
                agent_id: "a".to_string(),
                current_visit: String::new(),
                current_stage: "plan".to_string(),
                iteration: 0,
                status,
                spawned_children_ids: vec![],
                pending_wait: None,
                accepts_messages: true,
            },
            OpenCalls::new("run-1", calls),
            crate::inference_call::RemoteJobs(leviath_providers::jobs::JobLog::new(held.clone())),
        ))
        .id();
    (world, entity, results)
}

/// The next report, which a test that expects one does not wait for ever
/// for.
async fn reported(results: &mut CallSpendResults) -> CallSpend {
    tokio::time::timeout(Duration::from_secs(10), results.0.recv())
        .await
        .expect("a call was reported in time")
        .expect("the sink is open")
}

fn give_up(world: &mut World) {
    let mut schedule = bevy_ecs::schedule::Schedule::default();
    schedule.add_systems(give_up_jobs);
    schedule.run(world);
}

/// Another model answered in place of the one whose job the run was waiting
/// on, and the run holds the job no longer. The provider billed it when it
/// took it, so it is recorded: once, as a call that was cut off, under the
/// call that stood for it, with no lookup and no figure.
#[tokio::test]
async fn a_job_another_model_answered_in_place_of_is_recorded_once() {
    use crate::components::AgentStatus;
    let stood = OpenCall {
        job: Some("job-a".into()),
        carries: true,
        settling_since: Some(1),
        iteration: 2,
        ..call("att-a")
    };
    let held = jobs(&[("shop/m/video", "job-a")]);
    let (mut world, entity, mut results) = job_world(
        Shop {
            record: Some(TokenUsage::new(1, 0, 0, 1).with_reported_cost(Some(9.0))),
            ..Shop::priced()
        },
        AgentStatus::Active,
        vec![stood],
        &held,
    );
    // While the run holds the job, nothing is given up.
    give_up(&mut world);
    tokio::task::yield_now().await;
    assert!(results.0.try_recv().is_err());

    // The other model answers, which clears the run's jobs.
    world
        .get::<crate::inference_call::RemoteJobs>(entity)
        .unwrap()
        .0
        .clear();
    give_up(&mut world);
    let spend = reported(&mut results).await;
    assert_eq!(spend.attempt, "att-a");
    assert_eq!(spend.entity, entity);
    assert_eq!(spend.ending, CallEnding::CutOff);
    assert_eq!(
        spend.cost,
        CallCost::Unknown(leviath_providers::CostUnknown::NoLookup),
        "a job is never looked up"
    );
    assert_eq!(spend.provider_call_id.as_deref(), Some("job-a"));
    assert_eq!(spend.iteration, 2);
    // Once.
    give_up(&mut world);
    tokio::task::yield_now().await;
    assert!(results.0.try_recv().is_err());
}

/// A run that ends waits on no job. Each it held is given up and recorded,
/// at nothing where the provider charges nothing, and the run holds none
/// afterwards. One a call is still on is left to that call.
#[tokio::test]
async fn a_run_that_ends_gives_up_every_job_it_held() {
    use crate::components::AgentStatus;
    let stood = OpenCall {
        job: Some("job-a".into()),
        carries: true,
        settling_since: Some(1),
        ..call("att-a")
    };
    let on_it = OpenCall {
        job: Some("job-b".into()),
        ..call("att-b")
    };
    let held = jobs(&[("shop/m/video", "job-a"), ("shop/n/video", "job-b")]);
    let (mut world, entity, mut results) = job_world(
        Shop {
            rates: Some(ModelPricing::flat(0.0, 0.0)),
            ..Shop::priced()
        },
        AgentStatus::Cancelled,
        vec![stood, on_it],
        &held,
    );
    give_up(&mut world);
    let spend = reported(&mut results).await;
    assert_eq!(spend.attempt, "att-a");
    assert_eq!(
        spend.cost,
        CallCost::Computed(0.0),
        "free is a known figure"
    );
    assert!(results.0.try_recv().is_err(), "job-b is its call's to end");
    let left = world
        .get::<crate::inference_call::RemoteJobs>(entity)
        .unwrap()
        .0
        .jobs();
    assert_eq!(left, jobs(&[("shop/n/video", "job-b")]));

    // A world with no lane has nowhere to report to, and gives nothing up.
    let mut bare = World::new();
    give_up(&mut bare);
}

/// A run read back keeps the call that stood for a job it still holds: the
/// run picks the job up, or gives it up, and the call is not asked after as
/// one a stopped daemon left out. A finished run picks nothing up, so there
/// the same call is given up with its job, with no lookup.
#[tokio::test]
async fn a_call_standing_for_a_held_job_is_kept_for_a_run_brought_back() {
    let shop = Shop {
        record: Some(TokenUsage::new(3, 0, 0, 2).with_reported_cost(Some(0.02))),
        ..Shop::priced()
    };
    let (mut world, mut results) = lane_world(shop);
    let listed = OpenCalls::new(
        "run-1",
        vec![
            OpenCall {
                job: Some("job-1".into()),
                carries: true,
                ..call("stood")
            },
            OpenCall {
                provider_call_id: Some("gen-1".into()),
                ..call("cut")
            },
        ],
    )
    .to_state();
    let held = jobs(&[("shop/m/video", "job-1")]);

    let back = OpenCalls::for_run("run-1", &listed, &held, 9);
    let calls = back.calls();
    assert!(calls[0].carries && calls[0].job.as_deref() == Some("job-1"));
    assert!(!calls[1].carries && calls[1].job.is_none());
    let entity = world.spawn(back).id();
    assert_eq!(calls_out(&world, entity), 2);
    let bare = world.spawn_empty().id();
    assert_eq!(calls_out(&world, bare), 0);
    settle_orphaned(&world, entity);
    let spend = reported(&mut results).await;
    assert_eq!(spend.attempt, "cut");
    assert_eq!(spend.cost, CallCost::Reported(0.02));
    tokio::task::yield_now().await;
    assert!(
        results.0.try_recv().is_err(),
        "the job's call is left alone"
    );

    // The same file, of a run that finished.
    settle_recorded(&world, "run-1", &listed, &held);
    let mut settled = [reported(&mut results).await, reported(&mut results).await];
    settled.sort_by(|a, b| a.attempt.cmp(&b.attempt));
    assert_eq!(settled[0].attempt, "cut");
    assert_eq!(settled[1].attempt, "stood");
    assert_eq!(
        settled[1].cost,
        CallCost::Unknown(leviath_providers::CostUnknown::NoLookup)
    );
    assert_eq!(settled[1].provider_call_id.as_deref(), Some("job-1"));
}

#[test]
fn the_list_of_calls_out_changes_version_and_wakes_its_run() {
    let open = OpenCalls::default();
    assert_eq!(open.run_id(), "", "a run with no file");
    let wake = Arc::new(Notify::new());
    open.wake_with(wake.clone());
    let title = |attempt: &str| OpenCall {
        kind: InferenceKind::Title,
        stage: String::new(),
        ..call(attempt)
    };
    open.open(title("a"));
    open.open(title("b"));
    assert_eq!(open.version(), 2);
    open.note_id("b", "resp_1");
    assert_eq!(open.version(), 3);
    // Naming a call that is not out changes nothing.
    open.note_id("zzz", "resp_2");
    open.settling("zzz", 9);
    assert!(!open.close("zzz"));
    assert_eq!(open.version(), 3);
    assert!(open.close("a"));
    assert_eq!(open.version(), 4);
    let left = open.calls();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].provider_call_id.as_deref(), Some("resp_1"));
    assert!(format!("{open:?}").contains("resp_1"));

    // Round trip through the state's form: what a state lists has no trip
    // waiting on it, so it comes back as a call being settled.
    let state = open.to_state();
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].stage, None, "no stage owns a title call");
    let back = OpenCalls::for_run("run-9", &state, &no_jobs(), 40);
    assert_eq!(back.run_id(), "run-9");
    assert_eq!(
        back.calls(),
        vec![OpenCall {
            settling_since: Some(40),
            ..left[0].clone()
        }]
    );

    // A model name no id can hold is still listed, spelled the nearest way
    // one can.
    open.open(OpenCall {
        model: "has space".into(),
        ..title("c")
    });
    let state = open.to_state();
    assert_eq!(state.len(), 2);
    assert_eq!(state[1].model.model.as_str(), "has_space");
}

/// A run is owed its call for as long as the call is in flight, and for a
/// bounded while from the moment the call began to settle. How long the run
/// has been going, and how long ago it was first asked, have no say: a run
/// cancelled in its second minute is owed the call it cut off exactly as one
/// cancelled in its first second is.
#[test]
fn a_run_is_owed_a_call_in_flight_and_one_settling_however_old_the_run_is() {
    let open = OpenCalls::default();
    assert!(!open.owed(100), "nothing out, nothing owed");
    open.open(call("a"));
    // Asked on every pass while the run works, as the host does.
    assert!(open.owed(100));
    assert!(open.owed(100 + 10 * SETTLE_HOLD_SECS), "still in flight");

    // The run is cancelled much later, and the call begins to settle then.
    let cancelled = 100 + 20 * SETTLE_HOLD_SECS;
    open.settling("a", cancelled);
    open.settling("a", cancelled + 5);
    assert_eq!(
        open.calls()[0].settling_since,
        Some(cancelled),
        "the first time stands"
    );
    assert!(open.owed(cancelled));
    assert!(open.owed(cancelled + SETTLE_HOLD_SECS - 1));
    assert!(
        !open.owed(cancelled + SETTLE_HOLD_SECS),
        "and the wait for one call is bounded"
    );
    // A second call in flight keeps the run, whatever the first is doing.
    open.open(call("b"));
    assert!(open.owed(cancelled + 10 * SETTLE_HOLD_SECS));
    assert!(open.close("a"));
    assert!(open.close("b"));
    assert!(!open.owed(cancelled));

    let mut world = World::new();
    let with = world.spawn(open.clone()).id();
    let without = world.spawn_empty().id();
    assert!(!calls_owed(&world, with, 0));
    assert!(!calls_owed(&world, without, 0));
}

/// A world with the spend channel, a run in it, and the sink trips report
/// through.
fn spend_world() -> (World, Entity, SpendSink, OpenCalls) {
    let mut world = World::new();
    let (sink, results) = SpendSink::channel(Arc::new(Notify::new()));
    world.insert_resource(results);
    let open = OpenCalls::new("run-1", Vec::new());
    let entity = world
        .spawn((
            TokenTotals::default(),
            crate::pipeline::StageLedger(vec![leviath_core::run_meta::StageRecord::new(
                "plan".to_string(),
                0,
            )]),
            open.clone(),
        ))
        .id();
    (world, entity, sink, open)
}

fn spend(entity: Entity, attempt: &str, cost: CallCost, ending: CallEnding) -> CallSpend {
    CallSpend {
        entity,
        run_id: "run-1".to_string(),
        attempt: attempt.to_string(),
        kind: InferenceKind::Stage,
        stage: "plan".to_string(),
        iteration: 1,
        provider: "p".to_string(),
        model: "m".to_string(),
        usage: TokenUsage::new(10, 0, 0, 5),
        cost,
        ending,
        provider_call_id: None,
        visit: String::new(),
        finish_reason: None,
    }
}

fn collect(world: &mut World) {
    let mut schedule = bevy_ecs::schedule::Schedule::default();
    schedule.add_systems(collect_call_spend);
    schedule.run(world);
}

#[test]
fn every_reported_call_lands_on_its_run_whatever_the_run_is_doing() {
    let (mut world, entity, sink, open) = spend_world();
    for attempt in ["a", "b"] {
        open.open(call(attempt));
    }
    sink.report(spend(
        entity,
        "a",
        CallCost::Reported(0.25),
        CallEnding::Answered,
    ));
    sink.report(spend(
        entity,
        "b",
        CallCost::Unknown(leviath_providers::CostUnknown::NoLookup),
        CallEnding::CutOff,
    ));
    // The same call reported again, by a second trip that settled it: it
    // was accounted for, and is not counted twice.
    sink.report(spend(
        entity,
        "a",
        CallCost::Reported(0.25),
        CallEnding::Answered,
    ));
    collect(&mut world);

    let totals = world.get::<TokenTotals>(entity).unwrap();
    assert_eq!(totals.prompt_tokens, 20, "both calls' tokens, once each");
    assert_eq!(totals.cost.reported_calls, 1);
    assert_eq!(totals.cost.unknown.no_lookup, 1);
    assert_eq!(totals.cost.total_usd(), None, "one call has no figure");
    assert_eq!(totals.cost.priced_usd, 0.25);
    let ledger = world.get::<crate::pipeline::StageLedger>(entity).unwrap();
    assert_eq!(ledger.0[0].unknown_calls, 1);
    assert!(open.calls().is_empty(), "both are accounted for");
}

/// A call that reports after its run was unloaded and brought back lands on
/// the run as it is in memory now, found by its id.
#[test]
fn a_call_whose_run_came_back_lands_on_the_run_as_it_is_now() {
    let (mut world, _first, sink, open) = spend_world();
    let back = world
        .spawn((
            TokenTotals::default(),
            open.clone(),
            RunMetadata {
                run_id: "run-1".to_string(),
                ..crate::inference_usage::tests::metadata()
            },
        ))
        .id();
    open.open(call("a"));
    let gone = world.spawn_empty().id();
    world.despawn(gone);
    sink.report(spend(
        gone,
        "a",
        CallCost::Computed(1.0),
        CallEnding::CutOff,
    ));
    collect(&mut world);
    assert_eq!(
        world.get::<TokenTotals>(back).unwrap().cost.total_usd(),
        Some(1.0)
    );
    assert!(open.calls().is_empty());
}

/// A call that reports after its run left memory for good is handed to the
/// persistence lane as a step that settles it in the run's file.
#[test]
fn a_call_whose_run_left_memory_is_written_to_the_runs_file() {
    use crate::persistence_bridge::PersistMsg;
    let (mut world, _entity, sink, _open) = spend_world();
    let (lane, mut steps) = tokio::sync::mpsc::unbounded_channel();
    world.insert_resource(crate::pipeline::PersistenceStage(lane));
    let gone = world.spawn_empty().id();
    world.despawn(gone);
    sink.report(CallSpend {
        run_id: "run-gone".to_string(),
        provider_call_id: Some("gen-1".into()),
        ..spend(gone, "late", CallCost::Reported(0.5), CallEnding::CutOff)
    });
    // And one for a run with no file: nowhere to write it, said out loud.
    sink.report(CallSpend {
        run_id: String::new(),
        ..spend(gone, "lost", CallCost::Computed(1.0), CallEnding::Failed)
    });
    collect(&mut world);

    let sent = steps.try_recv().expect("one step for the run with a file");
    let PersistMsg::Step(step) = sent else {
        panic!("a late call is a run-file step");
    };
    assert_eq!(step.run_id, "run-gone");
    assert!(step.now.is_none(), "the file's own state is what it edits");
    assert_eq!(step.settles, ["late"]);
    assert_eq!(step.events.len(), 1);
    let shown = format!("{:?}", step.events[0]);
    assert!(
        shown.contains("Inference") && shown.contains("gen-1"),
        "{shown}"
    );
    assert!(steps.try_recv().is_err(), "and none for the run with none");

    // A lane that has stopped takes nothing, which is said too.
    drop(steps);
    sink.report(CallSpend {
        run_id: "run-gone".to_string(),
        ..spend(gone, "later", CallCost::Reported(0.5), CallEnding::CutOff)
    });
    collect(&mut world);
}

/// A run read off disk and placed a moment later can miss a call written to
/// its file in between: what it read still lists the call. The write is
/// remembered, and the run takes the call's cost from there when it is
/// placed, so the call is in its totals once and is not settled, or
/// journaled, a second time.
#[test]
fn a_call_written_to_a_file_after_it_was_read_is_taken_once_by_the_run_placed() {
    use crate::persistence_bridge::PersistMsg;
    let (mut world, _entity, sink, _open) = spend_world();
    let (lane, mut steps) = tokio::sync::mpsc::unbounded_channel();
    world.insert_resource(crate::pipeline::PersistenceStage(lane));
    world.insert_resource(LateSettled::default());
    let (journal, mut journaled) = tokio::sync::mpsc::unbounded_channel();
    world.insert_resource(crate::pipeline::JournalSender::new(journal, None));
    let gone = world.spawn_empty().id();
    world.despawn(gone);
    // The start-up settle of a finished run reports while the run is out of
    // memory, and the call is written to its file.
    for attempt in ["x", "y"] {
        sink.report(CallSpend {
            run_id: "run-back".to_string(),
            ..spend(gone, attempt, CallCost::Reported(0.5), CallEnding::CutOff)
        });
    }
    collect(&mut world);
    assert!(matches!(steps.try_recv(), Ok(PersistMsg::Step(_))));
    assert!(matches!(steps.try_recv(), Ok(PersistMsg::Step(_))));

    // The run is placed from a read made before those writes, x still on its
    // list, and y not (it was read after y landed).
    let open = OpenCalls::new("run-back", vec![call("x")]);
    let placed = world
        .spawn((
            TokenTotals::default(),
            open.clone(),
            RunMetadata {
                run_id: "run-back".to_string(),
                ..crate::inference_usage::tests::metadata()
            },
        ))
        .id();
    take_late(&mut world, placed);
    assert!(open.calls().is_empty(), "nothing is left to settle again");
    let totals = world.get::<TokenTotals>(placed).unwrap();
    assert_eq!(totals.cost.total_usd(), Some(0.5), "x, once, and not y");
    assert!(journaled.try_recv().is_err(), "the file holds its record");
    assert!(steps.try_recv().is_err());

    // Taken once: placed again, there is nothing to take.
    take_late(&mut world, placed);
    assert_eq!(
        world.get::<TokenTotals>(placed).unwrap().cost.total_usd(),
        Some(0.5)
    );
    // A run with no list, and a world that remembers nothing, take nothing.
    let bare = world.spawn_empty().id();
    take_late(&mut world, bare);
    take_late(&mut World::new(), bare);
}

/// What was written is remembered for a bounded while: long enough for a
/// run being placed, and no longer.
#[test]
fn a_late_write_is_forgotten_once_no_run_could_have_missed_it() {
    let mut late = LateSettled::default();
    let gone = entity();
    let old = CallSpend {
        run_id: "run-a".to_string(),
        ..spend(gone, "old", CallCost::Computed(1.0), CallEnding::CutOff)
    };
    late.wrote(&old, 100);
    late.wrote(
        &CallSpend {
            run_id: "run-b".to_string(),
            ..spend(gone, "new", CallCost::Computed(1.0), CallEnding::CutOff)
        },
        100 + LATE_KEPT_SECS - 1,
    );
    assert_eq!(late.0.len(), 2);
    late.wrote(
        &CallSpend {
            run_id: "run-b".to_string(),
            ..spend(gone, "newer", CallCost::Computed(1.0), CallEnding::CutOff)
        },
        100 + LATE_KEPT_SECS,
    );
    assert!(!late.0.contains_key("run-a"), "too old to have been missed");
    assert_eq!(late.0["run-b"].len(), 2);
}

#[test]
fn a_run_with_no_list_of_calls_still_has_its_call_recorded() {
    let mut world = World::new();
    let (sink, results) = SpendSink::channel(Arc::new(Notify::new()));
    world.insert_resource(results);
    let entity = world.spawn(TokenTotals::default()).id();
    sink.report(spend(
        entity,
        "a",
        CallCost::Computed(0.5),
        CallEnding::Answered,
    ));
    collect(&mut world);
    assert_eq!(
        world.get::<TokenTotals>(entity).unwrap().cost.total_usd(),
        Some(0.5)
    );
}

#[tokio::test]
async fn a_call_left_out_by_a_stopped_daemon_is_settled_as_cut_off() {
    let (sink, mut results) = SpendSink::channel(Arc::new(Notify::new()));
    let left = OpenCall {
        attempt: "old".into(),
        iteration: 4,
        provider_call_id: Some("gen-old".into()),
        settling_since: Some(1),
        ..call("old")
    };
    let entity = entity();

    // Its provider keeps a record: the record's figure.
    let shop: Arc<dyn Provider> = Arc::new(Shop {
        record: Some(TokenUsage::new(3, 0, 0, 2).with_reported_cost(Some(0.01))),
        ..Shop::priced()
    });
    settle_orphan(
        entity,
        "run-1".into(),
        left.clone(),
        Some(shop),
        sink.clone(),
    )
    .await;
    let spend = results.0.try_recv().expect("reported");
    assert_eq!(spend.cost, CallCost::Reported(0.01));
    assert_eq!(spend.ending, CallEnding::CutOff);
    assert_eq!(spend.attempt, "old");
    assert_eq!(spend.run_id, "run-1");
    assert_eq!(spend.provider_call_id.as_deref(), Some("gen-old"));
    assert_eq!(spend.iteration, 4);

    // Its provider is no longer configured: unknown, with nothing to ask.
    settle_orphan(entity, "run-1".into(), left, None, sink).await;
    let spend = results.0.try_recv().expect("reported");
    assert_eq!(
        spend.cost,
        CallCost::Unknown(leviath_providers::CostUnknown::NoLookup)
    );
    assert_eq!(spend.usage.total_tokens, 0);
}

/// A world with an inference lane whose reports the test reads, and `shop`
/// registered as the provider `shop`.
fn lane_world(shop: Shop) -> (World, CallSpendResults) {
    use crate::inference_pool::{InferencePoolConfig, InferencePools};
    let mut world = World::new();
    let (spend, results) = SpendSink::channel(Arc::new(Notify::new()));
    let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let (transition_outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let (compaction_outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let (content_summary_outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    world.insert_resource(crate::pipeline::InferenceStage {
        pools: Arc::new(InferencePools::new(InferencePoolConfig::new())),
        outcomes,
        transition_outcomes,
        compaction_outcomes,
        content_summary_outcomes,
        spend,
        wake: Arc::new(Notify::new()),
        runtime: tokio::runtime::Handle::current(),
        stream_inference: true,
    });
    let mut registry = crate::ProviderRegistry::new();
    registry.register("shop".to_string(), Arc::new(shop));
    world.insert_resource(crate::pipeline::Providers(registry));
    (world, results)
}

/// A finished run is never brought back, so the calls its file still lists
/// are settled where it is: asked after at the provider, and reported under
/// the run's id with no entity behind them.
#[tokio::test]
async fn the_calls_a_finished_runs_file_lists_are_settled_by_its_id() {
    let (world, mut results) = lane_world(Shop {
        record: Some(TokenUsage::new(3, 0, 0, 2).with_reported_cost(Some(0.02))),
        ..Shop::priced()
    });
    let listed = OpenCalls::new(
        "run-done",
        vec![OpenCall {
            provider_call_id: Some("gen-done".into()),
            ..call("cut")
        }],
    )
    .to_state();
    settle_recorded(&world, "run-done", &listed, &no_jobs());
    let spend = results.0.recv().await.expect("settled");
    assert_eq!(spend.run_id, "run-done");
    assert_eq!(spend.entity, Entity::PLACEHOLDER);
    assert_eq!(spend.cost, CallCost::Reported(0.02));
    assert_eq!(spend.ending, CallEnding::CutOff);

    // A run brought back settles the same way, on its own entity.
    let mut world = world;
    let open = OpenCalls::for_run("run-back", &listed, &no_jobs(), 9);
    let entity = world.spawn(open).id();
    settle_orphaned(&world, entity);
    let spend = results.0.recv().await.expect("settled");
    assert_eq!((spend.entity, spend.run_id.as_str()), (entity, "run-back"));

    // A run with no list, and a world with no lane, have nothing to settle
    // and nowhere to report.
    let bare = world.spawn_empty().id();
    settle_orphaned(&world, bare);
    settle_recorded(&World::new(), "run-done", &listed, &no_jobs());
    assert!(results.0.try_recv().is_err());
}

/// A trip that died leaves its call on the list with nothing to end it. It
/// is settled where the death is noticed: the calls its ticket made that
/// are still in flight, and no other.
#[tokio::test]
async fn a_dead_trips_call_is_settled_where_the_death_is_noticed() {
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    open.open(OpenCall {
        provider_call_id: Some("gen-lost".into()),
        ..call("lost")
    });
    // Another lane's call, and one of this lane's that is already settling.
    open.open(OpenCall {
        kind: InferenceKind::Title,
        ..call("title")
    });
    open.open(OpenCall {
        settling_since: Some(3),
        ..call("settling")
    });
    let shop: Arc<dyn Provider> = Arc::new(Shop {
        record: Some(TokenUsage::new(1, 0, 0, 1).with_reported_cost(Some(0.03))),
        ..Shop::priced()
    });
    ticket.settle_lost(&tokio::runtime::Handle::current(), Some(shop));
    let spend = results.0.recv().await.expect("settled");
    assert_eq!(spend.attempt, "lost");
    assert_eq!(spend.cost, CallCost::Reported(0.03));
    assert_eq!(spend.ending, CallEnding::CutOff);
    assert!(results.0.try_recv().is_err(), "only the dead trip's call");
    let left = open.calls();
    assert!(
        left[0].settling_since.is_some(),
        "and it is marked settling"
    );
    assert_eq!(left[1].settling_since, None);
}

/// What a lane keeps for a trip that dies settles the trip's call, and in a
/// world with nowhere to account to there is nothing to settle.
#[tokio::test]
async fn what_a_lane_keeps_for_a_dead_trip_settles_its_call() {
    let (ticket, open, mut results) = ticket(entity(), InferenceKind::Stage);
    open.open(call("lost"));
    let shop: Arc<dyn Provider> = Arc::new(Shop::priced());
    LostCall {
        ticket: Some(ticket),
        provider: shop.clone(),
        runtime: tokio::runtime::Handle::current(),
    }
    .settle();
    let spend = results.0.recv().await.expect("settled");
    assert_eq!(spend.attempt, "lost");
    assert_eq!(
        spend.cost,
        CallCost::Unknown(leviath_providers::CostUnknown::NoLookup)
    );
    LostCall {
        ticket: None,
        provider: shop,
        runtime: tokio::runtime::Handle::current(),
    }
    .settle();
    assert!(results.0.try_recv().is_err());
}
