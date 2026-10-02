//! A call driven to its end the way the world drives it, and the systems that
//! do it in a real world.

use super::*;
use crate::inference_pool::{InferencePoolConfig, InferencePools};
use leviath_providers::{InferenceResponse, Provider};
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedSender};

/// Drive `job` to its end as the world does, without a world: each trip is
/// made, settled with [`InferenceCall::settle`], and a waiting call is sent
/// again once its due time comes, until the call is done. The finished
/// outcome goes to `results` after the call lets go of its permit, and a
/// cancel ends it with no outcome at all, as a cancelled agent's call does.
///
/// The bridge's own tests drive their jobs through this, so the schedule they
/// assert is the one the world keeps.
pub(crate) async fn run_inference_job(
    job: InferenceJob,
    results: UnboundedSender<InferenceOutcome>,
    wake: Arc<Notify>,
    policy: RetryPolicy,
    cancel: crate::cancel::CancelToken,
) {
    let entity = job.entity;
    let (mut call, refused) = InferenceCall::new(job, policy, CallLane::Stage);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut attempt = call.attempt(entity, refused, false);
    loop {
        run_attempt(attempt, tx.clone(), Arc::new(Notify::new()), cancel.clone()).await;
        let Ok(mut outcome) = rx.try_recv() else {
            return; // cancelled: dropping the call frees its permit
        };
        if call.settle(&mut outcome) == Next::Done {
            drop(call);
            let _ = results.send(outcome);
            wake.notify_one();
            return;
        }
        let due = call.clock.due.expect("a waiting call has a due time");
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep_until(due) => {}
        }
        if call.clock.take_due(Instant::now()) == Some(Due::Expired) {
            drop(call);
            let _ = results.send(InferenceOutcome {
                entity,
                result: Err(job_timed_out(policy.job_timeout)),
                attempt_id: String::new(),
                latency: Duration::ZERO,
                pricing: None,
                attempt: None,
            });
            wake.notify_one();
            return;
        }
        let renew = std::mem::take(&mut call.renew_next);
        attempt = call.attempt(entity, None, renew);
    }
}

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

fn response(text: &str) -> InferenceResponse {
    InferenceResponse {
        parts: Vec::new(),
        content: text.to_string(),
        tool_calls: vec![],
        tokens_used: leviath_providers::TokenUsage::new(1, 0, 0, 1),
        finish_reason: leviath_providers::FinishReason::Complete,
        reasoning: None,
    }
}

/// Fails with a transient error until it has been asked `fail` times, then
/// answers. Counts every trip.
pub(crate) struct Flaky {
    fail: usize,
    calls: std::sync::atomic::AtomicUsize,
}

impl Flaky {
    /// A provider whose first `fail` trips fail.
    pub(crate) fn failing(fail: usize) -> Self {
        Self {
            fail,
            calls: Default::default(),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for Flaky {
    async fn infer(&self, _r: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match n < self.fail {
            true => Err(ProviderError::RequestFailed("reset".into())),
            false => Ok(response("answered")),
        }
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "flaky"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
}

#[tokio::test]
async fn flaky_provider_metadata_is_exercised() {
    let p = Flaky::failing(0);
    assert_eq!(p.name(), "flaky");
    assert_eq!(p.count_tokens("t", "m").await, 1);
    assert_eq!(p.max_context_tokens("m"), 100_000);
    let _ = p.capabilities("m");
}

/// A retry schedule with no waiting in it, `attempts` long.
fn quick(attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts: attempts,
        base_delay: Duration::from_millis(1),
        capacity_base_delay: Duration::from_millis(1),
        capacity_max_delay: Duration::from_millis(1),
        reached_base_delay: Duration::from_millis(1),
        max_total_backoff: Duration::from_secs(60),
        job_timeout: Duration::from_secs(60),
    }
}

/// A world with the inference lane wired, the systems that drive a call in
/// its schedule, and an agent waiting on a call to `provider`.
struct LaneWorld {
    world: World,
    schedule: Schedule,
    stage_rx: mpsc::UnboundedReceiver<InferenceOutcome>,
    routing_rx: mpsc::UnboundedReceiver<InferenceOutcome>,
    pools: Arc<InferencePools>,
    agent: Entity,
}

fn lane_world(status: crate::components::AgentStatus) -> LaneWorld {
    let mut cfg = InferencePoolConfig::new();
    cfg.set_limit("m", 1);
    let pools = Arc::new(InferencePools::new(cfg));
    let (outcomes, stage_rx) = mpsc::unbounded_channel();
    let (transition_outcomes, routing_rx) = mpsc::unbounded_channel();
    let (compaction_outcomes, _c) = mpsc::unbounded_channel();
    let (content_summary_outcomes, _s) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(InferenceStage {
        pools: pools.clone(),
        outcomes,
        transition_outcomes,
        compaction_outcomes,
        content_summary_outcomes,
        wake: Arc::new(Notify::new()),
        runtime: tokio::runtime::Handle::current(),
        stream_inference: false,
    });
    let agent = world
        .spawn(crate::components::AgentState {
            agent_id: "a".to_string(),
            current_visit: String::new(),
            current_stage: "s".to_string(),
            iteration: 0,
            status,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        })
        .id();
    let mut schedule = Schedule::default();
    schedule.add_systems(fire_due_calls);
    LaneWorld {
        world,
        schedule,
        stage_rx,
        routing_rx,
        pools,
        agent,
    }
}

impl LaneWorld {
    /// Start a call to `provider` on `lane`, as dispatch does.
    fn start(&mut self, provider: Arc<dyn Provider>, policy: RetryPolicy, lane: CallLane) {
        let job = InferenceJob {
            entity: self.agent,
            refused: None,
            provider,
            request: request(),
            permit: self.pools.try_acquire("p", "m").expect("a free slot"),
            calibration: None,
            stream: false,
            hydration: None,
            journal: None,
        };
        let stage = self.world.resource::<InferenceStage>();
        let (call, _cancel) = start_call(stage, job, policy, lane);
        self.world.entity_mut(self.agent).insert(call);
    }

    /// Wait for the next outcome on `lane` and settle it as a collect system
    /// does. Returns the outcome with what the settling decided.
    async fn next(&mut self, lane: CallLane) -> (InferenceOutcome, Next) {
        let rx = match lane {
            CallLane::Stage => &mut self.stage_rx,
            CallLane::Routing => &mut self.routing_rx,
        };
        let mut outcome = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an outcome arrives")
            .expect("the lane is open");
        let agent = self.agent;
        let mut state = bevy_ecs::system::SystemState::<(
            Query<&mut InferenceCall>,
            Res<InferenceStage>,
            Commands,
        )>::new(&mut self.world);
        let (mut calls, stage, mut commands) = state.get_mut(&mut self.world).expect("the params");
        let next = settle_call(
            calls.get_mut(agent).ok(),
            &mut outcome,
            Some(&stage),
            &mut commands,
        );
        state.apply(&mut self.world);
        (outcome, next)
    }

    /// Tick the firing system until the call has sent its next trip.
    async fn fire(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            self.schedule.run(&mut self.world);
            let waiting = self
                .world
                .get::<InferenceCall>(self.agent)
                .is_some_and(|c| c.clock.due.is_some());
            if !waiting {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "the call never fired");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

/// The world, not the task, retries a failed trip: the first trip's transient
/// failure leaves the call on the agent waiting on a due time, the firing
/// system sends the second trip when it comes, and the answer finishes the
/// call and gives its slot back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_trip_waits_in_the_world_and_the_next_one_answers() {
    let mut lane = lane_world(crate::components::AgentStatus::Active);
    let provider = Arc::new(Flaky {
        fail: 1,
        calls: Default::default(),
    });
    lane.start(provider.clone(), quick(3), CallLane::Stage);

    let (outcome, next) = lane.next(CallLane::Stage).await;
    assert!(outcome.result.is_err());
    assert!(
        matches!(next, Next::Waiting(_)),
        "a transient failure is tried again"
    );
    let call = lane
        .world
        .get::<InferenceCall>(lane.agent)
        .expect("the call is still held");
    assert!(
        call.clock.due.is_some(),
        "and waits on its backoff, in the world"
    );
    assert!(
        lane.pools.try_acquire("p", "m").is_none(),
        "holding its slot"
    );

    lane.fire().await;
    let (outcome, next) = lane.next(CallLane::Stage).await;
    assert_eq!(next, Next::Done);
    assert_eq!(outcome.result.expect("answered").content, "answered");
    assert_eq!(provider.calls(), 2);
    assert!(lane.world.get::<InferenceCall>(lane.agent).is_none());
    assert!(
        lane.pools.try_acquire("p", "m").is_some(),
        "the slot is free again"
    );
}

/// The routing lane retries the same way, and reports on its own channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routing_call_is_retried_on_its_own_lane() {
    let mut lane = lane_world(crate::components::AgentStatus::Active);
    let provider = Arc::new(Flaky {
        fail: 1,
        calls: Default::default(),
    });
    lane.start(provider.clone(), quick(3), CallLane::Routing);
    let (_, next) = lane.next(CallLane::Routing).await;
    assert!(matches!(next, Next::Waiting(_)));
    lane.fire().await;
    let (outcome, next) = lane.next(CallLane::Routing).await;
    assert_eq!(next, Next::Done);
    assert!(outcome.result.is_ok());
    assert!(
        lane.stage_rx.try_recv().is_err(),
        "nothing went to the stage lane"
    );
}

/// A paused agent's call keeps retrying: pause lets the step in flight
/// finish, and its retries are part of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_agents_call_still_retries() {
    let mut lane = lane_world(crate::components::AgentStatus::Paused);
    let provider = Arc::new(Flaky {
        fail: 1,
        calls: Default::default(),
    });
    lane.start(provider.clone(), quick(3), CallLane::Stage);
    let (_, next) = lane.next(CallLane::Stage).await;
    assert!(matches!(next, Next::Waiting(_)));
    lane.fire().await;
    let (outcome, _) = lane.next(CallLane::Stage).await;
    assert!(outcome.result.is_ok());
}

/// An agent that ends while its call waits out a backoff gives the slot back
/// on the next tick, and no further trip is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_agents_waiting_call_gives_its_slot_back() {
    let mut lane = lane_world(crate::components::AgentStatus::Active);
    let provider = Arc::new(Flaky {
        fail: 5,
        calls: Default::default(),
    });
    let mut slow = quick(5);
    slow.base_delay = Duration::from_secs(3600);
    lane.start(provider.clone(), slow, CallLane::Stage);
    let (_, next) = lane.next(CallLane::Stage).await;
    assert!(matches!(next, Next::Waiting(_)));
    lane.world
        .get_mut::<crate::components::AgentState>(lane.agent)
        .expect("state")
        .status = crate::components::AgentStatus::Cancelled;
    lane.schedule.run(&mut lane.world);
    assert!(lane.world.get::<InferenceCall>(lane.agent).is_none());
    assert!(
        lane.pools.try_acquire("p", "m").is_some(),
        "the slot is free"
    );
    assert_eq!(provider.calls(), 1, "no further trip");
}

/// A call whose whole allowance runs out while it waits reports the job
/// timeout on its lane, without another trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_out_of_time_while_waiting_reports_the_timeout() {
    let mut lane = lane_world(crate::components::AgentStatus::Active);
    let provider = Arc::new(Flaky {
        fail: 5,
        calls: Default::default(),
    });
    let mut short = quick(5);
    short.base_delay = Duration::from_secs(3600);
    short.job_timeout = Duration::from_millis(200);
    lane.start(provider.clone(), short, CallLane::Stage);
    let (_, next) = lane.next(CallLane::Stage).await;
    assert!(matches!(next, Next::Waiting(_)));
    lane.fire().await;
    let (outcome, next) = lane.next(CallLane::Stage).await;
    assert_eq!(next, Next::Done);
    let err = outcome.result.expect_err("timed out");
    assert!(err.to_string().contains("job timeout"), "{err}");
    assert_eq!(provider.calls(), 1);
}

/// An outcome with no call behind it (one a pause held and resume replays)
/// is applied as it is.
#[tokio::test]
async fn an_outcome_with_no_call_is_applied_as_it_is() {
    let mut lane = lane_world(crate::components::AgentStatus::Active);
    let agent = lane.agent;
    let mut outcome = InferenceOutcome {
        entity: agent,
        result: Err(ProviderError::RequestFailed("reset".into())),
        attempt_id: String::new(),
        latency: Duration::from_millis(5),
        pricing: None,
        attempt: None,
    };
    let mut state = bevy_ecs::system::SystemState::<(
        Query<&mut InferenceCall>,
        Res<InferenceStage>,
        Commands,
    )>::new(&mut lane.world);
    let (mut calls, _stage, mut commands) = state.get_mut(&mut lane.world).expect("the params");
    let next = settle_call(calls.get_mut(agent).ok(), &mut outcome, None, &mut commands);
    assert_eq!(next, Next::Done);
    assert_eq!(outcome.latency, Duration::from_millis(5));
}

#[test]
fn the_lanes_are_named_for_their_supervisor() {
    assert_eq!(CallLane::Stage.name(), "inference");
    assert_eq!(CallLane::Routing.name(), "transition-choice");
}

/// A due time at the deadline is an expiry, one before it a send, and a due
/// time fires once.
#[tokio::test]
async fn a_due_time_fires_once_and_says_what_for() {
    let mut clock = RetryClock::start(&RetryPolicy {
        job_timeout: Duration::from_secs(60),
        ..RetryPolicy::default()
    });
    assert_eq!(clock.take_due(Instant::now()), None, "nothing is due");
    let due = clock.wait(Duration::ZERO);
    assert_eq!(clock.take_due(due), Some(Due::Send));
    assert_eq!(clock.take_due(due), None, "fired once");
    let due = clock.wait(Duration::from_secs(3600));
    assert_eq!(due, clock.deadline(), "a wait never outlasts the call");
    assert_eq!(clock.take_due(due), Some(Due::Expired));
}

/// Past the deadline a failure is never retried, however much budget is left.
#[tokio::test]
async fn a_failure_past_the_deadline_is_not_retried() {
    let policy = RetryPolicy {
        job_timeout: Duration::ZERO,
        ..quick(5)
    };
    let mut clock = RetryClock::start(&policy);
    assert_eq!(
        clock.after_failure(&policy, &ProviderError::RequestFailed("reset".into())),
        None
    );
}
