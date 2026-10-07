//! Worker starts run off the tick, and land on a later pass.

use super::tests::{
    TestSpawner, cfg, conversation_text, fanout_blueprint, install, item, spawn_parent, status_of,
};
use super::*;
use tokio::sync::{Notify, Semaphore};

/// Starts that wait until the test lets them go, so a pass of the collector
/// can be seen not to wait for one.
struct Gated {
    gate: Arc<Semaphore>,
    inner: Arc<dyn FanOutSpawner>,
}

impl FanOutSpawner for Gated {
    fn prepare_worker(&self, request: SpawnRequest, caller: Caller) -> WorkerPrep {
        let gate = self.gate.clone();
        let inner = self.inner.prepare_worker(request, caller);
        Box::pin(async move {
            let _open = gate.acquire().await.expect("the gate stays open");
            inner.await
        })
    }

    fn find_worker(&self, query: &str) -> Result<BlueprintRef, String> {
        self.inner.find_worker(query)
    }
}

/// Starts that never finish, or that blow up.
enum Broken {
    Waits,
    Panics,
}

impl FanOutSpawner for Broken {
    fn prepare_worker(&self, _request: SpawnRequest, _caller: Caller) -> WorkerPrep {
        match self {
            Broken::Waits => Box::pin(std::future::pending()),
            Broken::Panics => Box::pin(async { panic!("the starter blew up") }),
        }
    }

    fn find_worker(&self, query: &str) -> Result<BlueprintRef, String> {
        BlueprintRef::parse(query).map_err(|e| e.to_string())
    }
}

/// A world whose worker starts run on this test's runtime, with a parent
/// fanned out over `items` under a cap of `cap`.
fn runtime_world(spawner: Arc<dyn FanOutSpawner>, cap: u32, items: &[&str]) -> (World, Entity) {
    let mut world = World::new();
    world.insert_resource(WorkerStarts::new(
        tokio::runtime::Handle::current(),
        Arc::new(Notify::new()),
    ));
    install(&mut world, spawner);
    let config = cfg(Some("merge"), cap, WorkerFailure::Continue);
    let e = spawn_parent(&mut world, fanout_blueprint(config.clone()), "");
    begin_fan_out(
        &mut world,
        e,
        config,
        items.iter().map(|id| item(id)).collect(),
        FanOutOrigin::Stage,
    );
    (world, e)
}

/// Pass the collector until `done` holds of the world.
async fn collect_until(world: &mut World, done: impl Fn(&World) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        fan_out_collect(world);
        if done(world) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the starts never landed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

/// The collector begins a start and goes on: the item is starting, counted as
/// running and saved as still to start, and nothing is in the world until the
/// start lands, when a later pass places and links it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_is_prepared_off_the_tick_and_placed_when_it_lands() {
    let gate = Arc::new(Semaphore::new(0));
    let spawner = Arc::new(Gated {
        gate: gate.clone(),
        inner: TestSpawner::ok(),
    });
    let (mut world, e) = runtime_world(spawner, 2, &["a", "b", "c"]);

    fan_out_collect(&mut world);
    let w = world.get::<FanOutWaiting>(e).expect("parked");
    assert_eq!(w.starting.len(), 2, "two begin under a cap of two");
    assert!(w.active.is_empty());
    assert_eq!(w.progress(), [1, 2, 0, 0], "a start counts as running");
    assert_eq!(w.outstanding(), 3);
    assert_eq!(
        w.to_state().pending.len(),
        3,
        "a start has no run to come back to yet, so it is saved as still to start"
    );
    assert!(
        world.get::<SubAgentChildren>(e).is_none(),
        "nothing placed yet"
    );

    gate.add_permits(2);
    collect_until(&mut world, |world| {
        world
            .get::<FanOutWaiting>(world_parent(world))
            .is_some_and(|w| w.active.len() == 2)
    })
    .await;
    let w = world.get::<FanOutWaiting>(e).expect("parked");
    assert!(w.starting.is_empty());
    assert_eq!(w.pending.len(), 1, "the third waits for a slot");
    let kids = &world.get::<SubAgentChildren>(e).expect("linked").children;
    assert_eq!(kids.len(), 2);
    assert_eq!(
        world
            .get::<ParentRef>(kids[0])
            .expect("linked")
            .parent_entity,
        e
    );
    assert_eq!(
        world
            .get::<AgentState>(e)
            .unwrap()
            .spawned_children_ids
            .len(),
        2,
        "recorded on the parent for a restart"
    );
}

/// The one fan-out parent in a world.
fn world_parent(world: &World) -> Entity {
    world
        .iter_entities()
        .find(|e| e.contains::<RunSpecC>())
        .expect("a parent")
        .id()
}

/// A start that lands after its parent stopped waiting is placed, so its run
/// says how it ended, and cancelled; one that failed leaves nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_lands_after_its_parent_left_is_cancelled() {
    let gate = Arc::new(Semaphore::new(0));
    let spawner = Arc::new(Gated {
        gate: gate.clone(),
        inner: TestSpawner::refusing(&["b"]),
    });
    let (mut world, e) = runtime_world(spawner, 2, &["a", "b"]);
    fan_out_collect(&mut world);
    set_status(&mut world, e, AgentStatus::Cancelled);
    // The parent is seen cancelled and stops waiting before either start can
    // land, so both land with no parent waiting on them, every time.
    fan_out_collect(&mut world);
    assert!(world.get::<FanOutWaiting>(e).is_none());
    gate.add_permits(2);
    collect_until(&mut world, |world| {
        world.iter_entities().any(|w| {
            w.get::<AgentState>()
                .is_some_and(|s| s.agent_id == "worker-a")
        })
    })
    .await;
    // Both reports are in; one more pass sees the refusal too.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    fan_out_collect(&mut world);
    let worker = world
        .iter_entities()
        .find(|w| {
            w.get::<AgentState>()
                .is_some_and(|s| s.agent_id == "worker-a")
        })
        .expect("placed")
        .id();
    assert_eq!(status_of(&world, worker), AgentStatus::Cancelled);
    assert!(
        world.get::<ParentRef>(worker).is_none(),
        "not linked to a parent that left"
    );
    assert!(world.get::<FanOutWaiting>(e).is_none());
    assert!(
        !world.iter_entities().any(|w| w
            .get::<AgentState>()
            .is_some_and(|s| s.agent_id == "worker-b")),
        "a refused start places nothing"
    );
}

/// A start that lands in the same pass its parent is found cancelled is
/// abandoned there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_landing_as_its_parent_is_found_cancelled_is_cancelled() {
    let gate = Arc::new(Semaphore::new(0));
    let spawner = Arc::new(Gated {
        gate: gate.clone(),
        inner: TestSpawner::ok(),
    });
    let (mut world, e) = runtime_world(spawner, 1, &["a"]);
    fan_out_collect(&mut world);
    gate.add_permits(1);
    // Let the start land without a pass seeing it, then cancel the parent.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    set_status(&mut world, e, AgentStatus::Cancelled);
    fan_out_collect(&mut world);
    let worker = world
        .iter_entities()
        .find(|w| {
            w.get::<AgentState>()
                .is_some_and(|s| s.agent_id == "worker-a")
        })
        .expect("placed")
        .id();
    assert_eq!(status_of(&world, worker), AgentStatus::Cancelled);
}

/// A world with no runtime runs a start in place, and one that would have to
/// wait is refused rather than waited on.
#[test]
fn a_start_that_would_wait_needs_the_worlds_runtime() {
    let mut world = World::new();
    install(&mut world, Arc::new(Broken::Waits));
    let config = cfg(Some("merge"), 2, WorkerFailure::Continue);
    let e = spawn_parent(&mut world, fanout_blueprint(config.clone()), "");
    begin_fan_out(&mut world, e, config, vec![item("a")], FanOutOrigin::Stage);
    fan_out_collect(&mut world);
    assert_eq!(
        status_of(&world, e),
        AgentStatus::Active,
        "the fan-out finished"
    );
    let convo = conversation_text(&world, e);
    assert!(convo.contains("needs the world's runtime"), "{convo}");
}

/// A start that dies without reporting is reported by its supervisor, as that
/// item's failure, and the fan-out finishes on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_panics_is_the_items_failure() {
    let _silent = crate::test_support::SilentPanics::install();
    let (mut world, e) = runtime_world(Arc::new(Broken::Panics), 2, &["a"]);
    collect_until(&mut world, |world| {
        world.get::<FanOutWaiting>(world_parent(world)).is_none()
    })
    .await;
    let convo = conversation_text(&world, e);
    assert!(convo.contains("panicked"), "{convo}");
}

/// The run's agent budget counts a worker still being prepared, so starting
/// several at once cannot overshoot it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_counts_against_the_runs_agent_budget() {
    let gate = Arc::new(Semaphore::new(0));
    let spawner = Arc::new(Gated {
        gate,
        inner: TestSpawner::ok(),
    });
    let (mut world, e) = runtime_world(spawner, 0, &["a", "b"]);
    world.insert_resource(FanOutBudget(2));
    fan_out_collect(&mut world);
    let w = world.get::<FanOutWaiting>(e).expect("parked");
    assert_eq!(w.starting.len(), 1);
    assert_eq!(w.failures.len(), 1);
    assert!(w.failures[0].1.contains("ceiling"), "{:?}", w.failures);
}
