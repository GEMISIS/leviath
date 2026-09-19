//! Tests for the write side.
//!
//! Whole mutations run against the schema over an isolated runs directory and
//! a fake daemon, so what is asserted is what a client reads back: the run
//! after the act, or an error carrying the code to branch on.

use async_graphql::{EmptySubscription, Request, Schema};
use leviath_runtime::control_socket::{ControlClient, ControlResponse};

use super::Mutation;
use crate::commands::serve::graphql::query::Query;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};
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

/// Run one mutation against a schema wired to the given daemon.
async fn mutate(control: ControlClient, query: &str) -> async_graphql::Response {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;
    let schema = Schema::build(Query, Mutation, EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
}

/// The three acts, each answering with the run as it is afterwards.
///
/// The fake daemon says yes without changing anything, so the status here is
/// the record's; what this checks is that the run comes back at all, which is
/// what saves a client a second request.
#[tokio::test]
async fn each_act_answers_with_the_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-acts", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Running)).expect("run written");

        for field in ["pauseAgent", "resumeAgent", "cancelAgent"] {
            let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
            let answer = mutate(
                control,
                &format!(
                    "mutation {{ {field}(runId: \"run-a\") {{ run {{ id status }} warnings }} }}"
                ),
            )
            .await;
            assert!(answer.errors.is_empty(), "{field}: {:?}", answer.errors);
            let json = serde_json::to_value(&answer.data).expect("data serializes");
            assert_eq!(json[field]["run"]["id"], "run-a", "{field}");
            assert_eq!(json[field]["run"]["status"], "RUNNING", "{field}");
            assert_eq!(
                json[field]["warnings"].as_array().map(Vec::len),
                Some(0),
                "{field}"
            );
        }
    })
    .await;
}

/// A finished run is a conflict, with the code a client branches on, rather
/// than a request that quietly does nothing.
#[tokio::test]
async fn a_finished_run_is_refused_with_a_conflict() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-done", |_d| async move {
        create_run(&run_in("run-done", RunStatus::Complete)).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            "mutation { pauseAgent(runId: \"run-done\") { run { id } } }",
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("has finished"), "{}", error.message);
        let extensions = error.extensions.as_ref().expect("extensions");
        assert_eq!(
            extensions.get("code").map(ToString::to_string),
            Some("\"CONFLICT\"".to_string())
        );
        assert_eq!(
            extensions.get("httpStatus").map(ToString::to_string),
            Some("409".to_string())
        );
    })
    .await;
}

/// A run the daemon does not know is a `NOT_FOUND`, and the message names both
/// things the daemon's one "no" can mean.
#[tokio::test]
async fn a_run_the_daemon_refuses_is_not_found() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-ghost", |_d| async move {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        let answer = mutate(
            control,
            "mutation { resumeAgent(runId: \"ghost\") { run { id } } }",
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("not paused"), "{}", error.message);
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"NOT_FOUND\"".to_string())
        );
    })
    .await;
}

/// A daemon that cannot be reached is its own failure, because its remedy is
/// its own: get the daemon back, then ask again.
#[tokio::test]
async fn an_unreachable_daemon_is_told_apart_from_a_missing_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-nodaemon", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Running)).expect("run written");
        let answer = mutate(
            no_daemon_client(),
            "mutation { cancelAgent(runId: \"run-a\") { run { id } } }",
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"DAEMON_UNAVAILABLE\"".to_string())
        );
    })
    .await;
}

/// The daemon acted, but this server cannot read the record afterwards. That
/// is this server's problem, and answering "not found" about a run that just
/// moved would point the blame at the caller.
#[tokio::test]
async fn a_record_that_will_not_read_after_the_act_is_internal() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-unread", |_d| async move {
        // No run written, and a daemon that accepts anyway: the record read
        // that follows the act is what fails.
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        let answer = mutate(
            control,
            "mutation { pauseAgent(runId: \"run-gone\") { run { id } } }",
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("would not read"),
            "{}",
            error.message
        );
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"INTERNAL\"".to_string())
        );
    })
    .await;
}
