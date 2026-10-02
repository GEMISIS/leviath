//! A worker its parent never recorded runs its item once.

use super::tests::{cfg, fanout_blueprint, item, spawn_parent, status_of};
use super::*;

/// A worker of the test graph for `item` (none when `None`), named `run_id`,
/// as a restart places one.
fn worker(world: &mut World, run_id: &str, item: Option<&str>) -> Entity {
    let mut spec = crate::test_graph::spec_c(
        run_id,
        fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
    )
    .0
    .as_ref()
    .clone();
    spec.run_id = crate::spec::names::RunId::new(run_id).unwrap();
    if let Some(item) = item {
        spec.delivery
            .metadata
            .insert(WORK_ITEM_LABEL.to_string(), item.to_string());
    }
    world
        .spawn((
            RunSpecC(Arc::new(spec)),
            AgentState {
                agent_id: run_id.to_string(),
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

#[test]
fn a_worker_for_a_queued_item_is_adopted_and_any_other_is_cancelled() {
    let mut world = World::new();
    let config = cfg(Some("merge"), 2, WorkerFailure::Continue);
    let parent = spawn_parent(&mut world, fanout_blueprint(config.clone()), "");
    begin_fan_out(
        &mut world,
        parent,
        config,
        vec![item("a"), item("b"), item("c")],
        FanOutOrigin::Stage,
    );

    let a = worker(&mut world, "run-a", Some("a"));
    let b = worker(&mut world, "run-b", Some("b"));
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, a),
        Some(UnrecordedWorker::Adopted)
    );
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, b),
        Some(UnrecordedWorker::Adopted)
    );
    let w = world.get::<FanOutWaiting>(parent).unwrap();
    assert_eq!(
        w.pending.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        ["c"]
    );
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
    assert_eq!(world.get::<ParentRef>(a).unwrap().parent_entity, parent);

    // A second worker for an item already running, and one for an item the
    // fan-out never had, are cancelled.
    let again = worker(&mut world, "run-a2", Some("a"));
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, again),
        Some(UnrecordedWorker::Cancelled)
    );
    assert_eq!(status_of(&world, again), AgentStatus::Cancelled);

    // Not a fan-out worker, or not a run at all: left as it is.
    let child = worker(&mut world, "run-child", None);
    assert_eq!(settle_unrecorded_worker(&mut world, parent, child), None);
    let bare = world.spawn_empty().id();
    assert_eq!(settle_unrecorded_worker(&mut world, parent, bare), None);
    assert_eq!(status_of(&world, child), AgentStatus::Active);
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
    let a = worker(&mut world, "run-a", Some("a"));
    assert_eq!(
        settle_unrecorded_worker(&mut world, parent, a),
        Some(UnrecordedWorker::Cancelled)
    );
}
