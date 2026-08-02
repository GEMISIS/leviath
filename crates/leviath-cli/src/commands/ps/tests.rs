use super::*;
use leviath_runtime::components::WaitReason;
use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

/// A daemon with room to spare and nothing to report about itself.
fn healthy_daemon() -> DaemonHealth {
    DaemonHealth {
        tools_workers: 8,
        redrive_secs: 30,
        ..Default::default()
    }
}

fn entry(run_id: &str, status: AgentStatus) -> RunListEntry {
    RunListEntry {
        run_id: run_id.to_string(),
        status,
        wait_reason: None,
        stage: "implement".to_string(),
        stage_index: None,
        num_stages: None,
        iteration: 0,
        tool_calls: 0,
        last_progress_at: None,
        unattended: false,
        empty_output: false,
    }
}

#[test]
fn status_cell_spells_out_why_a_run_is_waiting() {
    let mut e = entry("run-a", AgentStatus::Waiting);
    e.wait_reason = Some(WaitReason::ToolApproval);
    assert_eq!(status_cell(&e), "waiting: tool approval");
    e.wait_reason = Some(WaitReason::UserPrompt);
    assert_eq!(status_cell(&e), "waiting: user prompt");
    e.wait_reason = Some(WaitReason::TaintGate);
    assert_eq!(status_cell(&e), "waiting: taint gate");
    e.wait_reason = Some(WaitReason::InteractionPoint);
    assert_eq!(status_cell(&e), "waiting: checkpoint");
    e.wait_reason = Some(WaitReason::FanOutWorkers { outstanding: 3 });
    assert_eq!(status_cell(&e), "waiting: workers(3)");
    e.wait_reason = Some(WaitReason::Children { outstanding: 2 });
    assert_eq!(status_cell(&e), "waiting: children(2)");
}

/// A `Waiting` the host could not attribute still has to render, and it must
/// read as the bare status rather than as a half-written reason.
#[test]
fn status_cell_falls_back_to_the_bare_status() {
    assert_eq!(status_cell(&entry("r", AgentStatus::Waiting)), "waiting");
    assert_eq!(status_cell(&entry("r", AgentStatus::Active)), "active");
    assert_eq!(status_cell(&entry("r", AgentStatus::Idle)), "idle");
    assert_eq!(status_cell(&entry("r", AgentStatus::Paused)), "paused");
    assert_eq!(status_cell(&entry("r", AgentStatus::Complete)), "complete");
    assert_eq!(
        status_cell(&entry("r", AgentStatus::Cancelled)),
        "cancelled"
    );
    assert_eq!(
        status_cell(&entry(
            "r",
            AgentStatus::Error {
                message: "boom".to_string()
            }
        )),
        "error: boom"
    );
}

/// A run that finished with nothing to show for it reads that way, instead of
/// being indistinguishable from one that did the work (issue #192).
#[test]
fn status_cell_marks_a_finished_run_that_produced_nothing() {
    let mut e = entry("r", AgentStatus::Complete);
    e.empty_output = true;
    assert_eq!(status_cell(&e), "complete (no output)");
    e.status = AgentStatus::Cancelled;
    assert_eq!(status_cell(&e), "cancelled (no output)");
    // The waiting reason still wins: a blocked run needs answering, and it
    // has not finished producing anything yet.
    e.status = AgentStatus::Waiting;
    e.wait_reason = Some(WaitReason::ToolApproval);
    assert_eq!(status_cell(&e), "waiting: tool approval");
}

/// A non-waiting status never carries a reason, even if one somehow rode along.
#[test]
fn status_cell_ignores_a_reason_on_a_non_waiting_run() {
    let mut e = entry("run-a", AgentStatus::Active);
    e.wait_reason = Some(WaitReason::ToolApproval);
    assert_eq!(status_cell(&e), "active");
}

#[test]
fn humanize_age_picks_the_largest_small_unit() {
    assert_eq!(humanize_age(0), "0s");
    assert_eq!(humanize_age(59), "59s");
    assert_eq!(humanize_age(60), "1m");
    assert_eq!(humanize_age(3599), "59m");
    assert_eq!(humanize_age(3600), "1h");
    assert_eq!(humanize_age(86_399), "23h");
    assert_eq!(humanize_age(86_400), "1d");
    // A clock that moved backwards must not print a negative age.
    assert_eq!(humanize_age(-5), "0s");
}

#[test]
fn age_cell_reads_from_last_progress_not_the_heartbeat() {
    let mut e = entry("run-a", AgentStatus::Active);
    assert_eq!(age_cell(&e, 1_000), "-", "no snapshot yet, nothing to age");
    e.last_progress_at = Some(940);
    assert_eq!(age_cell(&e, 1_000), "1m");
}

#[test]
fn stage_cell_shows_position_only_for_multi_stage_blueprints() {
    let mut e = entry("run-a", AgentStatus::Active);
    assert_eq!(stage_cell(&e), "implement");
    e.stage_index = Some(1);
    e.num_stages = Some(4);
    assert_eq!(stage_cell(&e), "implement 2/4");
    // A single-stage blueprint has no position worth printing.
    e.stage_index = Some(0);
    e.num_stages = Some(1);
    assert_eq!(stage_cell(&e), "implement");
    // A half-known position is not a position.
    e.num_stages = None;
    assert_eq!(stage_cell(&e), "implement");
}

#[test]
fn format_runs_handles_empty() {
    assert_eq!(
        format_runs(&[], &[], &healthy_daemon(), 0),
        "no agents running"
    );
}

/// Issue #205: a scheduler spawns a run, the run dies on its first inference,
/// and the daemon unloads it. Nothing is running, but "no agents running" is the
/// answer that cost forty minutes of spawn-and-revert, because it reads exactly
/// like a run that was never spawned. The row has to be there, with the reason.
#[test]
fn format_runs_shows_a_finished_run_when_nothing_is_running() {
    let mut died = entry(
        "worker-1785616492",
        AgentStatus::Error {
            message: "HTTP 402 Payment Required".to_string(),
        },
    );
    died.last_progress_at = Some(1_140);

    let out = format_runs(&[], &[died], &healthy_daemon(), 1_200);
    assert_ne!(out, "no agents running");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("RUN"), "header row: {out}");
    assert!(lines[1].contains("worker-1785616492"), "{out}");
    assert!(lines[1].contains("HTTP 402"), "the reason it ended: {out}");
    assert!(lines[1].contains("1m"), "and how long ago: {out}");
}

/// Finished runs are listed under the live ones, not mixed into them, and a
/// finished run is never counted as needing an answer.
#[test]
fn format_runs_lists_finished_runs_after_the_live_ones() {
    let out = format_runs(
        &[entry("run-live", AgentStatus::Active)],
        &[entry("run-ended", AgentStatus::Complete)],
        &healthy_daemon(),
        0,
    );
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[1].contains("run-live"), "{out}");
    assert!(lines[2].contains("run-ended"), "{out}");
    assert!(!out.contains("needs an answer"), "{out}");
}

/// The whole point of the change: two runs both `Waiting`, telling the operator
/// which one needs them and which one is fine.
#[test]
fn format_runs_distinguishes_two_kinds_of_waiting() {
    let mut blocked = entry("child-1", AgentStatus::Waiting);
    blocked.wait_reason = Some(WaitReason::ToolApproval);
    blocked.iteration = 1;
    blocked.tool_calls = 1;
    blocked.last_progress_at = Some(600);

    let mut parked = entry("waiter-longer-id", AgentStatus::Waiting);
    parked.wait_reason = Some(WaitReason::Children { outstanding: 3 });
    parked.stage = "delegate".to_string();
    parked.iteration = 2;
    parked.tool_calls = 1;
    parked.last_progress_at = Some(1_190);

    let out = format_runs(&[blocked, parked], &[], &healthy_daemon(), 1_200);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("RUN"), "header row: {out}");
    assert!(lines[0].contains("AGE"), "header row: {out}");
    assert!(lines[1].contains("waiting: tool approval"), "{out}");
    assert!(lines[1].contains("10m"), "stuck for ten minutes: {out}");
    assert!(lines[2].contains("waiting: children(3)"), "{out}");
    assert!(lines[2].contains("10s"), "moved ten seconds ago: {out}");
    // Only the approval needs a person; the children resolve themselves.
    assert!(out.ends_with("1 run needs an answer: lev respond"), "{out}");
}

/// The call-out counts only the runs a person has to unblock, and stays away
/// entirely when there are none.
#[test]
fn format_runs_calls_out_only_the_runs_needing_an_answer() {
    let healthy = {
        let mut e = entry("run-a", AgentStatus::Waiting);
        e.wait_reason = Some(WaitReason::FanOutWorkers { outstanding: 4 });
        e
    };
    assert!(
        !format_runs(std::slice::from_ref(&healthy), &[], &healthy_daemon(), 0)
            .contains("needs an answer"),
        "a fan-out parent is not blocked on anyone"
    );
    assert!(
        !format_runs(
            &[entry("run-b", AgentStatus::Active)],
            &[],
            &healthy_daemon(),
            0
        )
        .contains("needs an answer")
    );

    let prompt = |id: &str, reason: WaitReason| {
        let mut e = entry(id, AgentStatus::Waiting);
        e.wait_reason = Some(reason);
        e
    };
    let one = format_runs(
        &[prompt("run-c", WaitReason::TaintGate), healthy.clone()],
        &[],
        &healthy_daemon(),
        0,
    );
    assert!(one.ends_with("1 run needs an answer: lev respond"), "{one}");

    let two = format_runs(
        &[
            prompt("run-c", WaitReason::TaintGate),
            prompt("run-d", WaitReason::InteractionPoint),
            healthy,
        ],
        &[],
        &healthy_daemon(),
        0,
    );
    assert!(two.ends_with("2 runs need an answer: lev respond"), "{two}");
}

/// Everything from a given column onwards, by character (never by byte - a run
/// id is arbitrary text).
fn from_column(line: &str, column: usize) -> String {
    line.chars().skip(column).collect()
}

/// Columns line up against the header and the widest cell, and no line carries
/// trailing whitespace.
#[test]
fn format_runs_aligns_columns() {
    let short = entry("a", AgentStatus::Active);
    let mut long = entry("a-much-longer-run-id", AgentStatus::Waiting);
    long.wait_reason = Some(WaitReason::UserPrompt);
    let out = format_runs(&[short, long], &[], &healthy_daemon(), 0);
    let lines: Vec<&str> = out.lines().collect();

    let status_col = lines[0]
        .chars()
        .collect::<String>()
        .find("STATUS")
        .expect("header has a STATUS column");
    assert!(
        from_column(lines[1], status_col).starts_with("active"),
        "the short row's status starts under the header: {out}"
    );
    assert!(
        from_column(lines[2], status_col).starts_with("waiting: user prompt"),
        "the long row's status starts under the same header: {out}"
    );
    for line in &lines {
        assert_eq!(line.trim_end(), *line, "no trailing blanks: {out:?}");
    }
}

/// A healthy daemon says nothing about itself. Every row can look busy while the
/// factory as a whole is fine, and a footer on every listing would train
/// operators to skip the one that matters.
#[test]
fn a_healthy_daemon_adds_no_footer() {
    let out = format_runs(
        &[entry("run-a", AgentStatus::Active)],
        &[],
        &healthy_daemon(),
        0,
    );
    assert!(!out.contains("lanes:"), "{out}");
    // Parked batches on their own are not a problem: they hold no capacity.
    let parked = DaemonHealth {
        tools_parked: 3,
        ..healthy_daemon()
    };
    let out = format_runs(&[entry("run-a", AgentStatus::Active)], &[], &parked, 0);
    assert!(!out.contains("lanes:"), "{out}");
}

/// A full lane with work behind it is worth mentioning, even while runs are
/// still moving - it is the first sign of the shape that wedges.
#[test]
fn a_saturated_lane_is_reported_under_the_table() {
    let health = DaemonHealth {
        tools_busy: 8,
        tools_workers: 8,
        tools_queued: 12,
        tools_parked: 3,
        ..healthy_daemon()
    };
    let out = format_runs(&[entry("run-a", AgentStatus::Active)], &[], &health, 0);
    assert!(
        out.ends_with("lanes: tools 8/8 busy, 3 parked, 12 queued"),
        "{out}"
    );
}

/// The dead-cycle streak, in cycles and in the wall-clock time an operator
/// actually cares about.
#[test]
fn a_dead_cycle_streak_is_reported_in_cycles_and_minutes() {
    let health = DaemonHealth {
        tools_busy: 8,
        tools_workers: 8,
        tools_queued: 12,
        dead_cycles: 4,
        ..healthy_daemon()
    };
    let out = format_runs(&[entry("run-a", AgentStatus::Active)], &[], &health, 0);
    assert!(
        out.ends_with("lanes: tools 8/8 busy, 12 queued  ·  no progress for 4 cycles (2m)"),
        "{out}"
    );

    // A streak counts even when the lane has since drained - the daemon still
    // has not moved, and that is the part worth saying.
    let drained = DaemonHealth {
        dead_cycles: 1,
        ..healthy_daemon()
    };
    let out = format_runs(&[entry("run-a", AgentStatus::Active)], &[], &drained, 0);
    assert!(
        out.ends_with("lanes: tools 0/8 busy  ·  no progress for 1 cycles (30s)"),
        "{out}"
    );
}

/// The footer sits below the "needs an answer" call-out rather than replacing
/// it: they answer different questions and an operator may need both.
#[test]
fn the_footer_and_the_answer_call_out_coexist() {
    let mut blocked = entry("run-a", AgentStatus::Waiting);
    blocked.wait_reason = Some(WaitReason::ToolApproval);
    let health = DaemonHealth {
        dead_cycles: 2,
        ..healthy_daemon()
    };
    let out = format_runs(&[blocked], &[], &health, 0);
    assert!(out.contains("1 run needs an answer: lev respond"), "{out}");
    assert!(out.contains("no progress for 2 cycles"), "{out}");
}

/// Bind a control listener at a fresh id under `dir` and serve one canned
/// response, returning the id clients connect to and the server task.
fn fake_daemon(dir: &std::path::Path, response_line: &'static str) -> (ControlId, JoinHandle<()>) {
    let id = control_id(dir);
    let mut listener = bind_control_listener(&id).unwrap();
    let handle = tokio::spawn(async move {
        let stream = listener
            .accept()
            .await
            .expect("accept succeeds")
            .expect("our own connection is admitted");
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        let _request = lines.next_line().await.unwrap();
        write_half
            .write_all(response_line.as_bytes())
            .await
            .unwrap();
        write_half.write_all(b"\n").await.unwrap();
    });
    (id, handle)
}

async fn list(response_line: &'static str, args: &PsArgs) -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let (id, server) = fake_daemon(dir.path(), response_line);
    let result = send_list(&ControlClient::new(id), args).await;
    server.await.unwrap();
    result
}

/// One run still going and one the daemon has finished with, so both halves of
/// the reply are exercised by the table and the `--json` paths alike.
const LISTING: &str = r#"{"result":"list","runs":[{"run_id":"run-a","status":"Waiting","reason":"tool_approval","stage":"implement","iteration":3,"tool_calls":7,"unattended":true}],"finished":[{"run_id":"run-b","status":{"Error":{"message":"HTTP 402 Payment Required"}},"stage":"implement","iteration":0,"tool_calls":0}]}"#;

#[tokio::test]
async fn send_list_prints_runs() {
    assert!(list(LISTING, &PsArgs::default()).await.is_ok());
}

#[tokio::test]
async fn send_list_prints_json() {
    assert!(list(LISTING, &PsArgs { json: true }).await.is_ok());
}

#[tokio::test]
async fn send_list_rejects_unexpected_response() {
    let err = list(r#"{"result":"ok","ok":true}"#, &PsArgs::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unexpected"));
}

#[tokio::test]
async fn send_list_errors_when_daemon_absent() {
    let dir = tempfile::tempdir().unwrap();
    let err = send_list(
        &ControlClient::new(control_id(&dir.path().join("no-daemon"))),
        &PsArgs::default(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not reachable"));
}
