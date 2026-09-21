//! Tests for [`super`] - the `lev msg` / `cancel` / `pause` / `resume` /
//! `respond` request cores.

use super::*;
use crate::test_support::fixtures;
use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

/// Every request line a [`fake_daemon`] was sent, in order.
type Received = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Bind a control listener at a fresh id under `dir` and serve `responses`,
/// one per connection in the order given. Returns the id clients connect to,
/// the request lines the daemon reads, and the server task.
fn fake_daemon(
    dir: &std::path::Path,
    responses: Vec<String>,
) -> (ControlId, Received, JoinHandle<()>) {
    let id = control_id(dir);
    let mut listener = bind_control_listener(&id).unwrap();
    let received: Received = Received::default();
    let recorder = std::sync::Arc::clone(&received);
    let handle = tokio::spawn(async move {
        for response_line in responses {
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let request = lines.next_line().await.unwrap().expect("a request line");
            leviath_core::sync::lock(&recorder).push(request);
            write_half
                .write_all(response_line.as_bytes())
                .await
                .unwrap();
            write_half.write_all(b"\n").await.unwrap();
        }
    });
    (id, received, handle)
}

/// Run `op` against a fake daemon serving `responses`, reporting the outcome
/// and the `op` field of every request the daemon was sent.
async fn served<F, Fut>(
    responses: Vec<String>,
    op: F,
) -> (anyhow::Result<()>, Vec<serde_json::Value>)
where
    F: FnOnce(ControlClient) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let dir = tempfile::tempdir().unwrap();
    let (id, received, server) = fake_daemon(dir.path(), responses);
    let result = op(ControlClient::new(id)).await;
    // The client has its replies, so whatever is left of the queue is a
    // request this run never made.
    server.abort();
    let requests = leviath_core::sync::lock(&received)
        .iter()
        .map(|line| serde_json::from_str(line).expect("the daemon is sent JSON"))
        .collect();
    (result, requests)
}

/// Run `op` against a fake daemon that replies `response_line`.
async fn with_daemon<F, Fut>(response_line: impl Into<String>, op: F) -> anyhow::Result<()>
where
    F: FnOnce(ControlClient) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    served(vec![response_line.into()], op).await.0
}

fn msg_args() -> MsgArgs {
    MsgArgs {
        agent_id: "a".to_string(),
        content: "hi".to_string(),
        attach: Vec::new(),
    }
}

#[test]
fn message_parts_take_attachments_and_named_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"notes").unwrap();
    let (text, parts) = message_parts(
        "see @a.png and @gone.png",
        &["b.txt:notes".into()],
        dir.path(),
    )
    .unwrap();
    assert_eq!(text, "see @a.png and @gone.png");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].name, "b.txt");
    assert_eq!(parts[0].region.as_deref(), Some("notes"));
    assert_eq!(parts[1].name, "a.png");
    assert!(message_parts("x", &["missing.bin".into()], dir.path()).is_err());
    std::fs::write(dir.path().join("empty.png"), b"").unwrap();
    let err = message_parts("see @empty.png", &[], dir.path()).unwrap_err();
    assert!(err.to_string().contains("nothing to attach"), "{err}");
}

#[tokio::test]
async fn message_with_an_unreadable_attachment_never_dials() {
    // No daemon behind this id: the attachment fails first, so nothing
    // is ever dialled, and a daemon that never hears from us is right.
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
    let mut args = msg_args();
    args.attach = vec!["/no/such/file.png".to_string()];
    let err = send_message(&client, &args).await.unwrap_err();
    assert!(err.to_string().contains("could not read"), "{err}");
}

#[tokio::test]
async fn message_applied() {
    let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
        send_message(&c, &msg_args()).await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn message_not_delivered() {
    let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
        send_message(&c, &msg_args()).await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("no agent accepted"));
}

#[tokio::test]
async fn pause_applied() {
    let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
        pause_run(
            &c,
            &PauseArgs {
                run_id: "r".to_string(),
            },
        )
        .await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn pause_refused() {
    let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
        pause_run(
            &c,
            &PauseArgs {
                run_id: "r".to_string(),
            },
        )
        .await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("not pausable"));
}

#[tokio::test]
async fn resume_applied() {
    let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
        resume_run(
            &c,
            &ResumeArgs {
                run_id: "r".to_string(),
            },
        )
        .await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn resume_refused() {
    let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
        resume_run(
            &c,
            &ResumeArgs {
                run_id: "r".to_string(),
            },
        )
        .await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("not paused"));
}

#[tokio::test]
async fn cancel_applied() {
    let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
        cancel_run(
            &c,
            &CancelArgs {
                run_id: "r".to_string(),
                force: false,
            },
        )
        .await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn cancel_unknown_run() {
    let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
        cancel_run(
            &c,
            &CancelArgs {
                run_id: "r".to_string(),
                force: false,
            },
        )
        .await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("no such run"));
}

#[tokio::test]
async fn unexpected_response_is_an_error() {
    // Both the `send_bool` path (`lev msg`) and `cancel_run`'s own match
    // reject a response shape they didn't ask for.
    let r = with_daemon(r#"{"result":"spawned","run_id":"x"}"#, |c| async move {
        send_message(&c, &msg_args()).await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("unexpected"));

    let r = with_daemon(r#"{"result":"spawned","run_id":"x"}"#, |c| async move {
        cancel_run(
            &c,
            &CancelArgs {
                run_id: "r".to_string(),
                force: false,
            },
        )
        .await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("unexpected"));
}

/// `lev msg` has no on-disk fallback - an unreachable daemon is simply an
/// error, unlike `lev cancel`.
#[tokio::test]
async fn message_to_an_unreachable_daemon_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
    let err = send_message(&client, &msg_args()).await.unwrap_err();
    assert!(err.to_string().contains("not reachable"));
}

/// A run directory that cannot be rewritten is reported as such, rather than
/// as a successful cancel.
#[tokio::test]
async fn forcing_a_run_whose_metadata_cannot_be_written_reports_the_failure() {
    crate::runstate::with_isolated_runs_dir_async("ctl-force-unwritable", |_base| async {
        let dir = crate::runstate::run_dir("blocked-1");
        std::fs::create_dir_all(dir.join("meta.json")).unwrap();

        let err = cancel_run(
            &ControlClient::new(control_id(std::path::Path::new("/nonexistent"))),
            &CancelArgs {
                run_id: "blocked-1".to_string(),
                force: true,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("could not write"), "got: {err}");
    })
    .await;
}

#[tokio::test]
async fn not_reachable_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
    let err = cancel_run(
        &client,
        &CancelArgs {
            run_id: "r".to_string(),
            force: false,
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not reachable"));
}

/// Write a live-looking run into the (isolated) runs dir.
fn seed_live_run(run_id: &str) {
    crate::runstate::create_run(&crate::runstate::RunMeta {
        status: crate::runstate::RunStatus::Running,
        ..fixtures::run_meta(run_id)
    })
    .unwrap();
}

fn status_of(run_id: &str) -> crate::runstate::RunStatus {
    crate::runstate::read_meta(run_id).unwrap().status
}

/// `--force` never contacts the daemon, so a kill stays possible when the
/// daemon is dead, wedged, or was never started.
#[tokio::test]
async fn force_cancels_on_disk_without_a_daemon() {
    crate::runstate::with_isolated_runs_dir_async("ctl-force-cancel", |_base| async {
        seed_live_run("stuck-1");
        let dir = tempfile::tempdir().unwrap();
        // A socket path with nothing listening on it.
        let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));

        cancel_run(
            &client,
            &CancelArgs {
                run_id: "stuck-1".to_string(),
                force: true,
            },
        )
        .await
        .expect("forced cancel succeeds with no daemon");

        assert_eq!(status_of("stuck-1"), crate::runstate::RunStatus::Cancelled);
    })
    .await;
}

/// Without `--force`, an unreachable daemon falls back to the on-disk write
/// rather than leaving the user with an error and a run still marked live.
#[tokio::test]
async fn an_unreachable_daemon_falls_back_to_cancelling_on_disk() {
    crate::runstate::with_isolated_runs_dir_async("ctl-fallback-cancel", |_base| async {
        seed_live_run("stuck-2");
        let dir = tempfile::tempdir().unwrap();
        let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));

        cancel_run(
            &client,
            &CancelArgs {
                run_id: "stuck-2".to_string(),
                force: false,
            },
        )
        .await
        .expect("the fallback succeeds");

        assert_eq!(status_of("stuck-2"), crate::runstate::RunStatus::Cancelled);
    })
    .await;
}

/// Forcing a run that already finished is reported, not treated as a failure.
#[tokio::test]
async fn forcing_an_already_finished_run_is_not_an_error() {
    crate::runstate::with_isolated_runs_dir_async("ctl-force-terminal", |_base| async {
        crate::runstate::create_run(&crate::runstate::RunMeta {
            status: crate::runstate::RunStatus::Complete,
            ..fixtures::run_meta("done-1")
        })
        .unwrap();

        cancel_run(
            &ControlClient::new(control_id(std::path::Path::new("/nonexistent"))),
            &CancelArgs {
                run_id: "done-1".to_string(),
                force: true,
            },
        )
        .await
        .expect("already-finished is reported, not an error");

        assert_eq!(
            status_of("done-1"),
            crate::runstate::RunStatus::Complete,
            "and the recorded outcome is left intact"
        );
    })
    .await;
}

/// Forcing an id that names no run at all is still an honest failure.
#[tokio::test]
async fn forcing_an_unknown_run_reports_no_such_run() {
    crate::runstate::with_isolated_runs_dir_async("ctl-force-missing", |_base| async {
        let err = cancel_run(
            &ControlClient::new(control_id(std::path::Path::new("/nonexistent"))),
            &CancelArgs {
                run_id: "never-existed".to_string(),
                force: true,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no such run"), "got: {err}");
    })
    .await;
}

// ─── lev respond ──────────────────────────────────────────────────────────

fn respond_args() -> RespondArgs {
    RespondArgs {
        request_id: Some("q1".to_string()),
        value: None,
        choice: None,
        approve: false,
        deny: false,
        feedback: None,
        session: false,
        stage: false,
        json: false,
        attach: Vec::new(),
    }
}

/// A text answer takes its files from `--attach` and from `@path` in
/// the words; anything else refuses `--attach` outright.
#[test]
fn a_text_answer_carries_its_files_and_a_choice_refuses_them() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mark.png"), b"\x89PNG\r\n\x1a\nmark").unwrap();
    std::fs::write(dir.path().join("notes.md"), "# n").unwrap();
    let attach = vec!["notes.md:brief".to_string()];
    let answered = attach_answer(
        InteractionResponse::text("q1", "the arm is wrong, see @mark.png"),
        &attach,
        dir.path(),
    )
    .unwrap();
    assert_eq!(
        answered.value.as_deref(),
        Some("the arm is wrong, see @mark.png")
    );
    assert_eq!(answered.parts.len(), 2);
    assert_eq!(answered.parts[0].name, "notes.md");
    assert_eq!(answered.parts[0].region.as_deref(), Some("brief"));
    assert_eq!(answered.parts[1].name, "mark.png");
    assert!(answered.parts[1].region.is_none());

    let bare = attach_answer(InteractionResponse::choice("q1", 1), &[], dir.path()).unwrap();
    assert!(bare.parts.is_empty());
    let err = attach_answer(InteractionResponse::choice("q1", 1), &attach, dir.path()).unwrap_err();
    assert!(err.to_string().contains("text answer"), "{err}");
    let err = attach_answer(
        InteractionResponse::text("q1", "x"),
        &["/no/such/file.png".to_string()],
        dir.path(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("file.png"), "{err}");
}

/// An attachment that cannot be read fails the answer before any daemon
/// is dialled: there is none behind this id, and the error is the file's.
#[tokio::test]
async fn respond_refuses_a_bad_attachment_before_contacting_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(dir.path()));
    let err = respond(
        &client,
        &RespondArgs {
            value: Some("here".to_string()),
            attach: vec!["/no/such/file.png".to_string()],
            ..respond_args()
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("file.png"), "{err}");
}

/// `lev respond <id> --deny --feedback TEXT` is a deny carrying the text;
/// blank text is the plain deny, the same rule every other client follows.
#[test]
fn build_response_deny_with_feedback() {
    let r = build_response(
        "q1",
        &RespondArgs {
            deny: true,
            feedback: Some("use git log, not git show".to_string()),
            ..respond_args()
        },
    );
    assert_eq!(
        r,
        InteractionResponse::deny_with_feedback("q1", "use git log, not git show")
    );
    let blank = build_response(
        "q1",
        &RespondArgs {
            deny: true,
            feedback: Some("  ".to_string()),
            ..respond_args()
        },
    );
    assert_eq!(
        blank,
        InteractionResponse::approval("q1", false, ApprovalScope::Once)
    );
}

/// `--feedback` without `--deny` is refused at the parser, with `--deny`
/// named in the message, and it never combines with `--approve`.
#[test]
fn feedback_flag_is_only_valid_with_deny() {
    use clap::Parser;
    #[derive(Parser, Debug)]
    struct Cli {
        #[command(flatten)]
        respond: RespondArgs,
    }
    let ok = Cli::try_parse_from(["lev", "q1", "--deny", "--feedback", "why"]).unwrap();
    assert!(ok.respond.deny);
    assert_eq!(ok.respond.feedback.as_deref(), Some("why"));

    let alone = Cli::try_parse_from(["lev", "q1", "--feedback", "why"]).unwrap_err();
    assert!(alone.to_string().contains("--deny"), "{alone}");

    // clap lets `--approve --feedback` through, so the command checks.
    let with_approve =
        Cli::try_parse_from(["lev", "q1", "--approve", "--feedback", "why"]).unwrap();
    let err = check_feedback_flag(&with_approve.respond).unwrap_err();
    assert!(err.to_string().contains("--deny"), "{err}");
    assert!(check_feedback_flag(&ok.respond).is_ok());
    assert!(check_feedback_flag(&respond_args()).is_ok());
}

/// The check runs before anything is sent: a bad flag combination is an
/// error with no daemon involved.
#[tokio::test]
async fn respond_refuses_feedback_without_deny_before_contacting_the_daemon() {
    let client = ControlClient::new(leviath_runtime::control_socket::control_id(
        std::path::Path::new("/no/such/daemon"),
    ));
    let err = respond(
        &client,
        &RespondArgs {
            approve: true,
            feedback: Some("why".to_string()),
            ..respond_args()
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("--deny"), "{err}");
}

#[test]
fn build_response_free_text_uses_value_or_empty() {
    let with_value = build_response(
        "q1",
        &RespondArgs {
            value: Some("hello".to_string()),
            ..respond_args()
        },
    );
    assert_eq!(with_value, InteractionResponse::text("q1", "hello"));
    // Missing value → empty string.
    assert_eq!(
        build_response("q1", &respond_args()),
        InteractionResponse::text("q1", "")
    );
}

#[test]
fn build_response_choice_selects_index() {
    let r = build_response(
        "q1",
        &RespondArgs {
            choice: Some(2),
            ..respond_args()
        },
    );
    assert_eq!(r, InteractionResponse::choice("q1", 2));
}

#[test]
fn build_response_approve_and_deny_and_session_scope() {
    let approved = build_response(
        "q1",
        &RespondArgs {
            approve: true,
            ..respond_args()
        },
    );
    assert_eq!(
        approved,
        InteractionResponse::approval("q1", true, ApprovalScope::Once)
    );
    let session = build_response(
        "q1",
        &RespondArgs {
            approve: true,
            session: true,
            json: false,
            ..respond_args()
        },
    );
    assert_eq!(
        session,
        InteractionResponse::approval("q1", true, ApprovalScope::Run)
    );
    let stage = build_response(
        "q1",
        &RespondArgs {
            approve: true,
            stage: true,
            ..respond_args()
        },
    );
    assert_eq!(
        stage,
        InteractionResponse::approval("q1", true, ApprovalScope::Stage)
    );
    let denied = build_response(
        "q1",
        &RespondArgs {
            deny: true,
            ..respond_args()
        },
    );
    assert_eq!(
        denied,
        InteractionResponse::approval("q1", false, ApprovalScope::Once)
    );
}

#[test]
fn kind_label_covers_every_kind() {
    for (kind, label) in [
        (InteractionKind::FreeText, "free-text"),
        (InteractionKind::MultipleChoice, "choice"),
        (InteractionKind::Confirm, "confirm"),
        (InteractionKind::ToolApproval, "tool-approval"),
        (InteractionKind::EditText, "edit-text"),
    ] {
        assert_eq!(kind_label(&kind), label);
    }
}

#[test]
fn format_interaction_renders_options_and_tool() {
    let mut req = InteractionRequest::multiple_choice(
        "q1",
        "Pick",
        vec!["a".to_string(), "b".to_string()],
        "plan",
    );
    req.tool_name = Some("bash".to_string());
    let out = format_interaction("agent-x", &req);
    assert!(out.contains("q1  [choice]  agent=agent-x  stage=plan"));
    assert!(out.contains("Pick"));
    assert!(out.contains("0) a"));
    assert!(out.contains("1) b"));
    assert!(out.contains("tool: bash"));
}

// ─── naming the interaction to answer ────────────────────────────────────

/// An `ok: true` reply.
const APPLIED: &str = r#"{"result":"ok","ok":true}"#;
/// An `ok: false` reply: the daemon holds no such interaction.
const UNKNOWN: &str = r#"{"result":"ok","ok":false}"#;

/// A `ListInteractions` reply holding one open question per `(agent, id)`.
fn interactions_line(open: &[(&str, &str)]) -> String {
    let interactions: Vec<(String, InteractionRequest)> = open
        .iter()
        .map(|(agent_id, id)| {
            (
                (*agent_id).to_string(),
                InteractionRequest::free_text(*id, "What now?", "plan", true),
            )
        })
        .collect();
    serde_json::to_string(&ControlResponse::Interactions { interactions })
        .expect("a listing serializes")
}

/// The request id of every answer among `requests`, in the order sent. Empty
/// means the daemon was never asked to answer anything.
fn answered_ids(requests: &[serde_json::Value]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request["op"] == "answer_interaction")
        .map(|request| {
            request["response"]["request_id"]
                .as_str()
                .expect("an answer names its request")
                .to_string()
        })
        .collect()
}

/// Answer `typed` against a daemon holding `open`, with an applied reply
/// waiting behind the listing.
async fn answer_with(
    typed: &str,
    open: &[(&str, &str)],
) -> (anyhow::Result<()>, Vec<serde_json::Value>) {
    let args = RespondArgs {
        request_id: Some(typed.to_string()),
        approve: true,
        ..respond_args()
    };
    served(
        vec![interactions_line(open), APPLIED.to_string()],
        |c| async move { respond(&c, &args).await },
    )
    .await
}

/// The two ids a person is choosing between: same shape, different runs.
const FIRST: (&str, &str) = (
    "probe-1789971553-793b8652da33",
    "probe-1789971553-793b8652da33-approve-call_1",
);
const SECOND: (&str, &str) = (
    "probe-1789971554-8a2b1c3d4e5f",
    "probe-1789971554-8a2b1c3d4e5f-approve-call_1",
);

#[tokio::test]
async fn respond_answers_an_interaction() {
    let (r, requests) = answer_with("q1", &[("agent-a", "q1")]).await;
    r.expect("the daemon holds q1");
    assert_eq!(answered_ids(&requests), vec!["q1".to_string()]);
}

#[tokio::test]
async fn respond_reports_no_open_interaction() {
    let (r, requests) = answer_with("q1", &[]).await;
    assert_eq!(r.unwrap_err().to_string(), "no such open interaction");
    assert!(answered_ids(&requests).is_empty());
}

/// The point of the whole thing: the first half of an id is enough to answer,
/// and it reaches the run that half names.
#[tokio::test]
async fn a_prefix_naming_one_interaction_answers_that_one() {
    let (r, requests) = answer_with("probe-1789971553", &[FIRST, SECOND]).await;
    r.expect("a prefix that names one interaction answers it");
    assert_eq!(answered_ids(&requests), vec![FIRST.1.to_string()]);
}

/// The rule that matters: a prefix two runs answer to is refused outright,
/// with both of them named, and nothing is answered. Guessing here approves
/// work the person at the keyboard never looked at.
#[tokio::test]
async fn an_ambiguous_prefix_is_refused_and_answers_nothing() {
    let elsewhere = ("agent-c", "other-1789971555-1c2d3e4f5a6b-approve-call_1");
    let (r, requests) = answer_with("probe-", &[FIRST, SECOND, elsewhere]).await;
    let err = r.unwrap_err().to_string();
    assert!(
        err.contains("'probe-' is the start of 2 open interactions"),
        "{err}"
    );
    assert!(
        err.contains("give enough of an id to name just one"),
        "{err}"
    );
    assert!(err.contains(FIRST.1), "{err}");
    assert!(err.contains(SECOND.1), "{err}");
    assert!(err.contains(&format!("agent={}", FIRST.0)), "{err}");
    assert!(
        !err.contains(elsewhere.1),
        "only the candidates are listed: {err}"
    );
    assert!(
        answered_ids(&requests).is_empty(),
        "an applied reply was waiting and nothing claimed it: {requests:?}"
    );
}

/// Which interaction an id names is read off the daemon's own listing, so a
/// daemon that cannot be reached is the error, not a guess at the id.
#[tokio::test]
async fn answering_reports_a_daemon_that_cannot_be_reached() {
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
    let err = respond(&client, &respond_args()).await.unwrap_err();
    assert!(err.to_string().contains("not reachable"), "{err}");
}

/// A full id names one request by construction, so it is answered even when
/// longer ids start with it.
#[tokio::test]
async fn a_full_id_wins_over_the_longer_ids_it_starts() {
    let open = [
        ("agent-a", "ask-call_1"),
        ("agent-b", "ask-call_10"),
        ("agent-c", "ask-call_11"),
    ];
    let (r, requests) = answer_with("ask-call_1", &open).await;
    r.expect("a full id is never in doubt");
    assert_eq!(answered_ids(&requests), vec!["ask-call_1".to_string()]);
}

/// An id is matched from its start, not anywhere inside it: the tail is the
/// part two runs most often share, and it names no run.
#[tokio::test]
async fn the_tail_of_an_id_names_nothing() {
    let (r, requests) = answer_with("approve-call_1", &[FIRST]).await;
    assert_eq!(r.unwrap_err().to_string(), "no such open interaction");
    assert!(answered_ids(&requests).is_empty());
}

/// An empty id is a mistake, not a way to answer whatever is open.
#[tokio::test]
async fn an_empty_id_is_refused_rather_than_taking_the_only_open_one() {
    let (r, requests) = answer_with("", &[FIRST]).await;
    let err = r.unwrap_err().to_string();
    assert!(err.contains("name the interaction to answer"), "{err}");
    assert!(answered_ids(&requests).is_empty());
}

/// Answered by someone else between the listing and the answer: the id was
/// resolved, and the daemon still gets the last word on whether it lands.
#[tokio::test]
async fn an_interaction_that_goes_away_mid_answer_is_reported() {
    let args = RespondArgs {
        request_id: Some("probe-1789971553".to_string()),
        approve: true,
        ..respond_args()
    };
    let (r, requests) = served(
        vec![interactions_line(&[FIRST]), UNKNOWN.to_string()],
        |c| async move { respond(&c, &args).await },
    )
    .await;
    assert_eq!(r.unwrap_err().to_string(), "no such open interaction");
    assert_eq!(answered_ids(&requests), vec![FIRST.1.to_string()]);
}

/// A full id says nothing a bare `answered` doesn't; a prefix names what it
/// reached, so the person can see which run they just let through.
#[test]
fn only_a_prefix_answer_names_the_id_it_reached() {
    assert_eq!(answered_line(FIRST.1, FIRST.1), "answered");
    assert_eq!(
        answered_line("probe-1789971553", FIRST.1),
        format!("answered {}", FIRST.1)
    );
}

// ─── --json ──────────────────────────────────────────────────────────

#[test]
fn open_interaction_serializes_the_agent_id_alongside_the_request() {
    // `#[serde(flatten)]` is what puts `id` and `prompt` at the top level
    // next to `agent_id`. Losing it would nest the request under a key no
    // caller expects.
    let mut request = InteractionRequest::multiple_choice(
        "q1",
        "Pick",
        vec!["a".to_string(), "b".to_string()],
        "plan",
    );
    request.tool_name = Some("bash".to_string());
    let open = OpenInteraction {
        agent_id: "agent-x",
        request: &request,
    };
    let value: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&open).unwrap()).unwrap();
    assert_eq!(value["agent_id"], serde_json::json!("agent-x"));
    assert_eq!(value["id"], serde_json::json!("q1"));
    assert_eq!(value["stage_name"], serde_json::json!("plan"));
    assert_eq!(value["options"], serde_json::json!(["a", "b"]));
    assert_eq!(value["tool_name"], serde_json::json!("bash"));
}

#[tokio::test]
async fn respond_lists_open_interactions_as_json() {
    let req = InteractionRequest::free_text("q1", "What now?", "plan", true);
    let line = serde_json::to_string(&ControlResponse::Interactions {
        interactions: vec![("agent-a".to_string(), req)],
    })
    .unwrap();
    let r = with_daemon(line, |c| async move {
        respond(
            &c,
            &RespondArgs {
                request_id: None,
                json: true,
                ..respond_args()
            },
        )
        .await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn respond_lists_nothing_open_as_json() {
    let line = serde_json::to_string(&ControlResponse::Interactions {
        interactions: Vec::new(),
    })
    .unwrap();
    let r = with_daemon(line, |c| async move {
        respond(
            &c,
            &RespondArgs {
                request_id: None,
                json: true,
                ..respond_args()
            },
        )
        .await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn respond_answers_an_interaction_as_json() {
    let args = RespondArgs {
        request_id: Some("probe-1789971553".to_string()),
        json: true,
        ..respond_args()
    };
    let (r, requests) = served(
        vec![interactions_line(&[FIRST]), APPLIED.to_string()],
        |c| async move { respond(&c, &args).await },
    )
    .await;
    r.expect("a prefix answers under --json too");
    assert_eq!(answered_ids(&requests), vec![FIRST.1.to_string()]);
}

#[tokio::test]
async fn respond_lists_open_interactions() {
    let req = InteractionRequest::free_text("q1", "What now?", "plan", true);
    let line = serde_json::to_string(&ControlResponse::Interactions {
        interactions: vec![("agent-a".to_string(), req)],
    })
    .unwrap();
    let r = with_daemon(line, |c| async move {
        respond(
            &c,
            &RespondArgs {
                request_id: None,
                ..respond_args()
            },
        )
        .await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn respond_lists_when_none_open() {
    let line = serde_json::to_string(&ControlResponse::Interactions {
        interactions: vec![],
    })
    .unwrap();
    let r = with_daemon(line, |c| async move {
        respond(
            &c,
            &RespondArgs {
                request_id: None,
                ..respond_args()
            },
        )
        .await
    })
    .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn respond_list_rejects_unexpected_response() {
    let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
        respond(
            &c,
            &RespondArgs {
                request_id: None,
                ..respond_args()
            },
        )
        .await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("unexpected"));
}

#[tokio::test]
async fn respond_list_errors_when_daemon_absent() {
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
    let err = respond(
        &client,
        &RespondArgs {
            request_id: None,
            ..respond_args()
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not reachable"));
}
