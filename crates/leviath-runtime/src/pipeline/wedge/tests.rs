use super::*;

fn agent_state(status: AgentStatus) -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_stage: "implement".to_string(),
        iteration: 3,
        status,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

/// An agent holding no phase marker at all, first seen that way `age` seconds
/// ago. This is the state the pipeline's invariants say cannot happen.
fn spawn_wedged(world: &mut World, age: i64) -> Entity {
    let now = chrono::Utc::now().timestamp();
    world
        .spawn((
            agent_state(AgentStatus::Active),
            StageIoBuffer::default(),
            Wedged { since: now - age },
        ))
        .id()
}

fn run(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(fail_wedged_runs);
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

/// The whole point: a run nothing can drive is failed, so the capacity it was
/// holding comes back and an observer stops being told it is running.
#[test]
fn a_run_no_system_can_reach_is_failed() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(300));
    let e = spawn_wedged(&mut world, 301);

    run(&mut world);

    let message = error_message(&status_of(&world, e));
    assert!(message.contains("implement"), "names the stage: {message}");
    assert!(message.contains("never move again"), "{message}");
    // The record is spent, and the operator sees why in the stage log.
    assert!(world.get::<Wedged>(e).is_none());
    let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
    assert!(
        logs.iter().any(|(_, line)| line.starts_with("[wedged]")),
        "expected a [wedged] log line, got: {logs:?}"
    );
}

/// Inside the grace period nothing is failed, and the record is stamped so the
/// next tick measures the whole wait rather than restarting the clock.
#[test]
fn inside_the_grace_period_it_is_only_recorded() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(300));
    let now = chrono::Utc::now().timestamp();
    let e = world
        .spawn((agent_state(AgentStatus::Active), StageIoBuffer::default()))
        .id();

    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Active);
    let since = world.get::<Wedged>(e).expect("recorded").since;
    assert!((since - now).abs() <= 5, "stamped at roughly now: {since}");

    // A second pass keeps the original start rather than pushing it forward,
    // which is what stops a wedged run being granted a fresh grace period on
    // every tick, for ever.
    run(&mut world);
    assert_eq!(world.get::<Wedged>(e).expect("still recorded").since, since);
}

/// An empty fan-out state, built through serde so the test does not need
/// `FanOutWaiting`'s private fields.
fn empty_fan_out_state() -> crate::fanout::FanOutState {
    serde_json::from_value(serde_json::json!({
        "config": { "worker_agent": "w", "split_prompt": "s" },
        "max_workers": 1,
        "pending": [],
        "active": [],
        "summaries": [],
        "failures": [],
    }))
    .expect("a minimal fan-out state deserializes")
}

/// The enforcement test for [`Unreachable`]. Every marker in that filter gets an
/// agent aged far past the timeout; if any one of them is missing from the
/// filter, that agent is failed and this test names it.
///
/// A contributor adding a phase marker will write a test for their new resting
/// state, and this watchdog will kill it until they add the marker here.
#[test]
fn every_phase_marker_exempts_its_agent() {
    type Insert = fn(&mut World, Entity);
    let markers: &[(&str, Insert)] = &[
        ("ReadyToInfer", |w, e| {
            w.entity_mut(e).insert(ReadyToInfer);
        }),
        ("AwaitingInference", |w, e| {
            w.entity_mut(e).insert(AwaitingInference);
        }),
        ("ProcessResponse", |w, e| {
            w.entity_mut(e).insert(ProcessResponse);
        }),
        ("ReadyForTools", |w, e| {
            w.entity_mut(e).insert(ReadyForTools);
        }),
        ("AwaitingTools", |w, e| {
            w.entity_mut(e).insert(AwaitingTools);
        }),
        ("ReadyForTransition", |w, e| {
            w.entity_mut(e).insert(ReadyForTransition);
        }),
        ("ResolveTransition", |w, e| {
            w.entity_mut(e).insert(ResolveTransition);
        }),
        ("ToolsNeedRefresh", |w, e| {
            w.entity_mut(e).insert(ToolsNeedRefresh);
        }),
        ("StageJustEntered", |w, e| {
            w.entity_mut(e).insert(StageJustEntered {
                index: 0,
                name: "s".to_string(),
            });
        }),
        ("AwaitingTransitionChoice", |w, e| {
            w.entity_mut(e).insert(AwaitingTransitionChoice(vec![]));
        }),
        ("AwaitingTransitionResponse", |w, e| {
            w.entity_mut(e).insert(AwaitingTransitionResponse(vec![]));
        }),
        ("WaitingForChildren", |w, e| {
            w.entity_mut(e).insert(WaitingForChildren);
        }),
        ("AwaitingCompaction", |w, e| {
            w.entity_mut(e).insert(AwaitingCompaction);
        }),
        ("PendingEdgeCompact", |w, e| {
            w.entity_mut(e).insert(PendingEdgeCompact(vec![]));
        }),
        ("AwaitingContentSummary", |w, e| {
            w.entity_mut(e)
                .insert(crate::context_transform::AwaitingContentSummary);
        }),
        ("PendingContentSummary", |w, e| {
            w.entity_mut(e)
                .insert(crate::context_transform::PendingContentSummary(vec![]));
        }),
        ("PendingTitle", |w, e| {
            w.entity_mut(e).insert(crate::title::PendingTitle);
        }),
        ("AwaitingTitle", |w, e| {
            w.entity_mut(e)
                .insert(crate::title::AwaitingTitle(i64::MAX));
        }),
        ("AwaitingInteraction", |w, e| {
            w.entity_mut(e)
                .insert(crate::components::AwaitingInteraction);
        }),
        ("AwaitingGatePrompt", |w, e| {
            w.entity_mut(e)
                .insert(crate::gate_prompt::AwaitingGatePrompt(0));
        }),
        ("GateResolved", |w, e| {
            w.entity_mut(e)
                .insert(crate::gate_prompt::GateResolved::default());
        }),
        ("ReadyForInteractionPoint", |w, e| {
            w.entity_mut(e)
                .insert(crate::interaction_points::ReadyForInteractionPoint);
        }),
        ("AwaitingInteractionPoint", |w, e| {
            w.entity_mut(e)
                .insert(crate::interaction_points::AwaitingInteractionPoint);
        }),
        ("FanOutWaiting", |w, e| {
            crate::fanout::restore_fan_out_waiting(w, e, empty_fan_out_state(), &|_| None);
        }),
        ("InFlightWork", |w, e| {
            w.entity_mut(e).insert(InFlightWork(vec![]));
        }),
        ("PanickedInParallel", |w, e| {
            w.entity_mut(e)
                .insert(crate::tick_scope::PanickedInParallel {
                    message: "boom".to_string(),
                });
        }),
    ];

    for (name, insert) in markers {
        let mut world = World::new();
        world.insert_resource(WedgeTimeout(300));
        let e = spawn_wedged(&mut world, 10_000);
        insert(&mut world, e);

        run(&mut world);

        assert_eq!(
            status_of(&world, e),
            AgentStatus::Active,
            "an agent resting on {name} is reachable and must not be failed; \
             add it to `Unreachable`"
        );
    }
}

/// A paused run is stopped because somebody stopped it. It must not be failed,
/// and it must not come back with a clock already running against it.
#[test]
fn a_paused_run_is_never_failed_and_loses_its_record() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(300));
    let e = spawn_wedged(&mut world, 10_000);
    world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Paused;

    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Paused);
    assert!(
        world.get::<Wedged>(e).is_none(),
        "resuming must not find a run already most of the way to failed"
    );
}

/// A finished run holds no marker either, and is not a problem. The host reaps
/// it once its state has been persisted and reported.
#[test]
fn a_finished_run_is_left_to_the_host() {
    for status in [
        AgentStatus::Complete,
        AgentStatus::Cancelled,
        AgentStatus::Error {
            message: "already failed".to_string(),
        },
    ] {
        let mut world = World::new();
        world.insert_resource(WedgeTimeout(300));
        let e = spawn_wedged(&mut world, 10_000);
        world.get_mut::<AgentState>(e).unwrap().status = status.clone();

        run(&mut world);

        assert_eq!(status_of(&world, e), status);
        assert!(world.get::<Wedged>(e).is_none());
    }
}

/// A terminal agent that was never recorded takes the other side of the branch:
/// nothing to clear, nothing to fail.
#[test]
fn a_finished_run_with_no_record_is_a_no_op() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(300));
    let e = world
        .spawn((agent_state(AgentStatus::Complete), StageIoBuffer::default()))
        .id();

    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Complete);
    assert!(world.get::<Wedged>(e).is_none());
}

/// Issue #204's territory, not this watchdog's. A run parked on a question
/// somebody may still answer holds an interaction marker, and killing it is a
/// worse failure than leaving the slot occupied.
#[test]
fn a_run_blocked_on_a_person_is_left_to_issue_204() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(300));
    let e = spawn_wedged(&mut world, 10_000);
    world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Waiting;
    world
        .entity_mut(e)
        .insert(crate::components::AwaitingInteraction);

    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Waiting);
}

/// An agent that becomes reachable again drops its record, so a later, unrelated
/// wedge is measured from when it started rather than inheriting an old clock.
#[test]
fn a_recovered_agent_starts_a_fresh_clock() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(300));
    let e = spawn_wedged(&mut world, 299);
    world.entity_mut(e).insert(ReadyToInfer);

    // Reachable: the filter skips it entirely, so nothing happens to it.
    run(&mut world);
    assert_eq!(status_of(&world, e), AgentStatus::Active);

    // The engine removes the record along with the marker; do the same and
    // confirm the clock restarts rather than firing immediately.
    world.entity_mut(e).remove::<ReadyToInfer>();
    world.entity_mut(e).remove::<Wedged>();
    run(&mut world);
    assert_eq!(
        status_of(&world, e),
        AgentStatus::Active,
        "the old 299-second wait must not carry over"
    );
    assert!(world.get::<Wedged>(e).is_some());
}

/// Not every agent carries a stage log (a bare embedded world may not), and the
/// failure must still land.
#[test]
fn an_agent_without_a_stage_log_still_fails() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(300));
    let now = chrono::Utc::now().timestamp();
    let e = world
        .spawn((
            agent_state(AgentStatus::Active),
            Wedged { since: now - 301 },
        ))
        .id();

    run(&mut world);

    let _ = error_message(&status_of(&world, e));
}

#[test]
fn a_zero_timeout_disables_the_watchdog() {
    let mut world = World::new();
    world.insert_resource(WedgeTimeout(0));
    let e = spawn_wedged(&mut world, 10_000);

    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Active);
}

/// A world that never installed the resource gets the default, which is off. An
/// embedded runtime therefore behaves exactly as it did before this existed.
///
/// Off by default is deliberate, not an oversight: this watchdog fails runs, and
/// an upgrade that starts killing work nobody asked it to kill is worse than the
/// leak it prevents. Pinned so the default cannot drift silently.
#[test]
fn a_world_without_the_resource_is_off_by_default() {
    let mut world = World::new();
    let e = spawn_wedged(&mut world, 10_000);

    run(&mut world);

    assert_eq!(status_of(&world, e), AgentStatus::Active);
    assert_eq!(DEFAULT_WEDGE_TIMEOUT_SECS, 0);
    assert_eq!(WedgeTimeout::default().0, DEFAULT_WEDGE_TIMEOUT_SECS);
}
