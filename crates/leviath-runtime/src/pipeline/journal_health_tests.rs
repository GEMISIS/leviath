//! Tests for failing a run whose journal cannot be written.

use super::*;
use std::path::Path;

/// Run metadata carrying only the field this system reads.
fn run_metadata(run_id: &str) -> RunMetadata {
    RunMetadata {
        run_id: run_id.to_string(),
        agent_name: "a".to_string(),
        agent_path: String::new(),
        task: String::new(),
        model: None,
        workdir: String::new(),
        num_stages: 1,
        started_at: 0,
        parent_run_id: None,
        metadata: std::collections::HashMap::new(),
        callback_url: None,
        callback_secret: None,
        title: None,
        title_error: None,
        blueprint_digest: None,
        unattended: false,
        yolo_profile: None,
        read_paths: None,
        output_request: None,
        model_override: None,
    }
}

fn agent_state(status: AgentStatus) -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_stage: "implement".to_string(),
        current_visit: String::new(),
        iteration: 3,
        status,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

/// An agent in `status`, running as `run_id`.
fn spawn_run(world: &mut World, run_id: &str, status: AgentStatus) -> Entity {
    world
        .spawn((
            agent_state(status),
            run_metadata(run_id),
            StageIoBuffer::default(),
        ))
        .id()
}

/// A world holding `stats` as the persistence lane's health.
fn world_with(stats: Arc<PersistLaneStats>) -> World {
    let mut world = World::new();
    world.insert_resource(PersistLaneHealth(stats));
    world
}

fn run(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(fail_runs_with_unwritable_journals);
    schedule.run(world);
}

fn status_of(world: &World, e: Entity) -> AgentStatus {
    world.get::<AgentState>(e).unwrap().status.clone()
}

/// Assert `status` is an `Error`, by discriminant rather than a `matches!` arm
/// that would leave an unreachable branch behind, and hand back the message.
fn error_message(status: &AgentStatus) -> String {
    assert_eq!(
        std::mem::discriminant(status),
        std::mem::discriminant(&AgentStatus::Error {
            message: String::new()
        }),
        "expected the run to be failed, got: {status:?}"
    );
    match status {
        AgentStatus::Error { message } => message.clone(),
        other => format!("{other:?}"),
    }
}

/// The whole point: a run whose journal will not take a record stops, and says
/// which file could not be written.
#[test]
fn a_run_whose_journal_cannot_be_written_is_failed() {
    let stats = Arc::new(PersistLaneStats::new());
    stats.journal_append_failed(
        "run-1",
        Path::new("/runs/run-1/run.lvr"),
        "Read-only file system",
    );
    let mut world = world_with(stats);
    let e = spawn_run(&mut world, "run-1", AgentStatus::Active);

    run(&mut world);

    let message = error_message(&status_of(&world, e));
    assert!(message.contains("/runs/run-1/run.lvr"), "{message}");
    assert!(message.contains("Read-only file system"), "{message}");
    assert!(
        message.contains("journal could not be written"),
        "{message}"
    );
    let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
    assert!(
        logs.iter().any(|(_, line)| line.starts_with("[journal]")),
        "expected a [journal] log line, got: {logs:?}"
    );
}

/// **A deleted run must never fail loudly.** The lane drops a write for a run
/// whose directory is gone without recording anything, so nothing is named here
/// and every run the world still holds keeps going.
#[test]
fn a_deleted_run_leaves_every_run_alone() {
    let stats = Arc::new(PersistLaneStats::new());
    // What a delete produces: writes dropped, nothing counted, nobody named.
    stats.append_attempted();
    assert!(stats.report().is_healthy());

    let mut world = world_with(stats);
    let e = spawn_run(&mut world, "run-1", AgentStatus::Active);

    run(&mut world);

    assert_eq!(
        status_of(&world, e),
        AgentStatus::Active,
        "a dropped write for a deleted run is not a failure"
    );
    assert!(
        world.get::<StageIoBuffer>(e).unwrap().logs.is_empty(),
        "and nothing is said about it"
    );
}

/// Only the run that lost the record: a daemon hosting fifty runs must not fail
/// forty-nine of them because one run's directory went read-only.
#[test]
fn only_the_named_run_is_failed() {
    let stats = Arc::new(PersistLaneStats::new());
    stats.journal_append_failed("run-1", Path::new("/runs/run-1/run.lvr"), "No space left");
    let mut world = world_with(stats);
    let failing = spawn_run(&mut world, "run-1", AgentStatus::Active);
    let bystander = spawn_run(&mut world, "run-2", AgentStatus::Active);

    run(&mut world);

    error_message(&status_of(&world, failing));
    assert_eq!(status_of(&world, bystander), AgentStatus::Active);
}

/// A run that has already finished is left as it is: its record is closed, and
/// overwriting a `Complete` with an error would rewrite history in the other
/// direction.
#[test]
fn a_run_that_has_already_ended_is_left_alone() {
    let stats = Arc::new(PersistLaneStats::new());
    stats.journal_append_failed("run-1", Path::new("/runs/run-1/run.lvr"), "No space left");
    let mut world = world_with(stats);
    let e = spawn_run(&mut world, "run-1", AgentStatus::Complete);

    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Complete);
}

/// The lane names a run once. A second tick with nothing new must not re-fail a
/// run, and must cost no query at all.
#[test]
fn a_named_run_is_acted_on_once() {
    let stats = Arc::new(PersistLaneStats::new());
    stats.journal_append_failed("run-1", Path::new("/runs/run-1/run.lvr"), "No space left");
    let mut world = world_with(stats);
    let e = spawn_run(&mut world, "run-1", AgentStatus::Active);

    run(&mut world);
    // Put the run back on its feet; a second pass must not knock it over again.
    world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Active;
    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Active);
}

/// A run the world no longer holds is dropped rather than kept for ever.
#[test]
fn a_run_the_world_no_longer_holds_is_dropped() {
    let stats = Arc::new(PersistLaneStats::new());
    stats.journal_append_failed("gone", Path::new("/runs/gone/run.lvr"), "No space left");
    let mut world = world_with(stats);
    let other = spawn_run(&mut world, "run-2", AgentStatus::Active);

    run(&mut world);

    assert_eq!(status_of(&world, other), AgentStatus::Active);
}

/// An agent built without a stage-log buffer is still failed. The buffer is
/// where the reason is written for a person reading the run's logs, and an
/// embedded world that never attached one must not cost the run its status.
#[test]
fn a_run_with_no_stage_log_is_still_failed() {
    let stats = Arc::new(PersistLaneStats::new());
    stats.journal_append_failed("run-1", Path::new("/runs/run-1/run.lvr"), "No space left");
    let mut world = world_with(stats);
    let e = world
        .spawn((agent_state(AgentStatus::Active), run_metadata("run-1")))
        .id();

    run(&mut world);

    error_message(&status_of(&world, e));
}
