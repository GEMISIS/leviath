//! Tests for the lifecycle acts.
//!
//! Each one drives a fake daemon, so what is asserted is the request that was
//! sent and the failure the reply produced, rather than a status code either
//! surface happens to render.

use leviath_runtime::control_socket::{ControlRequest, ControlResponse};

use super::{Action, act, is_terminal};
use crate::commands::serve::core::error::ServeError;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client};
use crate::runstate::{RunMeta, RunStatus, create_run};

/// A run on disk in the given state.
fn run_in(id: &str, status: RunStatus) -> RunMeta {
    let mut meta = RunMeta::new(
        id.to_string(),
        "test-agent".to_string(),
        "/agents/test".to_string(),
        "do the thing".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.status = status;
    meta
}

/// A state wired to the given control client and nothing else.
fn state_with(control: leviath_runtime::control_socket::ControlClient) -> super::AppState {
    let mut state = crate::commands::serve::testutil::state_with_agent_paths(Vec::new());
    state.control = control;
    state
}

/// Which states nothing can move a run out of.
#[test]
fn only_the_states_nothing_follows_are_terminal() {
    assert!(is_terminal(&RunStatus::Complete));
    assert!(is_terminal(&RunStatus::Error));
    assert!(is_terminal(&RunStatus::Cancelled));
    // Still takes messages, so stopping it is a reasonable thing to ask.
    assert!(!is_terminal(&RunStatus::CompleteInteractive));
    assert!(!is_terminal(&RunStatus::Running));
    assert!(!is_terminal(&RunStatus::Starting));
    assert!(!is_terminal(&RunStatus::WaitingInput));
    assert!(!is_terminal(&RunStatus::Paused));
}

/// Each action sends its own control request, and a daemon that says yes is
/// the whole answer.
#[tokio::test]
async fn each_action_sends_its_own_request() {
    let cases = [
        (
            Action::Pause,
            ControlRequest::Pause {
                run_id: String::new(),
            },
        ),
        (
            Action::Resume,
            ControlRequest::Resume {
                run_id: String::new(),
            },
        ),
        (
            Action::Cancel,
            ControlRequest::Cancel {
                run_id: String::new(),
            },
        ),
    ];
    for (action, expected) in cases {
        let (control, _dir, _srv) = fake_daemon(move |req| {
            assert_eq!(
                std::mem::discriminant(&req),
                std::mem::discriminant(&expected)
            );
            ControlResponse::Ok { ok: true }
        });
        act(&state_with(control), "run-a", action)
            .await
            .expect("the daemon said yes");
    }
}

/// A run that has already finished is a conflict, and the message says which
/// state it finished in: "not found" about a run sitting in the listing is
/// what sends somebody looking for a wrong run id.
#[tokio::test]
async fn a_finished_run_is_a_conflict_that_names_its_state() {
    crate::runstate::with_isolated_runs_dir_async("lifecycle-terminal", |_d| async move {
        create_run(&run_in("run-done", RunStatus::Complete)).expect("run written");
        // The daemon is never asked, so a client would get this answer even
        // with the daemon down.
        let failure = act(&state_with(no_daemon_client()), "run-done", Action::Pause)
            .await
            .expect_err("a finished run cannot be paused");
        assert!(matches!(failure, ServeError::Conflict(_)), "{failure:?}");
        assert!(failure.to_string().contains("complete"), "{failure}");
        assert!(failure.to_string().contains("paused"), "{failure}");
    })
    .await;
}

/// A run still taking messages can be cancelled: finishing its stages is not
/// the same as being over.
#[tokio::test]
async fn a_run_that_still_takes_messages_can_be_cancelled() {
    crate::runstate::with_isolated_runs_dir_async("lifecycle-interactive", |_d| async move {
        create_run(&run_in("run-open", RunStatus::CompleteInteractive)).expect("run written");
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        act(&state_with(control), "run-open", Action::Cancel)
            .await
            .expect("an interactive run can be cancelled");
    })
    .await;
}

/// The daemon's "no" is one answer for several reasons, so the failure names
/// the reasons rather than claiming the run does not exist.
#[tokio::test]
async fn a_refusal_names_what_it_could_mean() {
    let cases = [
        (Action::Pause, "not pausable"),
        (Action::Resume, "not paused"),
        (Action::Cancel, "not found"),
    ];
    for (action, expected) in cases {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        let failure = act(&state_with(control), "ghost", action)
            .await
            .expect_err("the daemon said no");
        assert!(matches!(failure, ServeError::NotFound(_)), "{failure:?}");
        assert!(failure.to_string().contains(expected), "{failure}");
    }
}

/// An answer to a different question is this server's problem, not the
/// caller's, and it is reported as one.
#[tokio::test]
async fn an_answer_to_another_question_is_internal() {
    let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
    });
    let failure = act(&state_with(control), "run-a", Action::Pause)
        .await
        .expect_err("a reply with no arm for it");
    assert!(matches!(failure, ServeError::Internal(_)), "{failure:?}");
}

/// No daemon is not a missing run: the remedy is to get the daemon back, and
/// the failure says so.
#[tokio::test]
async fn a_daemon_that_cannot_be_reached_says_so() {
    let failure = act(&state_with(no_daemon_client()), "run-a", Action::Pause)
        .await
        .expect_err("no daemon");
    assert!(
        matches!(failure, ServeError::DaemonUnavailable(_)),
        "{failure:?}"
    );
    assert!(failure.to_string().contains("not reachable"), "{failure}");
}
