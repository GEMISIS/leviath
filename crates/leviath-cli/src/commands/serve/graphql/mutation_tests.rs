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
    let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
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

/// A spawn answers with the run it started.
///
/// The run id comes back inside the run itself, so a client renders the new row
/// without a second request.
#[tokio::test]
async fn a_spawn_answers_with_the_run_it_started() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn", |_d| async move {
        let agents = tempfile::tempdir().expect("a temp dir");
        let agent = agents.path().join("coder");
        std::fs::create_dir_all(&agent).expect("the agent dir");
        std::fs::write(
            agent.join(leviath_core::files::MANIFEST_FILENAME),
            "[agent]\nname = \"coder\"\n\n[stages.only]\nmode = \"autonomous\"\n",
        )
        .expect("manifest written");

        // The daemon accepts, and the run's record is written the way a real
        // spawn's placeholder metadata is.
        let (control, _socket, _srv) = fake_daemon(|req| match req {
            leviath_runtime::control_socket::ControlRequest::Spawn { args } => {
                // What the daemon persists at spawn, which is what the
                // mutation then reads back.
                let mut meta = run_in(&args.run_id, RunStatus::Starting);
                meta.task = args.task.clone();
                meta.metadata = args.metadata.clone();
                create_run(&meta).expect("run written");
                ControlResponse::Spawned {
                    run_id: args.run_id,
                }
            }
            other => panic!("the spawn is what reaches the daemon: {other:?}"),
        });
        let mut state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        state.control = control;
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();

        let answer = schema
            .execute(Request::new(
                r#"mutation { spawnAgent(input: {
                     blueprint: "coder", task: "write the thing", workdir: "/tmp",
                     metadata: [{ key: "ticket", value: "42" }]
                   }) { run { id task status metadata { key value } } warnings } }"#,
            ))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let run = &json["spawnAgent"]["run"];
        assert!(
            run["id"].as_str().unwrap_or_default().starts_with("coder-"),
            "{run}"
        );
        assert_eq!(run["task"], "write the thing");
        assert_eq!(run["status"], "STARTING");
        assert_eq!(run["metadata"][0]["key"], "ticket");
    })
    .await;
}

/// A spawn this server is configured to refuse says which decision refused it.
#[tokio::test]
async fn a_spawn_the_server_refuses_is_forbidden() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-refused", |_d| async move {
        let agents = tempfile::tempdir().expect("a temp dir");
        let agent = agents.path().join("coder");
        std::fs::create_dir_all(&agent).expect("the agent dir");
        std::fs::write(
            agent.join(leviath_core::files::MANIFEST_FILENAME),
            "[agent]\nname = \"coder\"\n\n[stages.only]\nmode = \"autonomous\"\n",
        )
        .expect("manifest written");
        let mut state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        state.limits = std::sync::Arc::new(crate::commands::serve::types::ServeLimits {
            no_remote_yolo: true,
            ..Default::default()
        });
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();

        let answer = schema
            .execute(Request::new(
                r#"mutation { spawnAgent(input: {
                     blueprint: "coder", task: "t", workdir: "/tmp", yolo: true
                   }) { run { id } } }"#,
            ))
            .await;
        let error = answer.errors.first().expect("a refusal");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"FORBIDDEN\"".to_string())
        );

        let negative = schema
            .execute(Request::new(
                r#"mutation { spawnAgent(input: {
                     blueprint: "coder", task: "t", maxDepth: -1
                   }) { run { id } } }"#,
            ))
            .await;
        assert!(
            negative
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("negative"),
            "{:?}",
            negative.errors
        );
    })
    .await;
}

/// A message answers with the run, so a client sees the state it landed in.
#[tokio::test]
async fn a_message_answers_with_the_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-message", |_d| async move {
        create_run(&run_in("run-a", RunStatus::WaitingInput)).expect("run written");
        let (control, _socket, _srv) = fake_daemon(|req| match req {
            leviath_runtime::control_socket::ControlRequest::Message {
                agent_id, content, ..
            } => {
                assert_eq!(agent_id, "run-a");
                assert_eq!(content, "keep going");
                ControlResponse::Ok { ok: true }
            }
            other => panic!("the message is what reaches the daemon: {other:?}"),
        });

        let answer = mutate(
            control,
            r#"mutation { sendMessage(runId: "run-a", message: "keep going") {
                 run { id status } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["sendMessage"]["run"]["id"], "run-a");
    })
    .await;
}

/// A run that does not take messages says that, rather than reading as missing.
#[tokio::test]
async fn a_run_that_takes_no_messages_says_so() {
    crate::runstate::with_isolated_runs_dir_async("graphql-message-refused", |_d| async move {
        let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        let answer = mutate(
            control,
            r#"mutation { sendMessage(runId: "run-a", message: "hello") { run { id } } }"#,
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert!(
            error.message.contains("not accepting messages"),
            "{}",
            error.message
        );
    })
    .await;
}

/// Each answer variant reaches the daemon as what that kind of ask takes.
#[tokio::test]
async fn each_answer_variant_reaches_the_daemon() {
    let cases = [
        (
            r#"mutation { answerInteraction(input: { text: { requestId: "ask-1", value: "yes" } })
                 { requestId accepted } }"#,
            "text",
        ),
        (
            r#"mutation { answerInteraction(input: { choice: { requestId: "ask-1", choiceIndex: 1 } })
                 { requestId accepted } }"#,
            "choice",
        ),
        (
            r#"mutation { answerInteraction(input: { approval: { requestId: "ask-1", approved: true,
                 scope: SESSION } }) { requestId accepted } }"#,
            "approval",
        ),
    ];
    for (query, kind) in cases {
        let (control, _socket, _srv) = fake_daemon(move |req| match req {
            leviath_runtime::control_socket::ControlRequest::AnswerInteraction { response } => {
                assert_eq!(response.request_id, "ask-1");
                match kind {
                    "text" => assert_eq!(response.value.as_deref(), Some("yes")),
                    "choice" => assert_eq!(response.choice_index, Some(1)),
                    _ => {
                        assert_eq!(response.approved, Some(true));
                        assert_eq!(
                            response.scope,
                            Some(leviath_core::interaction::ApprovalScope::Run),
                            "SESSION is the run-long scope"
                        );
                    }
                }
                ControlResponse::Ok { ok: true }
            }
            other => panic!("the answer is what reaches the daemon: {other:?}"),
        });
        let answer = mutate(control, query).await;
        assert!(answer.errors.is_empty(), "{kind}: {:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["answerInteraction"]["accepted"], true, "{kind}");
        assert_eq!(json["answerInteraction"]["requestId"], "ask-1", "{kind}");
    }
}

/// A second answer to one request is not an error: it reads as not accepted.
///
/// Two people clicking the same prompt is ordinary, and the first one won.
#[tokio::test]
async fn a_second_answer_reads_as_not_accepted() {
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
    let answer = mutate(
        control,
        r#"mutation { answerInteraction(input: { text: { requestId: "ask-1", value: "yes" } })
             { requestId accepted } }"#,
    )
    .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["answerInteraction"]["accepted"], false);

    // A daemon that cannot be reached is still a failure: nothing was answered,
    // and the remedy is not the client's.
    let mut state = state_with_agent_paths(Vec::new());
    state.control = no_daemon_client();
    let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
        .data(state)
        .finish();
    let answer = schema
        .execute(Request::new(
            r#"mutation { answerInteraction(input: { text: { requestId: "ask-1", value: "y" } })
                 { accepted } }"#,
        ))
        .await;
    assert_eq!(
        answer
            .errors
            .first()
            .expect("a failure")
            .extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .map(ToString::to_string),
        Some("\"DAEMON_UNAVAILABLE\"".to_string())
    );
}

/// Feedback is what the model reads instead of the call, so it goes with a
/// denial. Sending it with an approval is a request that contradicts itself.
#[tokio::test]
async fn feedback_with_an_approval_is_refused() {
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
    let answer = mutate(
        control,
        r#"mutation { answerInteraction(input: { approval: { requestId: "ask-1",
             approved: true, feedback: "do it differently" } }) { accepted } }"#,
    )
    .await;
    let error = answer.errors.first().expect("a refusal");
    assert!(
        error.message.contains("goes with a denial"),
        "{}",
        error.message
    );
    assert_eq!(
        error
            .extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .map(ToString::to_string),
        Some("\"BAD_USER_INPUT\"".to_string())
    );
}

/// A denial carrying feedback is the redirect case, and it goes through.
#[tokio::test]
async fn a_denial_may_carry_feedback() {
    let (control, _socket, _srv) = fake_daemon(|req| match req {
        leviath_runtime::control_socket::ControlRequest::AnswerInteraction { response } => {
            assert_eq!(response.approved, Some(false));
            assert_eq!(response.feedback.as_deref(), Some("read the file instead"));
            ControlResponse::Ok { ok: true }
        }
        other => panic!("unexpected: {other:?}"),
    });
    let answer = mutate(
        control,
        r#"mutation { answerInteraction(input: { approval: { requestId: "ask-1",
             approved: false, feedback: "read the file instead" } }) { accepted } }"#,
    )
    .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
}

/// A negative choice index is refused: the options are a zero-based list.
#[tokio::test]
async fn a_negative_choice_is_refused() {
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
    let answer = mutate(
        control,
        r#"mutation { answerInteraction(input: { choice: { requestId: "ask-1",
             choiceIndex: -1 } }) { accepted } }"#,
    )
    .await;
    assert!(
        answer
            .errors
            .first()
            .expect("a refusal")
            .message
            .contains("negative"),
        "{:?}",
        answer.errors
    );
}

/// The approval inbox: every open ask, each naming the run it is parked on.
#[tokio::test]
async fn the_inbox_lists_every_open_ask_with_its_run() {
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Interactions {
        interactions: vec![(
            "run-a".to_string(),
            leviath_core::interaction::InteractionRequest {
                id: "ask-1".to_string(),
                kind: leviath_core::interaction::InteractionKind::ToolApproval,
                prompt: "Run `rm -rf build`?".to_string(),
                options: Vec::new(),
                tool_name: Some("shell".to_string()),
                tool_arguments: None,
                required: true,
                stage_name: "build".to_string(),
                body: None,
                body_format: Default::default(),
            },
        )],
    });

    let answer = mutate(
        control,
        "{ openInteractions { runId request { id kind prompt tool stageName required } } }",
    )
    .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    let inbox = &json["openInteractions"][0];
    assert_eq!(inbox["runId"], "run-a");
    assert_eq!(inbox["request"]["id"], "ask-1");
    assert_eq!(inbox["request"]["kind"], "TOOL_APPROVAL");
    assert_eq!(inbox["request"]["tool"], "shell");
    assert_eq!(inbox["request"]["stageName"], "build");
    assert_eq!(inbox["request"]["required"], true);
}

/// A delete removes a run and its sub-agents, and says what it removed.
#[tokio::test]
async fn a_delete_takes_a_runs_sub_agents_with_it() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete", |_d| async move {
        create_run(&run_in("root", RunStatus::Complete)).expect("run written");
        let mut worker = run_in("worker", RunStatus::Complete);
        worker.parent_run_id = Some("root".to_string());
        create_run(&worker).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(ids: ["root"]) { deleted skipped { id reason } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let deleted = json["deleteRuns"]["deleted"].as_array().expect("deleted");
        assert_eq!(deleted.len(), 2, "the run and its worker: {deleted:?}");
        assert_eq!(
            json["deleteRuns"]["skipped"].as_array().map(Vec::len),
            Some(0)
        );
        assert!(
            !crate::commands::serve::core::blueprints::run_dir("root").exists(),
            "the record is gone"
        );
    })
    .await;
}

/// A live run is skipped with its reason, and the rest of the sweep goes on.
///
/// Partial success is the normal outcome here, which is why it is a list rather
/// than a failure.
#[tokio::test]
async fn a_live_run_is_skipped_rather_than_removed() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-live", |_d| async move {
        create_run(&run_in("finished", RunStatus::Complete)).expect("run written");
        create_run(&run_in("still-going", RunStatus::Running)).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(ids: ["finished", "still-going"]) {
                 deleted skipped { id reason } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["deleteRuns"]["deleted"][0], "finished");
        let skipped = &json["deleteRuns"]["skipped"][0];
        assert_eq!(skipped["id"], "still-going");
        assert!(
            skipped["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("cancel it"),
            "it says what to do: {skipped}"
        );
    })
    .await;
}

/// A sweep by age takes the finished runs older than the mark, and leaves the
/// rest.
#[tokio::test]
async fn a_sweep_by_age_takes_the_old_finished_runs() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-sweep", |_d| async move {
        let mut old = run_in("old", RunStatus::Complete);
        old.updated_at = 100;
        create_run(&old).expect("run written");
        let mut recent = run_in("recent", RunStatus::Complete);
        recent.updated_at = 5_000;
        create_run(&recent).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            "mutation { deleteRuns(before: 1000) { deleted } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(
            json["deleteRuns"]["deleted"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(json["deleteRuns"]["deleted"][0], "old");
    })
    .await;
}

/// A delete with no predicate, or with two, is refused. Neither is a request
/// anybody meant to send.
#[tokio::test]
async fn a_delete_needs_exactly_one_predicate() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-refused", |_d| async move {
        let neither = mutate(no_daemon_client(), "mutation { deleteRuns { deleted } }").await;
        assert!(
            neither
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("refusing to delete every run"),
            "{:?}",
            neither.errors
        );

        let both = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(ids: ["a"], before: 1000) { deleted } }"#,
        )
        .await;
        assert!(
            both.errors
                .first()
                .expect("a refusal")
                .message
                .contains("two predicates"),
            "{:?}",
            both.errors
        );
    })
    .await;
}

/// A manifest exercising the blueprint writes.
fn manifest_text(name: &str, version: &str) -> String {
    format!(
        "[agent]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"d\"\n\n\
         [stages.only]\nmode = \"autonomous\"\n"
    )
}

/// Installing a blueprint, then replacing it, then removing it.
///
/// The digest changes with the bytes, which is what tells a client the two
/// revisions apart.
#[tokio::test]
async fn a_blueprint_can_be_installed_replaced_and_removed() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let created = mutate(
            no_daemon_client(),
            &format!(
                r#"mutation {{ createBlueprint(name: "writer", manifest: "{}")
                     {{ name version digest source }} }}"#,
                manifest_text("writer", "1.0.0")
                    .replace('\n', "\\n")
                    .replace('"', "\\\"")
            ),
        )
        .await;
        assert!(created.errors.is_empty(), "{:?}", created.errors);
        let json = serde_json::to_value(&created.data).expect("data serializes");
        assert_eq!(json["createBlueprint"]["name"], "writer");
        assert_eq!(json["createBlueprint"]["version"], "1.0.0");
        // A blueprint read from the installed set is never a run's snapshot.
        assert_eq!(json["createBlueprint"]["source"], "INSTALLED");
        let first_digest = json["createBlueprint"]["digest"]
            .as_str()
            .expect("a digest")
            .to_string();

        let updated = mutate(
            no_daemon_client(),
            &format!(
                r#"mutation {{ updateBlueprint(name: "writer", manifest: "{}")
                     {{ version digest }} }}"#,
                manifest_text("writer", "2.0.0")
                    .replace('\n', "\\n")
                    .replace('"', "\\\"")
            ),
        )
        .await;
        assert!(updated.errors.is_empty(), "{:?}", updated.errors);
        let json = serde_json::to_value(&updated.data).expect("data serializes");
        assert_eq!(json["updateBlueprint"]["version"], "2.0.0");
        assert_ne!(
            json["updateBlueprint"]["digest"]
                .as_str()
                .unwrap_or_default(),
            first_digest,
            "different bytes, different identity"
        );

        let removed = mutate(
            no_daemon_client(),
            r#"mutation { deleteBlueprint(name: "writer") }"#,
        )
        .await;
        assert!(removed.errors.is_empty(), "{:?}", removed.errors);
    })
    .await;
}

/// The refusals: a name already taken, a name that is not installed, a manifest
/// that will not parse, and a name that could escape the agents directory.
#[tokio::test]
async fn the_blueprint_writes_refuse_what_they_should() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let manifest = manifest_text("taken", "1.0.0").replace('\n', "\\n").replace('"', "\\\"");
        let first = mutate(
            no_daemon_client(),
            &format!(r#"mutation {{ createBlueprint(name: "taken", manifest: "{manifest}") {{ name }} }}"#),
        )
        .await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);

        // Creating it again is a conflict: replacing somebody's agent is what
        // an edit is for.
        let again = mutate(
            no_daemon_client(),
            &format!(r#"mutation {{ createBlueprint(name: "taken", manifest: "{manifest}") {{ name }} }}"#),
        )
        .await;
        assert_eq!(
            again
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"CONFLICT\"".to_string())
        );

        // Editing one that is not installed is a miss, not a create.
        let missing = mutate(
            no_daemon_client(),
            &format!(r#"mutation {{ updateBlueprint(name: "ghost", manifest: "{manifest}") {{ name }} }}"#),
        )
        .await;
        assert_eq!(
            missing
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"NOT_FOUND\"".to_string())
        );

        let unparseable = mutate(
            no_daemon_client(),
            r#"mutation { createBlueprint(name: "broken", manifest: "not a manifest") { name } }"#,
        )
        .await;
        assert!(
            unparseable
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("Invalid manifest"),
            "{:?}",
            unparseable.errors
        );

        let traversing = mutate(
            no_daemon_client(),
            &format!(r#"mutation {{ createBlueprint(name: "../escape", manifest: "{manifest}") {{ name }} }}"#),
        )
        .await;
        assert!(
            traversing
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("Invalid blueprint name"),
            "{:?}",
            traversing.errors
        );

        let gone = mutate(
            no_daemon_client(),
            r#"mutation { deleteBlueprint(name: "ghost") }"#,
        )
        .await;
        assert!(
            gone.errors
                .first()
                .expect("a refusal")
                .message
                .contains("not found"),
            "{:?}",
            gone.errors
        );
    })
    .await;
}

/// Validation reports what it found. A manifest that will not install is a
/// report with the reasons, not a failed request.
#[tokio::test]
async fn validation_reports_rather_than_fails() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let good = manifest_text("checked", "1.0.0").replace('\n', "\\n").replace('"', "\\\"");
        let answer = mutate(
            no_daemon_client(),
            &format!(
                r#"mutation {{ validateBlueprint(manifest: "{good}") {{ valid errors warnings }} }}"#
            ),
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["validateBlueprint"]["valid"], true);
        assert_eq!(
            json["validateBlueprint"]["errors"].as_array().map(Vec::len),
            Some(0)
        );

        let bad = mutate(
            no_daemon_client(),
            r#"mutation { validateBlueprint(manifest: "not a manifest") { valid errors } }"#,
        )
        .await;
        assert!(bad.errors.is_empty(), "a finding is not a request failure");
        let json = serde_json::to_value(&bad.data).expect("data serializes");
        assert_eq!(json["validateBlueprint"]["valid"], false);
        assert!(
            !json["validateBlueprint"]["errors"]
                .as_array()
                .expect("errors")
                .is_empty(),
            "it says why"
        );
    })
    .await;
}
