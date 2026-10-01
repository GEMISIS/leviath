use super::*;
use crate::runfile::reader_tests::{initial, spec};
use leviath_core::run_archive::RunRecord;
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
fn step(run_id: &str) -> Option<Box<crate::runfile::lane::RunFileStep>> {
    Some(Box::new(crate::runfile::lane::RunFileStep {
        run_id: run_id.to_string(),
        spec: std::sync::Arc::new(spec()),
        state: initial(),
        at: 1,
    }))
}

/// A job carrying its run-file step.
fn stepped(run_id: &str) -> Box<PersistJob> {
    Box::new(PersistJob {
        run_file: step(run_id),
        ..job(run_id)
    })
}

/// A record that a tool batch was dispatched.
fn batch_record(call_id: &str) -> RunRecord {
    RunRecord::ToolBatch {
        calls: vec![leviath_core::run_archive::ToolCallRecord {
            id: call_id.to_string(),
            execution_id: String::new(),
            name: "shell".to_string(),
            arguments: "{}".to_string(),
            result: None,
            thought_signature: None,
        }],
        at: 1,
        stage_index: 0,
        iteration: 0,
        visit_id: String::new(),
        requested_by: String::new(),
        response: "running".to_string(),
    }
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

/// A snapshot writes its run's step into the run file and nothing in the
/// older layout: no `meta.json`, no `context.json`, no LVR1 journal.
#[tokio::test]
async fn a_snapshot_writes_its_step_and_none_of_the_older_files() {
    let dir = tempfile::tempdir().unwrap();
    let stats = run_lane(dir.path(), vec![PersistMsg::Snapshot(stepped("run-1"))]).await;
    let run_dir = dir.path().join("run-1");
    let read =
        crate::runfile::RunFileReader::open(&run_dir.join(leviath_core::files::RUN_FILE)).unwrap();
    assert_eq!(read.latest_state().unwrap(), initial());
    for old in [
        leviath_core::files::META_FILE,
        leviath_core::files::CONTEXT_FILE,
        leviath_core::files::STAGES_FILE,
        leviath_core::files::FANOUT_FILE,
        leviath_core::files::INTERACTIONS_FILE,
    ] {
        assert!(!run_dir.join(old).exists(), "{old} is not written");
    }
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
    let outcome = write_snapshot(dir.path(), &j, None).await;
    assert!(outcome.dir_made && outcome.files.is_none());
    let run_dir = dir.path().join("run-perms");
    for path in [
        run_dir.join(leviath_core::FINAL_OUTPUT_FILE),
        run_dir.join("stages").join("0").join("taint_audit.json"),
        run_dir.join("stages").join("0").join("logs.log"),
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
    append_stage_line(&dir.path().join("r"), 0, "logs.log", "x", "r").await;
}

/// A world with no runs dir writes nothing and still acks every append.
#[tokio::test]
async fn worker_without_a_runs_dir_drains_messages_and_writes_nothing() {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ack, acked) = tokio::sync::oneshot::channel();
    tx.send(PersistMsg::Snapshot(stepped("r"))).unwrap();
    tx.send(PersistMsg::Append {
        run_id: "r".to_string(),
        record: Box::new(batch_record("c1")),
        ack: Some(ack),
    })
    .unwrap();
    tx.send(PersistMsg::Append {
        run_id: "r".to_string(),
        record: Box::new(batch_record("c2")),
        ack: None,
    })
    .unwrap();
    drop(tx);
    persistence_worker(None, rx, health()).await;
    assert_eq!(acked.await.unwrap(), Appended::NoJournal);
}

/// A record with an ack is written as a step of its own before the ack, so
/// a tool batch is on disk before it runs. One for a run with no file open,
/// or for a deleted run, lands nowhere and says so.
#[tokio::test]
async fn an_acked_record_is_a_step_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let (first, landed) = tokio::sync::oneshot::channel();
    let (early, before) = tokio::sync::oneshot::channel();
    let (gone, deleted) = tokio::sync::oneshot::channel();
    let (quiet, nothing) = tokio::sync::oneshot::channel();
    run_lane(
        dir.path(),
        vec![
            PersistMsg::Append {
                run_id: "run-1".to_string(),
                record: Box::new(batch_record("c0")),
                ack: Some(early),
            },
            PersistMsg::Snapshot(stepped("run-1")),
            PersistMsg::Append {
                run_id: "run-1".to_string(),
                record: Box::new(batch_record("c1")),
                ack: Some(first),
            },
            PersistMsg::Append {
                run_id: "run-1".to_string(),
                record: Box::new(RunRecord::StatusChanged {
                    status: leviath_core::run_meta::RunStatus::Running,
                    at: 1,
                }),
                ack: Some(quiet),
            },
            PersistMsg::Append {
                run_id: "never-written".to_string(),
                record: Box::new(batch_record("c2")),
                ack: Some(gone),
            },
        ],
    )
    .await;
    assert_eq!(before.await.unwrap(), Appended::NoJournal, "no file yet");
    assert_eq!(landed.await.unwrap(), Appended::Landed { position: 1 });
    assert_eq!(
        nothing.await.unwrap(),
        Appended::NoJournal,
        "a record that is no event of the run's writes no step"
    );
    assert_eq!(deleted.await.unwrap(), Appended::NoJournal);
    let read = crate::runfile::RunFileReader::open(
        &dir.path().join("run-1").join(leviath_core::files::RUN_FILE),
    )
    .unwrap();
    let steps = read.deltas(1, read.last_seq()).unwrap();
    assert!(
        matches!(&steps[0].events[0], crate::state::RunEvent::ToolStarted(call) if call.id == "c1"),
        "{steps:?}"
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

/// An acked record whose step cannot be written says so, and fails the run.
#[tokio::test]
async fn an_acked_record_that_cannot_be_written_says_so() {
    crate::test_support::with_tracing(|| {});
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("run-1")).unwrap();
    let mut lane = crate::runfile::lane::RunFileLane::new("m", "w");
    let stats = health();
    record_run_file(&mut lane, dir.path(), *step("run-1").unwrap(), &stats).await;
    lane.note("run-1", &batch_record("c1"));
    lane.break_writes("run-1");
    assert_eq!(
        flush_run_file(&mut lane, "run-1", &stats).await,
        Appended::Failed
    );
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
    let stats = run_lane(
        dir.path(),
        vec![PersistMsg::Snapshot(stepped("run-blocked"))],
    )
    .await;
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
    let (ack, acked) = tokio::sync::oneshot::channel();
    tx.send(PersistMsg::Snapshot(stepped("run-1"))).unwrap();
    tx.send(PersistMsg::Append {
        run_id: "run-1".to_string(),
        record: Box::new(batch_record("c0")),
        ack: Some(ack),
    })
    .unwrap();
    acked.await.unwrap();
    std::fs::remove_dir_all(dir.path().join("run-1")).unwrap();
    tx.send(PersistMsg::Snapshot(stepped("run-1"))).unwrap();
    tx.send(PersistMsg::StageLines {
        run_id: "run-1".to_string(),
        output_appends: vec![(0, "x".to_string())],
        log_appends: vec![],
    })
    .unwrap();
    drop(tx);
    lane.await.unwrap();
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
    let lost = outcome.files.expect("both were lost");
    assert!(
        lost.path.extension().is_some_and(|e| e == "tmp"),
        "{lost:?}"
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
