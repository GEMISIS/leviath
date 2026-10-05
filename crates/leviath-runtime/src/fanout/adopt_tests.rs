//! A worker its parent never recorded runs its item once, and only the
//! worker the parent would have started for the item is adopted.

use super::tests::{cfg, fanout_blueprint, item, run_meta, spawn_parent, status_of};
use super::*;
use crate::spec::inputs::{InputValue, RawInput};
use crate::spec::names::{InputName, RunId, StageName};
use crate::spec::run_spec::RunSpec;

/// The spec of a worker of `parent` for `item`, named `run_id`, as the
/// parent's start would have resolved it.
fn worker_spec(world: &World, parent: Entity, run_id: &str, item: &str) -> RunSpec {
    let mut spec = world.get::<RunSpecC>(parent).unwrap().0.as_ref().clone();
    spec.placement.parent = Some(spec.run_id.clone());
    spec.placement.depth = 1;
    spec.placement.worker_stage = Some(StageName::new("w").unwrap());
    spec.placement.work_item = Some(item.to_string());
    spec.run_id = RunId::new(run_id).unwrap();
    spec
}

/// Place a worker with `spec` the way a restart does.
fn place(world: &mut World, spec: RunSpec) -> Entity {
    let run_id = spec.run_id.to_string();
    world
        .spawn((
            RunSpecC(Arc::new(spec)),
            run_meta(&run_id),
            AgentState {
                agent_id: run_id.clone(),
                current_visit: String::new(),
                current_stage: "w".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: vec![],
                pending_wait: None,
                accepts_messages: true,
            },
        ))
        .id()
}

/// A worker of `parent` for `item`, named `run_id`.
fn worker(world: &mut World, parent: Entity, run_id: &str, item: &str) -> Entity {
    let spec = worker_spec(world, parent, run_id, item);
    place(world, spec)
}

fn fanning_parent(world: &mut World, items: Vec<WorkItem>) -> Entity {
    let config = cfg(Some("merge"), 2, WorkerFailure::Continue);
    let parent = spawn_parent(world, fanout_blueprint(config.clone()), "");
    begin_fan_out(world, parent, config, items, FanOutOrigin::Stage);
    parent
}

fn queued(world: &World, parent: Entity) -> Vec<String> {
    let w = world.get::<FanOutWaiting>(parent).unwrap();
    w.pending.iter().map(|i| i.id.clone()).collect()
}

#[test]
fn a_worker_for_a_queued_item_is_adopted_and_any_other_is_cancelled() {
    let mut world = World::new();
    let parent = fanning_parent(&mut world, vec![item("a"), item("b"), item("c")]);

    let a = worker(&mut world, parent, "run-a", "a");
    let b = worker(&mut world, parent, "run-b", "b");
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, a),
        Some(UnrecordedWorker::Adopted)
    );
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, b),
        Some(UnrecordedWorker::Adopted)
    );
    assert_eq!(queued(&world, parent), ["c"]);
    let w = world.get::<FanOutWaiting>(parent).unwrap();
    assert_eq!(
        w.to_state().active,
        [
            ("a".to_string(), "run-a".to_string()),
            ("b".to_string(), "run-b".to_string())
        ]
    );
    assert_eq!(
        world.get::<SubAgentChildren>(parent).unwrap().children,
        [a, b]
    );
    assert_eq!(
        world
            .get::<AgentState>(parent)
            .unwrap()
            .spawned_children_ids,
        ["run-a", "run-b"]
    );
    let link = world.get::<ParentRef>(a).unwrap();
    assert_eq!((link.parent_entity, link.depth), (parent, 1));

    // A second worker for an item already running, and one for an item the
    // fan-out never had, are cancelled.
    let again = worker(&mut world, parent, "run-a2", "a");
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, again),
        Some(UnrecordedWorker::Cancelled)
    );
    assert_eq!(status_of(&world, again), AgentStatus::Cancelled);
    let never = worker(&mut world, parent, "run-x", "x");
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, never),
        Some(UnrecordedWorker::Cancelled)
    );

    // Not a fan-out worker, or not a run at all: left as it is.
    let mut spec = worker_spec(&world, parent, "run-child", "c");
    spec.placement.work_item = None;
    let child = place(&mut world, spec);
    assert_eq!(settle_unrecorded_worker(&mut world, parent, child), None);
    let bare = world.spawn_empty().id();
    assert_eq!(settle_unrecorded_worker(&mut world, parent, bare), None);
    assert_eq!(status_of(&world, child), AgentStatus::Active);
    assert_eq!(queued(&world, parent), ["c"]);
}

/// A parent with no fan-out under way waits on no item: its worker is
/// cancelled.
#[test]
fn a_worker_whose_parent_is_not_fanning_out_is_cancelled() {
    let mut world = World::new();
    let parent = spawn_parent(
        &mut world,
        fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
        "",
    );
    let a = worker(&mut world, parent, "run-a", "a");
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, a),
        Some(UnrecordedWorker::Cancelled)
    );
}

/// A worker that names its parent's queued item but is not what the parent
/// would start for it is cancelled, and the item stays queued to start
/// fresh: one started with other inputs, under another parent, at another
/// depth, on another stage, from another blueprint, or with another model.
#[test]
fn a_worker_that_does_not_match_its_item_is_cancelled_and_the_item_runs_fresh() {
    type Change = fn(&mut RunSpec);
    let changes: [(&str, Change); 6] = [
        ("inputs", |s| {
            s.inputs.0.insert(
                InputName::new("task").unwrap(),
                InputValue::Text("something else".into()),
            );
        }),
        ("parent", |s| s.placement.parent = None),
        ("depth", |s| s.placement.depth = 2),
        ("stage", |s| {
            s.placement.worker_stage = Some(StageName::new("other").unwrap())
        }),
        ("blueprint", |s| {
            s.origin = crate::spec::run_spec::SpecOrigin::Raw;
        }),
        ("model", |s| {
            s.requested_model = Some(crate::spec::names::ModelRef::parse("p/other").unwrap());
        }),
    ];
    for (what, change) in changes {
        let mut world = World::new();
        let parent = fanning_parent(&mut world, vec![item("a")]);
        let mut spec = worker_spec(&world, parent, "run-a", "a");
        change(&mut spec);
        let a = place(&mut world, spec);
        assert_eq!(
            settle_unrecorded_worker(&mut world, parent, a),
            Some(UnrecordedWorker::Cancelled),
            "a worker with other {what}"
        );
        assert_eq!(queued(&world, parent), ["a"], "{what}: the item runs fresh");
        assert!(world.get::<ParentRef>(a).is_none(), "{what}: not linked");
    }
}

/// The inputs compared are the ones the item gives, checked as a start
/// checks them: a worker started with them is adopted.
#[test]
fn a_worker_started_with_its_items_inputs_is_adopted() {
    let mut world = World::new();
    let with_task = WorkItem {
        id: "a".into(),
        inputs: [("task".to_string(), RawInput::Text("do a".into()))].into(),
    };
    let parent = fanning_parent(&mut world, vec![with_task]);
    {
        let mut spec = world.get::<RunSpecC>(parent).unwrap().0.as_ref().clone();
        spec.graph.inputs = vec![crate::spec::inputs::InputDecl {
            name: InputName::new("task").unwrap(),
            ty: crate::spec::inputs::InputType::Text {
                multiline: true,
                min_len: None,
                max_len: None,
            },
            required: true,
            default: None,
            description: None,
            binds: vec![],
        }];
        world.entity_mut(parent).insert(RunSpecC(Arc::new(spec)));
    }
    let mut spec = worker_spec(&world, parent, "run-a", "a");
    spec.inputs.0.insert(
        InputName::new("task").unwrap(),
        InputValue::Text("do a".into()),
    );
    let a = place(&mut world, spec);
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, a),
        Some(UnrecordedWorker::Adopted)
    );
}

/// Adopting a worker whose file holds only its first record does what a
/// start does as it places one: seeds its context from its parent's. One
/// whose file went further already holds that, and is not seeded twice.
#[test]
fn an_adopted_worker_is_seeded_from_its_parent_once() {
    use crate::spec::graph::{ContentTransform, ContextTransformDef, RegionMappingDef};
    use crate::spec::names::{BlueprintName, BlueprintRef, RegionName};

    for stepped in [false, true] {
        let mut world = World::new();
        let mut config = cfg(Some("merge"), 2, WorkerFailure::Continue);
        config.worker = WorkerSource::Blueprint(BlueprintRef::parse("helper").unwrap());
        let mut graph = fanout_blueprint(config.clone());
        graph.transforms = vec![ContextTransformDef {
            from: BlueprintName::new("t").unwrap(),
            to: BlueprintName::new("helper").unwrap(),
            mappings: vec![RegionMappingDef {
                from: RegionName::new("conversation").unwrap(),
                to: RegionName::new("conversation").unwrap(),
                transform: ContentTransform::Direct,
            }],
        }];
        let parent = spawn_parent(&mut world, graph, "");
        world
            .get_mut::<ContextWindow>(parent)
            .unwrap()
            .add_to_region("conversation", "the parent's notes".into(), 5)
            .unwrap();
        begin_fan_out(
            &mut world,
            parent,
            config,
            vec![item("a")],
            FanOutOrigin::Stage,
        );

        let mut spec = worker_spec(&world, parent, "run-a", "a");
        spec.placement.worker_stage = None;
        spec.origin = crate::spec::run_spec::SpecOrigin::Blueprint {
            blueprint: BlueprintRef::parse("helper").unwrap(),
            version: "1".into(),
            manifest: String::new(),
        };
        let a = place(&mut world, spec);
        let mut window = ContextWindow::new(12_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        world.entity_mut(a).insert(window);
        let lane = crate::pipeline::PersistLaneHealth(Arc::new(
            crate::persist_stats::PersistLaneStats::new(),
        ));
        lane.0.stepped("run-a", u64::from(stepped));
        world.insert_resource(lane);

        assert_eq!(
            settle_unrecorded_worker(&mut world, parent, a),
            Some(UnrecordedWorker::Adopted)
        );
        let held = super::tests::conversation_text(&world, a);
        match stepped {
            false => assert_eq!(held, "the parent's notes", "seeded as a start seeds"),
            true => assert_eq!(held, "", "its file already held what it was seeded with"),
        }
    }
}

/// A worker runs what its fan-out starts when it came from the same place:
/// the parent's own graph for a stage worker, the blueprint named (at the
/// revision named, when one is), the blueprint directory named, or the
/// blueprint a query finds now.
#[test]
fn a_worker_is_adopted_only_when_it_runs_what_the_fan_out_starts() {
    use crate::spec::names::{BlueprintName, BlueprintPath, BlueprintRef, Digest};
    use crate::spec::run_spec::SpecOrigin;
    let installed = |name: &str, digest: Option<&[u8]>| SpecOrigin::Blueprint {
        blueprint: BlueprintRef {
            name: BlueprintName::new(name).unwrap(),
            digest: digest.map(Digest::of),
        },
        version: "1".into(),
        manifest: String::new(),
    };
    let dir = BlueprintPath::new(std::env::temp_dir().join("helper").to_string_lossy()).unwrap();
    let from_dir = |path: &BlueprintPath| SpecOrigin::BlueprintFile {
        path: path.clone(),
        name: BlueprintName::new("helper").unwrap(),
        digest: None,
        version: String::new(),
    };
    let pinned = BlueprintRef {
        name: BlueprintName::new("helper").unwrap(),
        digest: Some(Digest::of(b"one")),
    };
    let other_dir =
        BlueprintPath::new(std::env::temp_dir().join("other").to_string_lossy()).unwrap();
    let query = |q: &str| WorkerSource::Query(q.to_string());
    // (case, fan-out source, the parent's origin when it changes, the
    // worker's origin, a spawner installed, adopted)
    type Case = (
        &'static str,
        WorkerSource,
        Option<SpecOrigin>,
        SpecOrigin,
        bool,
        bool,
    );
    let cases: Vec<Case> = vec![
        (
            "raw stage",
            WorkerSource::Stage(StageName::new("w").unwrap()),
            Some(SpecOrigin::Raw),
            SpecOrigin::Raw,
            false,
            true,
        ),
        (
            "any revision",
            WorkerSource::Blueprint(BlueprintRef::parse("helper").unwrap()),
            None,
            installed("helper", Some(b"two")),
            false,
            true,
        ),
        (
            "the revision named",
            WorkerSource::Blueprint(pinned.clone()),
            None,
            installed("helper", Some(b"one")),
            false,
            true,
        ),
        (
            "another revision",
            WorkerSource::Blueprint(pinned),
            None,
            installed("helper", Some(b"two")),
            false,
            false,
        ),
        (
            "another blueprint",
            WorkerSource::Blueprint(BlueprintRef::parse("helper").unwrap()),
            None,
            installed("other", None),
            false,
            false,
        ),
        (
            "its directory",
            WorkerSource::BlueprintFile(dir.clone()),
            None,
            from_dir(&dir),
            false,
            true,
        ),
        (
            "another directory",
            WorkerSource::BlueprintFile(other_dir),
            None,
            from_dir(&dir),
            false,
            false,
        ),
        (
            "found by query",
            query("helper"),
            None,
            installed("helper", None),
            true,
            true,
        ),
        (
            "a query nothing answers",
            query("nobody"),
            None,
            installed("helper", None),
            true,
            false,
        ),
        (
            "a query with no spawner",
            query("helper"),
            None,
            installed("helper", None),
            false,
            false,
        ),
    ];
    for (case, source, parent_origin, origin, spawner, adopted) in cases {
        let mut world = World::new();
        let mut config = cfg(Some("merge"), 2, WorkerFailure::Continue);
        let stage = match &source {
            WorkerSource::Stage(stage) => Some(stage.clone()),
            _ => None,
        };
        config.worker = source;
        let parent = spawn_parent(&mut world, fanout_blueprint(config.clone()), "");
        if let Some(parent_origin) = parent_origin {
            let mut spec = world.get::<RunSpecC>(parent).unwrap().0.as_ref().clone();
            spec.origin = parent_origin;
            world.entity_mut(parent).insert(RunSpecC(Arc::new(spec)));
        }
        if spawner {
            world.insert_resource(FanOutSpawnerRes(super::tests::TestSpawner::ok()));
        }
        begin_fan_out(
            &mut world,
            parent,
            config,
            vec![item("a")],
            FanOutOrigin::Stage,
        );
        let mut spec = worker_spec(&world, parent, "run-a", "a");
        spec.origin = origin;
        spec.placement.worker_stage = stage;
        let a = place(&mut world, spec);
        let expected = match adopted {
            true => UnrecordedWorker::Adopted,
            false => UnrecordedWorker::Cancelled,
        };
        assert_eq!(
            settle_unrecorded_worker(&mut world, parent, a),
            Some(expected),
            "{case}"
        );
    }
}

/// A worker its parent could not start now, because the tree is as deep as
/// it may go, is not adopted either: the item starts fresh and is refused
/// as a start is.
#[test]
fn a_worker_past_the_depth_limit_is_cancelled() {
    let mut world = World::new();
    let parent = fanning_parent(&mut world, vec![item("a")]);
    world.entity_mut(parent).insert(SubAgentChildren {
        children: vec![],
        max_child_depth: 0,
    });
    let a = worker(&mut world, parent, "run-a", "a");
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, a),
        Some(UnrecordedWorker::Cancelled)
    );
    assert_eq!(queued(&world, parent), ["a"]);
}

/// A caller's own labels never make a run a fan-out worker. A run placed
/// under the parent exactly as a worker would be, whose metadata names a
/// queued item under `fan_out_item`, is no worker: it is left as it is and
/// the item stays queued for its own start.
#[test]
fn a_label_a_caller_wrote_does_not_make_a_run_a_worker() {
    let mut world = World::new();
    let parent = fanning_parent(&mut world, vec![item("a")]);
    let mut forged = world.get::<RunSpecC>(parent).unwrap().0.as_ref().clone();
    forged.placement.parent = Some(forged.run_id.clone());
    forged.placement.depth = 1;
    forged.placement.worker_stage = Some(StageName::new("w").unwrap());
    forged.run_id = RunId::new("run-forged").unwrap();
    forged
        .delivery
        .metadata
        .insert("fan_out_item".to_string(), "a".to_string());
    let run = place(&mut world, forged);
    assert_eq!(settle_unrecorded_worker(&mut world, parent, run), None);
    assert_eq!(queued(&world, parent), ["a"]);
    assert_eq!(status_of(&world, run), AgentStatus::Active);
}
