//! Tests for [`super`].
//!
//! A sibling file rather than an inline `mod tests`, and deliberately:
//! the helpers below poll a background lane, and whether a poll loop
//! iterates at all depends on how fast that lane happens to be. Inline,
//! the gate measures that scaffolding and fails intermittently on a
//! sleep that legitimately did not need to run. llvm-cov excludes this
//! layout by default, which is the sanctioned answer for a test module
//! whose own branches cannot be exercised deterministically
//! (see CONTRIBUTING, "Where a test module lives").

use super::*;
use crate::dynamic_interaction::InteractionBackend;
use crate::inference_pool::InferencePoolConfig;
use crate::pipeline::{
    AgentBlueprint, ReadyToInfer, StageCursor, StageInference, StageInferences, StageProgress,
    StageSetup, StageSetups, ToolService, VisitCounts, WaitingForChildren,
};
use crate::tool_bridge::BoxedToolExec;
use leviath_core::{Region, RegionKind};
use leviath_providers::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider, ProviderError,
    TokenUsage,
};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

struct Script {
    responses: Mutex<std::collections::VecDeque<InferenceResponse>>,
}
#[async_trait::async_trait]
impl Provider for Script {
    async fn infer(&self, _req: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Other("exhausted".to_string()))
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

struct NoTools;
impl ToolService for NoTools {
    fn exec_for(
        &self,
        _e: Entity,
        calls: Vec<leviath_providers::ToolCall>,
        _progress: crate::pipeline::ToolProgress,
    ) -> BoxedToolExec {
        Box::new(move || {
            Box::pin(async move { calls.into_iter().map(|c| (c.id, String::new())).collect() })
        })
    }
}

fn text(content: &str) -> InferenceResponse {
    InferenceResponse {
        content: content.to_string(),
        tool_calls: vec![],
        tokens_used: TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        finish_reason: FinishReason::Complete,
    }
}

fn host_with(responses: Vec<InferenceResponse>) -> WorldHost {
    let mut registry = crate::providers::ProviderRegistry::new();
    registry.register(
        "script".to_string(),
        Arc::new(Script {
            responses: Mutex::new(responses.into_iter().collect()),
        }),
    );
    let world = PipelineWorld::new(
        registry,
        Arc::new(NoTools),
        InferencePoolConfig::new(),
        1,
        None,
        Handle::current(),
    );
    WorldHost::new(world)
}

fn blueprint() -> leviath_core::Blueprint {
    let layout = leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        )],
        12_000,
    );
    let s = leviath_core::Stage::new(
        "s".to_string(),
        leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
    );
    leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout)
}

fn window() -> crate::components::ContextWindow {
    let mut w = crate::components::ContextWindow::new(10_000);
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    w
}

fn agent_state(agent_id: &str) -> AgentState {
    AgentState {
        agent_id: agent_id.to_string(),
        current_stage: "s".to_string(),
        iteration: 0,
        status: AgentStatus::Active,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

fn si() -> StageInference {
    StageInference {
        provider_name: "script".to_string(),
        model: "m".to_string(),
        tools: vec![],
        tool_filter: None,
        fallbacks: Vec::new(),
        output: None,
    }
}

fn setup() -> StageSetup {
    StageSetup {
        inference_config: crate::components::InferenceConfig {
            temperature: None,
            max_output_tokens: None,
            extra_params: Default::default(),
            batch_tool_hint: false,
            shell_hint: false,
            request_timeout_secs: None,
        },
        routing: None,
        accepts_messages: true,
        context_layout: None,
        system_prompt: None,
        output: None,
    }
}

/// Spawn a simple agent into the host and register it under `run_id`.
fn spawn(host: &mut WorldHost, run_id: &str, agent_id: &str) -> Entity {
    let e = host.world_mut().spawn_agent((
        AgentBlueprint(blueprint()),
        StageCursor { index: 0 },
        agent_state(agent_id),
        crate::components::MessageInbox::default(),
        StageProgress::default(),
        StageInferences(vec![si()]),
        StageSetups(vec![setup()]),
        VisitCounts::default(),
        window(),
        si(),
        setup().inference_config,
        ReadyToInfer,
    ));
    host.register(run_id, e);
    e
}

/// A [`ForceTerminator`] that records each run id it was asked to terminate
/// and reports success for everything but `"never-existed"`. Shared by the
/// tests that expect it to fire and the ones that expect it not to, so its
/// body is exercised rather than existing only to go unused.
fn recording_terminator(seen: Arc<Mutex<Vec<String>>>) -> ForceTerminator {
    Box::new(move |run_id| {
        seen.lock().unwrap().push(run_id.to_string());
        run_id != "never-existed"
    })
}

/// A [`Reloader`] that pages any run id in as a fresh agent.
fn paging_reloader() -> Reloader {
    Box::new(|world, run_id| Some(world.spawn_agent((agent_state(run_id),))))
}

async fn ask<T>(host: &mut WorldHost, make: impl FnOnce(oneshot::Sender<T>) -> ControlOp) -> T {
    let (tx, rx) = oneshot::channel();
    host.handle(make(tx));
    rx.await.unwrap()
}

/// A provider whose call never returns while `hang` is set - the stalled
/// request that holds its pool permit until something cancels the job.
///
/// The non-hanging arm is not decoration: a body that only ever diverges has
/// no reachable return, so the answering path is what keeps this honest (and
/// measurable) - the same shape `inference_bridge`'s `Scripted` uses.
struct Hangs {
    hang: bool,
}
#[async_trait::async_trait]
impl Provider for Hangs {
    async fn infer(&self, _req: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        if self.hang {
            std::future::pending().await
        } else {
            Err(ProviderError::Other("not hanging".to_string()))
        }
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "hangs"
    }
    fn capabilities(&self, _m: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// The stalling provider's own surface. `infer` never returns by design, so
/// it is reached under a timeout; the rest are plain accessors the dispatch
/// path reads.
#[tokio::test]
async fn the_hanging_provider_answers_everything_except_a_hanging_infer() {
    fn request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "m".to_string(),
            max_tokens: 1,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }
    let p = Hangs { hang: true };
    assert_eq!(p.name(), "hangs");
    assert_eq!(p.count_tokens("t", "m").await, 1);
    assert_eq!(p.max_context_tokens("m"), 100_000);
    let _ = p.capabilities("m");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), p.infer(&request()))
            .await
            .is_err(),
        "hanging: the whole point is that the call never lands"
    );
    // ...and the answering arm, so the call has a reachable way out.
    assert!(Hangs { hang: false }.infer(&request()).await.is_err());
}

/// A host whose only provider hangs, with model `m` capped at `limit`
/// concurrent inferences - so the second agent to want a slot is starved
/// until the first one gives its permit back.
fn host_with_full_pool(limit: usize) -> WorldHost {
    let mut registry = crate::providers::ProviderRegistry::new();
    registry.register("script".to_string(), Arc::new(Hangs { hang: true }));
    let mut pools = InferencePoolConfig::new();
    pools.set_limit("m", limit);
    WorldHost::new(PipelineWorld::new(
        registry,
        Arc::new(NoTools),
        pools,
        1,
        None,
        Handle::current(),
    ))
}

/// How long [`serve_until_inferring`] waits at each park before calling the loop
/// wedged. A wake that is coming lands as soon as the freeing task is
/// polled, so this is only ever spent proving the *absence* of one.
const PARK: std::time::Duration = std::time::Duration::from_millis(250);

/// Drive `host` exactly the way [`WorldHost::serve`] does - run the world to
/// quiescence, then park until something wakes it - and report whether
/// `entity` got dispatched within `rounds` parks. `false` means the loop
/// parked with nothing left to wake it, which is the daemon wedging.
///
/// Takes the entity rather than a predicate closure on purpose: a generic
/// parameter would give each call site its own instantiation, and no single
/// one of them exercises both the "it happened" and "we wedged" exits.
async fn serve_until_inferring(
    host: &mut WorldHost,
    rounds: usize,
    park: std::time::Duration,
    entity: Entity,
) -> bool {
    let wake = host.world_mut().wake_handle();
    for _ in 0..rounds {
        host.world_mut().run_to_fixed_point();
        if is_inferring(host, entity) {
            return true;
        }
        if tokio::time::timeout(park, wake.notified()).await.is_err() {
            break; // parked with no wake pending - nothing will re-drive us
        }
    }
    false
}

/// Whether `entity` has been handed a pool permit and dispatched.
fn is_inferring(host: &mut WorldHost, entity: Entity) -> bool {
    host.world_mut()
        .world()
        .get::<crate::pipeline::AwaitingInference>(entity)
        .is_some()
}

/// Regression for #189 ("slots=0 for hours, in_progress frozen").
///
/// Releasing an inference permit has to wake the tick loop, because
/// `dispatch_inference` leaves a slot-starved agent `ReadyToInfer` to be
/// "retried on a later tick" - and the loop is event-driven, so a later tick
/// only happens when something wakes it. A cancelled job frees its permit
/// from a detached task, *after* the tick chain has already run to
/// quiescence over the cancel. If that release is silent, the freed slot is
/// invisible: capacity sits idle while every agent queued behind it stays
/// parked, for as long as it takes some unrelated event to wake the loop.
#[tokio::test]
async fn releasing_a_cancelled_runs_permit_wakes_the_starved_agent_behind_it() {
    let mut host = host_with_full_pool(1);

    // Dispatch the holder first and on its own, so which agent wins the
    // single permit is decided here rather than by the parallel dispatch.
    let holder = spawn(&mut host, "run-a", "agent-a");
    host.world_mut().run_to_fixed_point();
    assert!(is_inferring(&mut host, holder), "the holder takes the slot");

    let starved = spawn(&mut host, "run-b", "agent-b");
    host.world_mut().run_to_fixed_point();
    assert!(
        !is_inferring(&mut host, starved),
        "the second agent is starved on the full pool"
    );
    // And it stays starved for as long as the slot is genuinely held - the
    // cap is real, not an artifact of the wake. Several rounds, because the
    // first park consumes the wake the spawn itself stored; the loop has to
    // reach a park with nothing pending before "wedged" means anything.
    assert!(
        !serve_until_inferring(&mut host, 3, PARK, starved).await,
        "no slot, no dispatch"
    );

    // Cancel the holder the way `lev cancel` does. The tick chain aborts its
    // in-flight work; the permit itself comes back later, on the job's task.
    assert!(
        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "run-a".to_string(),
            reply,
        })
        .await
    );

    assert!(
        serve_until_inferring(&mut host, 8, PARK, starved).await,
        "the freed slot must wake the loop so the starved agent can take it; \
         without that wake the daemon parks with capacity it cannot see"
    );
}

/// The backstop, on its own terms: `serve` must make progress from a timer
/// alone, with nothing ever waking it. Whatever else goes silent - a release
/// that forgets to notify, a lane that reports nothing - the daemon still
/// re-examines the world instead of parking indefinitely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_redrives_the_world_on_its_own_timer_with_no_wake() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Counts ticks from inside the schedule, so the assertion is about the
    // loop actually running - not about some state that a single startup
    // pass could equally have produced.
    static TICKS: AtomicUsize = AtomicUsize::new(0);
    TICKS.store(0, Ordering::SeqCst);
    fn count_ticks() {
        TICKS.fetch_add(1, Ordering::SeqCst);
    }

    let mut host = host_with(vec![]);
    host.world_mut().add_test_system(count_ticks);
    host.set_redrive_interval(std::time::Duration::from_millis(20));
    let shutdown = host.world_mut().shutdown_handle();

    let (op_tx, op_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        host.serve(op_rx).await;
    });

    // Nothing is ever sent on `op_tx`, nothing is spawned, and no wake is
    // signalled: an empty world quiesces immediately, so every tick past the
    // first handful is one the timer produced.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let ticks = TICKS.load(Ordering::SeqCst);
    shutdown.notify_one();
    drop(op_tx);
    handle.await.unwrap();

    assert!(
        ticks > 3,
        "the timer must keep driving the world with nothing waking it; saw {ticks} ticks"
    );
}

/// A two-stage linear blueprint (`one` -> `two`), for the stage-boundary
/// tests. No transitions declared: `resolve_transition_sync` falls through to
/// the next stage in order, which is the ordinary case.
fn two_stage_blueprint() -> leviath_core::Blueprint {
    let layout = leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        )],
        12_000,
    );
    let model = leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string());
    // Both stages end by running out of iterations, which is how a stage that
    // keeps calling tools finishes. That boundary is the one the driver used
    // to miss: `enforce_max_iterations` and `resolve_transition` both run in
    // the same tick, so the agent leaves `ReadyToInfer` and comes back to it
    // with every marker count exactly as it was.
    let mut one = leviath_core::Stage::new("one".to_string(), model.clone());
    one.max_iterations = Some(1);
    let mut two = leviath_core::Stage::new("two".to_string(), model);
    two.max_iterations = Some(1);
    let stages = vec![one, two];
    leviath_core::Blueprint::new("t".to_string(), "d".to_string(), stages, layout)
}

/// Spawn an agent that starts at stage `one` of [`two_stage_blueprint`].
fn spawn_two_stage(host: &mut WorldHost, run_id: &str, agent_id: &str) -> Entity {
    let mut state = agent_state(agent_id);
    state.current_stage = "one".to_string();
    let e = host.world_mut().spawn_agent((
        AgentBlueprint(two_stage_blueprint()),
        StageCursor { index: 0 },
        state,
        crate::components::MessageInbox::default(),
        StageProgress::default(),
        StageInferences(vec![si(), si()]),
        StageSetups(vec![setup(), setup()]),
        VisitCounts::default(),
        window(),
        si(),
        setup().inference_config,
        ReadyToInfer,
    ));
    host.register(run_id, e);
    e
}

/// A response that asks for one tool call - what a working stage returns
/// right up to the iteration that ends it.
fn tool_call(id: &str) -> InferenceResponse {
    InferenceResponse {
        tool_calls: vec![leviath_providers::ToolCall {
            id: id.to_string(),
            name: "noop".to_string(),
            arguments: serde_json::Value::Null,
            thought_signature: None,
        }],
        ..text("working")
    }
}

/// Regression for #197 ("entering the next stage waits for the re-drive
/// tick").
///
/// `serve` is event-driven; its 30s re-drive is a correctness backstop for a
/// wake that never came, not the mechanism ordinary work runs on. A stage
/// boundary that only makes progress on the timer puts up to 30s of dead time
/// on every transition - a five-stage run loses minutes to nothing.
///
/// The re-drive is set out of reach here, so the run can only finish through
/// the wake path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stage_boundary_is_crossed_without_waiting_for_the_redrive() {
    let mut host = host_with(vec![tool_call("c1"), tool_call("c2")]);
    host.set_redrive_interval(std::time::Duration::from_secs(3600));
    spawn_two_stage(&mut host, "run-a", "agent-a");

    let mut events = host.subscribe();
    let shutdown = host.world_mut().shutdown_handle();
    let (op_tx, op_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move { host.serve(op_rx).await });

    // Watch the event stream rather than the world: `serve` owns the host for
    // as long as it runs. Everything before `Completed` (spawn, status,
    // tokens) streams past on the way.
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("the event stream must outlive the run");
            if let WorldEvent::Completed { status, .. } = event {
                break status;
            }
        }
    })
    .await;

    shutdown.notify_one();
    drop(op_tx);
    handle.await.unwrap();

    assert_eq!(
        completed.expect("the run must reach stage two and finish on wakes alone"),
        "complete"
    );
}

/// The heartbeat's two levels. Under pressure it is worth an `info` line;
/// idle it must not be, or a healthy daemon spams the log forever.
#[tokio::test]
async fn the_lane_heartbeat_distinguishes_pressure_from_idle() {
    leviath_testkit::with_tracing(|| async {
        // Idle: no agents, no pools touched, nothing queued.
        let mut host = host_with_full_pool(1);
        let idle = host.world_mut().lane_snapshot();
        assert!(!idle.is_under_pressure(), "an empty world is not pressured");
        assert_eq!(idle.inference_summary(), "none");
        host.log_lane_pressure(&idle); // the `debug` arm

        // Two agents, one slot: one infers, one is queued behind a full pool.
        spawn(&mut host, "run-a", "agent-a");
        spawn(&mut host, "run-b", "agent-b");
        host.world_mut().run_to_fixed_point();

        let busy = host.world_mut().lane_snapshot();
        assert_eq!(busy.agents.active, 2);
        assert_eq!(busy.inference_summary(), "m=1/1");
        assert!(
            busy.is_under_pressure(),
            "a full pool with active agents is exactly the state worth reporting"
        );
        host.log_lane_pressure(&busy); // the `info` arm
    })
    .await;
}

/// A daemon with work queued and nothing moving is what issue #191 reported,
/// and until now it looked identical to a busy one. Each re-drive that finds
/// the lanes full and the world unchanged is one dead cycle.
#[tokio::test]
async fn re_drives_that_go_nowhere_under_pressure_count_as_dead_cycles() {
    leviath_testkit::with_tracing(|| async {
        // Two agents, one inference slot, and a provider that never answers:
        // one is stuck mid-call, the other is queued behind a full pool.
        let mut host = host_with_full_pool(1);
        spawn(&mut host, "run-a", "agent-a");
        spawn(&mut host, "run-b", "agent-b");
        host.world_mut().run_to_fixed_point();
        host.emit_events();

        // The first re-drive has nothing to compare against.
        host.observe_redrive();
        assert_eq!(host.dead_cycles, 0, "the first cycle sets the baseline");

        host.observe_redrive();
        assert_eq!(host.dead_cycles, 1, "a whole interval, nothing moved");
        host.observe_redrive();
        assert_eq!(host.dead_cycles, 2, "and another - this is the `warn` arm");
    })
    .await;
}

/// Any sign of life clears the count. A daemon that moves once every few
/// minutes is slow, not wedged, and must not accumulate towards relief.
#[tokio::test]
async fn a_run_that_moves_clears_the_dead_cycle_count() {
    let mut host = host_with_full_pool(1);
    let entity = spawn(&mut host, "run-a", "agent-a");
    spawn(&mut host, "run-b", "agent-b");
    host.world_mut().run_to_fixed_point();
    host.emit_events();
    host.observe_redrive();
    host.observe_redrive();
    assert_eq!(host.dead_cycles, 1, "wedged to begin with");

    // One run advances an iteration, which is exactly what the fingerprint
    // is built to notice.
    host.world_mut()
        .world_mut()
        .get_mut::<AgentState>(entity)
        .expect("the agent is loaded")
        .iteration += 1;
    host.emit_events();

    host.observe_redrive();
    assert_eq!(host.dead_cycles, 0, "something moved");
}

/// Fill the world's tool lane and queue one batch behind it, returning a
/// handle that releases the blocking batch.
///
/// Uses the world's real lane rather than poking the counters, because the
/// point of relief is that the queued batch actually runs afterwards.
async fn wedge_the_tool_lane(host: &mut WorldHost) -> crate::cancel::CancelToken {
    let snapshot = host.world_mut().lane_snapshot();
    let stage = host
        .world_mut()
        .world()
        .resource::<crate::pipeline::ToolStage>()
        .clone();
    // A cancel token rather than a `Notify`: it latches, so a batch that has
    // not started yet still sees the release rather than waiting for a
    // wake-up that already happened.
    let release = crate::cancel::CancelToken::new();
    let submit = |exec: crate::tool_bridge::BoxedToolExec| {
        stage.stats.enqueued();
        stage
            .jobs
            .send(crate::tool_bridge::ToolJob {
                // The lane never looks at the entity; these batches belong to
                // no agent.
                entity: Entity::from_raw_u32(9_001).expect("a small index is a valid id"),
                exec,
                cancel: crate::cancel::CancelToken::new(),
            })
            .expect("the lane is serving");
    };
    // Every batch here blocks until `release` fires. That is deliberate: a
    // batch that can finish on its own makes the lane's occupancy a moving
    // target, and the counts these tests assert on stop being deterministic.
    // `release_the_lane` lets them all go at the end.
    let blocker = || {
        let held = release.clone();
        submit(Box::new(move || {
            Box::pin(async move {
                held.cancelled().await;
                Vec::new()
            })
        }));
    };
    // Take whatever capacity is still free, so the lane is genuinely full
    // rather than merely busy.
    for _ in 0..snapshot.tools_workers.saturating_sub(snapshot.tools_busy) {
        blocker();
    }
    // Wait for them to actually be holding it before queueing anything
    // behind them: batches race each other for a permit, so one submitted
    // alongside could get in first.
    await_full_lane(host).await;
    blocker(); // and one behind them, which can only run once there is room
    await_saturation(host).await;
    release
}

/// Block until every unit of the world's tool-lane capacity is held.
async fn await_full_lane(host: &mut WorldHost) {
    await_lane(host, "the lane filled up", |snapshot| {
        snapshot.tools_busy >= snapshot.tools_workers
    })
    .await;
}

/// Block until the world's tool lane reports itself saturated.
async fn await_saturation(host: &mut WorldHost) {
    await_lane(host, "the lane saturated", |snapshot| {
        snapshot.tools_saturated
    })
    .await;
}

/// Block until the world's tool lane drains its queue.
async fn await_drained_queue(host: &mut WorldHost) {
    await_lane(host, "the queued batch got in", |snapshot| {
        snapshot.tools_queued == 0
    })
    .await;
}

/// Poll the lane until `done`, or fail with `context`. Bounded so a wedge in
/// the code under test fails the run instead of hanging it.
async fn await_lane(
    host: &mut WorldHost,
    context: &str,
    done: fn(&crate::world::LaneSnapshot) -> bool,
) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !done(&host.world_mut().lane_snapshot()) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect(context);
}

/// Let every wedged batch finish and wait for the lane to empty, so the
/// batches are exercised end to end rather than abandoned mid-await.
///
/// Takes a slice rather than one token so a test that wedged the lane twice
/// releases both before waiting; releasing one and waiting would wait for
/// batches still held by the other.
async fn release_the_lane(host: &mut WorldHost, releases: &[crate::cancel::CancelToken]) {
    for release in releases {
        release.cancel();
    }
    await_lane(host, "the lane emptied", |snapshot| {
        snapshot.tools_busy == 0 && snapshot.tools_queued == 0
    })
    .await;
}

/// The relief valve: a tool lane that has not drained in long enough gets
/// wider, so whatever is queued behind the jam can run.
///
/// Additive on purpose. Killing whatever holds the lane is the tempting
/// reading of "reclaim stuck slots", and it is wrong: a run parked on an
/// `ask_user` is behaving correctly, and an operator killing healthy
/// `waiting` runs is the story behind issue #184.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lane_that_never_drains_is_widened_rather_than_emptied() {
    leviath_testkit::with_tracing(|| async {
        let mut host = host_with_full_pool(1);
        host.set_dead_cycles_before_relief(2);
        let release = wedge_the_tool_lane(&mut host).await;

        host.observe_redrive(); // baseline
        host.observe_redrive(); // 1
        assert_eq!(host.relief_granted, 0, "still inside the grace period");
        host.observe_redrive(); // 2 → relief
        assert_eq!(host.relief_granted, 1, "the lane got wider");
        assert_eq!(
            host.dead_cycles, 0,
            "the streak restarts so relief is not granted again immediately"
        );
        assert_eq!(host.health().tools_workers, 2);

        // Which is the whole point: the batch that was queued behind the
        // jam gets a permit, while the batch already holding one keeps it.
        await_drained_queue(&mut host).await;
        assert_eq!(host.world_mut().lane_snapshot().tools_busy, 2);
        release_the_lane(&mut host, &[release]).await;
    })
    .await;
}

/// The give-back half: once the jam is over and the lane has been healthy
/// for the decay margin, the granted capacity is reclaimed - a historical
/// wedge no longer raises the daemon's concurrency ceiling (and with it,
/// its peak memory) for the rest of its life.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relief_decays_back_once_the_lane_is_healthy_again() {
    leviath_testkit::with_tracing(|| async {
        let mut host = host_with_full_pool(1);
        host.set_dead_cycles_before_relief(2);
        let release = wedge_the_tool_lane(&mut host).await;
        host.observe_redrive(); // baseline
        host.observe_redrive(); // 1
        host.observe_redrive(); // 2 → relief
        assert_eq!(host.relief_granted, 1);
        assert_eq!(host.health().tools_workers, 2);

        // Unjam and drain, so the relief permit sits idle.
        await_drained_queue(&mut host).await;
        release_the_lane(&mut host, &[release]).await;

        // Healthy cycles accumulate; nothing comes back inside the margin.
        for _ in 0..(HEALTHY_CYCLES_BEFORE_DECAY - 1) {
            host.observe_redrive();
        }
        assert_eq!(host.relief_granted, 1, "still inside the decay margin");
        host.observe_redrive(); // margin reached → one permit reclaimed
        assert_eq!(host.relief_granted, 0, "the extra permit went back");
        assert_eq!(host.health().tools_workers, 1);

        // And with nothing granted, further healthy cycles change nothing.
        host.observe_redrive();
        assert_eq!(host.healthy_cycles, 0, "the countdown is parked");
    })
    .await;
}

/// Decay only ever takes *idle* permits: a lane whose relief capacity is
/// genuinely in use keeps it, however healthy the queue looks, and the
/// reclaim happens later once the work finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relief_decay_takes_only_idle_permits() {
    leviath_testkit::with_tracing(|| async {
        let mut host = host_with_full_pool(1);
        host.set_dead_cycles_before_relief(1);
        let release = wedge_the_tool_lane(&mut host).await;
        host.observe_redrive();
        host.observe_redrive(); // → relief
        assert_eq!(host.relief_granted, 1);
        await_drained_queue(&mut host).await;

        // Both permits are now BUSY (the original wedge plus the queued
        // batch that relief let in) and the queue is empty: healthy by the
        // decay's measure, but there is nothing idle to take.
        for _ in 0..(HEALTHY_CYCLES_BEFORE_DECAY + 2) {
            host.observe_redrive();
        }
        assert_eq!(host.relief_granted, 1, "busy permits are never taken");
        assert_eq!(host.health().tools_workers, 2);

        // Once the work releases, the next healthy cycle reclaims it.
        release_the_lane(&mut host, &[release]).await;
        host.observe_redrive();
        assert_eq!(host.relief_granted, 0);
        assert_eq!(host.health().tools_workers, 1);
    })
    .await;
}

/// Relief is capped at one extra lane's worth over the daemon's life. If
/// that much did not help, the problem is not capacity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relief_stops_after_one_extra_lane_s_worth() {
    leviath_testkit::with_tracing(|| async {
        let mut host = host_with_full_pool(1);
        host.set_dead_cycles_before_relief(1);
        let release = wedge_the_tool_lane(&mut host).await;

        host.observe_redrive();
        host.observe_redrive();
        assert_eq!(host.relief_granted, 1);

        // Wedge it again at the wider width and keep pushing: the budget is
        // spent, so nothing more is handed out.
        let release_two = wedge_the_tool_lane(&mut host).await;
        for _ in 0..4 {
            host.observe_redrive();
        }
        assert_eq!(host.relief_granted, 1, "the budget was already spent");
        release_the_lane(&mut host, &[release, release_two]).await;
    })
    .await;
}

/// Relief is off when the operator says so, and detection carries on
/// regardless - the streak is still counted and still reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relief_can_be_turned_off_without_turning_off_detection() {
    leviath_testkit::with_tracing(|| async {
        let mut host = host_with_full_pool(1);
        host.set_dead_cycles_before_relief(0);
        let release = wedge_the_tool_lane(&mut host).await;

        for _ in 0..4 {
            host.observe_redrive();
        }
        assert_eq!(host.relief_granted, 0, "relief is disabled");
        assert_eq!(host.dead_cycles, 3, "but the streak is still counted");
        release_the_lane(&mut host, &[release]).await;
    })
    .await;
}

/// Every re-drive hands the sink a daemon-wide sample, including the quiet
/// ones. A wedged daemon produces no per-run telemetry at all, which is
/// exactly why the health sample cannot be conditional on something having
/// happened.
#[tokio::test]
async fn each_re_drive_reports_lane_health_to_the_telemetry_sink() {
    let sink = Arc::new(leviath_core::telemetry::MemorySink::default());
    let mut host = host_with_full_pool(1);
    host.world_mut()
        .world_mut()
        .insert_resource(crate::telemetry::Telemetry(sink.clone()));
    spawn(&mut host, "run-a", "agent-a");
    spawn(&mut host, "run-b", "agent-b");
    host.world_mut().run_to_fixed_point();
    host.emit_events();

    host.observe_redrive();
    host.observe_redrive();

    let samples = sink.lane_samples();
    assert_eq!(samples.len(), 2, "one per re-drive");
    assert_eq!(samples[0].dead_cycles, 0);
    assert_eq!(samples[1].dead_cycles, 1, "the streak is carried through");
    assert_eq!(samples[1].agents_active, 2);
}

/// The same tick reports which providers are out of service, and reports
/// the empty case too: a collector needs that to see a provider come back,
/// not merely stop being mentioned (issue #201).
#[tokio::test]
async fn each_re_drive_reports_providers_out_of_service() {
    let sink = Arc::new(leviath_core::telemetry::MemorySink::default());
    let mut host = host_with(vec![]);
    host.world_mut()
        .world_mut()
        .insert_resource(crate::telemetry::Telemetry(sink.clone()));
    let policy = crate::pipeline::CircuitPolicy {
        failures_before_open: 1,
        cooldown_secs: 300,
    };
    let mut circuits = crate::pipeline::ProviderCircuits::default();
    circuits.record_failure(
        "openrouter",
        leviath_providers::UnavailableReason::CreditsExhausted,
        chrono::Utc::now().timestamp(),
        &policy,
    );
    host.world_mut().world_mut().insert_resource(circuits);
    host.world_mut().world_mut().insert_resource(policy);

    host.observe_redrive();

    let samples = sink.provider_samples();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].len(), 1);
    assert_eq!(samples[0][0].provider, "openrouter");
    assert_eq!(samples[0][0].reason, "credits-exhausted");
    assert_eq!(samples[0][0].consecutive_failures, 1);
    assert!(samples[0][0].retry_in_secs > 0);
    // It also reaches `lev ps` through the health snapshot.
    assert_eq!(host.health().providers_down.len(), 1);

    // The provider recovers, and the empty sample says so.
    host.world_mut()
        .world_mut()
        .resource_mut::<crate::pipeline::ProviderCircuits>()
        .record_success("openrouter");
    host.observe_redrive();
    assert!(sink.provider_samples()[1].is_empty());
    assert!(host.health().providers_down.is_empty());
}

/// Stillness on its own is not a dead cycle. An idle daemon has nothing
/// queued and nothing to do, and counting it would fire relief at every quiet
/// spell.
#[tokio::test]
async fn an_idle_daemon_never_counts_a_dead_cycle() {
    let mut host = host_with_full_pool(1);
    host.emit_events();
    for _ in 0..3 {
        host.observe_redrive();
    }
    assert_eq!(host.dead_cycles, 0, "no pressure, no dead cycles");
}

/// Terminal agents are counted apart from live ones, so "nothing is running"
/// can't be read as "everything is running" just because finished runs are
/// still loaded.
#[tokio::test]
async fn the_lane_snapshot_counts_agents_by_status() {
    let mut host = host_with(vec![]);
    let active = spawn(&mut host, "run-active", "a");
    let paused = spawn(&mut host, "run-paused", "b");
    let waiting = spawn(&mut host, "run-waiting", "c");
    let done = spawn(&mut host, "run-done", "d");
    let idle = spawn(&mut host, "run-idle", "e");
    host.world_mut().set_status(paused, AgentStatus::Paused);
    host.world_mut().set_status(waiting, AgentStatus::Waiting);
    host.world_mut().set_status(done, AgentStatus::Complete);
    host.world_mut().set_status(idle, AgentStatus::Idle);

    let counts = host.world_mut().lane_snapshot().agents;
    assert_eq!(counts.active, 1);
    assert_eq!(counts.paused, 1);
    assert_eq!(counts.waiting, 1);
    assert_eq!(counts.terminal, 1);
    assert_eq!(counts.idle, 1);
    assert_eq!(
        counts.to_string(),
        "active=1 waiting=1 paused=1 idle=1 terminal=1"
    );
    // The other two terminal statuses land in the same bucket.
    host.world_mut().set_status(active, AgentStatus::Cancelled);
    host.world_mut().set_status(
        paused,
        AgentStatus::Error {
            message: "boom".to_string(),
        },
    );
    assert_eq!(host.world_mut().lane_snapshot().agents.terminal, 3);
}

#[tokio::test]
async fn status_and_list_reflect_registered_runs() {
    let mut host = host_with(vec![]);
    spawn(&mut host, "run-a", "agent-a");

    let status = ask(&mut host, |reply| ControlOp::Status {
        run_id: "run-a".to_string(),
        reply,
    })
    .await;
    assert_eq!(status, Some(AgentStatus::Active));

    let list = ask(&mut host, |reply| ControlOp::List { reply }).await.runs;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].run_id, "run-a");
    assert_eq!(list[0].status, AgentStatus::Active);
    // An active run is not waiting on anything, so there is nothing to explain.
    assert_eq!(list[0].wait_reason, None);

    // Unknown run.
    let none = ask(&mut host, |reply| ControlOp::Status {
        run_id: "ghost".to_string(),
        reply,
    })
    .await;
    assert_eq!(none, None);
}

/// The counterpart to `Status`: that says whether a run is done, this says
/// what it concluded. An embedder watching only for a `Completed` event had
/// no way to read a result except by scraping the log stream.
#[tokio::test]
async fn result_reports_the_submitted_answer() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "agent-a");
    // Nothing submitted yet.
    assert_eq!(
        ask(&mut host, |reply| ControlOp::Result {
            run_id: "run-a".to_string(),
            reply,
        })
        .await,
        None
    );

    host.world_mut()
        .world_mut()
        .entity_mut(e)
        .insert(crate::persistence::FinalOutput(
            leviath_core::output::FinalOutput::new(
                "<report/>",
                Some("vnd.acme+xml".to_string()),
                "summary".to_string(),
                7,
            ),
        ));
    let answer = ask(&mut host, |reply| ControlOp::Result {
        run_id: "run-a".to_string(),
        reply,
    })
    .await
    .expect("the run submitted one");
    // Byte-identical, with its label: nothing between here and the agent
    // reformats an answer.
    assert_eq!(answer.content, "<report/>");
    assert_eq!(answer.format.as_deref(), Some("vnd.acme+xml"));

    // An unknown run has no answer rather than an error.
    assert_eq!(
        ask(&mut host, |reply| ControlOp::Result {
            run_id: "ghost".to_string(),
            reply,
        })
        .await,
        None
    );
}

/// A paused standalone root whose paused snapshot has been dispatched is
/// paged out of the world: the entity is gone, but the listing and the
/// Status op still report it, and a Resume pages it back in.
#[tokio::test]
async fn a_persisted_paused_root_is_parked_and_pages_back_in() {
    let mut host = host_with(vec![]);
    // A reloader that restores the run the way `reload_run` does: paused,
    // ready to be resumed.
    host.set_reloader(Box::new(|world, run_id| {
        let mut state = agent_state(run_id);
        state.status = AgentStatus::Paused;
        Some(world.spawn_agent((state,)))
    }));
    // Parking runs the same teardown hook a reap does (sandbox + tool
    // state); record that it fired.
    let reaped = Arc::new(Mutex::new(0usize));
    let reaped_in_hook = reaped.clone();
    host.set_reaper(Box::new(move |_world, _entity| {
        *reaped_in_hook.lock().unwrap() += 1;
    }));
    let e = spawn(&mut host, "run-a", "agent-a");
    assert!(
        ask(&mut host, |reply| ControlOp::Pause {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );
    // Stamp the watermark as though the paused snapshot was dispatched.
    let mut wm = crate::pipeline::PersistWatermark::default();
    wm.stamp_status(leviath_core::run_meta::RunStatus::Paused);
    host.world_mut().world_mut().entity_mut(e).insert(wm);

    host.emit_events();

    // Paged out: the entity is despawned, the run id unmapped, and the
    // reap hook tore the agent's state down first.
    assert!(host.world.world().get::<AgentState>(e).is_none());
    assert!(!host.by_run_id.contains_key("run-a"));
    assert_eq!(*reaped.lock().unwrap(), 1);
    // But not lost: the listing still carries the paused row...
    let listing = ask(&mut host, |reply| ControlOp::List { reply }).await;
    let row = listing
        .runs
        .iter()
        .find(|r| r.run_id == "run-a")
        .expect("a parked run stays listed");
    assert_eq!(row.status, AgentStatus::Paused);
    // ...and Status answers from the parked map.
    let status = ask(&mut host, |reply| ControlOp::Status {
        run_id: "run-a".to_string(),
        reply,
    })
    .await;
    assert_eq!(status, Some(AgentStatus::Paused));

    // Resume pages the run back in through the reloader and unparks it.
    assert!(
        ask(&mut host, |reply| ControlOp::Resume {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );
    assert!(host.parked.is_empty(), "resumed run left the parked map");
    let e2 = host.by_run_id["run-a"];
    assert_eq!(host.world.agent_status(e2), Some(AgentStatus::Active));
}

/// The park gate holds until the paused state is known to be on its way to
/// disk, and never fires for a run with tree links.
#[tokio::test]
async fn a_paused_run_stays_resident_until_persisted_and_when_linked() {
    let mut host = host_with(vec![]);
    host.set_reloader(paging_reloader());
    let e = spawn(&mut host, "run-a", "agent-a");
    assert!(
        ask(&mut host, |reply| ControlOp::Pause {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );

    // Paused, but no watermark proof the paused snapshot was dispatched.
    host.emit_events();
    assert!(
        host.world.world().get::<AgentState>(e).is_some(),
        "an unpersisted pause stays resident"
    );

    // Persisted now, but carrying a child link: still resident.
    let mut wm = crate::pipeline::PersistWatermark::default();
    wm.stamp_status(leviath_core::run_meta::RunStatus::Paused);
    host.world_mut().world_mut().entity_mut(e).insert((
        wm,
        SubAgentChildren {
            children: vec![],
            max_child_depth: 1,
        },
    ));
    host.emit_events();
    assert!(
        host.world.world().get::<AgentState>(e).is_some(),
        "a run with tree links keeps the restart question open"
    );

    // Links gone: it parks - and this host has no reap hook installed, so
    // the park teardown's no-reaper path is exercised too.
    host.world_mut()
        .world_mut()
        .entity_mut(e)
        .remove::<SubAgentChildren>();
    host.emit_events();
    assert!(
        host.world.world().get::<AgentState>(e).is_none(),
        "unlinked and persisted: parked without a reaper"
    );
    assert!(host.parked.contains_key("run-a"));
}

#[tokio::test]
async fn pause_resume_cancel_by_run_id() {
    let mut host = host_with(vec![]);
    spawn(&mut host, "run-a", "agent-a");

    assert!(
        ask(&mut host, |reply| ControlOp::Pause {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );
    assert_eq!(
        host.world.agent_status(host.by_run_id["run-a"]),
        Some(AgentStatus::Paused)
    );

    // Pausing an already-paused run refuses rather than reporting success.
    assert!(
        !ask(&mut host, |reply| ControlOp::Pause {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );

    assert!(
        ask(&mut host, |reply| ControlOp::Resume {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );
    assert_eq!(
        host.world.agent_status(host.by_run_id["run-a"]),
        Some(AgentStatus::Active)
    );
    assert!(
        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );
    assert_eq!(
        host.world.agent_status(host.by_run_id["run-a"]),
        Some(AgentStatus::Cancelled)
    );

    // Unknown run ⇒ false.
    assert!(
        !ask(&mut host, |reply| ControlOp::Pause {
            run_id: "ghost".to_string(),
            reply
        })
        .await
    );
    assert!(
        !ask(&mut host, |reply| ControlOp::Resume {
            run_id: "ghost".to_string(),
            reply
        })
        .await
    );
    assert!(
        !ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "ghost".to_string(),
            reply
        })
        .await
    );
}

#[tokio::test]
async fn spawn_op_uses_installed_spawner_and_registers() {
    let mut host = host_with(vec![]);
    host.set_spawner(Box::new(|world, args| {
        Ok(world.spawn_agent((agent_state(&args.run_id),)))
    }));

    let result = ask(&mut host, |reply| ControlOp::Spawn {
        args: Box::new(SpawnArgs {
            run_id: "r1".to_string(),
            ..Default::default()
        }),
        reply,
    })
    .await;
    assert_eq!(result, Ok("r1".to_string()));

    // The run is now registered, so Status resolves it.
    let status = ask(&mut host, |reply| ControlOp::Status {
        run_id: "r1".to_string(),
        reply,
    })
    .await;
    assert_eq!(status, Some(AgentStatus::Active));
}

#[tokio::test]
async fn spawn_op_propagates_spawner_error() {
    let mut host = host_with(vec![]);
    host.set_spawner(Box::new(|_world, _args| Err("bad blueprint".to_string())));
    let result = ask(&mut host, |reply| ControlOp::Spawn {
        args: Box::new(SpawnArgs::default()),
        reply,
    })
    .await;
    assert_eq!(result, Err("bad blueprint".to_string()));
}

#[tokio::test]
async fn spawn_op_contains_a_panicking_spawner() {
    // A panic while building an agent (bad manifest, sandbox blow-up) must
    // not unwind the daemon's serve task - the run just fails to start.
    let mut host = host_with(vec![]);
    host.set_spawner(Box::new(|_world, _args| panic!("simulated spawn panic")));
    let (tx, rx) = oneshot::channel();
    crate::test_support::with_silenced_panics(|| {
        host.handle(ControlOp::Spawn {
            args: Box::new(SpawnArgs::default()),
            reply: tx,
        });
    });
    assert_eq!(rx.await.unwrap(), Err("agent spawn panicked".to_string()));
    // The host is still usable afterwards, and the run never registered.
    let status = ask(&mut host, |reply| ControlOp::Status {
        run_id: SpawnArgs::default().run_id,
        reply,
    })
    .await;
    assert!(status.is_none());
}

#[tokio::test]
async fn spawn_op_errors_without_a_spawner() {
    let mut host = host_with(vec![]);
    let result = ask(&mut host, |reply| ControlOp::Spawn {
        args: Box::new(SpawnArgs::default()),
        reply,
    })
    .await;
    assert!(result.unwrap_err().contains("cannot spawn"));
}

// ─── sub-agent bridge ──────────────────────────────────────────────────

async fn ask_sub<T>(
    host: &mut WorldHost,
    make: impl FnOnce(oneshot::Sender<T>) -> SubAgentOp,
) -> T {
    let (tx, rx) = oneshot::channel();
    host.handle_subagent(make(tx));
    rx.await.unwrap()
}

/// A spawner that adds a bare child agent and returns it.
fn child_spawner() -> Spawner {
    Box::new(|world, args| Ok(world.spawn_agent((agent_state(&args.run_id),))))
}

#[tokio::test]
async fn subagent_spawn_links_child_and_registers() {
    let mut host = host_with(vec![]);
    host.set_spawner(child_spawner());
    let parent = spawn(&mut host, "parent", "parent");

    let result = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
        args: Box::new(SpawnArgs {
            run_id: "child".to_string(),
            ..Default::default()
        }),
        parent_run_id: "parent".to_string(),
        max_depth: 3,
        reply,
    })
    .await;
    assert_eq!(result, Ok("child".to_string()));

    let child = host.by_run_id["child"];
    // The child links back to the parent at depth 1.
    let pref = host.world.world().get::<ParentRef>(child).unwrap();
    assert_eq!(pref.parent_entity, parent);
    assert_eq!(pref.depth, 1);
    // The parent tracks the child.
    let kids = host.world.world().get::<SubAgentChildren>(parent).unwrap();
    assert_eq!(kids.children, vec![child]);
}

#[tokio::test]
async fn subagent_spawn_appends_to_existing_children() {
    let mut host = host_with(vec![]);
    host.set_spawner(child_spawner());
    spawn(&mut host, "parent", "parent");
    for id in ["c1", "c2"] {
        let r = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: id.to_string(),
                ..Default::default()
            }),
            parent_run_id: "parent".to_string(),
            max_depth: 3,
            reply,
        })
        .await;
        assert!(r.is_ok());
    }
    let parent = host.by_run_id["parent"];
    let kids = host.world.world().get::<SubAgentChildren>(parent).unwrap();
    assert_eq!(kids.children.len(), 2);
}

#[tokio::test]
async fn subagent_spawn_rejects_beyond_max_depth() {
    let mut host = host_with(vec![]);
    host.set_spawner(child_spawner());
    spawn(&mut host, "parent", "parent");
    let result = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
        args: Box::new(SpawnArgs {
            run_id: "child".to_string(),
            ..Default::default()
        }),
        parent_run_id: "parent".to_string(),
        max_depth: 0, // child would be depth 1 > 0
        reply,
    })
    .await;
    assert!(result.unwrap_err().contains("depth limit"));
    assert!(!host.by_run_id.contains_key("child"));
}

#[tokio::test]
async fn subagent_spawn_unknown_parent_and_no_spawner_and_spawner_error() {
    // Unknown parent.
    let mut host = host_with(vec![]);
    host.set_spawner(child_spawner());
    let r = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
        args: Box::new(SpawnArgs::default()),
        parent_run_id: "ghost".to_string(),
        max_depth: 3,
        reply,
    })
    .await;
    assert!(r.unwrap_err().contains("not live"));

    // No spawner installed.
    let mut host2 = host_with(vec![]);
    spawn(&mut host2, "parent", "parent");
    let r = ask_sub(&mut host2, |reply| SubAgentOp::Spawn {
        args: Box::new(SpawnArgs::default()),
        parent_run_id: "parent".to_string(),
        max_depth: 3,
        reply,
    })
    .await;
    assert!(r.unwrap_err().contains("cannot spawn"));

    // Spawner rejects.
    let mut host3 = host_with(vec![]);
    host3.set_spawner(Box::new(|_w, _a| Err("bad blueprint".to_string())));
    spawn(&mut host3, "parent", "parent");
    let r = ask_sub(&mut host3, |reply| SubAgentOp::Spawn {
        args: Box::new(SpawnArgs::default()),
        parent_run_id: "parent".to_string(),
        max_depth: 3,
        reply,
    })
    .await;
    assert_eq!(r, Err("bad blueprint".to_string()));
}

/// A parent asking after a finished child receives its answer, not just a
/// status word. `wait_for_agent`'s schema has always promised this.
#[tokio::test]
async fn subagent_check_carries_the_childs_submitted_answer() {
    let mut host = host_with(vec![]);
    let entity = spawn(&mut host, "run-a", "run-a");
    host.world
        .world_mut()
        .entity_mut(entity)
        .insert(crate::persistence::FinalOutput(
            leviath_core::output::FinalOutput::new(
                "changed src/lib.rs and its test",
                Some("markdown".to_string()),
                "fix_worker".to_string(),
                5,
            ),
        ));
    let report = ask_sub(&mut host, |reply| SubAgentOp::Check {
        run_id: "run-a".to_string(),
        reply,
    })
    .await
    .expect("the run is live");
    let output = report.final_output.expect("the answer came back");
    assert_eq!(output.content, "changed src/lib.rs and its test");
    assert_eq!(output.stage, "fix_worker");
}

#[tokio::test]
async fn subagent_check_reports_status_or_none() {
    let mut host = host_with(vec![]);
    spawn(&mut host, "run-a", "run-a");
    let status = ask_sub(&mut host, |reply| SubAgentOp::Check {
        run_id: "run-a".to_string(),
        reply,
    })
    .await;
    assert_eq!(
        status,
        Some(SubAgentReport {
            status: AgentStatus::Active,
            // A working child has nothing to hand back yet.
            final_output: None,
        })
    );

    let none = ask_sub(&mut host, |reply| SubAgentOp::Check {
        run_id: "ghost".to_string(),
        reply,
    })
    .await;
    assert_eq!(none, None);
}

/// `send_to_agent` and `kill_agent` took any run id at all, so an agent
/// could reach into an unrelated run - cancel it, inject text, or hand it
/// data that arrives `Public` regardless of the sender's taint. That last
/// one is a laundering channel straight through taint tracking.
/// The converse of the refusal: a run the caller *did* spawn is reachable,
/// so scoping did not simply block everything. This also walks the
/// `SubAgentChildren` link rather than matching the caller itself.
#[tokio::test]
async fn subagent_ops_reach_a_run_the_caller_spawned() {
    let mut host = host_with(vec![]);
    let parent = spawn(&mut host, "parent", "parent");
    let child = spawn(&mut host, "child", "child");
    host.world_mut()
        .world_mut()
        .entity_mut(parent)
        .insert(SubAgentChildren {
            children: vec![child],
            max_child_depth: 3,
        });

    let delivered = ask_sub(&mut host, |reply| SubAgentOp::Send {
        run_id: "child".to_string(),
        caller_run_id: "parent".to_string(),
        content: "carry on".to_string(),
        target_region: None,
        reply,
    })
    .await;
    assert!(delivered, "a run we spawned is ours to message");
}

#[tokio::test]
async fn subagent_ops_refuse_a_run_outside_the_callers_tree() {
    let mut host = host_with(vec![]);
    spawn(&mut host, "run-a", "run-a");
    spawn(&mut host, "outsider", "outsider");

    let delivered = ask_sub(&mut host, |reply| SubAgentOp::Send {
        run_id: "outsider".to_string(),
        caller_run_id: "run-a".to_string(),
        content: "take this".to_string(),
        target_region: None,
        reply,
    })
    .await;
    assert!(!delivered, "a run we did not spawn is not ours to message");

    let killed = ask_sub(&mut host, |reply| SubAgentOp::Kill {
        run_id: "outsider".to_string(),
        caller_run_id: "run-a".to_string(),
        reply,
    })
    .await;
    assert!(!killed, "nor ours to cancel");

    // A run id that resolves to nothing at all is likewise not ours - the
    // walk never starts, rather than defaulting to reachable.
    let phantom = ask_sub(&mut host, |reply| SubAgentOp::Send {
        run_id: "no-such-run".to_string(),
        caller_run_id: "run-a".to_string(),
        content: "hello?".to_string(),
        target_region: None,
        reply,
    })
    .await;
    assert!(!phantom, "an unknown run id is in nobody's tree");
}

#[tokio::test]
async fn subagent_send_delivers_to_inbox() {
    let mut host = host_with(vec![]);
    spawn(&mut host, "run-a", "run-a");
    let ok = ask_sub(&mut host, |reply| SubAgentOp::Send {
        run_id: "run-a".to_string(),
        caller_run_id: "run-a".to_string(),
        content: "hello child".to_string(),
        target_region: None,
        reply,
    })
    .await;
    assert!(ok);
}

/// The op's `target_region` reaches the named region, not just the
/// default conversation. The `send_to_agent` tool advertised this
/// parameter from the start, but the op had no field to carry it, so it
/// was silently dropped on this path.
#[tokio::test]
async fn subagent_send_delivers_into_the_target_region() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    host.world
        .world_mut()
        .get_mut::<crate::components::ContextWindow>(e)
        .unwrap()
        .add_region(Region::new(
            "notes".to_string(),
            RegionKind::Clearable,
            5000,
        ));

    let ok = ask_sub(&mut host, |reply| SubAgentOp::Send {
        run_id: "run-a".to_string(),
        caller_run_id: "run-a".to_string(),
        content: "filed under notes".to_string(),
        target_region: Some("notes".to_string()),
        reply,
    })
    .await;
    assert!(ok);

    host.world.tick(); // intake → inbox → window
    let window = host
        .world
        .world()
        .get::<crate::components::ContextWindow>(e)
        .unwrap();
    assert!(window.get_region("notes").unwrap().current_tokens > 0);
    assert_eq!(window.get_region("conversation").unwrap().current_tokens, 0);
}

#[tokio::test]
async fn subagent_kill_cancels_the_whole_tree() {
    let mut host = host_with(vec![]);
    host.set_spawner(child_spawner());
    spawn(&mut host, "parent", "parent");
    ask_sub(&mut host, |reply| SubAgentOp::Spawn {
        args: Box::new(SpawnArgs {
            run_id: "child".to_string(),
            ..Default::default()
        }),
        parent_run_id: "parent".to_string(),
        max_depth: 3,
        reply,
    })
    .await
    .unwrap();

    let ok = ask_sub(&mut host, |reply| SubAgentOp::Kill {
        run_id: "parent".to_string(),
        caller_run_id: "parent".to_string(),
        reply,
    })
    .await;
    assert!(ok);
    assert_eq!(
        host.world.agent_status(host.by_run_id["parent"]),
        Some(AgentStatus::Cancelled)
    );
    assert_eq!(
        host.world.agent_status(host.by_run_id["child"]),
        Some(AgentStatus::Cancelled)
    );

    // Killing an unknown run is a no-op.
    let miss = ask_sub(&mut host, |reply| SubAgentOp::Kill {
        run_id: "ghost".to_string(),
        caller_run_id: "ghost".to_string(),
        reply,
    })
    .await;
    assert!(!miss);
}

/// A user-facing cancel must reach the sub-agent tree, not just the root -
/// otherwise the children keep running with nobody to report to. Before this,
/// only the model-facing `kill_agent` tool cascaded.
#[tokio::test]
async fn cancel_cascades_to_the_whole_tree() {
    let mut host = host_with(vec![]);
    host.set_spawner(child_spawner());
    spawn(&mut host, "parent", "parent");
    ask_sub(&mut host, |reply| SubAgentOp::Spawn {
        args: Box::new(SpawnArgs {
            run_id: "child".to_string(),
            ..Default::default()
        }),
        parent_run_id: "parent".to_string(),
        max_depth: 3,
        reply,
    })
    .await
    .unwrap();

    assert!(
        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "parent".to_string(),
            reply
        })
        .await
    );
    assert_eq!(
        host.world.agent_status(host.by_run_id["child"]),
        Some(AgentStatus::Cancelled),
        "cancelling the parent cancels its children"
    );
}

/// A child that was already reaped is skipped rather than tripping the
/// cancel: `SubAgentChildren` still names it, but the entity is gone, so
/// there is no agent id to close interactions for.
#[tokio::test]
async fn cancel_tolerates_a_child_that_has_already_been_reaped() {
    let mut host = host_with(vec![]);
    let parent = spawn(&mut host, "parent", "parent");
    let ghost = host.world_mut().spawn_agent((agent_state("ghost"),));
    host.world_mut()
        .world_mut()
        .entity_mut(parent)
        .insert(SubAgentChildren {
            children: vec![ghost],
            max_child_depth: 3,
        });
    host.world_mut().world_mut().despawn(ghost);

    assert!(
        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "parent".to_string(),
            reply
        })
        .await,
        "the parent is still cancelled"
    );
    assert_eq!(
        host.world.agent_status(parent),
        Some(AgentStatus::Cancelled)
    );
}

/// Cancelling a run closes its open prompts. The blocked `ask` waits off the
/// lane, so it no longer starves anyone, but a prompt left open for a run
/// that no longer exists is still surfaced to whoever is meant to answer it.
#[tokio::test]
async fn cancel_closes_the_runs_open_interactions() {
    let mut host = host_with(vec![]);
    let hub = host.interactions();
    spawn(&mut host, "run-a", "agent-a");

    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move {
        backend
            .ask(InteractionRequest::free_text("q", "ask", "stage", true))
            .await
    });
    // Wait for the ask to register, then let the host emit it - so the
    // emitted-interaction set is non-empty and the cancel has something to
    // prune, rather than pruning an empty set.
    while hub.pending().is_empty() {
        tokio::task::yield_now().await;
    }
    host.emit_events();
    assert!(
        !host.emitted_interactions.is_empty(),
        "the open request was emitted"
    );

    ask(&mut host, |reply| ControlOp::Cancel {
        run_id: "run-a".to_string(),
        reply,
    })
    .await;

    // The blocked future is released rather than parked forever. Bounded,
    // because the regression this guards *is* an unbounded wait: without the
    // per-agent cancel this await simply never returns, and a test that hangs
    // rather than fails is worse than no test.
    tokio::time::timeout(std::time::Duration::from_secs(5), asking)
        .await
        .expect("cancelling the run releases its blocked ask")
        .expect("the ask task did not panic");
    // ...and the request stops being advertised to `lev respond` / the
    // dashboard for a run that is going away.
    assert!(hub.pending().is_empty(), "no orphaned prompt is left open");
    assert!(
        host.emitted_interactions.is_empty(),
        "and it is pruned from the emitted set, not re-announced forever"
    );
}

/// The floor under every kill: a run the reloader can't rebuild must still be
/// terminated, via the daemon's on-disk force-terminator. Replying `false` and
/// writing nothing is what made such a run permanent.
#[tokio::test]
async fn cancel_falls_back_to_the_force_terminator_when_the_world_cannot_hold_the_run() {
    let mut host = host_with(vec![]);
    // A reloader that always declines - the deleted-blueprint case.
    host.set_reloader(Box::new(|_world, _run_id| None));
    let terminated = Arc::new(Mutex::new(Vec::new()));
    host.set_force_terminator(recording_terminator(terminated.clone()));

    assert!(
        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "unreloadable".to_string(),
            reply
        })
        .await,
        "a run that can't be reloaded is still terminated"
    );
    assert!(
        !ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "never-existed".to_string(),
            reply
        })
        .await,
        "`false` is reserved for a run that exists nowhere"
    );
    assert_eq!(
        *terminated.lock().unwrap(),
        vec!["unreloadable".to_string(), "never-existed".to_string()]
    );
}

/// A live run is cancelled in the world; the on-disk fallback is not consulted
/// (the persistence lane records the status change).
#[tokio::test]
async fn cancel_does_not_force_terminate_a_run_it_could_cancel() {
    let mut host = host_with(vec![]);
    spawn(&mut host, "run-a", "agent-a");
    let terminated = Arc::new(Mutex::new(Vec::new()));
    host.set_force_terminator(recording_terminator(terminated.clone()));

    assert!(
        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "run-a".to_string(),
            reply
        })
        .await
    );
    assert_eq!(
        host.world.agent_status(host.by_run_id["run-a"]),
        Some(AgentStatus::Cancelled)
    );
    assert!(
        terminated.lock().unwrap().is_empty(),
        "the disk fallback stayed unused"
    );
}

/// Agents that enter the world outside a `Spawn` op (fan-out workers, built
/// directly by the fan-out spawner) are adopted into the run-id map, so they
/// are listed, reaped and - the point here - cancellable by id. Left
/// unregistered, a cancel missed the map and paged a *second* copy of the run
/// in from disk while the original kept going.
#[tokio::test]
async fn unregistered_world_agents_are_adopted_and_become_cancellable() {
    let mut host = host_with(vec![]);
    let entity = host.world_mut().spawn_agent((
        agent_state("worker"),
        RunMetadata {
            run_id: "worker-run".to_string(),
            agent_name: "w".to_string(),
            agent_path: String::new(),
            task: String::new(),
            model: None,
            workdir: String::new(),
            num_stages: 1,
            started_at: 0,
            parent_run_id: None,
            metadata: Default::default(),
            callback_url: None,
            callback_secret: None,
            title: None,
            unattended: false,
            read_paths: None,
            output_request: None,
        },
    ));
    assert!(
        !host.by_run_id.contains_key("worker-run"),
        "not registered by the spawn itself"
    );

    host.emit_events();

    assert_eq!(host.live_entity("worker-run"), Some(entity), "adopted");
    // A reloader that would mint a duplicate if the map were still missing it.
    host.set_reloader(paging_reloader());
    assert!(
        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "worker-run".to_string(),
            reply
        })
        .await
    );
    assert_eq!(
        host.world.agent_status(entity),
        Some(AgentStatus::Cancelled),
        "the original entity is cancelled, not a reloaded copy"
    );
}

#[tokio::test]
async fn interaction_ops_list_answer_and_cancel() {
    let mut host = host_with(vec![]);
    let hub = host.interactions();
    let backend = hub.backend_for("agent-a");

    // An agent's ask is registered on the hub.
    let asking = tokio::spawn(async move {
        backend
            .ask(leviath_core::interaction::InteractionRequest::free_text(
                "q1", "prompt?", "stage", true,
            ))
            .await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // ListInteractions surfaces it.
    let list = ask(&mut host, |reply| ControlOp::ListInteractions { reply }).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0, "agent-a");

    // AnswerInteraction fulfils it.
    let ok = ask(&mut host, |reply| ControlOp::AnswerInteraction {
        response: leviath_core::interaction::InteractionResponse::text("q1", "hi"),
        reply,
    })
    .await;
    assert!(ok);
    assert_eq!(asking.await.unwrap().value.as_deref(), Some("hi"));

    // CancelInteraction on an unknown id ⇒ false.
    let cancelled = ask(&mut host, |reply| ControlOp::CancelInteraction {
        request_id: "gone".to_string(),
        reply,
    })
    .await;
    assert!(!cancelled);
}

#[tokio::test]
async fn cancel_interaction_op_wakes_asker() {
    let mut host = host_with(vec![]);
    let backend = host.interactions().backend_for("agent-a");
    let asking = tokio::spawn(async move {
        backend
            .ask(leviath_core::interaction::InteractionRequest::free_text(
                "q2", "p", "s", true,
            ))
            .await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let ok = ask(&mut host, |reply| ControlOp::CancelInteraction {
        request_id: "q2".to_string(),
        reply,
    })
    .await;
    assert!(ok);
    assert_eq!(asking.await.unwrap().request_id, "q2");
}

#[tokio::test]
async fn message_op_is_delivered() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "agent-a");

    let ok = ask(&mut host, |reply| ControlOp::Message {
        agent_id: "agent-a".to_string(),
        content: "hi".to_string(),
        target_region: Some("conversation".to_string()),
        reply,
    })
    .await;
    assert!(ok);

    // One tick delivers the message into context.
    host.world_mut().tick();
    assert!(
        host.world
            .world()
            .get::<crate::components::ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens
            > 0
    );
}

#[tokio::test]
async fn serve_drives_agents_and_handles_ops_until_shutdown() {
    let mut host = host_with(vec![text("t1"), text("t2"), text("t3"), text("t4")]);
    spawn(&mut host, "run-a", "agent-a");
    let shutdown = host.world_mut().shutdown_handle();
    // Watch the event stream rather than the entity: a run that finishes is
    // reaped out of the world once it has been seen terminal, so the
    // broadcast is the durable record that it ran to completion.
    let mut events = host.subscribe();
    let (op_tx, op_rx) = mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        host.serve(op_rx).await;
    });

    // Query status via the live serve loop.
    let (tx, rx) = oneshot::channel();
    op_tx
        .send(ControlOp::Status {
            run_id: "run-a".to_string(),
            reply: tx,
        })
        .unwrap();
    let _ = rx.await.unwrap();

    // The agent ran to completion under the serve loop.
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(WorldEvent::Completed { run_id, status, .. }) = events.recv().await {
                return (run_id, status);
            }
        }
    })
    .await
    .expect("the serve loop must drive the agent to a terminal status");
    assert_eq!(completed, ("run-a".to_string(), "complete".to_string()));

    shutdown.notify_one();
    handle.await.unwrap();
}

#[tokio::test]
async fn serve_awaits_spawn_preprocessor_before_spawning() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut host = host_with(vec![]);
    let ran = Arc::new(AtomicBool::new(false));
    let ran_pp = ran.clone();
    host.set_spawn_preprocessor(Box::new(move |_args| {
        let ran = ran_pp.clone();
        Box::pin(async move {
            ran.store(true, Ordering::SeqCst);
        })
    }));
    let ran_spawn = ran.clone();
    host.set_spawner(Box::new(move |world, args| {
        // The preprocessor must have completed before the spawner runs.
        assert!(ran_spawn.load(Ordering::SeqCst));
        Ok(world.spawn_agent((agent_state(&args.run_id),)))
    }));
    let (op_tx, op_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        host.serve(op_rx).await;
    });
    let (tx, rx) = oneshot::channel();
    op_tx
        .send(ControlOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "rp".to_string(),
                ..Default::default()
            }),
            reply: tx,
        })
        .unwrap();
    let result = rx.await.unwrap();
    drop(op_tx); // close the channel so serve() returns
    handle.await.unwrap();
    assert_eq!(result, Ok("rp".to_string()));
    assert!(ran.load(Ordering::SeqCst), "preprocessor ran");
}

#[tokio::test]
async fn serve_awaits_preprocessor_for_subagent_spawn() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut host = host_with(vec![]);
    host.set_spawner(child_spawner());
    // An inert parent: no `ReadyToInfer`, so it never infers, never errors on
    // the empty response script, and stays live for the child to attach to.
    let parent = host.world_mut().spawn_agent((agent_state("parent"),));
    host.register("parent", parent);
    // Count preprocessor invocations: it must fire for the sub-agent Spawn,
    // and NOT for the non-Spawn Check op (the `_ => None` arm).
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_pp = calls.clone();
    host.set_spawn_preprocessor(Box::new(move |_args| {
        let calls = calls_pp.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
        })
    }));
    let sub_tx = host.subagent_sender();
    let shutdown = host.world_mut().shutdown_handle();
    let (op_tx, op_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        host.serve(op_rx).await;
    });

    // A non-Spawn sub-agent op does not invoke the preprocessor.
    let (ctx, crx) = oneshot::channel();
    sub_tx
        .send(SubAgentOp::Check {
            run_id: "parent".to_string(),
            reply: ctx,
        })
        .unwrap();
    let _ = crx.await.unwrap();

    // A sub-agent Spawn does.
    let (stx, srx) = oneshot::channel();
    sub_tx
        .send(SubAgentOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "child".to_string(),
                ..Default::default()
            }),
            parent_run_id: "parent".to_string(),
            max_depth: 3,
            reply: stx,
        })
        .unwrap();
    assert_eq!(srx.await.unwrap(), Ok("child".to_string()));

    shutdown.notify_one();
    drop(op_tx);
    handle.await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the Spawn preprocessed"
    );
}

#[tokio::test]
async fn serve_spawns_without_a_preprocessor() {
    // A Spawn op through serve() with no preprocessor installed exercises the
    // `None` arm of the preprocessor branch.
    let mut host = host_with(vec![]);
    host.set_spawner(Box::new(|world, args| {
        Ok(world.spawn_agent((agent_state(&args.run_id),)))
    }));
    let (op_tx, op_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        host.serve(op_rx).await;
    });
    let (tx, rx) = oneshot::channel();
    op_tx
        .send(ControlOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "np".to_string(),
                ..Default::default()
            }),
            reply: tx,
        })
        .unwrap();
    let result = rx.await.unwrap();
    drop(op_tx);
    handle.await.unwrap();
    assert_eq!(result, Ok("np".to_string()));
}

#[tokio::test]
async fn shutdown_op_stops_the_serve_loop() {
    let mut host = host_with(vec![]);
    let (op_tx, op_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move { host.serve(op_rx).await });

    let (tx, rx) = oneshot::channel();
    op_tx.send(ControlOp::Shutdown { reply: tx }).unwrap();
    assert!(rx.await.unwrap());
    // The serve loop returns once the world's shutdown is signalled.
    handle.await.unwrap();
}

#[tokio::test]
async fn flush_and_stop_delegates_to_the_world() {
    // The host's flush-and-stop drains the world's persistence lane; calling it
    // (even with no agents) returns cleanly and is idempotent.
    let mut host = host_with(vec![]);
    host.flush_and_stop().await;
    host.flush_and_stop().await; // second call is a no-op
}

#[tokio::test]
async fn serve_loop_services_subagent_ops_via_the_sender() {
    let mut host = host_with(vec![]);
    spawn(&mut host, "run-a", "run-a");
    let sub_tx = host.subagent_sender();
    let (op_tx, op_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move { host.serve(op_rx).await });

    // A Check submitted on the sub-agent channel is serviced by the serve loop.
    let (tx, rx) = oneshot::channel();
    sub_tx
        .send(SubAgentOp::Check {
            run_id: "run-a".to_string(),
            reply: tx,
        })
        .unwrap();
    assert!(rx.await.unwrap().is_some());

    let (stx, srx) = oneshot::channel();
    op_tx.send(ControlOp::Shutdown { reply: stx }).unwrap();
    assert!(srx.await.unwrap());
    handle.await.unwrap();
}

#[test]
fn status_str_covers_all_variants() {
    assert_eq!(status_str(&AgentStatus::Idle), "idle");
    assert_eq!(status_str(&AgentStatus::Active), "active");
    assert_eq!(status_str(&AgentStatus::Paused), "paused");
    assert_eq!(status_str(&AgentStatus::Waiting), "waiting");
    assert_eq!(status_str(&AgentStatus::Complete), "complete");
    assert_eq!(
        status_str(&AgentStatus::Error {
            message: "x".to_string()
        }),
        "error"
    );
    assert_eq!(status_str(&AgentStatus::Cancelled), "cancelled");
}

/// The `Completed` event carries the answer, read off the live entity rather
/// than off disk: the event fires the moment the run goes terminal, and the
/// persist tick that writes `meta.json` has not necessarily run yet. A
/// subscriber that had to poll for the file would see the completion first
/// and the answer some time later, or never.
#[tokio::test]
async fn a_completed_event_carries_the_answer() {
    let mut host = host_with(vec![text("done")]);
    let mut rx = host.subscribe();
    let entity = spawn(&mut host, "run-out", "agent-out");
    host.world_mut()
        .world_mut()
        .entity_mut(entity)
        .insert(crate::persistence::FinalOutput(
            leviath_core::output::FinalOutput::new(
                "what the run concluded",
                Some("markdown".to_string()),
                "summary".to_string(),
                7,
            ),
        ));

    host.world_mut().run_until_idle(20).await;
    host.emit_events();

    let answer = std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|e| match e {
            WorldEvent::Completed { final_output, .. } => Some(final_output),
            _ => None,
        })
        .expect("the run completed");
    assert_eq!(
        answer.expect("and it had an answer").content,
        "what the run concluded"
    );
}

/// A run that never submitted carries no answer, rather than an empty one a
/// subscriber would have to tell apart from a real empty answer.
#[tokio::test]
async fn a_completed_event_without_an_answer_carries_none() {
    let mut host = host_with(vec![text("done")]);
    let mut rx = host.subscribe();
    spawn(&mut host, "run-silent", "agent-silent");

    host.world_mut().run_until_idle(20).await;
    host.emit_events();

    let answer = std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|e| match e {
            WorldEvent::Completed { final_output, .. } => Some(final_output),
            _ => None,
        })
        .expect("the run completed");
    assert!(answer.is_none());
}

#[tokio::test]
async fn emit_events_broadcasts_agent_changes() {
    let mut host = host_with(vec![text("done")]);
    let mut rx = host.subscribe();
    let entity = spawn(&mut host, "run-a", "agent-a");
    // Attach run metadata so the `Spawned` event carries the blueprint name.
    host.world_mut()
        .world_mut()
        .entity_mut(entity)
        .insert(RunMetadata {
            run_id: "run-a".to_string(),
            agent_name: "coder".to_string(),
            agent_path: "/a".to_string(),
            task: "t".to_string(),
            model: None,
            workdir: "/w".to_string(),
            num_stages: 1,
            started_at: 0,
            parent_run_id: None,
            metadata: std::collections::HashMap::new(),
            callback_url: None,
            callback_secret: None,
            title: None,
            unattended: false,
            read_paths: None,
            output_request: None,
        });

    // First emission after spawn: Spawned + Status + Tokens + Context.
    host.emit_events();
    let first: Vec<WorldEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        first
            .iter()
            .any(|e| matches!(e, WorldEvent::Spawned { .. }))
    );
    assert!(first.iter().any(|e| matches!(e, WorldEvent::Status { .. })));
    assert!(first.iter().any(|e| matches!(e, WorldEvent::Tokens { .. })));
    assert!(
        first
            .iter()
            .any(|e| matches!(e, WorldEvent::Context { .. }))
    );

    // A second emission with nothing changed emits nothing (skip branches).
    host.emit_events();
    assert!(rx.try_recv().is_err());

    // Drive to completion, then emit: a terminal `Completed` fires.
    host.world_mut().run_until_idle(20).await;
    host.emit_events();
    let done: Vec<WorldEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        done.iter()
            .any(|e| matches!(e, WorldEvent::Completed { .. }))
    );

    // Once terminal and unchanged, a further emission fires nothing.
    host.emit_events();
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok())
            .collect::<Vec<_>>()
            .is_empty()
    );
}

#[tokio::test]
async fn emit_events_unloads_terminal_agents_when_safe() {
    let mut host = host_with(vec![]);

    // A terminal root: emitted on the first pass, unloaded on the second.
    let root = {
        let mut s = agent_state("root");
        s.status = AgentStatus::Complete;
        host.world.world_mut().spawn(s).id()
    };
    host.register("root", root);
    host.emit_events();
    assert!(
        host.live_entity("root").is_some(),
        "not reaped on the first terminal pass (event must go out first)"
    );
    host.emit_events();
    assert!(host.live_entity("root").is_none(), "reaped after emit");
    assert!(
        host.world.world().get::<AgentState>(root).is_none(),
        "entity despawned"
    );

    // A terminal child under a LIVE (Active) parent is deferred.
    let parent = host.world.world_mut().spawn(agent_state("parent")).id();
    host.register("parent", parent);
    let child = {
        let mut s = agent_state("child");
        s.status = AgentStatus::Complete;
        host.world
            .world_mut()
            .spawn((
                s,
                ParentRef {
                    parent_entity: parent,
                    parent_agent_id: "parent".to_string(),
                    depth: 1,
                },
            ))
            .id()
    };
    host.register("child", child);
    host.emit_events();
    host.emit_events();
    assert!(
        host.live_entity("child").is_some(),
        "not reaped while its parent is live"
    );

    // Once the parent is terminal, the child becomes reapable.
    host.world
        .world_mut()
        .get_mut::<AgentState>(parent)
        .unwrap()
        .status = AgentStatus::Complete;
    host.emit_events();
    host.emit_events();
    assert!(
        host.live_entity("child").is_none(),
        "reaped once its parent is terminal"
    );

    // A terminal child whose parent entity was despawned is also reapable.
    let ghost = host.world.world_mut().spawn_empty().id();
    host.world.world_mut().despawn(ghost);
    let orphan = {
        let mut s = agent_state("orphan");
        s.status = AgentStatus::Complete;
        host.world
            .world_mut()
            .spawn((
                s,
                ParentRef {
                    parent_entity: ghost,
                    parent_agent_id: "gone".to_string(),
                    depth: 1,
                },
            ))
            .id()
    };
    host.register("orphan", orphan);
    host.emit_events();
    host.emit_events();
    assert!(
        host.live_entity("orphan").is_none(),
        "reaped: parent entity despawned"
    );
}

#[tokio::test]
async fn emit_events_does_not_reap_non_terminal_agents() {
    let mut host = host_with(vec![]);
    let active = host.world.world_mut().spawn(agent_state("active")).id();
    host.register("active", active);
    host.emit_events();
    host.emit_events();
    assert!(host.live_entity("active").is_some());
}

#[tokio::test]
async fn reaper_runs_once_per_agent_before_despawn() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut host = host_with(vec![]);

    // The reap hook records that it saw a still-live entity, proving it runs
    // before despawn. A `static` counter dodges the `'static` closure bound.
    static SEEN_LIVE: AtomicUsize = AtomicUsize::new(0);
    SEEN_LIVE.store(0, Ordering::SeqCst);
    host.set_reaper(Box::new(|world, entity| {
        // Branch-free (`live as usize`) so the whole closure body is covered
        // by a single firing; the assertion below confirms `live` was true.
        let live = world.world().get::<AgentState>(entity).is_some();
        SEEN_LIVE.fetch_add(live as usize, Ordering::SeqCst);
    }));

    let root = {
        let mut s = agent_state("root");
        s.status = AgentStatus::Complete;
        host.world.world_mut().spawn(s).id()
    };
    host.register("root", root);
    host.emit_events(); // first pass: emit terminal event, not yet reaped
    assert_eq!(SEEN_LIVE.load(Ordering::SeqCst), 0);
    host.emit_events(); // second pass: reaper fires, then despawn
    assert!(host.live_entity("root").is_none(), "reaped after emit");
    assert_eq!(
        SEEN_LIVE.load(Ordering::SeqCst),
        1,
        "reaper ran exactly once, while the entity was still live"
    );
}

// ─── Runs that finished but are still worth reporting (issue #205) ───────

/// Unload `run_id` the way the daemon does: an agent that has gone terminal
/// with `status`, then the two passes it takes to emit and reap it.
fn unload_with(host: &mut WorldHost, run_id: &str, status: AgentStatus) {
    let mut s = agent_state(run_id);
    s.status = status;
    let e = host.world.world_mut().spawn(s).id();
    host.register(run_id, e);
    host.emit_events();
    host.emit_events();
}

/// The failure behind issue #205: a run that died on its first inference was
/// unloaded a pass later and vanished, so a scheduler polling the listing
/// could not tell it from a run that had never been spawned. It now keeps
/// its place, and keeps the whole error rather than the status word - which
/// is the reason the row is built from the world and not from `Emitted`,
/// whose `status` is a `&'static str`.
#[tokio::test]
async fn an_unloaded_run_stays_in_the_listing_with_the_reason_it_ended() {
    let mut host = host_with(vec![]);
    let died = AgentStatus::Error {
        message: "HTTP 402 Payment Required".to_string(),
    };
    unload_with(&mut host, "worker-1", died.clone());

    assert!(host.live_entity("worker-1").is_none(), "unloaded");
    let listing = ask(&mut host, |reply| ControlOp::List { reply }).await;
    assert!(listing.runs.is_empty(), "nothing is running");
    assert_eq!(listing.finished.len(), 1);
    assert_eq!(listing.finished[0].run_id, "worker-1");
    assert_eq!(listing.finished[0].status, died);
    // A run that never persisted a snapshot still has to show an age, or the
    // listing answers "when did it die" with a dash.
    assert!(listing.finished[0].last_progress_at.is_some());
}

/// The window is what keeps the listing from growing without end.
#[tokio::test]
async fn an_unloaded_run_leaves_the_listing_once_it_is_stale() {
    let mut host = host_with(vec![]);
    unload_with(&mut host, "worker-1", AgentStatus::Complete);
    let window = DEFAULT_FINISHED_RETENTION_SECS as i64;
    let at = host.finished.front().expect("just unloaded").0;

    // Inside the window it stays...
    host.prune_finished(at + window);
    assert_eq!(host.finished().len(), 1);
    // ...and one second past it, it goes.
    host.prune_finished(at + window + 1);
    assert!(host.finished().is_empty());
}

/// `0` is how an operator asks for the old behaviour back.
#[tokio::test]
async fn a_zero_window_keeps_nothing() {
    let mut host = host_with(vec![]);
    host.set_finished_retention_secs(0);
    unload_with(&mut host, "worker-1", AgentStatus::Complete);

    assert!(host.live_entity("worker-1").is_none(), "still unloaded");
    assert!(host.finished().is_empty());
}

/// However often a run is recorded, it is one row - the newest.
#[tokio::test]
async fn a_run_is_listed_once_however_often_it_is_recorded() {
    let mut host = host_with(vec![]);
    let entry = |status| RunListEntry {
        run_id: "worker-1".to_string(),
        status,
        wait_reason: None,
        stage: "work".to_string(),
        stage_index: None,
        num_stages: None,
        iteration: 0,
        tool_calls: 0,
        last_progress_at: None,
        unattended: false,
        empty_output: false,
        read_paths: None,
        has_final_output: false,
    };
    host.record_finished(entry(AgentStatus::Cancelled), 100);
    host.record_finished(entry(AgentStatus::Complete), 200);

    let finished = host.finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].status, AgentStatus::Complete);
}

/// A factory that finishes runs faster than the window empties keeps the
/// most recent ones rather than growing for ever.
#[tokio::test]
async fn the_listing_of_finished_runs_is_capped() {
    let mut host = host_with(vec![]);
    for i in 0..=MAX_RETAINED_FINISHED {
        host.record_finished(
            RunListEntry {
                run_id: format!("worker-{i}"),
                status: AgentStatus::Complete,
                wait_reason: None,
                stage: "work".to_string(),
                stage_index: None,
                num_stages: None,
                iteration: 0,
                tool_calls: 0,
                last_progress_at: None,
                unattended: false,
                empty_output: false,
                read_paths: None,
                has_final_output: false,
            },
            100,
        );
    }

    let finished = host.finished();
    assert_eq!(finished.len(), MAX_RETAINED_FINISHED);
    assert_eq!(
        finished[0].run_id, "worker-1",
        "the oldest is the one dropped"
    );
}

/// The status query agrees with the listing rather than reporting no such
/// run a moment after the listing still had one.
#[tokio::test]
async fn the_status_of_an_unloaded_run_is_still_answerable() {
    let mut host = host_with(vec![]);
    unload_with(&mut host, "worker-1", AgentStatus::Complete);

    let status = ask(&mut host, |reply| ControlOp::Status {
        run_id: "worker-1".to_string(),
        reply,
    })
    .await;
    assert_eq!(status, Some(AgentStatus::Complete));
}

/// Spawn a `Waiting` agent (optionally with an extra marker component) and
/// register it under `run_id`.
fn register_waiting(host: &mut WorldHost, run_id: &str) -> Entity {
    let mut s = agent_state(run_id);
    s.status = AgentStatus::Waiting;
    let e = host.world.world_mut().spawn(s).id();
    host.register(run_id, e);
    e
}

/// Regression: a `Waiting` agent must NEVER be unloaded. Every `Waiting`
/// state carries a live, unpersisted continuation, so flushing it to disk
/// strands the run. The worst case is an agent parked on a human approval
/// (`AwaitingInteraction`): unloading it means the answer has no entity to
/// wake and the run hangs in "waiting" forever.
#[tokio::test]
async fn emit_events_never_unloads_waiting_agents() {
    use crate::components::AwaitingInteraction;

    let mut host = host_with(vec![]);

    // Parked on a human prompt (`AwaitingInteraction`) - the reported bug:
    // the blocked `ask` future is unpersisted, so unloading strands the run.
    let asking = register_waiting(&mut host, "asking");
    host.world
        .world_mut()
        .entity_mut(asking)
        .insert(AwaitingInteraction);
    // Gated on children, and a plain parked agent.
    let gated = register_waiting(&mut host, "gated");
    host.world
        .world_mut()
        .entity_mut(gated)
        .insert(WaitingForChildren);
    register_waiting(&mut host, "parked");

    // Many serve passes - none of them may reap a Waiting agent.
    for _ in 0..5 {
        host.emit_events();
    }
    for run_id in ["asking", "gated", "parked"] {
        assert!(
            host.live_entity(run_id).is_some(),
            "a Waiting agent was unloaded and can no longer be resumed"
        );
    }
}

#[tokio::test]
async fn resolve_or_reload_pages_in_and_registers() {
    let mut host = host_with(vec![]);
    // No reloader installed → a miss stays a miss.
    assert!(host.resolve_or_reload("ghost").is_none());

    // A reloader that declines (run not resumable from disk) → still a miss,
    // and nothing gets registered.
    host.set_reloader(Box::new(|_world, _run_id| None));
    assert!(host.resolve_or_reload("gone").is_none());
    assert!(
        host.live_entity("gone").is_none(),
        "a declined reload registers nothing"
    );

    // With a reloader that resolves → an unloaded run is paged in and registered.
    host.set_reloader(Box::new(|world, run_id| {
        Some(world.spawn_agent((agent_state(run_id),)))
    }));
    let paged = host.resolve_or_reload("paged").expect("reloaded");
    assert_eq!(
        host.live_entity("paged"),
        Some(paged),
        "registered after reload"
    );

    // A live run is returned without invoking the reloader (no re-spawn).
    assert_eq!(host.resolve_or_reload("paged"), Some(paged));
}

#[tokio::test]
async fn cancel_pages_in_an_unloaded_run() {
    let mut host = host_with(vec![]);
    host.set_reloader(paging_reloader());
    // Cancelling a run that isn't in memory pages it in, then cancels it.
    let cancelled = ask(&mut host, |reply| ControlOp::Cancel {
        run_id: "unloaded".to_string(),
        reply,
    })
    .await;
    assert!(cancelled, "reloaded then cancelled");
    assert_eq!(
        host.world
            .agent_status(host.live_entity("unloaded").unwrap()),
        Some(AgentStatus::Cancelled)
    );
}

#[tokio::test]
async fn emit_events_broadcasts_new_interactions_once() {
    let mut host = host_with(vec![]);
    let mut rx = host.subscribe();
    let backend = host.interactions().backend_for("agent-a");
    let asking = tokio::spawn(async move {
        backend
            .ask(leviath_core::interaction::InteractionRequest::free_text(
                "q1", "p", "s", true,
            ))
            .await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    host.emit_events();
    let evs: Vec<WorldEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        evs.iter()
            .any(|e| matches!(e, WorldEvent::Interaction { .. }))
    );
    // A second emission does not re-broadcast the same interaction.
    host.emit_events();
    assert!(rx.try_recv().is_err());

    // Answer it so the asking task finishes cleanly.
    assert!(
        host.interactions()
            .answer(leviath_core::interaction::InteractionResponse::text(
                "q1", "ok"
            ))
    );
    let _ = asking.await;
}

#[tokio::test]
async fn event_sender_feeds_subscribers() {
    let host = host_with(vec![]);
    let mut rx = host.subscribe();
    let event = WorldEvent::Completed {
        run_id: "r".to_string(),
        agent_id: "a".to_string(),
        status: "complete".to_string(),
        final_output: None,
    };
    host.event_sender().send(event.clone()).unwrap();
    assert_eq!(rx.try_recv().unwrap(), event);
}

#[tokio::test]
async fn emit_events_skips_despawned_agents() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "agent-a");
    host.world_mut().world_mut().despawn(e);
    // The stale run-id mapping is skipped; must not panic.
    host.emit_events();
}

#[tokio::test]
async fn serve_returns_when_control_channel_closes() {
    let mut host = host_with(vec![text("done")]);
    let (op_tx, op_rx) = mpsc::unbounded_channel();
    drop(op_tx); // close immediately
    host.serve(op_rx).await; // must return, not hang
}

#[tokio::test]
async fn mock_helpers_are_exercised() {
    // Keep the test mocks' non-driven methods measured (metadata, the
    // exhausted-infer error path, and the no-op tool exec).
    let p = Script {
        responses: Mutex::new(std::collections::VecDeque::new()),
    };
    assert_eq!(p.name(), "script");
    assert_eq!(p.count_tokens("t", "m").await, 1);
    assert_eq!(p.max_context_tokens("m"), 100_000);
    let _ = p.capabilities("m");
    let req = InferenceRequest {
        system: vec![],
        messages: vec![],
        model: "m".to_string(),
        max_tokens: 1,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };
    assert!(p.infer(&req).await.is_err()); // exhausted

    let exec = NoTools.exec_for(
        Entity::from_raw_u32(1).expect("a small literal index is always a valid entity id"),
        vec![leviath_providers::ToolCall {
            id: "c".to_string(),
            name: "n".to_string(),
            arguments: serde_json::Value::Null,
            thought_signature: None,
        }],
        crate::pipeline::noop_progress(),
    );
    assert_eq!(exec().await, vec![("c".to_string(), String::new())]);
}

#[tokio::test]
async fn list_skips_despawned_entity() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "agent-a");
    // Despawn the entity behind the world's back; the run-id map is now stale.
    host.world_mut().world_mut().despawn(e);

    let list = ask(&mut host, |reply| ControlOp::List { reply }).await.runs;
    assert!(list.is_empty()); // stale mapping filtered out
    let status = ask(&mut host, |reply| ControlOp::Status {
        run_id: "run-a".to_string(),
        reply,
    })
    .await;
    assert_eq!(status, None);
}

// ─── Wait reasons (issue #184) ───────────────────────────────────────────

/// Park `entity` at `Waiting` with `marker` attached, the way the engine
/// would, and ask the host to explain it.
fn waiting_because(
    host: &mut WorldHost,
    entity: Entity,
    attach: impl FnOnce(&mut bevy_ecs::world::EntityWorldMut),
) -> Option<WaitReason> {
    {
        let world = host.world_mut().world_mut();
        world
            .get_mut::<AgentState>(entity)
            .expect("spawned agent has state")
            .status = AgentStatus::Waiting;
        let mut e = world.entity_mut(entity);
        attach(&mut e);
    }
    host.wait_reason(entity)
}

/// A run that is not waiting has nothing to explain, whatever markers it
/// happens to be carrying.
#[tokio::test]
async fn wait_reason_is_none_unless_the_agent_is_waiting() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    host.world_mut()
        .world_mut()
        .entity_mut(e)
        .insert(crate::pipeline::WaitingForChildren);
    assert_eq!(host.wait_reason(e), None);
}

/// An entity the world no longer holds cannot be explained either.
#[tokio::test]
async fn wait_reason_is_none_for_an_unknown_entity() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    host.world_mut().world_mut().despawn(e);
    assert_eq!(host.wait_reason(e), None);
}

/// `Waiting` with nothing claiming it: report nothing rather than guess.
#[tokio::test]
async fn wait_reason_is_none_when_nothing_claims_the_wait() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    assert_eq!(waiting_because(&mut host, e, |_| {}), None);
}

#[tokio::test]
async fn wait_reason_reports_a_taint_gate() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    let reason = waiting_because(&mut host, e, |entity| {
        entity.insert(crate::gate_prompt::AwaitingGatePrompt(1));
    });
    assert_eq!(reason, Some(WaitReason::TaintGate));
}

#[tokio::test]
async fn wait_reason_reports_an_interaction_point() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    let reason = waiting_because(&mut host, e, |entity| {
        entity.insert(crate::interaction_points::AwaitingInteractionPoint);
    });
    assert_eq!(reason, Some(WaitReason::InteractionPoint));
}

/// A stage holding for sub-agents counts only the children that have not
/// finished - the whole point is telling the operator how much is left.
#[tokio::test]
async fn wait_reason_counts_unfinished_children() {
    let mut host = host_with(vec![]);
    let parent = spawn(&mut host, "run-a", "run-a");
    let running = spawn(&mut host, "run-b", "run-b");
    let done = spawn(&mut host, "run-c", "run-c");
    {
        let world = host.world_mut().world_mut();
        world
            .get_mut::<AgentState>(done)
            .expect("child has state")
            .status = AgentStatus::Complete;
    }
    let reason = waiting_because(&mut host, parent, |entity| {
        entity.insert((
            crate::pipeline::WaitingForChildren,
            SubAgentChildren {
                children: vec![running, done],
                max_child_depth: 3,
            },
        ));
    });
    assert_eq!(reason, Some(WaitReason::Children { outstanding: 1 }));
}

/// The marker can outlive the child list (a reload that lost them); report
/// the wait rather than dropping it.
#[tokio::test]
async fn wait_reason_reports_children_with_none_recorded() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    let reason = waiting_because(&mut host, e, |entity| {
        entity.insert(crate::pipeline::WaitingForChildren);
    });
    assert_eq!(reason, Some(WaitReason::Children { outstanding: 0 }));
}

/// Open a real hub request for `agent_id` and leave it pending, returning
/// the task holding it (dropping the host cancels it).
fn open_prompt(
    host: &WorldHost,
    agent_id: &str,
    request: InteractionRequest,
) -> tokio::task::JoinHandle<InteractionResponse> {
    let backend = host.interactions().backend_for(agent_id.to_string());
    tokio::spawn(async move {
        use crate::dynamic_interaction::InteractionBackend;
        backend.ask(request).await
    })
}

/// Let the spawned `ask` reach its first poll, so its request is registered
/// before the assertion looks for it. `submit` inserts before it awaits, so
/// yielding is enough - no sleeping, and no timeout branch to leave uncovered.
async fn await_pending(host: &WorldHost, agent_id: &str) {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        host.interactions()
            .pending()
            .iter()
            .any(|(id, _)| id == agent_id),
        "the hub registered a request for {agent_id}"
    );
}

#[tokio::test]
async fn wait_reason_distinguishes_a_tool_approval_from_a_question() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");

    let approval = open_prompt(
        &host,
        "run-a",
        InteractionRequest::tool_approval(
            "req-1",
            "shell",
            serde_json::json!({}),
            "implement",
            &[],
        ),
    );
    await_pending(&host, "run-a").await;
    let reason = waiting_because(&mut host, e, |entity| {
        entity.insert(AwaitingInteraction);
    });
    assert_eq!(reason, Some(WaitReason::ToolApproval));
    // Release the prompt (rather than abandoning it) so the awaiting task
    // finishes instead of leaking into the next case.
    assert_eq!(host.interactions().cancel_for_agent("run-a"), 1);
    approval.await.expect("the asking task finishes");

    let question = open_prompt(
        &host,
        "run-a",
        InteractionRequest::free_text("req-2", "which one?", "implement", true),
    );
    await_pending(&host, "run-a").await;
    assert_eq!(host.wait_reason(e), Some(WaitReason::UserPrompt));
    assert_eq!(host.interactions().cancel_for_agent("run-a"), 1);
    question.await.expect("the asking task finishes");
}

/// The marker without a matching hub entry (the request cleared in the same
/// tick) still reads as a prompt rather than as nothing.
#[tokio::test]
async fn wait_reason_falls_back_to_user_prompt_without_a_hub_entry() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    let reason = waiting_because(&mut host, e, |entity| {
        entity.insert(AwaitingInteraction);
    });
    assert_eq!(reason, Some(WaitReason::UserPrompt));
}

/// A gate prompt opens a hub request of its own, so the gate-blocked agent
/// carries `AwaitingInteraction` too. The specific marker has to win, or
/// every gate would report as a generic prompt.
#[tokio::test]
async fn a_gate_outranks_the_generic_interaction_marker() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    let reason = waiting_because(&mut host, e, |entity| {
        entity.insert((
            AwaitingInteraction,
            crate::gate_prompt::AwaitingGatePrompt(1),
        ));
    });
    assert_eq!(reason, Some(WaitReason::TaintGate));
}

/// A fan-out parent reports how many workers are left, so "waiting" reads as
/// progress against a denominator rather than an unexplained stall.
#[tokio::test]
async fn wait_reason_counts_outstanding_fan_out_workers() {
    let mut host = host_with(vec![]);
    let parent = spawn(&mut host, "run-a", "run-a");
    let worker = spawn(&mut host, "run-b", "run-b");
    {
        let world = host.world_mut().world_mut();
        world
            .get_mut::<AgentState>(parent)
            .expect("parent has state")
            .status = AgentStatus::Waiting;
        // One worker in flight and two items not yet started ⇒ three left.
        crate::fanout::restore_fan_out_waiting(
            world,
            parent,
            crate::fanout::FanOutState {
                config: leviath_core::blueprint::FanOutConfig {
                    worker_agent: None,
                    worker_stage: Some("work".to_string()),
                    worker_query: None,
                    merge_stage: None,
                    max_workers: 2,
                    on_worker_failure: Default::default(),
                    split_prompt: String::new(),
                    results_region: None,
                    max_items: None,
                },
                max_workers: 2,
                pending: vec![
                    crate::fanout::WorkItem::default(),
                    crate::fanout::WorkItem::default(),
                ],
                active: vec![("item-1".to_string(), "run-b".to_string())],
                summaries: Vec::new(),
                failures: Vec::new(),
            },
            &|run_id| (run_id == "run-b").then_some(worker),
        );
    }
    assert_eq!(
        host.wait_reason(parent),
        Some(WaitReason::FanOutWorkers { outstanding: 3 })
    );
}

/// With run metadata attached, the listing reports the blueprint's shape and
/// whether the run is unattended - an unattended run sitting on a prompt is
/// the shape of a bug.
#[tokio::test]
async fn list_reports_blueprint_shape_and_unattended() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    host.world_mut().world_mut().entity_mut(e).insert((
        RunMetadata {
            run_id: "run-a".to_string(),
            agent_name: "coder".to_string(),
            agent_path: "/tmp/agent".to_string(),
            task: "t".to_string(),
            model: None,
            workdir: "/tmp".to_string(),
            num_stages: 3,
            started_at: 0,
            parent_run_id: None,
            metadata: HashMap::new(),
            callback_url: None,
            callback_secret: None,
            title: None,
            unattended: true,
            read_paths: None,
            output_request: None,
        },
        TokenTotals {
            tool_calls: 9,
            ..Default::default()
        },
        {
            let mut watermark = crate::pipeline::PersistWatermark::default();
            watermark.backdate(1_700);
            watermark
        },
    ));
    let list = ask(&mut host, |reply| ControlOp::List { reply }).await.runs;
    assert_eq!(list[0].num_stages, Some(3));
    assert_eq!(list[0].tool_calls, 9);
    assert!(list[0].unattended);
    assert_eq!(list[0].last_progress_at, Some(1_700));
    // No outcome flags on this agent at all, so there is nothing to
    // report and the listing does not invent a verdict.
    assert!(!list[0].empty_output);
}

/// A run that stopped having produced nothing says so in the listing -
/// otherwise it is indistinguishable from one that did the work (#192).
#[tokio::test]
async fn list_reports_a_finished_run_that_produced_nothing() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    host.world_mut()
        .world_mut()
        .entity_mut(e)
        .insert(crate::persistence::RunOutcomeFlags::default());
    // Still running: nothing to say yet.
    assert!(!ask(&mut host, |reply| ControlOp::List { reply }).await.runs[0].empty_output);

    host.world_mut()
        .world_mut()
        .get_mut::<AgentState>(e)
        .expect("spawned agent has state")
        .status = AgentStatus::Complete;
    assert!(ask(&mut host, |reply| ControlOp::List { reply }).await.runs[0].empty_output);

    // ...unless it never had a way to write, which is not its failing.
    host.world_mut()
        .world_mut()
        .get_mut::<crate::persistence::RunOutcomeFlags>(e)
        .expect("just inserted")
        .0
        .no_output_tools = true;
    assert!(!ask(&mut host, |reply| ControlOp::List { reply }).await.runs[0].empty_output);
}

/// A run whose whole deliverable is its answer modified no files, and used
/// to read `complete (no output)` in `lev ps` while `meta.json` said
/// otherwise - the two surfaces disagreeing about the same run, which one
/// shared `is_empty_output` exists to prevent.
#[tokio::test]
async fn a_submitted_answer_clears_the_listing_s_empty_verdict() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    host.world_mut()
        .world_mut()
        .entity_mut(e)
        .insert(crate::persistence::RunOutcomeFlags::default());
    host.world_mut()
        .world_mut()
        .get_mut::<AgentState>(e)
        .expect("spawned agent has state")
        .status = AgentStatus::Complete;
    // Finished having written nothing: empty, and no answer to point at.
    let before = ask(&mut host, |reply| ControlOp::List { reply }).await.runs;
    assert!(before[0].empty_output);
    assert!(!before[0].has_final_output);

    host.world_mut()
        .world_mut()
        .entity_mut(e)
        .insert(crate::persistence::FinalOutput(
            leviath_core::output::FinalOutput::new(
                "here is what I found",
                None,
                "summary".to_string(),
                0,
            ),
        ));
    let after = ask(&mut host, |reply| ControlOp::List { reply }).await.runs;
    assert!(!after[0].empty_output, "an answer is output");
    // The flag travels; the answer itself does not - `lev result` fetches it.
    assert!(after[0].has_final_output);
}

/// The listing carries the reason and the progress context, not just a word.
#[tokio::test]
async fn list_explains_a_waiting_run() {
    let mut host = host_with(vec![]);
    let e = spawn(&mut host, "run-a", "run-a");
    waiting_because(&mut host, e, |entity| {
        entity.insert(crate::pipeline::WaitingForChildren);
    });
    let list = ask(&mut host, |reply| ControlOp::List { reply }).await.runs;
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].wait_reason,
        Some(WaitReason::Children { outstanding: 0 })
    );
    assert_eq!(list[0].stage_index, Some(0));
    // No RunMetadata on this fixture, so there is nothing to claim about the
    // blueprint's shape or how it was launched.
    assert_eq!(list[0].num_stages, None);
    assert!(!list[0].unattended);
}

#[test]
fn every_world_event_variant_carries_its_run_id() {
    let rid = "run-x".to_string();
    let aid = "agent-x".to_string();
    let events = vec![
        WorldEvent::Spawned {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            blueprint: "b".to_string(),
        },
        WorldEvent::Status {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            status: "active".to_string(),
            stage: "s".to_string(),
            iteration: 1,
            tool_calls: 0,
            accepts_messages: false,
        },
        WorldEvent::Tokens {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            prompt_tokens: 1,
            completion_tokens: 2,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        WorldEvent::Context {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            total_tokens: 3,
            max_tokens: 4,
        },
        WorldEvent::Interaction {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            request: InteractionRequest::free_text("i", "p", "s", true),
        },
        WorldEvent::Completed {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            status: "complete".to_string(),
            final_output: None,
        },
        WorldEvent::StageTransition {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            from: "a".to_string(),
            to: "b".to_string(),
            iteration: 1,
        },
        WorldEvent::ToolCallStarted {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            call_id: "c".to_string(),
            tool: "t".to_string(),
        },
        WorldEvent::ToolCallFinished {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            call_id: "c".to_string(),
            tool: "t".to_string(),
            ok: true,
            summary: "s".to_string(),
        },
        WorldEvent::Log {
            run_id: rid.clone(),
            agent_id: aid.clone(),
            line: "l".to_string(),
        },
    ];
    for ev in events {
        assert_eq!(ev.run_id(), "run-x");
    }
}
