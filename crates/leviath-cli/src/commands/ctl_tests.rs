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

/// A run that is paused already is said to be, rather than "not pausable";
/// one in any other state keeps the daemon's refusal.
#[tokio::test]
async fn pausing_a_paused_run_says_it_is_already_paused() {
    let pause = |c: ControlClient| async move {
        pause_run(
            &c,
            &PauseArgs {
                run_id: "r".to_string(),
            },
        )
        .await
    };
    let refused = r#"{"result":"ok","ok":false}"#.to_string();
    let paused = serde_json::to_string(&ControlResponse::Status {
        status: Some(leviath_runtime::components::AgentStatus::Paused),
    })
    .unwrap();
    let (r, requests) = served(vec![refused.clone(), paused], pause).await;
    let said = r.unwrap_err().to_string();
    assert!(said.contains("already paused"), "{said}");
    assert!(said.contains("lev resume r"), "{said}");
    assert_eq!(requests.len(), 2, "the status is asked after the refusal");

    let gone = serde_json::to_string(&ControlResponse::Status { status: None }).unwrap();
    let (r, _) = served(vec![refused, gone], pause).await;
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
        request_id: "q1".to_string(),
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
    let attach = || {
        crate::commands::run::attach::attach_all(&["notes.md:brief".to_string()], dir.path())
            .unwrap()
    };
    let answered = attach_answer(
        InteractionResponse::text("q1", "the arm is wrong, see @mark.png"),
        attach(),
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

    let bare = attach_answer(InteractionResponse::choice("q1", 1), Vec::new(), dir.path()).unwrap();
    assert!(bare.parts.is_empty());
    let err =
        attach_answer(InteractionResponse::choice("q1", 1), attach(), dir.path()).unwrap_err();
    assert!(err.to_string().contains("text answer"), "{err}");
    std::fs::write(dir.path().join("empty.png"), b"").unwrap();
    let err = attach_answer(
        InteractionResponse::text("q1", "see @empty.png"),
        Vec::new(),
        dir.path(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("empty.png"), "{err}");
}

/// Bare `lev respond` parses, and says where the question ids are instead
/// of clap's bare "required argument".
#[tokio::test]
async fn respond_without_an_id_points_at_lev_interactions() {
    use clap::{CommandFactory, FromArgMatches, Parser};
    #[derive(Parser)]
    struct Respond {
        #[command(flatten)]
        args: RespondArgs,
    }
    let matches = Respond::command()
        .try_get_matches_from(["respond", "--json"])
        .expect("bare respond parses");
    let args = Respond::from_arg_matches(&matches).unwrap().args;
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(dir.path()));
    let err = respond(&client, &args).await.unwrap_err().to_string();
    assert!(err.contains("needs the id of the question"), "{err}");
    assert!(err.contains("`lev interactions` lists"), "{err}");
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

/// The open questions the respond tests answer, one of each kind.
fn approval_q() -> InteractionRequest {
    InteractionRequest::tool_approval("q1", "bash", serde_json::json!({}), "s", &[])
}
fn confirm_q() -> InteractionRequest {
    InteractionRequest::confirm("q1", "Sure?", "s")
}
fn choice_q() -> InteractionRequest {
    InteractionRequest::multiple_choice(
        "q1",
        "Pick",
        vec!["Postgres".to_string(), "SQLite".to_string()],
        "s",
    )
}
fn text_q() -> InteractionRequest {
    InteractionRequest::free_text("q1", "Why?", "s", true)
}

/// `args` with `answer` as its positional answer.
fn answering(answer: &str) -> RespondArgs {
    RespondArgs {
        value: Some(answer.to_string()),
        ..respond_args()
    }
}

/// `lev respond <id> --deny --feedback TEXT` is a deny carrying the text;
/// blank text is the plain deny, the same rule every other client follows.
#[test]
fn build_response_deny_with_feedback() {
    let deny = |feedback: &str| RespondArgs {
        deny: true,
        feedback: Some(feedback.to_string()),
        ..respond_args()
    };
    assert_eq!(
        build_response(&approval_q(), &deny("use git log, not git show")),
        Ok(InteractionResponse::deny_with_feedback(
            "q1",
            "use git log, not git show"
        ))
    );
    assert_eq!(
        build_response(&approval_q(), &deny("  ")),
        Ok(InteractionResponse::approval(
            "q1",
            false,
            ApprovalScope::Once
        ))
    );
}

/// Every kind answered by what it shows: the word, the number it is listed
/// under, and the old flags, which stay as aliases.
#[test]
fn every_kind_is_answered_by_name_number_or_flag() {
    let yes = |scope| Ok(InteractionResponse::approval("q1", true, scope));
    let no = Ok(InteractionResponse::approval(
        "q1",
        false,
        ApprovalScope::Once,
    ));
    let approve = |session, stage| RespondArgs {
        approve: true,
        session,
        stage,
        ..respond_args()
    };
    let choice = |index| RespondArgs {
        choice: Some(index),
        ..respond_args()
    };
    let deny = RespondArgs {
        deny: true,
        ..respond_args()
    };
    let approval = approval_q();
    for (args, want) in [
        (answering("allow"), yes(ApprovalScope::Once)),
        (answering("1"), yes(ApprovalScope::Once)),
        (approve(false, false), yes(ApprovalScope::Once)),
        (choice(0), yes(ApprovalScope::Once)),
        (answering("allow-stage"), yes(ApprovalScope::Stage)),
        (answering("2"), yes(ApprovalScope::Stage)),
        (approve(false, true), yes(ApprovalScope::Stage)),
        (answering("Allow-Run"), yes(ApprovalScope::Run)),
        (answering("3"), yes(ApprovalScope::Run)),
        (approve(true, false), yes(ApprovalScope::Run)),
        (choice(2), yes(ApprovalScope::Run)),
        (answering("deny"), no.clone()),
        (answering("4"), no.clone()),
        (deny.clone(), no.clone()),
        (choice(3), no.clone()),
    ] {
        assert_eq!(build_response(&approval, &args), want, "{args:?}");
    }
    let redirect = Ok(InteractionResponse::deny_with_feedback("q1", "use the API"));
    for answer in ["deny", "deny-feedback", "4", "5"] {
        let args = RespondArgs {
            feedback: Some("use the API".to_string()),
            ..answering(answer)
        };
        assert_eq!(build_response(&approval, &args), redirect, "{answer}");
    }

    let confirm = confirm_q();
    for (args, want) in [
        (answering("yes"), yes(ApprovalScope::Once)),
        (answering("1"), yes(ApprovalScope::Once)),
        (approve(false, false), yes(ApprovalScope::Once)),
        (choice(0), yes(ApprovalScope::Once)),
        (answering("NO"), no.clone()),
        (answering("2"), no.clone()),
        (deny, no.clone()),
        (choice(1), no),
    ] {
        assert_eq!(build_response(&confirm, &args), want, "{args:?}");
    }

    let picked = |index| Ok(InteractionResponse::choice("q1", index));
    let choices = choice_q();
    for (args, want) in [
        (answering("SQLite"), picked(1)),
        (answering("sq"), picked(1)),
        (answering("2"), picked(1)),
        (choice(1), picked(1)),
        (answering("postgres"), picked(0)),
    ] {
        assert_eq!(build_response(&choices, &args), want, "{args:?}");
    }

    // A text question takes the words as written, a number included, and an
    // empty "" is an answer the person typed.
    let text = text_q();
    for typed in ["hello", "1", ""] {
        assert_eq!(
            build_response(&text, &answering(typed)),
            Ok(InteractionResponse::text("q1", typed))
        );
    }
}

/// A wrong answer is refused with the answers that would work, ready to copy.
#[test]
fn a_wrong_answer_is_refused_with_the_right_ones() {
    assert_eq!(
        build_response(&approval_q(), &answering("maybe")).unwrap_err(),
        "\"maybe\" is not an answer to this approval; answer allow, allow-stage, allow-run, \
         deny or deny-feedback (or 1-5)"
    );
    assert_eq!(
        build_response(&choice_q(), &answering("s")),
        Ok(InteractionResponse::choice("q1", 1))
    );
    let mut close = choice_q();
    close.options.push("SQL Server".to_string());
    assert_eq!(
        build_response(&close, &answering("sql")).unwrap_err(),
        "\"sql\" could be any of sqlite or sql-server; give more of it, or its number"
    );
}

/// `--feedback` goes with a deny however the deny is written; beside
/// `--approve` it is refused before anything is asked. `--stage` and
/// `--session` widen `--approve` and nothing else.
#[test]
fn flags_that_only_fit_one_answer_are_refused_beside_the_others() {
    use clap::Parser;
    #[derive(Parser, Debug)]
    struct Cli {
        #[command(flatten)]
        respond: RespondArgs,
    }
    let ok = Cli::try_parse_from(["lev", "q1", "--deny", "--feedback", "why"]).unwrap();
    assert!(ok.respond.deny);
    assert_eq!(ok.respond.feedback.as_deref(), Some("why"));
    let word = Cli::try_parse_from(["lev", "q1", "deny", "--feedback", "why"]).unwrap();
    assert_eq!(word.respond.value.as_deref(), Some("deny"));
    assert!(check_flags(&word.respond).is_ok());

    let with_approve =
        Cli::try_parse_from(["lev", "q1", "--approve", "--feedback", "why"]).unwrap();
    let err = check_flags(&with_approve.respond).unwrap_err();
    assert!(err.to_string().contains("goes with a deny"), "{err}");
    assert!(check_flags(&ok.respond).is_ok());
    assert!(check_flags(&respond_args()).is_ok());

    for argv in [
        ["lev", "q1", "allow", "--stage"],
        ["lev", "q1", "--choice=0", "--session"],
    ] {
        let args = Cli::try_parse_from(argv).unwrap().respond;
        let err = check_flags(&args).unwrap_err();
        assert!(
            err.to_string().contains("allow-stage or allow-run"),
            "{err}"
        );
    }
    for argv in [
        ["lev", "q1", "--approve", "--stage"],
        ["lev", "q1", "--deny", "--session"],
    ] {
        assert!(check_flags(&Cli::try_parse_from(argv).unwrap().respond).is_ok());
    }
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
    assert!(err.to_string().contains("goes with a deny"), "{err}");
}

/// Naming an interaction is not answering it. With no answer given the command
/// refuses, says how to look at the question instead, and never dials the
/// daemon: the answer would be permanent, and an empty one reads to the run
/// like nobody answered.
#[tokio::test]
async fn respond_without_an_answer_is_refused_before_contacting_the_daemon() {
    let client = ControlClient::new(control_id(std::path::Path::new("/no/such/daemon")));
    let err = respond(&client, &respond_args()).await.unwrap_err();
    let err = err.to_string();
    assert!(
        err.contains("refusing to answer 'q1' without an answer"),
        "{err}"
    );
    assert!(err.contains("lev interactions q1"), "{err}");
}

/// Two answers is refused rather than one of them quietly dropped.
#[test]
fn one_answer_and_only_one() {
    assert!(
        check_one_answer(&RespondArgs {
            approve: true,
            ..respond_args()
        })
        .is_ok()
    );
    let err = check_one_answer(&RespondArgs {
        value: Some("yes".to_string()),
        choice: Some(1),
        ..respond_args()
    })
    .unwrap_err();
    assert!(err.to_string().contains("give one answer"), "{err}");
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
    assert!(out.contains("\n    [1] a  (a)\n    [2] b  (b)"), "{out}");
    assert!(out.contains("tool: bash"));
    assert!(
        out.ends_with("answer with: lev respond q1 a|b  (or the start of one, or 1-2)"),
        "{out}"
    );
}

/// An approval lists its options the way the dashboard numbers them, each
/// beside what to type, the deny that takes words included.
#[test]
fn an_approval_lists_its_options_from_one_beside_their_words() {
    let out = format_interaction("agent-x", &approval_q());
    assert!(out.contains("\n    [1] Allow once  (allow)\n"), "{out}");
    assert!(out.contains("  (allow-stage)\n"), "{out}");
    assert!(out.contains("  (allow-run)\n"), "{out}");
    assert!(out.contains("\n    [4] Deny  (deny)\n"), "{out}");
    assert!(
        out.contains("\n    [5] Deny with feedback  (deny --feedback \"TEXT\")"),
        "{out}"
    );
    let text = format_interaction("agent-x", &text_q());
    assert!(!text.contains("[1]"), "{text}");
    assert!(
        text.ends_with("answer with: lev respond q1 \"your answer\""),
        "{text}"
    );
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
        request_id: typed.to_string(),
        value: Some("go on".to_string()),
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
    let args = RespondArgs {
        value: Some("yes".to_string()),
        ..respond_args()
    };
    let err = respond(&client, &args).await.unwrap_err();
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
    assert!(err.contains("needs the id of the question"), "{err}");
    assert!(answered_ids(&requests).is_empty());
    // `lev interactions ""` refuses it the same way.
    let err = show_interaction(&[], "", false).unwrap_err().to_string();
    assert!(err.contains("name an interaction"), "{err}");
    assert!(err.contains("lev interactions"), "{err}");
}

/// Answered by someone else between the listing and the answer: the id was
/// resolved, and the daemon still gets the last word on whether it lands.
#[tokio::test]
async fn an_interaction_that_goes_away_mid_answer_is_reported() {
    let args = RespondArgs {
        request_id: "probe-1789971553".to_string(),
        value: Some("go on".to_string()),
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
    let open = OpenInteraction::new("agent-x", &request);
    let value: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&open).unwrap()).unwrap();
    assert_eq!(value["agent_id"], serde_json::json!("agent-x"));
    assert_eq!(value["id"], serde_json::json!("q1"));
    assert_eq!(value["stage_name"], serde_json::json!("plan"));
    assert_eq!(value["options"], serde_json::json!(["a", "b"]));
    assert_eq!(value["tool_name"], serde_json::json!("bash"));
    assert_eq!(
        value["answer_options"],
        serde_json::json!([
            {"id": "a", "label": "a", "number": 1, "answer": "lev respond q1 a"},
            {"id": "b", "label": "b", "number": 2, "answer": "lev respond q1 b"},
        ])
    );
}

#[tokio::test]
async fn respond_answers_an_interaction_as_json() {
    let args = RespondArgs {
        request_id: "probe-1789971553".to_string(),
        value: Some("go on".to_string()),
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

/// An answer the question cannot take is refused before it is sent, with the
/// line that would answer it. Nothing reaches the daemon.
#[tokio::test]
async fn an_answer_of_the_wrong_shape_is_refused_with_the_right_one() {
    let choice = InteractionRequest::multiple_choice(
        "q1",
        "Pick",
        vec!["a".to_string(), "b".to_string()],
        "plan",
    );
    let listing = serde_json::to_string(&ControlResponse::Interactions {
        interactions: vec![("agent-a".to_string(), choice)],
    })
    .unwrap();
    // Past the last option: the text path would be read against the labels,
    // but an index has to name one.
    let args = RespondArgs {
        choice: Some(2),
        ..respond_args()
    };
    let (r, requests) = served(vec![listing, APPLIED.to_string()], |c| async move {
        respond(&c, &args).await
    })
    .await;
    let err = r.unwrap_err().to_string();
    assert_eq!(
        err,
        "'q1' has no option 2: counting from 0, its options are 0-1; nothing was answered. \
         Answer with: lev respond q1 a|b  (or the start of one, or 1-2)"
    );
    assert!(answered_ids(&requests).is_empty());
}

/// An answer the parser takes and the question still cannot: `--approve` on
/// a text question is refused by the same check the daemon runs.
#[tokio::test]
async fn a_flag_the_question_cannot_take_is_refused_before_sending() {
    let args = RespondArgs {
        approve: true,
        ..respond_args()
    };
    let (r, requests) = served(
        vec![interactions_line(&[("agent-a", "q1")]), APPLIED.to_string()],
        |c| async move { respond(&c, &args).await },
    )
    .await;
    let err = r.unwrap_err().to_string();
    assert!(err.starts_with("'q1' is a text question"), "{err}");
    assert!(answered_ids(&requests).is_empty());
}

/// `--attach` beside a choice is refused once the question says it is a
/// choice, and nothing is answered.
#[tokio::test]
async fn a_file_beside_a_choice_is_refused_and_answers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.md");
    std::fs::write(&notes, "# n").unwrap();
    let listing = serde_json::to_string(&ControlResponse::Interactions {
        interactions: vec![("agent-a".to_string(), choice_q())],
    })
    .unwrap();
    let args = RespondArgs {
        attach: vec![notes.to_string_lossy().to_string()],
        ..answering("sqlite")
    };
    let (r, requests) = served(vec![listing, APPLIED.to_string()], |c| async move {
        respond(&c, &args).await
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("text answer"));
    assert!(answered_ids(&requests).is_empty());
}

/// The agent's round trip: read the listing as JSON, send an option's word
/// back, and the daemon is handed the decision that word names.
#[tokio::test]
async fn an_option_word_from_the_listing_answers_with_that_option() {
    let listing = serde_json::to_string(&ControlResponse::Interactions {
        interactions: vec![("agent-a".to_string(), approval_q())],
    })
    .unwrap();
    let open: Vec<(String, InteractionRequest)> = vec![("agent-a".to_string(), approval_q())];
    let shown = serde_json::to_value(OpenInteraction::new(&open[0].0, &open[0].1)).unwrap();
    let word = shown["answer_options"][2]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(word, "allow-run");
    let args = answering(&word);
    let (r, requests) = served(vec![listing, APPLIED.to_string()], |c| async move {
        respond(&c, &args).await
    })
    .await;
    r.expect("the word answers");
    let sent = &requests[1]["response"];
    assert_eq!(sent["approved"], true);
    assert_eq!(sent["scope"], "session");
}

/// The daemon checks every answer as well, and its reason is what the person
/// reads, not "unexpected daemon response".
#[tokio::test]
async fn a_refusal_from_the_daemon_is_reported_with_its_reason() {
    let refusal = serde_json::to_string(&ControlResponse::Error {
        message: "'q1' is a text question: answer it with text".to_string(),
    })
    .unwrap();
    let args = RespondArgs {
        value: Some("go on".to_string()),
        ..respond_args()
    };
    let (r, _) = served(
        vec![interactions_line(&[("agent-a", "q1")]), refusal],
        |c| async move { respond(&c, &args).await },
    )
    .await;
    assert_eq!(
        r.unwrap_err().to_string(),
        "'q1' is a text question: answer it with text"
    );
}

// ─── lev interactions ─────────────────────────────────────────────────────

fn interactions_args(request_id: Option<&str>, json: bool) -> InteractionsArgs {
    InteractionsArgs {
        request_id: request_id.map(str::to_string),
        json,
    }
}

/// Listing and showing only ever read: whatever they print, the daemon is
/// asked for its listing and for nothing else.
async fn read_with(
    listing: String,
    args: InteractionsArgs,
) -> (anyhow::Result<()>, Vec<serde_json::Value>) {
    served(vec![listing, APPLIED.to_string()], |c| async move {
        interactions(&c, &args).await
    })
    .await
}

#[tokio::test]
async fn interactions_lists_and_shows_without_answering() {
    for args in [
        interactions_args(None, false),
        interactions_args(None, true),
        interactions_args(Some("probe-1789971553"), false),
        interactions_args(Some(FIRST.1), true),
    ] {
        let (r, requests) = read_with(interactions_line(&[FIRST, SECOND]), args).await;
        r.expect("a read of what is open succeeds");
        let ops: Vec<&str> = requests.iter().filter_map(|r| r["op"].as_str()).collect();
        assert!(
            ops.iter()
                .all(|op| ["list_interactions", "list"].contains(op)),
            "listings, nothing else: {requests:?}"
        );
        assert!(answered_ids(&requests).is_empty());
    }
}

#[tokio::test]
async fn interactions_lists_nothing_open() {
    assert_eq!(listing_text(&[], &[]), "no open interactions");
    for json in [false, true] {
        let (r, _) = read_with(interactions_line(&[]), interactions_args(None, json)).await;
        assert!(r.is_ok());
    }
}

/// Showing one takes the same id rules as answering one: an unknown id is an
/// error, and a start two runs share is refused with both named.
#[tokio::test]
async fn interactions_show_refuses_an_id_that_names_nothing_or_several() {
    let (r, _) = read_with(
        interactions_line(&[FIRST]),
        interactions_args(Some("nope"), false),
    )
    .await;
    assert_eq!(r.unwrap_err().to_string(), "no such open interaction");

    let (r, _) = read_with(
        interactions_line(&[FIRST, SECOND]),
        interactions_args(Some("probe-"), false),
    )
    .await;
    let err = r.unwrap_err().to_string();
    assert!(err.contains("is the start of 2 open interactions"), "{err}");
}

#[tokio::test]
async fn interactions_rejects_unexpected_response() {
    let (r, _) = read_with(APPLIED.to_string(), interactions_args(None, false)).await;
    assert!(r.unwrap_err().to_string().contains("unexpected"));
}

#[tokio::test]
async fn interactions_errors_when_daemon_absent() {
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
    let err = interactions(&client, &interactions_args(None, false))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not reachable"));
}

/// The full view carries what the listing has no room for: the call's
/// arguments, the document under review, and how to answer.
#[test]
fn the_full_view_shows_arguments_body_and_the_answer_line() {
    let mut req = InteractionRequest::tool_approval(
        "run-approve-1",
        "bash",
        serde_json::json!({"command": "rm -rf build"}),
        "implement",
        &[],
    );
    req.body = Some("line one\nline two".to_string());
    let out = format_interaction_detail("agent-x", &req);
    assert!(
        out.contains("run-approve-1  [tool-approval]  agent=agent-x"),
        "{out}"
    );
    assert!(
        out.contains("  arguments:\n    {\n      \"command\": \"rm -rf build\"\n    }"),
        "{out}"
    );
    assert!(out.contains("  body:\n    line one\n    line two"), "{out}");
    assert!(out.contains("  required: yes"), "{out}");
    assert!(
        out.ends_with(
            "answer with: lev respond run-approve-1 allow|allow-stage|allow-run|deny|deny-feedback  \
             (or 1-5; deny takes --feedback TEXT)"
        ),
        "{out}"
    );

    let optional = InteractionRequest::free_text("q", "Anything else?", "s", false);
    let out = format_interaction_detail("agent-y", &optional);
    assert!(out.contains("  required: no"), "{out}");
    assert!(!out.contains("arguments:"), "{out}");
    assert!(!out.contains("body:"), "{out}");
}

// ─── a held run's question ────────────────────────────────────────────────

/// A run held off this machine with `question` open on its file, and a live
/// run beside it with one open too, as a `List` reply.
fn held_listing(run_id: &str, question: &str) -> String {
    use crate::commands::serve::core::held::{listing, seed_held};
    let held = seed_held(run_id, question);
    let mut live = seed_held("live-1", "live-1-ask-1");
    live.status = leviath_runtime::components::AgentStatus::Waiting;
    live.wait_reason = Some(leviath_core::run_meta::WaitReason::UserPrompt);
    serde_json::to_string(&listing(vec![held, live])).unwrap()
}

/// A question a held run asked cannot be answered until the run is back, and
/// answering it says so, with what to put back, rather than that there is no
/// such question.
#[tokio::test]
async fn answering_a_held_runs_question_says_the_run_is_held() {
    crate::runstate::with_isolated_runs_dir_async("ctl-held-answer", |_base| async {
        for typed in ["held-1-ask-1", "held-1-ask"] {
            let args = RespondArgs {
                request_id: typed.to_string(),
                value: Some("blue".to_string()),
                ..respond_args()
            };
            let listing = held_listing("held-1", "held-1-ask-1");
            let (r, requests) = served(
                vec![interactions_line(&[]), listing, APPLIED.to_string()],
                |c| async move { respond(&c, &args).await },
            )
            .await;
            let err = r.unwrap_err().to_string();
            assert!(err.contains("run 'held-1'"), "{err}");
            assert!(err.contains("configure 'openai' again"), "{err}");
            assert!(
                err.ends_with("under a new one. `lev interactions` lists it then"),
                "{err}"
            );
            assert!(answered_ids(&requests).is_empty());
        }
        // Not one a held run asked: the plain answer.
        let args = RespondArgs {
            request_id: "live-1-ask-1".to_string(),
            value: Some("blue".to_string()),
            ..respond_args()
        };
        let listing = held_listing("held-1", "held-1-ask-1");
        let (r, _) = served(vec![interactions_line(&[]), listing], |c| async move {
            respond(&c, &args).await
        })
        .await;
        assert_eq!(r.unwrap_err().to_string(), "no such open interaction");
    })
    .await;
}

/// The listing names a held run's question as held, with why and what to
/// do, and only a held run's: a live run's questions are the daemon's to
/// list.
#[tokio::test]
async fn a_held_runs_question_is_listed_as_held() {
    crate::runstate::with_isolated_runs_dir_async("ctl-held-list", |_base| async {
        let listing = held_listing("held-1", "held-1-ask-1");
        let held = served(vec![listing], |c| async move {
            let held = held_questions(&c).await;
            assert_eq!(held.len(), 1);
            let text = format_held(&held[0]);
            assert!(text.starts_with("held-1-ask-1  [held]"), "{text}");
            assert!(text.contains("What colour?"), "{text}");
            assert!(text.contains("configure 'openai' again"), "{text}");
            // Listed once, as held, not after a line saying nothing is open.
            let listed = listing_text(&[], &held);
            assert!(listed.starts_with("held-1-ask-1  [held]"), "{listed}");
            assert!(!listed.contains("no open interactions"), "{listed}");
            Ok(())
        })
        .await;
        held.0.unwrap();
        // The listing prints it, and showing it by its id says why it waits.
        let listing = held_listing("held-1", "held-1-ask-1");
        let (r, _) = read_with(interactions_line(&[]), interactions_args(None, false)).await;
        r.unwrap();
        let (r, _) = served(
            vec![interactions_line(&[]), listing.clone()],
            |c| async move { interactions(&c, &interactions_args(None, false)).await },
        )
        .await;
        r.unwrap();
        let (r, _) = served(vec![interactions_line(&[]), listing], |c| async move {
            interactions(&c, &interactions_args(Some("held-1-ask-1"), false)).await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("run 'held-1'"));
        // A daemon that does not list its runs holds none.
        let (r, _) = served(vec![APPLIED.to_string()], |c| async move {
            assert!(held_questions(&c).await.is_empty());
            Ok(())
        })
        .await;
        r.unwrap();
    })
    .await;
}
