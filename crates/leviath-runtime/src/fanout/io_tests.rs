//! A fan-out's reads and copies run off the tick, and land on a later pass.

use super::tests::{
    TestSpawner, cfg, fanout_blueprint, install, item, pending, spawn_parent, topic_blueprint,
};
use super::*;
use crate::spec::graph::StageMode as Mode;
use leviath_core::mime::{Blob, BlobRef, BlobStore, MemoryBlobStore, MimeRegistry};
use std::sync::Mutex;
use std::thread::ThreadId;
use tokio::sync::Notify;

/// A world whose reads and copies run on this test's runtime.
fn io_world() -> World {
    let mut world = World::new();
    world.insert_resource(FanOutIo::new(
        tokio::runtime::Handle::current(),
        Arc::new(Notify::new()),
    ));
    world
}

/// A spawner that reads every worker as taking `topic` text, noting the
/// thread each read ran on; or one whose read blows up.
struct Reading {
    on: Mutex<Vec<ThreadId>>,
    panics: bool,
}

impl FanOutSpawner for Reading {
    fn prepare_worker(&self, request: SpawnRequest, caller: Caller) -> WorkerPrep {
        TestSpawner::ok().prepare_worker(request, caller)
    }

    fn find_worker(&self, query: &str) -> Result<BlueprintRef, String> {
        BlueprintRef::parse(query).map_err(|e| e.to_string())
    }

    fn worker_inputs(&self, _source: &SpawnSource) -> Option<Vec<InputDecl>> {
        self.on.lock().unwrap().push(std::thread::current().id());
        assert!(!self.panics, "the read blew up");
        Some(topic_blueprint(cfg(None, 1, WorkerFailure::Continue)).inputs)
    }
}

/// A parent in an ordinary stage with a `fan_out` call naming `probe` for
/// one item, under `spawner`.
fn calling(world: &mut World, spawner: Arc<dyn FanOutSpawner>, topic: serde_json::Value) -> Entity {
    install(world, spawner);
    let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailure::Continue));
    bp.stages[0].mode = Mode::Autonomous;
    let e = spawn_parent(world, bp, "");
    pending(
        world,
        e,
        serde_json::json!({"agent": "probe", "items": [{"id": "a", "inputs": {"topic": topic}}]}),
    );
    e
}

/// Pass `system` until `done` holds of the world.
async fn until(world: &mut World, system: fn(&mut World), done: impl Fn(&World) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        system(world);
        if done(world) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "it never landed");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

/// The worker's blueprint is read off the tick: the call waits on the read,
/// the agent counts as busy, and a later pass checks the items against what
/// came back and starts the fan-out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workers_inputs_are_read_off_the_tick() {
    let mut world = io_world();
    let spawner = Arc::new(Reading {
        on: Mutex::new(Vec::new()),
        panics: false,
    });
    let e = calling(&mut world, spawner.clone(), serde_json::json!(4));
    start_pending_fan_outs(&mut world);
    assert!(world.get::<FanOutWaiting>(e).is_none(), "not started yet");
    assert!(world.get::<ReadingWorkerInputs>(e).is_some());
    assert!(world.get::<PendingFanOut>(e).is_some(), "the call waits");
    until(&mut world, start_pending_fan_outs, |w| {
        w.get::<PendingFanOut>(e).is_none()
    })
    .await;
    let read_on = spawner.on.lock().unwrap().clone();
    assert_eq!(read_on.len(), 1, "read once");
    assert_ne!(read_on[0], std::thread::current().id(), "not on the tick");
    assert!(world.get::<FanOutWaiting>(e).is_none(), "a mistyped item");
    assert!(
        super::tests::conversation_text(&world, e).contains("items[0].inputs.topic"),
        "is refused against what was read"
    );
    assert!(world.get::<io::WorkerInputs>(e).is_none(), "used up");
}

/// A read that dies still lets the call go on, unchecked: each worker's own
/// start checks its inputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_that_panics_lets_the_call_go_on() {
    let silent = crate::test_support::SilentPanics::install();
    let mut world = io_world();
    let spawner = Arc::new(Reading {
        on: Mutex::new(Vec::new()),
        panics: true,
    });
    let e = calling(&mut world, spawner, serde_json::json!(4));
    until(&mut world, start_pending_fan_outs, |w| {
        w.get::<PendingFanOut>(e).is_none()
    })
    .await;
    drop(silent);
    assert!(
        world.get::<FanOutWaiting>(e).is_some(),
        "started, unchecked"
    );
}

/// A call whose agent is paused waits for it to resume; one whose agent
/// stopped is not started.
#[test]
fn a_call_waits_for_its_agent_to_be_active() {
    let mut world = World::new();
    let e = calling(&mut world, TestSpawner::ok(), serde_json::json!("rust"));
    let set = |world: &mut World, status| {
        world.get_mut::<AgentState>(e).unwrap().status = status;
    };
    set(&mut world, AgentStatus::Paused);
    start_pending_fan_outs(&mut world);
    assert!(world.get::<PendingFanOut>(e).is_some(), "held while paused");
    set(&mut world, AgentStatus::Cancelled);
    start_pending_fan_outs(&mut world);
    assert!(world.get::<FanOutWaiting>(e).is_none(), "never started");
    set(&mut world, AgentStatus::Active);
    start_pending_fan_outs(&mut world);
    assert!(
        world.get::<FanOutWaiting>(e).is_some(),
        "started once active"
    );
}

/// A store that notes the thread each read ran on, or blows up on one.
struct Watched {
    inner: MemoryBlobStore,
    on: Mutex<Vec<ThreadId>>,
    panics: bool,
}

impl BlobStore for Watched {
    fn put(&self, run_id: &str, blob: &Blob, reg: &MimeRegistry) -> std::io::Result<BlobRef> {
        self.inner.put(run_id, blob, reg)
    }

    fn read(&self, run_id: &str, sha256: &str) -> std::io::Result<Arc<[u8]>> {
        self.on.lock().unwrap().push(std::thread::current().id());
        assert!(!self.panics, "the store blew up");
        self.inner.read(run_id, sha256)
    }

    fn copy(&self, from_run: &str, to_run: &str, sha256: &str) -> std::io::Result<()> {
        self.inner.copy(from_run, to_run, sha256)
    }

    fn list(&self, run_id: &str) -> std::io::Result<Vec<String>> {
        self.inner.list(run_id)
    }
}

/// A fan-out over one item whose worker finished with a stored file in
/// `store`; returns the parent, the worker and the file's hash.
fn finished_worker(world: &mut World, store: Arc<Watched>) -> (Entity, Entity, String) {
    let registry = MimeRegistry::builtin();
    let png = leviath_core::mime::MimeType::parse("image/png").unwrap();
    let stored = store
        .put(
            "run-a",
            &Blob {
                mime_type: png.clone(),
                bytes: b"\x89PNG fake".to_vec(),
                name: Some("hero.png".to_string()),
            },
            &registry,
        )
        .unwrap();
    world.insert_resource(BlobStoreHandle(store));
    world.insert_resource(MimeRegistryHandle(Arc::new(registry)));
    install(world, TestSpawner::ok());
    let config = cfg(Some("merge"), 1, WorkerFailure::Continue);
    let parent = spawn_parent(world, fanout_blueprint(config.clone()), "");
    begin_fan_out(world, parent, config, vec![item("a")], FanOutOrigin::Stage);
    fan_out_collect(world);
    let worker = world.get::<SubAgentChildren>(parent).unwrap().children[0];
    let mut w = world.entity_mut(worker);
    w.get_mut::<AgentState>().unwrap().status = AgentStatus::Complete;
    w.insert(crate::persistence::FinalOutput(
        leviath_core::output::FinalOutput::new("drawn", None, "w".to_string(), 0).with_artifacts(
            vec![leviath_core::output::Artifact {
                name: "hero".to_string(),
                path: "hero.png".to_string(),
                mime_type: png,
                size: 9,
                sha256: stored.sha256.clone(),
            }],
        ),
    ));
    (parent, worker, stored.sha256)
}

/// A finished worker's files are copied up off the tick: the fan-out waits
/// for the copy, and finishes with the parts once it lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workers_files_are_handed_up_off_the_tick() {
    let mut world = io_world();
    let store = Arc::new(Watched {
        inner: MemoryBlobStore::new(),
        on: Mutex::new(Vec::new()),
        panics: false,
    });
    let (parent, _, sha) = finished_worker(&mut world, store.clone());
    fan_out_collect(&mut world);
    let w = world.get::<FanOutWaiting>(parent).expect("still waiting");
    assert_eq!(w.handing_up, 1, "the copy is out");
    assert_eq!(w.summaries.len(), 1, "the answer is in");
    assert!(w.parts.is_empty());
    until(&mut world, fan_out_collect, |w| {
        w.get::<FanOutWaiting>(parent).is_none()
    })
    .await;
    let read_on = store.on.lock().unwrap().clone();
    assert_eq!(read_on.len(), 1);
    assert_ne!(read_on[0], std::thread::current().id(), "not on the tick");
    assert!(
        store.inner.read("", &sha).is_ok(),
        "stored under the parent"
    );
    assert_eq!(super::tests::status_of(&world, parent), AgentStatus::Active);
}

/// A copy that dies hands up nothing, and the fan-out still finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copy_that_panics_still_finishes_the_fan_out() {
    let silent = crate::test_support::SilentPanics::install();
    let mut world = io_world();
    let store = Arc::new(Watched {
        inner: MemoryBlobStore::new(),
        on: Mutex::new(Vec::new()),
        panics: true,
    });
    let (parent, _, sha) = finished_worker(&mut world, store.clone());
    until(&mut world, fan_out_collect, |w| {
        w.get::<FanOutWaiting>(parent).is_none()
    })
    .await;
    drop(silent);
    assert!(store.inner.read("", &sha).is_err(), "nothing copied");
}

/// A copy that lands after its parent stopped waiting is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copy_for_a_parent_no_longer_waiting_is_dropped() {
    let mut world = io_world();
    let store = Arc::new(Watched {
        inner: MemoryBlobStore::new(),
        on: Mutex::new(Vec::new()),
        panics: false,
    });
    let (parent, _, _) = finished_worker(&mut world, store.clone());
    fan_out_collect(&mut world);
    world.get_mut::<AgentState>(parent).unwrap().status = AgentStatus::Cancelled;
    until(&mut world, fan_out_collect, |_| {
        !store.on.lock().unwrap().is_empty()
    })
    .await;
    // Let the copy's report reach the channel, then drain it.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    fan_out_collect(&mut world);
    assert!(world.get::<FanOutWaiting>(parent).is_none());
    assert!(io::drain_hand_ups(&mut world).is_empty(), "drained");
}

/// A world with no runtime copies the files in place, in the same pass.
#[test]
fn with_no_runtime_files_are_handed_up_in_place() {
    let mut world = World::new();
    let store = Arc::new(Watched {
        inner: MemoryBlobStore::new(),
        on: Mutex::new(Vec::new()),
        panics: false,
    });
    let (parent, _, sha) = finished_worker(&mut world, store.clone());
    fan_out_collect(&mut world);
    assert!(world.get::<FanOutWaiting>(parent).is_none(), "finished");
    assert!(
        store.inner.read("", &sha).is_ok(),
        "stored under the parent"
    );
}

/// The blueprint a worker names: none for a worker stage, which runs its own
/// run's graph; a query, a name or a path otherwise.
#[test]
fn the_blueprint_a_worker_names() {
    use io::WorkerBlueprint as B;
    assert!(
        B::of(&WorkerSource::Stage(
            crate::spec::names::StageName::new("w").unwrap()
        ))
        .is_none()
    );
    assert!(matches!(
        B::of(&WorkerSource::Query("tests".into())),
        Some(B::Query(q)) if q == "tests"
    ));
    assert!(matches!(
        B::of(&WorkerSource::Blueprint(
            BlueprintRef::parse("probe").unwrap()
        )),
        Some(B::Named(SpawnSource::Blueprint(_)))
    ));
    let path = crate::spec::names::BlueprintPath::new(
        std::env::temp_dir().join("probe").to_string_lossy(),
    )
    .unwrap();
    assert!(matches!(
        B::of(&WorkerSource::BlueprintFile(path)),
        Some(B::Named(SpawnSource::BlueprintFile(_)))
    ));
}

/// Inputs that land for an agent that went meanwhile are dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inputs_for_an_agent_that_went_are_dropped() {
    let mut world = io_world();
    let spawner = Arc::new(Reading {
        on: Mutex::new(Vec::new()),
        panics: false,
    });
    let e = calling(&mut world, spawner.clone(), serde_json::json!("rust"));
    start_pending_fan_outs(&mut world);
    world.despawn(e);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while spawner.on.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    start_pending_fan_outs(&mut world);
    assert!(world.get_entity(e).is_err());
}
