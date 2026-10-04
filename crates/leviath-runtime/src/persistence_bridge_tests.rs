use super::*;
use crate::runfile::lane::{RunFileStep, RunNow};
use crate::runfile::reader_tests::{initial, spec};
use crate::state::RunEvent;
use leviath_core::run_meta::RunMeta;
use tokio::sync::mpsc;

fn meta(run_id: &str) -> RunMeta {
    RunMeta::new(
        run_id.to_string(),
        "agent".to_string(),
        "/path".to_string(),
        "task".to_string(),
        None,
        "/work".to_string(),
        1,
    )
}

/// Fresh counters.
fn health() -> std::sync::Arc<PersistLaneStats> {
    std::sync::Arc::new(PersistLaneStats::new())
}

fn job(run_id: &str) -> PersistJob {
    PersistJob {
        run_id: run_id.to_string(),
        meta: meta(run_id),
        output_appends: vec![],
        log_appends: vec![],
        taint_audit: None,
        final_output: None,
        run_file: None,
    }
}

/// The run-file step of a run of `spec()` in its `initial()` state.
fn step(run_id: &str) -> Option<Box<RunFileStep>> {
    Some(Box::new(RunFileStep {
        run_id: run_id.to_string(),
        now: Some(RunNow {
            spec: std::sync::Arc::new(spec()),
            state: initial(),
        }),
        at: 1,
        events: Vec::new(),
        acks: Vec::new(),
    }))
}

/// A line that says something happened, as a step's event.
fn log(line: &str) -> RunEvent {
    RunEvent::Log(line.to_string())
}

/// A step of `events` for `run_id`, and the waiter that hears where it
/// landed.
fn events(
    run_id: &str,
    events: Vec<RunEvent>,
) -> (PersistMsg, tokio::sync::oneshot::Receiver<Appended>) {
    let (ack, landed) = tokio::sync::oneshot::channel();
    let mut step = RunFileStep::events(run_id, 1, events);
    step.acks.push(ack);
    (PersistMsg::Step(Box::new(step)), landed)
}

/// A snapshot whose step holds `events`, and the waiter that hears where it
/// landed.
fn stepped_with(
    run_id: &str,
    happened: Vec<RunEvent>,
) -> (PersistMsg, tokio::sync::oneshot::Receiver<Appended>) {
    let (ack, landed) = tokio::sync::oneshot::channel();
    let mut job = stepped(run_id);
    let step = job.run_file.as_mut().unwrap();
    step.events = happened;
    step.acks.push(ack);
    (PersistMsg::Snapshot(job), landed)
}

/// Every event `run_id`'s file holds, step by step.
fn steps_of(runs: &Path, run_id: &str) -> Vec<Vec<RunEvent>> {
    let read =
        crate::runfile::RunFileReader::open(&runs.join(run_id).join(leviath_core::files::RUN_FILE))
            .unwrap();
    read.deltas(1, read.last_seq())
        .unwrap()
        .into_iter()
        .map(|d| d.events)
        .collect()
}

/// A job carrying its run-file step.
fn stepped(run_id: &str) -> Box<PersistJob> {
    Box::new(PersistJob {
        run_file: step(run_id),
        ..job(run_id)
    })
}

/// Run the worker over `msgs` in one batch, under `runs`.
async fn run_lane(runs: &Path, msgs: Vec<PersistMsg>) -> std::sync::Arc<PersistLaneStats> {
    let (tx, rx) = mpsc::unbounded_channel();
    for msg in msgs {
        tx.send(msg).unwrap();
    }
    drop(tx);
    let stats = health();
    persistence_worker(Some(runs.to_path_buf()), rx, stats.clone()).await;
    stats
}

/// The whole loop, end to end: two snapshots for one run in a single batch,
/// both describing and carrying the answer. The first is dropped as
/// superseded, and the surviving one must still produce the sidecar.
#[tokio::test]
async fn worker_writes_the_sidecar_even_when_the_first_snapshot_is_coalesced_away() {
    let dir = tempfile::tempdir().unwrap();
    let answered = |body: &str| {
        let mut j = job("run-fast");
        j.meta.final_output = Some(leviath_core::output::FinalOutputDescriptor {
            format: None,
            stage: "out".to_string(),
            submitted_at: 100,
            bytes: body.len(),
            truncated: false,
            artifacts: Vec::new(),
        });
        j.final_output = Some(body.to_string());
        PersistMsg::Snapshot(Box::new(j))
    };
    run_lane(
        dir.path(),
        vec![answered("the answer"), answered("the answer")],
    )
    .await;
    let sidecar = dir
        .path()
        .join("run-fast")
        .join(leviath_core::FINAL_OUTPUT_FILE);
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "the answer");
}

/// A snapshot superseded in its batch is not written, but the stage lines it
/// carried are, before the newer snapshot's, so a fast run's output reaches
/// its stage log (which is what an ACP host streams from).
#[tokio::test]
async fn a_superseded_snapshots_stage_lines_still_reach_the_stage_log() {
    let dir = tempfile::tempdir().unwrap();
    let lines = |out: &str, log: &str| {
        PersistMsg::Snapshot(Box::new(PersistJob {
            output_appends: vec![(0, out.to_string())],
            log_appends: vec![(0, log.to_string())],
            ..job("run-fast")
        }))
    };
    run_lane(
        dir.path(),
        vec![lines("first", "[a]"), lines("second", "[b]")],
    )
    .await;
    let stage = dir.path().join("run-fast").join("stages").join("0");
    let output = std::fs::read_to_string(stage.join("output.log")).unwrap();
    assert_eq!(output.lines().collect::<Vec<_>>(), ["first", "second"]);
    let logs = std::fs::read_to_string(stage.join("logs.log")).unwrap();
    assert_eq!(logs.lines().collect::<Vec<_>>(), ["[a]", "[b]"]);
}

/// A snapshot writes its run's step into the run file, and the run file is
/// the only file it writes.
#[tokio::test]
async fn a_snapshot_writes_its_step_into_the_run_file_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let stats = run_lane(dir.path(), vec![PersistMsg::Snapshot(stepped("run-1"))]).await;
    let run_dir = dir.path().join("run-1");
    let read =
        crate::runfile::RunFileReader::open(&run_dir.join(leviath_core::files::RUN_FILE)).unwrap();
    assert_eq!(read.latest_state().unwrap(), initial());
    let written: Vec<String> = std::fs::read_dir(&run_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(written, [leviath_core::files::RUN_FILE]);
    let report = stats.report();
    assert_eq!(report.appends_attempted, 1);
    assert!(report.is_healthy());
}

/// The files written beside the run file are private to this user. The mode
/// assertion is Unix-only because that is where modes exist; on Windows
/// `leviath-sys` applies the equivalent ACL.
#[tokio::test]
async fn every_file_beside_the_run_file_is_private_to_this_user() {
    let dir = tempfile::tempdir().unwrap();
    let mut j = job("run-perms");
    j.final_output = Some("the answer".to_string());
    j.taint_audit = Some((0, "[]".to_string()));
    j.log_appends = vec![(0, "a line".to_string())];
    j.output_appends = vec![(0, "said".to_string())];
    let outcome = write_snapshot(dir.path(), &j, None).await;
    assert!(outcome.dir_made && outcome.files.is_none());
    assert_eq!(outcome.named.stages.len(), 1);
    assert!(outcome.named.final_output.is_some());
    let run_dir = dir.path().join("run-perms");
    for path in [
        run_dir.join(leviath_core::FINAL_OUTPUT_FILE),
        run_dir.join("stages").join("0").join("taint_audit.json"),
        run_dir.join("stages").join("0").join("logs.log"),
        run_dir.join("stages").join("0").join("output.log"),
    ] {
        assert!(path.exists(), "{} was written", path.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} should be private", path.display());
        }
    }
}

/// An answer this lane already wrote is not written again by a heartbeat,
/// and a run with no answer writes no sidecar.
#[tokio::test]
async fn an_answer_is_written_once_and_no_answer_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut j = job("run-answer");
    j.final_output = Some("first".to_string());
    j.meta.final_output = Some(leviath_core::output::FinalOutputDescriptor {
        format: None,
        stage: "out".to_string(),
        submitted_at: 100,
        bytes: 5,
        truncated: false,
        artifacts: Vec::new(),
    });
    let wrote = write_snapshot(dir.path(), &j, None).await.output;
    assert_eq!(wrote, Some((100, 5)));
    j.final_output = Some("second".to_string());
    assert!(write_snapshot(dir.path(), &j, wrote).await.output.is_none());
    let sidecar = dir
        .path()
        .join("run-answer")
        .join(leviath_core::FINAL_OUTPUT_FILE);
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "first");

    write_snapshot(dir.path(), &job("run-none"), None).await;
    assert!(
        !dir.path()
            .join("run-none")
            .join(leviath_core::FINAL_OUTPUT_FILE)
            .exists()
    );
}

/// Lines with no snapshot behind them still reach the stage logs, and a
/// finished run's answer watermark is dropped.
#[tokio::test]
async fn stage_lines_land_without_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut done = job("run-1");
    done.meta.status = leviath_core::run_meta::RunStatus::Complete;
    run_lane(
        dir.path(),
        vec![
            PersistMsg::Snapshot(Box::new(done)),
            PersistMsg::StageLines {
                run_id: "run-1".to_string(),
                output_appends: vec![(0, "out".to_string())],
                log_appends: vec![(0, "log".to_string())],
            },
        ],
    )
    .await;
    let stage = dir.path().join("run-1").join("stages").join("0");
    assert_eq!(
        std::fs::read_to_string(stage.join("output.log")).unwrap(),
        "out\n"
    );
    assert_eq!(
        std::fs::read_to_string(stage.join("logs.log")).unwrap(),
        "log\n"
    );
}

/// A stage log that cannot be opened is logged, not counted: the logs are a
/// readable view, not the run's history.
#[tokio::test]
async fn a_stage_log_that_cannot_be_opened_is_not_a_loss() {
    crate::test_support::with_tracing(|| {});
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("r").join("stages").join("0");
    std::fs::create_dir_all(stage.join("logs.log")).unwrap();
    let mut named = RunFiles::default();
    let lines = [(0, "x".to_string())];
    append_lines(&dir.path().join("r"), &[], &lines, "r", &mut named).await;
    assert_eq!(named, RunFiles::default(), "a log not written is not named");
}

/// A world with no runs dir writes nothing and still answers every step's
/// waiters.
#[tokio::test]
async fn worker_without_a_runs_dir_drains_messages_and_writes_nothing() {
    let (tx, rx) = mpsc::unbounded_channel();
    let (snapshot, snapshot_landed) = stepped_with("r", vec![log("a")]);
    let (step, step_landed) = events("r", vec![log("b")]);
    tx.send(snapshot).unwrap();
    tx.send(PersistMsg::Snapshot(Box::new(job("r")))).unwrap();
    tx.send(step).unwrap();
    tx.send(PersistMsg::StageLines {
        run_id: "r".to_string(),
        output_appends: vec![(0, "x".to_string())],
        log_appends: vec![],
    })
    .unwrap();
    drop(tx);
    persistence_worker(None, rx, health()).await;
    assert_eq!(snapshot_landed.await.unwrap(), Appended::NoJournal);
    assert_eq!(step_landed.await.unwrap(), Appended::NoJournal);
}

/// A step is written as it comes and tells its waiters where it landed, so
/// a tool batch is on disk before it runs. One for a run with no file yet,
/// one with nothing in it, and one for a run never written land nowhere and
/// say so.
#[tokio::test]
async fn a_step_tells_its_waiters_where_it_landed() {
    let dir = tempfile::tempdir().unwrap();
    let (early, before) = events("run-1", vec![log("early")]);
    let (first, landed) = events("run-1", vec![log("batch")]);
    let (quiet, nothing) = events("run-1", Vec::new());
    let (gone, deleted) = events("never-written", vec![log("lost")]);
    run_lane(
        dir.path(),
        vec![
            early,
            PersistMsg::Snapshot(stepped("run-1")),
            first,
            quiet,
            gone,
        ],
    )
    .await;
    assert_eq!(before.await.unwrap(), Appended::NoJournal, "no file yet");
    assert_eq!(landed.await.unwrap(), Appended::Landed { position: 1 });
    assert_eq!(
        nothing.await.unwrap(),
        Appended::NoJournal,
        "a step with nothing in it writes nothing"
    );
    assert_eq!(deleted.await.unwrap(), Appended::NoJournal);
    assert_eq!(steps_of(dir.path(), "run-1"), vec![vec![log("batch")]]);
    assert!(!dir.path().join("never-written").exists());
}

/// A snapshot superseded in its batch loses its state to the newer one, but
/// not what happened: its events and waiters ride on the run's next step,
/// ahead of that step's own, whether that is a step of events or the newest
/// snapshot, and a newest snapshot with no step of its own gets one for them.
#[tokio::test]
async fn a_superseded_snapshots_events_ride_on_the_next_step() {
    let dir = tempfile::tempdir().unwrap();
    let (s1, first) = stepped_with("run-1", vec![log("a")]);
    let (between, second) = events("run-1", vec![log("b")]);
    let (s2, third) = stepped_with("run-1", vec![log("c")]);
    let (s3, fourth) = stepped_with("run-1", vec![log("d")]);
    let (s4, fifth) = stepped_with("run-1", vec![log("e")]);
    run_lane(dir.path(), vec![PersistMsg::Snapshot(stepped("run-1"))]).await;
    run_lane(dir.path(), vec![s1, between, s2, s3, s4]).await;
    // The second batch's newest snapshot here has no step of its own.
    let (s5, sixth) = stepped_with("run-1", vec![log("f")]);
    run_lane(
        dir.path(),
        vec![s5, PersistMsg::Snapshot(Box::new(job("run-1")))],
    )
    .await;
    assert_eq!(first.await.unwrap(), Appended::Landed { position: 1 });
    assert_eq!(second.await.unwrap(), Appended::Landed { position: 1 });
    for later in [third, fourth, fifth] {
        assert_eq!(later.await.unwrap(), Appended::Landed { position: 2 });
    }
    assert_eq!(sixth.await.unwrap(), Appended::Landed { position: 3 });
    assert_eq!(
        steps_of(dir.path(), "run-1"),
        vec![
            vec![log("a"), log("b")],
            vec![log("c"), log("d"), log("e")],
            vec![log("f")],
        ]
    );
}

/// A run file step that cannot be written is the run's history lost: it is
/// counted and names its run, so the world fails it. A file beside it that
/// is lost is only counted.
#[tokio::test]
async fn a_lost_step_fails_its_run_and_a_lost_file_does_not() {
    crate::test_support::with_tracing(|| {});
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("run-lost")).unwrap();
    std::fs::write(
        dir.path()
            .join("run-lost")
            .join(leviath_core::files::RUN_FILE),
        b"not a run file",
    )
    .unwrap();
    let mut answered = job("run-file");
    answered.final_output = Some("x".to_string());
    std::fs::create_dir_all(
        dir.path()
            .join("run-file")
            .join(leviath_core::FINAL_OUTPUT_FILE)
            .with_extension("tmp"),
    )
    .unwrap();
    let stats = run_lane(
        dir.path(),
        vec![
            PersistMsg::Snapshot(stepped("run-lost")),
            PersistMsg::Snapshot(Box::new(answered)),
        ],
    )
    .await;
    let report = stats.report();
    assert_eq!(report.appends_failed, 1);
    assert_eq!(report.snapshots_failed, 1);
    assert_eq!(
        stats
            .take_unwritable()
            .iter()
            .map(|e| e.run_id.clone())
            .collect::<Vec<_>>(),
        vec!["run-lost".to_string()]
    );
}

/// A step that cannot be written tells its waiters so, and fails the run.
#[tokio::test]
async fn a_step_that_cannot_be_written_says_so() {
    crate::test_support::with_tracing(|| {});
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("run-1")).unwrap();
    let mut lane = crate::runfile::lane::RunFileLane::new("m", "w");
    let stats = health();
    record_run_file(&mut lane, dir.path(), *step("run-1").unwrap(), &stats).await;
    lane.break_writes("run-1");
    let (ack, landed) = tokio::sync::oneshot::channel();
    let mut broken = RunFileStep::events("run-1", 2, vec![log("lost")]);
    broken.acks.push(ack);
    record_run_file(&mut lane, dir.path(), broken, &stats).await;
    assert_eq!(landed.await.unwrap(), Appended::Failed);
    assert_eq!(stats.report().appends_failed, 1);
    assert_eq!(stats.take_unwritable()[0].run_id, "run-1");
}

/// A snapshot that cannot make its run directory never reaches the run
/// file, so no run is failed for it: a directory that will not be made is
/// also what a delete leaves behind.
#[tokio::test]
async fn a_snapshot_with_nowhere_to_write_fails_no_run() {
    crate::test_support::with_tracing(|| {});
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("run-blocked"), b"in the way").unwrap();
    let (blocked, landed) = stepped_with("run-blocked", vec![log("x")]);
    let stats = run_lane(dir.path(), vec![blocked]).await;
    assert_eq!(landed.await.unwrap(), Appended::NoJournal);
    let report = stats.report();
    assert_eq!(report.appends_attempted, 0);
    assert_eq!(report.snapshots_failed, 1);
    assert!(stats.take_unwritable().is_empty());
}

/// A run deleted between snapshots is not written back, and its writer is
/// forgotten.
#[tokio::test]
async fn a_deleted_run_is_not_written_back() {
    crate::test_support::with_tracing(|| {});
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let lane = tokio::spawn(persistence_worker(
        Some(dir.path().to_path_buf()),
        rx,
        health(),
    ));
    let (first, acked) = events("run-1", vec![log("c0")]);
    tx.send(PersistMsg::Snapshot(stepped("run-1"))).unwrap();
    tx.send(first).unwrap();
    acked.await.unwrap();
    std::fs::remove_dir_all(dir.path().join("run-1")).unwrap();
    let (again, dropped) = stepped_with("run-1", vec![log("c1")]);
    tx.send(again).unwrap();
    tx.send(PersistMsg::StageLines {
        run_id: "run-1".to_string(),
        output_appends: vec![(0, "x".to_string())],
        log_appends: vec![],
    })
    .unwrap();
    drop(tx);
    lane.await.unwrap();
    assert_eq!(dropped.await.unwrap(), Appended::NoJournal);
    assert!(
        !dir.path().join("run-1").exists(),
        "nothing brought it back"
    );
}

/// A temp write or a rename that fails is counted, and keeps the first loss.
#[tokio::test]
async fn a_lost_write_keeps_the_first_loss() {
    crate::test_support::with_tracing(|| {});
    let dir = tempfile::tempdir().unwrap();
    let run_dir = dir.path().join("r");
    std::fs::create_dir_all(&run_dir).unwrap();
    // The answer's temp path is a directory, so its write fails; the audit's
    // target is a non-empty directory, so its rename fails.
    std::fs::create_dir_all(
        run_dir
            .join(leviath_core::FINAL_OUTPUT_FILE)
            .with_extension("tmp"),
    )
    .unwrap();
    std::fs::create_dir_all(
        run_dir
            .join("stages")
            .join("0")
            .join("taint_audit.json")
            .join("x"),
    )
    .unwrap();
    let mut j = job("r");
    j.final_output = Some("x".to_string());
    j.taint_audit = Some((0, "[]".to_string()));
    let outcome = write_snapshot(dir.path(), &j, None).await;
    // A file that was not placed is not named.
    assert_eq!(outcome.named, RunFiles::default());
    let lost = outcome.files.expect("both were lost");
    assert!(
        lost.path.extension().is_some_and(|e| e == "tmp"),
        "{lost:?}"
    );
}

/// The answer, the stage logs and the taint audit are written beside the
/// run file and named by the step that follows them, with their sizes and,
/// for a file written whole, its digest; none of their bytes go into the run
/// file. Lines that arrive after the step are named when the lane stops.
#[tokio::test]
async fn the_files_beside_a_run_are_named_by_its_step_and_never_copied_in() {
    let dir = tempfile::tempdir().unwrap();
    let answer = "an answer long enough to find in the run file ".repeat(40);
    let mut first = *stepped("run-1");
    first.final_output = Some(answer.clone());
    first.taint_audit = Some((1, "[\"audit\"]".to_string()));
    first.output_appends = vec![(0, "said".to_string())];
    first.log_appends = vec![(0, "a line".to_string()), (0, "another".to_string())];
    run_lane(
        dir.path(),
        vec![
            PersistMsg::Snapshot(Box::new(first)),
            PersistMsg::StageLines {
                run_id: "run-1".to_string(),
                output_appends: vec![],
                log_appends: vec![(0, "later".to_string())],
            },
        ],
    )
    .await;
    let run_dir = dir.path().join("run-1");
    let path = run_dir.join(leviath_core::files::RUN_FILE);
    let files = crate::runfile::RunFileReader::open(&path)
        .unwrap()
        .latest_state()
        .unwrap()
        .files;
    let named = files.final_output.as_ref().unwrap();
    assert_eq!(named.read(&run_dir).unwrap(), answer.as_bytes());
    assert_eq!(
        named.sha256,
        Some(crate::spec::names::Digest::of(answer.as_bytes()))
    );
    let audit = files.stage_file(1, StageFile::TaintAudit).unwrap();
    assert_eq!(audit.read(&run_dir).unwrap(), b"[\"audit\"]");
    assert_eq!(files.stage_file(0, StageFile::Output).unwrap().bytes, 5);
    // The line that came after the step is named as the lane stops.
    assert_eq!(files.stage_file(0, StageFile::Logs).unwrap().bytes, 21);
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes.windows(20).any(|w| w == &answer.as_bytes()[..20]),
        "the answer is not copied into the run file"
    );
}

#[tokio::test]
async fn a_vanished_blocking_task_becomes_an_io_error() {
    let err = tokio::task::spawn(async { panic!("gone") })
        .await
        .unwrap_err();
    assert_eq!(vanished_task(err).kind(), std::io::ErrorKind::Other);
}

#[test]
fn is_terminal_run_classifies_statuses() {
    use leviath_core::run_meta::RunStatus;
    assert!(is_terminal_run(&RunStatus::Complete));
    assert!(is_terminal_run(&RunStatus::Error));
    assert!(is_terminal_run(&RunStatus::Cancelled));
    assert!(!is_terminal_run(&RunStatus::Running));
    assert!(!is_terminal_run(&RunStatus::CompleteInteractive));
}

#[test]
fn machine_id_is_persisted_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let runs = dir.path().join("runs");
    let first = load_or_create_machine_id(&runs);
    assert_eq!(load_or_create_machine_id(&runs), first);
    std::fs::write(dir.path().join("machine-id"), "  ").unwrap();
    assert_ne!(load_or_create_machine_id(&runs), "");
}

#[test]
fn generate_id_is_sixteen_hex_chars() {
    let id = generate_id();
    assert_eq!(id.len(), 16);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}
