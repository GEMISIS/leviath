//! The world's journal: records reach the world, are folded there into the
//! events of each run's next step, and go to the lane as whole steps.

use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::components::{AgentState, AgentStatus, ContextWindow};
use crate::runfile::record::{AttemptOutcome, AttemptRecord, RequestDigest};

fn attempt(id: &str) -> RunRecord {
    RunRecord::InferenceAttempt(Box::new(AttemptRecord {
        id: id.to_string(),
        stage: "s".to_string(),
        attempt: 1,
        provider: "mock".to_string(),
        model: "m".to_string(),
        outcome: AttemptOutcome::Succeeded,
        finish_reason: "stop".to_string(),
        stopped_for: None,
        duration_ms: 1,
        backoff_ms: 0,
        digest: RequestDigest {
            system_hash: 1,
            messages: 1,
            tools: 0,
            max_tokens: 10,
            temperature: 0.0,
        },
        model_input: None,
        at: 1,
    }))
}

fn usage() -> RunRecord {
    RunRecord::InferenceUsage {
        kind: Default::default(),
        stage: "s".to_string(),
        iteration: 1,
        provider: "mock".to_string(),
        model: "m".to_string(),
        prompt_tokens: 3,
        completion_tokens: 2,
        cached_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: None,
        cost_reported_by_provider: None,
        at: 1,
    }
}

fn done(call_id: &str) -> RunRecord {
    RunRecord::ToolCallDone {
        iteration: 1,
        call_id: call_id.to_string(),
        execution_id: "x1".to_string(),
        result: "ran".into(),
        outcome: None,
        at: 1,
    }
}

/// A world with a journal and a lane, and the lane's far end.
fn journal_world() -> (World, JournalSender, mpsc::UnboundedReceiver<PersistMsg>) {
    let (jtx, jrx) = mpsc::unbounded_channel();
    let (ltx, lrx) = mpsc::unbounded_channel();
    let journal = JournalSender::new(jtx, None);
    let mut world = World::new();
    world.insert_resource(journal.clone());
    world.insert_resource(JournalInbox(jrx));
    world.insert_resource(super::super::PersistenceStage(ltx));
    (world, journal, lrx)
}

fn agent() -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_visit: String::new(),
        current_stage: "s".to_string(),
        iteration: 0,
        status: AgentStatus::Active,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

fn spec() -> Arc<crate::spec::run_spec::RunSpec> {
    use crate::test_graph::{graph, layout, region, stage};
    crate::test_graph::both(graph(
        vec![stage("s")],
        layout(
            vec![region(
                "conversation",
                leviath_core::RegionKind::Clearable,
                10_000,
            )],
            12_000,
        ),
    ))
    .0
}

fn metadata(run_id: &str) -> crate::persistence::RunMetadata {
    crate::persistence::RunMetadata {
        run_id: run_id.to_string(),
        agent_name: "a".to_string(),
        agent_path: "/p".to_string(),
        task: "t".to_string(),
        model: None,
        workdir: std::env::temp_dir().to_string_lossy().to_string(),
        num_stages: 1,
        started_at: 0,
        parent_run_id: None,
        metadata: HashMap::new(),
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

/// A window whose conversation holds one stored part.
fn window_with_part() -> ContextWindow {
    let mut window = ContextWindow::new(10_000);
    window.add_region(leviath_core::Region::new(
        "conversation".to_string(),
        leviath_core::RegionKind::Clearable,
        10_000,
    ));
    let part = leviath_core::mime::Part::stored(leviath_core::mime::BlobRef {
        sha256: "ab".repeat(32),
        mime_type: leviath_core::mime::MimeType::parse("image/png").unwrap(),
        size: 3,
        width: None,
        height: None,
        duration_ms: None,
        tokens: 1,
        stand_in: "[image]".into(),
    });
    window
        .get_region_mut("conversation")
        .unwrap()
        .add_entry(
            leviath_core::region::EntryContent::from_parts(vec![part]),
            1,
        )
        .unwrap();
    window
}

/// A run placed from a spec, as `run_id`.
fn spawn_run(world: &mut World, run_id: &str) -> Entity {
    world
        .spawn((
            metadata(run_id),
            agent(),
            window_with_part(),
            crate::insert::RunSpecC(spec()),
        ))
        .id()
}

/// The steps on the lane, in order.
fn steps(lane: &mut mpsc::UnboundedReceiver<PersistMsg>) -> Vec<RunFileStep> {
    let mut out = Vec::new();
    while let Ok(msg) = lane.try_recv() {
        match msg {
            PersistMsg::Step(step) => out.push(*step),
            PersistMsg::Snapshot(job) => out.extend(job.run_file.map(|s| *s)),
            PersistMsg::StageLines { .. } => {}
        }
    }
    out
}

/// A record a system sends waits for the tick it was sent on; one a task
/// sends wakes the world, which may be parked with nothing else to do.
#[tokio::test]
async fn a_tasks_record_wakes_the_world_and_a_systems_does_not() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let wake = Arc::new(Notify::new());
    let journal = JournalSender::new(tx, Some(wake.clone()));
    journal.record("r1", usage());
    let woken = tokio::time::timeout(Duration::from_millis(20), wake.notified()).await;
    assert!(woken.is_err(), "a system's record does not wake the world");
    journal.waking().record("r1", usage());
    tokio::time::timeout(Duration::from_secs(5), wake.notified())
        .await
        .expect("a task's record wakes the world");
    assert_eq!(rx.try_recv().unwrap().run_id, "r1");
    // A handle that does not keep the journal open.
    let weak = journal.downgrade();
    assert!(weak.upgrade().is_some());
    drop(journal);
    assert!(weak.upgrade().is_none(), "nothing holds the journal open");
}

/// Each record becomes its run's events, in the order sent; a model call's
/// attempt waits in the world, across ticks, for the bill that makes it one
/// call; a finished call is noted; and a waiter is kept for the step.
#[test]
fn records_fold_into_each_runs_events_in_the_world() {
    let (mut world, journal, _lane) = journal_world();
    assert!(drain(&mut world).is_empty(), "nothing sent, nothing folded");
    journal.record("r1", attempt("a1"));
    journal.record("r2", done("c1"));
    let landed = journal.record_acked("r2", usage());
    let mut happened = drain(&mut world);
    let r1 = happened.remove("r1").unwrap();
    assert!(!r1.landed);
    assert!(r1.acks.is_empty());
    assert!(matches!(r1.events[..], [RunEvent::Attempt(_)]));
    let r2 = happened.remove("r2").unwrap();
    assert!(r2.landed, "a call finished");
    assert_eq!(r2.acks.len(), 1);
    drop(r2);
    assert!(landed.blocking_recv().is_err(), "the step never went out");
    // The attempt waits for its bill, one tick later.
    assert_eq!(world.resource::<RunJournals>().0.len(), 1);
    journal.record("r1", usage());
    let r1 = drain(&mut world).remove("r1").unwrap();
    let [RunEvent::Inference { attempt, .. }] = &r1.events[..] else {
        panic!("one call: {:?}", r1.events)
    };
    assert_eq!(attempt, "a1");
    assert!(
        world.resource::<RunJournals>().0.is_empty(),
        "nothing waits"
    );
}

/// A world with no journal folds nothing.
#[test]
fn a_world_with_no_journal_folds_nothing() {
    let mut world = World::new();
    assert!(drain(&mut world).is_empty());
    flush(&mut world);
}

/// A run's state, as its file records it, names every stored part its
/// context holds, and keeps naming one once it has left the context.
#[test]
fn the_world_names_the_parts_a_run_holds() {
    let (mut world, _journal, _lane) = journal_world();
    let run = spawn_run(&mut world, "r1");
    let first = run_now(&mut world, run).unwrap();
    assert_eq!(first.state.blobs.len(), 1);
    assert_eq!(first.state.blobs[0].mime_type, "image/png");
    world
        .get_mut::<ContextWindow>(run)
        .unwrap()
        .get_region_mut("conversation")
        .unwrap()
        .clear();
    let later = run_now(&mut world, run).unwrap();
    assert_eq!(later.state.blobs, first.state.blobs);
    // A run not placed from a spec keeps no file.
    let bare = world.spawn((metadata("r2"), agent())).id();
    assert!(run_now(&mut world, bare).is_none());
}

/// What happened goes to the lane as a step of its own: the run's state now
/// when a call of its batch finished, so the file holds the call as done, and
/// only the events otherwise. A world with no lane tells the waiters nothing
/// landed.
#[test]
fn what_happened_goes_to_the_lane_as_steps() {
    let (mut world, journal, mut lane) = journal_world();
    spawn_run(&mut world, "live");
    journal.record("live", done("c1"));
    journal.record("gone", done("c2"));
    journal.record("quiet", usage());
    flush(&mut world);
    let sent = steps(&mut lane);
    let shapes: Vec<(&str, bool, usize)> = sent
        .iter()
        .map(|s| (s.run_id.as_str(), s.now.is_some(), s.events.len()))
        .collect();
    assert_eq!(
        shapes,
        [("gone", false, 2), ("live", true, 2), ("quiet", false, 1)]
    );

    world.remove_resource::<super::super::PersistenceStage>();
    let landed = journal.record_acked("live", usage());
    flush(&mut world);
    assert_eq!(
        landed.blocking_recv().unwrap(),
        crate::persistence_bridge::Appended::NoJournal
    );
}

/// The persist system hands a snapshot's step what happened to its run, and
/// any other run's what happened as a step of its own.
#[test]
fn a_snapshot_carries_what_happened_to_its_run() {
    let (mut world, journal, mut lane) = journal_world();
    let run = spawn_run(&mut world, "r1");
    world.entity_mut(run).insert((
        crate::pipeline::StageCursor { index: 0 },
        crate::persistence::TokenTotals::default(),
        crate::pipeline::PersistWatermark::default(),
    ));
    journal.record("r1", usage());
    journal.record("other", usage());
    super::super::dispatch_persistence(&mut world);
    let sent = steps(&mut lane);
    let shapes: Vec<(&str, bool, usize)> = sent
        .iter()
        .map(|s| (s.run_id.as_str(), s.now.is_some(), s.events.len()))
        .collect();
    assert_eq!(shapes, [("r1", true, 1), ("other", false, 1)]);
}
