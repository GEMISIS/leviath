//! Tests for the shared wait loop and for `lev run --wait`'s composition,
//! driven against a scripted fake daemon on a real control socket.
//!
//! The scaffolding here (`ScriptedDaemon`, the event builders, the record
//! writers) is `pub(crate)` so the MCP server's tests drive the same fake.

use super::*;

use std::sync::{Arc, Mutex};

use leviath_core::interaction::{InteractionKind, InteractionRequest};
use leviath_core::run_meta::RunMeta;
use leviath_runtime::control_socket::{
    ControlRequest, ControlResponse, bind_control_listener, control_id,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

/// The run id the scripted daemon assigns to every spawn.
pub(crate) const RUN_ID: &str = "coder-test-run";

/// Aborts a background task when dropped.
pub(crate) struct AbortOnDrop(pub(crate) JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What one `Subscribe` connection does with its script.
#[derive(Clone)]
pub(crate) enum StreamScript {
    /// Send the events, then hold the connection open.
    Hold(Vec<WorldEvent>),
    /// Send the events, then close the connection (a daemon restart).
    Drop(Vec<WorldEvent>),
    /// Wait, then send the events and hold the connection open.
    Delayed(Duration, Vec<WorldEvent>),
}

/// A fake shared-world daemon: the n-th `Subscribe` plays `scripts[n]` (the
/// last script repeats), every other request is answered by the responder
/// and recorded.
pub(crate) struct ScriptedDaemon {
    client: ControlClient,
    dir: tempfile::TempDir,
    requests: Arc<Mutex<Vec<ControlRequest>>>,
    _accept: AbortOnDrop,
}

impl ScriptedDaemon {
    pub(crate) fn new(
        scripts: Vec<StreamScript>,
        responder: impl Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let mut listener = bind_control_listener(&id).unwrap();
        let scripts = Arc::new(scripts);
        let responder = Arc::new(responder);
        let requests: Arc<Mutex<Vec<ControlRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept = tokio::spawn(async move {
            loop {
                let Ok(Some(stream)) = listener.accept().await else {
                    break;
                };
                let scripts = scripts.clone();
                let responder = responder.clone();
                let recorded = recorded.clone();
                let subscribes = subscribes.clone();
                tokio::spawn(async move {
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut lines = BufReader::new(read_half).lines();
                    let Ok(Some(line)) = lines.next_line().await else {
                        return;
                    };
                    let Ok(req) = serde_json::from_str::<ControlRequest>(&line) else {
                        return;
                    };
                    match req {
                        ControlRequest::Subscribe => {
                            let nth = subscribes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let script = scripts
                                .get(nth)
                                .or(scripts.last())
                                .cloned()
                                .unwrap_or(StreamScript::Hold(Vec::new()));
                            let (events, hold) = match script {
                                StreamScript::Hold(events) => (events, true),
                                StreamScript::Drop(events) => (events, false),
                                StreamScript::Delayed(pause, events) => {
                                    tokio::time::sleep(pause).await;
                                    (events, true)
                                }
                            };
                            for ev in &events {
                                let mut out = serde_json::to_string(ev).unwrap();
                                out.push('\n');
                                if write_half.write_all(out.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                            if hold {
                                std::future::pending::<()>().await;
                            }
                        }
                        other => {
                            recorded.lock().unwrap().push(other.clone());
                            let mut out = serde_json::to_string(&responder(other)).unwrap();
                            out.push('\n');
                            let _ = write_half.write_all(out.as_bytes()).await;
                        }
                    }
                });
            }
        });
        Self {
            client: ControlClient::new(control_id(dir.path())),
            dir,
            requests,
            _accept: AbortOnDrop(accept),
        }
    }

    pub(crate) fn client(&self) -> ControlClient {
        self.client.clone()
    }

    /// Every non-subscribe request the daemon answered so far.
    pub(crate) fn requests(&self) -> Vec<ControlRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Stop accepting connections, so the next dial fails: the daemon is gone.
    pub(crate) fn shutdown(&self) {
        self._accept.0.abort();
        let _ = std::fs::remove_file(control_id(self.dir.path()));
    }
}

/// A client pointed at an address with no daemon behind it.
pub(crate) fn no_daemon_client() -> ControlClient {
    ControlClient::new(control_id(std::path::Path::new("/no/such/daemon")))
}

/// A responder that spawns `RUN_ID` and says yes to everything else.
pub(crate) fn spawn_ok(req: ControlRequest) -> ControlResponse {
    match req {
        ControlRequest::Spawn { .. } => ControlResponse::Spawned {
            run_id: RUN_ID.to_string(),
        },
        _ => ControlResponse::Ok { ok: true },
    }
}

// ─── Event builders ───────────────────────────────────────────────────────────

pub(crate) fn completed_for(run_id: &str, status: &str) -> WorldEvent {
    WorldEvent::Completed {
        run_id: run_id.to_string(),
        agent_id: run_id.to_string(),
        status: status.to_string(),
        final_output: None,
    }
}

pub(crate) fn completed(status: &str) -> WorldEvent {
    completed_for(RUN_ID, status)
}

pub(crate) fn answer(content: &str) -> FinalOutput {
    FinalOutput::new(
        content,
        Some("markdown".to_string()),
        "summary".to_string(),
        7,
    )
}

pub(crate) fn completed_with_answer(content: &str) -> WorldEvent {
    WorldEvent::Completed {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        status: "complete".to_string(),
        final_output: Some(answer(content)),
    }
}

pub(crate) fn status_event_for(run_id: &str) -> WorldEvent {
    WorldEvent::Status {
        run_id: run_id.to_string(),
        agent_id: run_id.to_string(),
        status: "active".to_string(),
        stage: "implement".to_string(),
        iteration: 1,
        tool_calls: 0,
        accepts_messages: false,
        wait_reason: None,
        title: None,
    }
}

pub(crate) fn status_event() -> WorldEvent {
    status_event_for(RUN_ID)
}

pub(crate) fn stage_transition() -> WorldEvent {
    WorldEvent::StageTransition {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        from: "plan".to_string(),
        to: "implement".to_string(),
        iteration: 1,
    }
}

pub(crate) fn tool_finished_for(run_id: &str, tool: &str, ok: bool, summary: &str) -> WorldEvent {
    WorldEvent::ToolCallFinished {
        run_id: run_id.to_string(),
        agent_id: run_id.to_string(),
        call_id: "c1".to_string(),
        tool: tool.to_string(),
        ok,
        summary: summary.to_string(),
    }
}

pub(crate) fn interaction_for(run_id: &str, id: &str) -> WorldEvent {
    WorldEvent::Interaction {
        run_id: run_id.to_string(),
        agent_id: run_id.to_string(),
        request: InteractionRequest {
            id: id.to_string(),
            kind: InteractionKind::ToolApproval,
            prompt: "Run bash `ls`?".to_string(),
            options: vec![],
            tool_name: Some("bash".to_string()),
            tool_arguments: None,
            required: true,
            stage_name: "implement".to_string(),
            body: None,
            body_format: Default::default(),
        },
    }
}

// ─── Record writers ───────────────────────────────────────────────────────────

/// A run record for `run_id` in `status`, with `parent` when it is a child.
pub(crate) fn meta_for(run_id: &str, status: RunStatus, parent: Option<&str>) -> RunMeta {
    let mut meta = RunMeta::new(
        run_id.to_string(),
        "coder".to_string(),
        "/agents/coder".to_string(),
        "do it".to_string(),
        None,
        "/work".to_string(),
        100,
    );
    meta.status = status;
    meta.parent_run_id = parent.map(str::to_string);
    meta
}

/// Write `meta` under `runs_dir`.
pub(crate) fn write_meta(runs_dir: &Path, meta: &RunMeta) {
    crate::runstate::create_run_in(&runs_dir.join(&meta.run_id), meta).unwrap();
}

/// Write a run record in `status` and, when given, its answer.
pub(crate) fn write_run(runs_dir: &Path, run_id: &str, status: RunStatus, output: Option<&str>) {
    let mut meta = meta_for(run_id, status, None);
    if let Some(content) = output {
        let out = answer(content);
        meta.final_output = Some(out.descriptor());
        crate::runstate::create_run_in(&runs_dir.join(run_id), &meta).unwrap();
        crate::runstate::write_final_output(&runs_dir.join(run_id), content).unwrap();
        return;
    }
    write_meta(runs_dir, &meta);
}

/// Timing that keeps the tests quick.
pub(crate) fn fast() -> WaitTiming {
    WaitTiming {
        tick: Duration::from_millis(20),
        resubscribe_pause: Duration::from_millis(10),
        output_retry: Duration::from_millis(20),
        output_retries: 10,
    }
}

async fn wait_fast(
    daemon: &ScriptedDaemon,
    runs_dir: &Path,
    timeout: Option<Duration>,
    seen: &mut Vec<WorldEvent>,
) -> WaitOutcome {
    let client = daemon.client();
    let stream = client.subscribe().await.unwrap();
    wait_for_run_with(
        &client,
        stream,
        RUN_ID,
        runs_dir,
        timeout,
        &mut |ev| seen.push(ev.clone()),
        &fast(),
    )
    .await
}

// ─── wait_for_run ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_completed_event_finishes_the_wait_with_its_answer() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![
            status_event(),
            completed_with_answer("All done."),
        ])],
        spawn_ok,
    );
    let mut seen = Vec::new();
    let outcome = wait_fast(&daemon, runs.path(), None, &mut seen).await;
    match outcome {
        WaitOutcome::Finished {
            status,
            final_output,
            error,
            tools_installed,
        } => {
            assert_eq!(status, RunStatus::Complete);
            assert_eq!(final_output.unwrap().content, "All done.");
            assert!(error.is_none());
            assert!(tools_installed.is_empty());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(seen.len(), 2);
}

#[tokio::test]
async fn descendants_count_and_strangers_do_not() {
    let runs = tempfile::tempdir().unwrap();
    write_meta(runs.path(), &meta_for(RUN_ID, RunStatus::Running, None));
    write_meta(
        runs.path(),
        &meta_for("child", RunStatus::Running, Some(RUN_ID)),
    );
    write_meta(
        runs.path(),
        &meta_for("grandchild", RunStatus::Running, Some("child")),
    );
    write_meta(runs.path(), &meta_for("stranger", RunStatus::Running, None));
    // A pair of records that claim each other as parent: not ours, and not
    // a loop for the resolver either.
    write_meta(
        runs.path(),
        &meta_for("loop-a", RunStatus::Running, Some("loop-b")),
    );
    write_meta(
        runs.path(),
        &meta_for("loop-b", RunStatus::Running, Some("loop-a")),
    );
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![
            status_event_for("stranger"),
            status_event_for("stranger"),    // cached negative
            status_event_for("unknown-run"), // no record: not cached, ignored
            status_event_for("loop-a"),
            status_event_for("loop-a"),
            tool_finished_for(
                "child",
                "install_tool",
                true,
                "Installed tool 'cargo_lint' at /t.",
            ),
            tool_finished_for(
                "child",
                "install_tool",
                true,
                "Installed tool 'cargo_lint' at /t.",
            ),
            tool_finished_for(
                "grandchild",
                "install_tool",
                false,
                "Installed tool 'nope' at /t.",
            ),
            tool_finished_for("grandchild", "shell", true, "Installed tool 'nope' at /t."),
            tool_finished_for(RUN_ID, "install_tool", true, "[error] something"),
            completed_for("child", "complete"), // a worker finishing is not the end
            completed_with_answer("Merged."),
        ])],
        spawn_ok,
    );
    let mut seen = Vec::new();
    let outcome = wait_fast(&daemon, runs.path(), None, &mut seen).await;
    match outcome {
        WaitOutcome::Finished {
            tools_installed, ..
        } => assert_eq!(tools_installed, vec!["cargo_lint", "cargo_lint"]),
        other => panic!("{other:?}"),
    }
    // Only the family's events reached the callback.
    assert_eq!(seen.len(), 7, "{seen:?}");
    assert!(
        seen.iter()
            .all(|e| ["child", "grandchild", RUN_ID].contains(&e.run_id()))
    );
}

#[tokio::test]
async fn a_descendants_question_is_returned_under_its_own_run_id() {
    let runs = tempfile::tempdir().unwrap();
    write_meta(
        runs.path(),
        &meta_for("worker", RunStatus::WaitingInput, Some(RUN_ID)),
    );
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![interaction_for(
            "worker", "appr-9",
        )])],
        spawn_ok,
    );
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Interaction { run_id, request } => {
            assert_eq!(run_id, "worker");
            assert_eq!(request.id, "appr-9");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_unknown_completion_label_falls_back_to_the_record_then_to_error() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed("hibernating")])],
        spawn_ok,
    );
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished {
            status,
            final_output,
            ..
        } => {
            assert_eq!(status, RunStatus::Error);
            assert!(final_output.is_none());
        }
        other => panic!("{other:?}"),
    }

    let runs = tempfile::tempdir().unwrap();
    let mut meta = meta_for(RUN_ID, RunStatus::Cancelled, None);
    meta.error = Some("stopped by hand".to_string());
    write_meta(runs.path(), &meta);
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed("hibernating")])],
        spawn_ok,
    );
    // The record is already terminal, so the wait answers from it before the
    // stream is even read.
    seen.clear();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished { status, error, .. } => {
            assert_eq!(status, RunStatus::Cancelled);
            assert_eq!(error.as_deref(), Some("stopped by hand"));
        }
        other => panic!("{other:?}"),
    }
    assert!(seen.is_empty());
}

#[tokio::test]
async fn an_unknown_label_on_a_live_record_reads_the_record_after_the_event() {
    let runs = tempfile::tempdir().unwrap();
    write_meta(runs.path(), &meta_for(RUN_ID, RunStatus::Running, None));
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed("hibernating")])],
        spawn_ok,
    );
    let mut seen = Vec::new();
    // The record says `running`, which is what the fallback reads; the outcome
    // is still reported as finished because the daemon said so.
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished { status, .. } => assert_eq!(status, RunStatus::Running),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_run_finished_on_disk_is_answered_without_waiting_for_an_event() {
    let runs = tempfile::tempdir().unwrap();
    write_run(
        runs.path(),
        RUN_ID,
        RunStatus::CompleteInteractive,
        Some("Interactive answer"),
    );
    let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished {
            status,
            final_output,
            ..
        } => {
            assert_eq!(status, RunStatus::CompleteInteractive);
            assert_eq!(final_output.unwrap().content, "Interactive answer");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_tick_notices_a_record_that_went_terminal_without_an_event() {
    let runs = tempfile::tempdir().unwrap();
    write_meta(runs.path(), &meta_for(RUN_ID, RunStatus::Running, None));
    let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![status_event()])], spawn_ok);
    let runs_path = runs.path().to_path_buf();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        write_run(&runs_path, RUN_ID, RunStatus::CompleteInteractive, None);
    });
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished {
            status,
            final_output,
            ..
        } => {
            assert_eq!(status, RunStatus::CompleteInteractive);
            assert!(final_output.is_none());
        }
        other => panic!("{other:?}"),
    }
    writer.await.unwrap();
}

#[tokio::test]
async fn a_deadline_ends_the_wait_with_the_status_on_disk_and_cancels_nothing() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![status_event()])], spawn_ok);
    let mut seen = Vec::new();
    let outcome = wait_fast(
        &daemon,
        runs.path(),
        Some(Duration::from_millis(50)),
        &mut seen,
    )
    .await;
    assert_eq!(outcome, WaitOutcome::TimedOut { status: None });

    write_meta(runs.path(), &meta_for(RUN_ID, RunStatus::Running, None));
    let outcome = wait_fast(
        &daemon,
        runs.path(),
        Some(Duration::from_millis(50)),
        &mut seen,
    )
    .await;
    assert_eq!(
        outcome,
        WaitOutcome::TimedOut {
            status: Some(RunStatus::Running)
        }
    );
    assert!(
        daemon.requests().is_empty(),
        "the wait sent the daemon a request"
    );
}

#[tokio::test]
async fn a_dropped_stream_is_resubscribed_and_the_run_followed_to_its_end() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(
        vec![
            StreamScript::Drop(vec![status_event()]),
            StreamScript::Hold(vec![completed_with_answer("After the restart.")]),
        ],
        spawn_ok,
    );
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished { final_output, .. } => {
            assert_eq!(final_output.unwrap().content, "After the restart.");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(seen.len(), 2);
}

#[tokio::test]
async fn a_run_that_finished_while_the_stream_was_down_is_read_off_disk() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Drop(vec![]), StreamScript::Hold(vec![])],
        spawn_ok,
    );
    let client = daemon.client();
    let stream = client.subscribe().await.unwrap();
    // The record turns terminal during the resubscribe pause, after the
    // wait's first look at it.
    let runs_path = runs.path().to_path_buf();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        write_run(
            &runs_path,
            RUN_ID,
            RunStatus::Complete,
            Some("Quietly done."),
        );
    });
    let outcome = wait_for_run_with(
        &client,
        stream,
        RUN_ID,
        runs.path(),
        None,
        &mut |_| {},
        &WaitTiming {
            tick: Duration::from_secs(30),
            resubscribe_pause: Duration::from_millis(200),
            ..fast()
        },
    )
    .await;
    writer.await.unwrap();
    match outcome {
        WaitOutcome::Finished { final_output, .. } => {
            assert_eq!(final_output.unwrap().content, "Quietly done.");
        }
        other => panic!("{other:?}"),
    }
}

/// `CompleteInteractive` emits no `Completed`, so a record that turned
/// terminal is noticed on the next event for the run, not only on the tick.
#[tokio::test]
async fn an_event_prompts_a_look_at_a_record_that_went_terminal_meanwhile() {
    let runs = tempfile::tempdir().unwrap();
    write_meta(runs.path(), &meta_for(RUN_ID, RunStatus::Running, None));
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Delayed(
            Duration::from_millis(150),
            vec![status_event()],
        )],
        spawn_ok,
    );
    let runs_path = runs.path().to_path_buf();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        write_run(
            &runs_path,
            RUN_ID,
            RunStatus::CompleteInteractive,
            Some("Chatty."),
        );
    });
    let client = daemon.client();
    let stream = client.subscribe().await.unwrap();
    let mut seen = Vec::new();
    let outcome = wait_for_run_with(
        &client,
        stream,
        RUN_ID,
        runs.path(),
        None,
        &mut |ev| seen.push(ev.clone()),
        &WaitTiming {
            tick: Duration::from_secs(30),
            ..fast()
        },
    )
    .await;
    match outcome {
        WaitOutcome::Finished {
            status,
            final_output,
            ..
        } => {
            assert_eq!(status, RunStatus::CompleteInteractive);
            assert_eq!(final_output.unwrap().content, "Chatty.");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(seen.len(), 1, "the event was what prompted the look");
    writer.await.unwrap();
}

#[tokio::test]
async fn a_daemon_that_keeps_dropping_the_stream_loses_the_run() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(vec![StreamScript::Drop(vec![])], spawn_ok);
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Lost { reason } => assert!(reason.contains("kept dropping"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_daemon_that_goes_away_for_good_loses_the_run() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(vec![StreamScript::Drop(vec![])], spawn_ok);
    let client = daemon.client();
    let stream = client.subscribe().await.unwrap();
    daemon.shutdown();
    let outcome = wait_for_run_with(
        &client,
        stream,
        RUN_ID,
        runs.path(),
        None,
        &mut |_| {},
        &fast(),
    )
    .await;
    match outcome {
        WaitOutcome::Lost { reason } => assert!(reason.contains("did not come back"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_answer_persisted_after_the_event_is_picked_up_by_the_retries() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed("complete")])],
        spawn_ok,
    );
    let runs_path = runs.path().to_path_buf();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_run(
            &runs_path,
            RUN_ID,
            RunStatus::Complete,
            Some("Late answer."),
        );
    });
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished { final_output, .. } => {
            assert_eq!(final_output.unwrap().content, "Late answer.");
        }
        other => panic!("{other:?}"),
    }
    writer.await.unwrap();
}

#[tokio::test]
async fn a_terminal_record_without_an_answer_ends_the_retries_early() {
    let runs = tempfile::tempdir().unwrap();
    write_run(runs.path(), RUN_ID, RunStatus::Running, None);
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed("complete")])],
        spawn_ok,
    );
    let runs_path = runs.path().to_path_buf();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_run(&runs_path, RUN_ID, RunStatus::Complete, None);
    });
    let started = std::time::Instant::now();
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished { final_output, .. } => assert!(final_output.is_none()),
        other => panic!("{other:?}"),
    }
    // Ten retries at 20 ms would be 200 ms; the terminal record cut it short.
    assert!(
        started.elapsed() < Duration::from_millis(180),
        "{:?}",
        started.elapsed()
    );
    writer.await.unwrap();
}

#[tokio::test]
async fn no_record_at_all_exhausts_the_retries_and_reports_no_answer() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed("complete")])],
        spawn_ok,
    );
    let mut seen = Vec::new();
    match wait_fast(&daemon, runs.path(), None, &mut seen).await {
        WaitOutcome::Finished {
            final_output,
            error,
            ..
        } => {
            assert!(final_output.is_none());
            assert!(error.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_default_clocks_are_used_by_the_plain_entry_point() {
    let runs = tempfile::tempdir().unwrap();
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed_with_answer("Plain.")])],
        spawn_ok,
    );
    let client = daemon.client();
    let stream = client.subscribe().await.unwrap();
    let outcome = wait_for_run(&client, stream, RUN_ID, runs.path(), None, &mut |_| {}).await;
    assert!(
        matches!(outcome, WaitOutcome::Finished { .. }),
        "{outcome:?}"
    );
    let timing = WaitTiming::default();
    assert_eq!(timing.tick, Duration::from_secs(1));
    assert_eq!(timing.resubscribe_pause, Duration::from_millis(200));
}

#[test]
fn the_installed_tool_name_is_read_from_the_summary_prefix() {
    assert_eq!(
        installed_tool_name("Installed tool 'cargo_lint' at /x/cargo_lint.rhai.").as_deref(),
        Some("cargo_lint")
    );
    assert_eq!(installed_tool_name("[error] install_tool: nope"), None);
    assert_eq!(installed_tool_name("Installed tool 'unterminated"), None);
}

// ─── spawn_and_wait / refuse_wait_with_count ──────────────────────────────────

use crate::daemon::client::{refuse_wait_with_count, spawn_and_wait, spawn_and_wait_with};
use leviath_runtime::host::SpawnArgs;

fn spawn_args() -> SpawnArgs {
    SpawnArgs {
        run_id: RUN_ID.to_string(),
        blueprint_path: "/no/such/agent.leviath".to_string(),
        task: "do it".to_string(),
        workdir: "/work".to_string(),
        ..SpawnArgs::default()
    }
}

#[test]
fn wait_refuses_a_batch() {
    assert!(refuse_wait_with_count(1, true).is_ok());
    assert!(refuse_wait_with_count(3, false).is_ok());
    let err = refuse_wait_with_count(3, true).unwrap_err().to_string();
    assert!(err.contains("--wait follows a single run"), "{err}");
}

#[tokio::test]
async fn spawn_and_wait_prints_the_answer_and_succeeds_on_a_complete_run() {
    crate::config::with_isolated_config_path_async("spawn_and_wait_ok", |_| async {
        let runs = tempfile::tempdir().unwrap();
        let daemon = ScriptedDaemon::new(
            vec![StreamScript::Hold(vec![completed_with_answer("Shipped.")])],
            spawn_ok,
        );
        spawn_and_wait(&daemon.client(), spawn_args(), false, runs.path())
            .await
            .unwrap();
        spawn_and_wait(&daemon.client(), spawn_args(), true, runs.path())
            .await
            .unwrap();
        assert!(
            daemon
                .requests()
                .iter()
                .all(|r| matches!(r, ControlRequest::Spawn { .. }))
        );
    })
    .await;
}

#[tokio::test]
async fn spawn_and_wait_reports_a_run_without_an_answer_and_fails_a_failed_one() {
    crate::config::with_isolated_config_path_async("spawn_and_wait_err", |_| async {
        let runs = tempfile::tempdir().unwrap();
        // No answer, but complete: prints the one-line note and succeeds.
        let daemon = ScriptedDaemon::new(
            vec![StreamScript::Hold(vec![completed("complete")])],
            spawn_ok,
        );
        spawn_and_wait(&daemon.client(), spawn_args(), false, runs.path())
            .await
            .unwrap();

        // Ended in error: the record's message is in the failure.
        let mut meta = meta_for(RUN_ID, RunStatus::Error, None);
        meta.error = Some("the model refused".to_string());
        write_meta(runs.path(), &meta);
        let daemon =
            ScriptedDaemon::new(vec![StreamScript::Hold(vec![completed("error")])], spawn_ok);
        let err = spawn_and_wait(&daemon.client(), spawn_args(), true, runs.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("status Error: the model refused"), "{err}");
    })
    .await;
}

#[tokio::test]
async fn spawn_and_wait_fails_when_the_run_parks_on_a_question_or_is_lost() {
    crate::config::with_isolated_config_path_async("spawn_and_wait_park", |_| async {
        let runs = tempfile::tempdir().unwrap();
        let daemon = ScriptedDaemon::new(
            vec![StreamScript::Hold(vec![interaction_for(RUN_ID, "q-1")])],
            spawn_ok,
        );
        let err = spawn_and_wait(&daemon.client(), spawn_args(), false, runs.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("lev respond q-1"), "{err}");

        let daemon = ScriptedDaemon::new(vec![StreamScript::Drop(vec![])], spawn_ok);
        let err = spawn_and_wait(&daemon.client(), spawn_args(), false, runs.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("lost track of run"), "{err}");
    })
    .await;
}

#[tokio::test]
async fn spawn_and_wait_fails_before_spawning_when_the_daemon_is_unreachable_or_refuses() {
    crate::config::with_isolated_config_path_async("spawn_and_wait_refused", |_| async {
        let runs = tempfile::tempdir().unwrap();
        let err = spawn_and_wait(&no_daemon_client(), spawn_args(), false, runs.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not reachable"), "{err}");

        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], |_| {
            ControlResponse::Error {
                message: "no such blueprint".to_string(),
            }
        });
        let err = spawn_and_wait(&daemon.client(), spawn_args(), false, runs.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("spawn failed: no such blueprint"), "{err}");
    })
    .await;
}

#[tokio::test]
async fn spawn_and_wait_with_a_deadline_stops_waiting_and_says_the_run_continues() {
    crate::config::with_isolated_config_path_async("spawn_and_wait_deadline", |_| async {
        let runs = tempfile::tempdir().unwrap();
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![status_event()])], spawn_ok);
        let err = spawn_and_wait_with(
            &daemon.client(),
            spawn_args(),
            false,
            runs.path(),
            Some(Duration::from_millis(30)),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("it continues"), "{err}");
    })
    .await;
}
