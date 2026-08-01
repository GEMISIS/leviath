use super::*;
use leviath_runtime::components::WaitReason;
use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

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
    assert_eq!(status_cell(&entry("r", AgentStatus::Cancelled)), "cancelled");
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
    assert_eq!(format_runs(&[], 0), "no agents running");
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

    let out = format_runs(&[blocked, parked], 1_200);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "header plus one line per run: {out}");
    assert!(lines[0].starts_with("RUN"), "header row: {out}");
    assert!(lines[0].contains("AGE"), "header row: {out}");
    assert!(lines[1].contains("waiting: tool approval"), "{out}");
    assert!(lines[1].contains("10m"), "stuck for ten minutes: {out}");
    assert!(lines[2].contains("waiting: children(3)"), "{out}");
    assert!(lines[2].contains("10s"), "moved ten seconds ago: {out}");
}

/// Columns line up against the header and the widest cell, and no line carries
/// trailing whitespace.
#[test]
fn format_runs_aligns_columns() {
    let short = entry("a", AgentStatus::Active);
    let mut long = entry("a-much-longer-run-id", AgentStatus::Waiting);
    long.wait_reason = Some(WaitReason::UserPrompt);
    let out = format_runs(&[short, long], 0);
    let lines: Vec<&str> = out.lines().collect();
    let run_col = lines[0].find("STATUS").expect("header has a STATUS column");
    for line in &lines[1..] {
        assert_eq!(
            &line[run_col..run_col + 1],
            line.split_whitespace().nth(1).unwrap().get(0..1).unwrap(),
            "status column starts at the header offset: {out}"
        );
    }
    for line in &lines {
        assert_eq!(line.trim_end(), *line, "no trailing blanks: {out:?}");
    }
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

const LISTING: &str = r#"{"result":"list","runs":[{"run_id":"run-a","status":"Waiting","reason":"tool_approval","stage":"implement","iteration":3,"tool_calls":7,"unattended":true}]}"#;

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
