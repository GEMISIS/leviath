//! Tests for the write side.
//!
//! Whole mutations run against the schema over an isolated runs directory and
//! a fake daemon, so what is asserted is what a client reads back: the run
//! after the act, or an error carrying the code to branch on.

use async_graphql::{EmptySubscription, Request, Schema};
use leviath_runtime::control_socket::{ControlClient, ControlResponse};

use super::{
    AnswerApprovalInput, AnswerChoiceInput, AnswerInteractionInput, AnswerTextInput, ApprovalScope,
    MetadataEntryInput, Mutation, RegionSeedInput, SpawnRunInput,
};
use crate::commands::serve::graphql::checks::YoloTestInput;
use crate::commands::serve::graphql::inputs::{BlueprintInput, RegionInput};
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

        for field in ["pauseRun", "resumeRun", "cancelRun"] {
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
            "mutation { pauseRun(runId: \"run-done\") { run { id } } }",
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
            "mutation { resumeRun(runId: \"ghost\") { run { id } } }",
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
            "mutation { cancelRun(runId: \"run-a\") { run { id } } }",
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
            "mutation { pauseRun(runId: \"run-gone\") { run { id } } }",
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
                r#"mutation { spawnRun(input: {
                     blueprint: { name: "coder" }, task: "write the thing", workdir: "/tmp",
                     metadata: [{ key: "ticket", value: "42" }]
                   }) { run { id task status metadata { key value } } warnings } }"#,
            ))
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        let run = &json["spawnRun"]["run"];
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

/// A spawn whose input arrives as a variable, which is how a client built in
/// code sends one.
///
/// Variable coercion reads the input object through its own path rather than out
/// of the query text, so an input that has only ever arrived inline is one whose
/// other half nobody has checked.
#[tokio::test]
async fn a_spawn_reads_its_input_from_a_variable() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-variable", |_d| async move {
        let agents = tempfile::tempdir().expect("a temp dir");
        let agent = agents.path().join("coder");
        std::fs::create_dir_all(&agent).expect("the agent dir");
        std::fs::write(
            agent.join(leviath_core::files::MANIFEST_FILENAME),
            "[agent]\nname = \"coder\"\n\n[context.regions.plan]\nkind = \"pinned\"\n\
             max_tokens = 100\n\n[stages.only]\nmode = \"autonomous\"\n",
        )
        .expect("manifest written");
        let (control, _socket, _srv) = fake_daemon(|req| match req {
            leviath_runtime::control_socket::ControlRequest::Spawn { args } => {
                let mut meta = run_in(&args.run_id, RunStatus::Starting);
                meta.task = args.task.clone();
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
            .execute(
                Request::new(
                    "mutation Start($input: SpawnRunInput!) { \
                       spawnRun(input: $input) { run { id task } } }",
                )
                .variables(async_graphql::Variables::from_json(
                    serde_json::json!({
                        "input": {
                            "blueprint": { "name": "coder" },
                            "task": "write it again",
                            "workdir": "/tmp",
                            "regions": [{ "region": { "name": "plan" }, "text": "start here" }],
                        }
                    }),
                )),
            )
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["spawnRun"]["run"]["task"], "write it again");
    })
    .await;
}

/// A spawn pinned to a revision that is not the installed one is a conflict,
/// and nothing is started.
///
/// The pin is what a client sends to start the blueprint it read rather than
/// whatever is on disk now, so it has to be checked before the daemon is asked
/// for anything - which is also why this needs no daemon at all.
#[tokio::test]
async fn a_spawn_pinned_to_another_revision_is_a_conflict() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-pinned", |_d| async move {
        let agents = tempfile::tempdir().expect("a temp dir");
        let agent = agents.path().join("coder");
        std::fs::create_dir_all(&agent).expect("the agent dir");
        std::fs::write(
            agent.join(leviath_core::files::MANIFEST_FILENAME),
            "[agent]\nname = \"coder\"\n\n[stages.only]\nmode = \"autonomous\"\n",
        )
        .expect("manifest written");
        let state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();

        let answer = schema
            .execute(Request::new(
                r#"mutation { spawnRun(input: {
                     blueprint: { name: "coder",
                       digest: "0000000000000000000000000000000000000000000000000000000000000000" },
                     task: "t"
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
            Some("\"CONFLICT\"".to_string()),
            "{}",
            error.message
        );
        assert!(error.message.contains("coder"), "{}", error.message);
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
                r#"mutation { spawnRun(input: {
                     blueprint: { name: "coder" }, task: "t", workdir: "/tmp", yolo: true
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
                r#"mutation { spawnRun(input: {
                     blueprint: { name: "coder" }, task: "t", maxDepth: -1
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

        // A spawn starts an installed blueprint, and a reference to one says
        // which name, not what the manifest says.
        let definition = schema
            .execute(Request::new(
                r#"mutation { spawnRun(input: {
                     blueprint: { name: "coder", content: "[agent]" }, task: "t"
                   }) { run { id } } }"#,
            ))
            .await;
        assert!(
            definition
                .errors
                .first()
                .expect("a refusal")
                .message
                .contains("unknown field \"content\""),
            "{:?}",
            definition.errors
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
                tool_arguments: Some(serde_json::json!({"command": "rm -rf build"})),
                required: true,
                stage_name: "build".to_string(),
                body: None,
                body_format: Default::default(),
            },
        )],
    });

    let answer = mutate(
        control,
        r#"{ openInteractions { runId request { id kind prompt stageName required
             toolCall { __typename toolName rawArguments
               ... on ShellCall { args { command } } } } } }"#,
    )
    .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    let inbox = &json["openInteractions"][0];
    assert_eq!(inbox["runId"], "run-a");
    assert_eq!(inbox["request"]["id"], "ask-1");
    assert_eq!(inbox["request"]["kind"], "TOOL_APPROVAL");
    assert_eq!(inbox["request"]["stageName"], "build");
    assert_eq!(inbox["request"]["required"], true);
    // The whole point of an approval is what it would run, so the ask carries
    // the call rather than only the tool's name.
    let call = &inbox["request"]["toolCall"];
    assert_eq!(call["__typename"], "ShellCall");
    assert_eq!(call["toolName"], "shell");
    assert_eq!(call["args"]["command"], "rm -rf build");
    assert_eq!(call["rawArguments"]["command"], "rm -rf build");
}

/// An approval that names a tool and no arguments comes through untyped.
///
/// A call with no arguments is not the same as a `shell` call with an empty
/// command, so it is not typed as one. What comes through says the tool is known
/// and the arguments do not fit it, which is the truth about that ask.
#[tokio::test]
async fn an_approval_with_no_arguments_says_the_arguments_do_not_fit() {
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Interactions {
        interactions: vec![(
            "run-a".to_string(),
            leviath_core::interaction::InteractionRequest {
                id: "ask-1".to_string(),
                kind: leviath_core::interaction::InteractionKind::ToolApproval,
                prompt: "Run it?".to_string(),
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
        r#"{ openInteractions { request { toolCall {
             __typename toolName rawArguments
             ... on UntypedToolCall { reason } } } } }"#,
    )
    .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    let call = &json["openInteractions"][0]["request"]["toolCall"];
    assert_eq!(call["__typename"], "UntypedToolCall");
    assert_eq!(call["toolName"], "shell");
    assert_eq!(call["reason"], "ARGUMENTS_DID_NOT_MATCH");
    assert_eq!(call["rawArguments"], serde_json::json!({}));
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

/// An export is started, polled, and handed over as a signed link.
///
/// One schema for both halves on purpose: the job lives in this server's own
/// registry, so starting it and polling it have to be the same server.
#[tokio::test]
async fn an_export_is_started_then_polled_for_its_link() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Complete)).expect("run written");
        create_run(&run_in("run-b", RunStatus::Running)).expect("run written");
        let state = state_with_agent_paths(Vec::new());
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();

        let started = schema
            .execute(Request::new(
                r#"mutation { bulkExportRuns(filter: { statusIn: [COMPLETE] },
                     fields: ["run_id", "status"]) { id status written downloadUrl } }"#,
            ))
            .await;
        assert!(started.errors.is_empty(), "{:?}", started.errors);
        let json = serde_json::to_value(&started.data).expect("data serializes");
        let id = json["bulkExportRuns"]["id"]
            .as_str()
            .expect("an id")
            .to_string();
        assert!(
            json["bulkExportRuns"]["downloadUrl"].is_null(),
            "nothing to fetch before it is written"
        );

        // Poll until the worker has finished: the mutation answers before the
        // file exists, which is the point of a job.
        let mut link = None;
        for _ in 0..200 {
            let polled = schema
                .execute(Request::new(format!(
                    "{{ bulkExport(id: \"{id}\") {{ status written error downloadUrl }} }}"
                )))
                .await;
            assert!(polled.errors.is_empty(), "{:?}", polled.errors);
            let json = serde_json::to_value(&polled.data).expect("data serializes");
            if json["bulkExport"]["status"] == "complete" {
                assert_eq!(json["bulkExport"]["written"], 1, "the filter was applied");
                assert!(json["bulkExport"]["error"].is_null());
                link = json["bulkExport"]["downloadUrl"]
                    .as_str()
                    .map(str::to_string);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let link = link.expect("the export finished with a link");
        assert!(link.starts_with(&format!("/api/exports/{id}?")), "{link}");
        assert!(link.contains("exp=") && link.contains("sig="), "{link}");
    })
    .await;
}

/// An id nobody started is null rather than an error: an export that expired
/// and one that never existed look the same, and both mean "ask again".
#[tokio::test]
async fn polling_an_unknown_export_answers_null() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export-unknown", |_d| async move {
        let answer = mutate(
            no_daemon_client(),
            "{ bulkExport(id: \"export-1-1\") { status } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert!(json["bulkExport"].is_null());
    })
    .await;
}

/// A field that no run carries is refused, and nothing is written.
#[tokio::test]
async fn an_export_of_an_unknown_field_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export-field", |_d| async move {
        let answer = mutate(
            no_daemon_client(),
            r#"mutation { bulkExportRuns(fields: ["run_id", "nope"]) { id } }"#,
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("nope"), "{}", error.message);
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string())
        );
    })
    .await;
}
/// A temporary agents directory holding one blueprint by that name.
///
/// A spawn checks the blueprint exists before it reaches the daemon, and with no
/// path configured that check reads the developer's own agents directory. A test
/// that passed only on a machine with `coder` installed is a test that says
/// nothing, so every spawn test brings its own.
fn agents_dir_with(name: &str) -> tempfile::TempDir {
    let agents = tempfile::tempdir().expect("a temp dir");
    let agent = agents.path().join(name);
    std::fs::create_dir_all(&agent).expect("the agent dir");
    std::fs::write(
        agent.join(leviath_core::files::MANIFEST_FILENAME),
        format!("[agent]\nname = \"{name}\"\n\n[stages.only]\nmode = \"autonomous\"\n"),
    )
    .expect("manifest written");
    agents
}

/// Run one mutation against a schema wired to `control` and an agents directory.
async fn mutate_with_agents(
    control: ControlClient,
    agents: &std::path::Path,
    query: &str,
) -> async_graphql::Response {
    let mut state = state_with_agent_paths(vec![agents.to_path_buf()]);
    state.control = control;
    let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
}

/// A spawn carries everything the request asked for down to the daemon.
///
/// The fake daemon answers yes and records what it was sent, so this asserts the
/// translation rather than the spawn: a field a client sets and the daemon never
/// sees is a field that silently does nothing.
#[tokio::test]
async fn a_spawn_carries_every_field_it_was_given() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-fields", |_d| async move {
        create_run(&run_in("coder-1", RunStatus::Running)).expect("run written");
        let agents = agents_dir_with("coder");
        let (control, _dir, _srv) = fake_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Spawn { args } => {
                // What a client asked for has to reach the daemon, so the
                // assertions are here rather than on the answer.
                assert!(
                    args.blueprint_path.contains("coder"),
                    "the blueprint it named: {}",
                    args.blueprint_path
                );
                assert_eq!(args.task, "fix the parser");
                assert_eq!(args.model.as_deref(), Some("gpt-5.6"));
                assert_eq!(args.workdir, "/work");
                assert_eq!(args.max_depth, Some(3));
                assert!(args.yolo, "the waiver travels");
                assert_eq!(args.yolo_profile.as_deref(), Some("cautious"));
                assert_eq!(
                    args.regions.get("plan").map(String::as_str),
                    Some("start here")
                );
                assert_eq!(args.metadata.get("ticket").map(String::as_str), Some("42"));
                ControlResponse::Spawned {
                    run_id: "coder-1".to_string(),
                }
            }
            other => panic!("the spawn is what reaches the daemon, not {other:?}"),
        });
        let answer = mutate_with_agents(
            control,
            agents.path(),
            r#"mutation { spawnRun(input: {
                 blueprint: { name: "coder" }, task: "fix the parser", model: "gpt-5.6",
                 workdir: "/work", maxDepth: 3, yolo: true, yoloProfile: "cautious",
                 outputFormat: "json", outputInstructions: "one object",
                 regions: [{ region: { name: "plan" }, text: "start here" }],
                 metadata: [{ key: "ticket", value: "42" }]
               }) { run { id } warnings } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["spawnRun"]["run"]["id"], "coder-1");
    })
    .await;
}

/// A spawn the daemon accepted whose record will not read is this server's
/// problem, and the message says so rather than blaming the caller.
#[tokio::test]
async fn a_record_that_will_not_read_after_a_spawn_is_internal() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-unread", |_d| async move {
        let agents = agents_dir_with("coder");
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "ghost".to_string(),
        });
        let answer = mutate_with_agents(
            control,
            agents.path(),
            r#"mutation { spawnRun(input: { blueprint: { name: "coder" }, task: "t" }) { run { id } } }"#,
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"INTERNAL\"".to_string()),
            "the daemon said yes, so the missing record is ours"
        );
        assert!(
            error.message.contains("would not read"),
            "{}",
            error.message
        );
    })
    .await;
}

/// Each approval scope is a different grant, and each reaches the daemon as
/// itself.
#[tokio::test]
async fn each_approval_scope_reaches_the_daemon() {
    for (word, expected) in [
        ("ONCE", leviath_core::interaction::ApprovalScope::Once),
        ("STAGE", leviath_core::interaction::ApprovalScope::Stage),
        ("SESSION", leviath_core::interaction::ApprovalScope::Run),
    ] {
        let (control, _dir, _srv) = fake_daemon(move |request| match request {
            leviath_runtime::control_socket::ControlRequest::AnswerInteraction { response } => {
                assert_eq!(
                    response.scope,
                    Some(expected),
                    "the scope travels as itself rather than as a default"
                );
                assert_eq!(response.approved, Some(true));
                ControlResponse::Ok { ok: true }
            }
            other => panic!("an answer, not {other:?}"),
        });
        let answer = mutate(
            control,
            &format!(
                r#"mutation {{ answerInteraction(input: {{ approval: {{
                     requestId: "r1", approved: true, scope: {word} }} }})
                     {{ accepted }} }}"#
            ),
        )
        .await;
        assert!(answer.errors.is_empty(), "{word}: {:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["answerInteraction"]["accepted"], true, "{word}");
    }
}

/// A cancel reaches the daemon as a cancel, and answers with the run.
#[tokio::test]
async fn a_cancel_reaches_the_daemon_as_a_cancel() {
    crate::runstate::with_isolated_runs_dir_async("graphql-cancel", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Running)).expect("run written");
        let (control, _dir, _srv) = fake_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Cancel { .. } => {
                ControlResponse::Ok { ok: true }
            }
            other => panic!("a cancel, not {other:?}"),
        });
        let answer = mutate(
            control,
            r#"mutation { cancelRun(runId: "run-a") { run { id status } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(json["cancelRun"]["run"]["id"], "run-a");
    })
    .await;
}

/// An export of everything is unpaged, and the page cap that governs a response
/// does not govern a file.
#[tokio::test]
async fn an_export_of_everything_is_not_a_page() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export-all", |_d| async move {
        for i in 0..3 {
            create_run(&run_in(&format!("run-{i}"), RunStatus::Complete)).expect("run written");
        }
        let state = state_with_agent_paths(Vec::new());
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state.clone())
            .finish();
        let started = schema
            .execute(Request::new("mutation { bulkExportRuns { id status } }"))
            .await;
        assert!(started.errors.is_empty(), "{:?}", started.errors);
        let json = serde_json::to_value(&started.data).expect("data serializes");
        let id = json["bulkExportRuns"]["id"].as_str().expect("an id");
        for _ in 0..200 {
            let job = state.caches.exports.get(id).expect("the job");
            if job.status == crate::commands::serve::core::export::ExportStatus::Complete {
                assert_eq!(job.written, 3, "every run, not one page of them");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the export did not finish");
    })
    .await;
}

/// A sweep by age takes the finished runs and leaves a live one out of it
/// entirely.
///
/// Not skipped: skipping is for a run somebody named. An age sweep is asking for
/// the old finished runs, and a run that is still going is not one of those, so
/// reporting it as a refusal would read as a failure where nothing failed.
#[tokio::test]
async fn a_delete_reports_both_halves() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-both", |_d| async move {
        let mut old = run_in("finished", RunStatus::Complete);
        old.updated_at = 100;
        create_run(&old).expect("run written");
        let mut live = run_in("running", RunStatus::Running);
        live.updated_at = 100;
        create_run(&live).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            "mutation { deleteRuns(before: 1000) { deleted skipped { id reason } } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = serde_json::to_value(&answer.data).expect("data serializes");
        assert_eq!(
            json["deleteRuns"]["deleted"],
            serde_json::json!(["finished"])
        );
        assert_eq!(
            json["deleteRuns"]["skipped"].as_array().map(Vec::len),
            Some(0),
            "the live run was never a candidate: {json}"
        );

        // Named rather than swept, the same run is a skip with its reason: that
        // is a request to delete it, and refusing one is worth saying.
        let named = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(ids: ["running"]) { deleted skipped { id reason } } }"#,
        )
        .await;
        assert!(named.errors.is_empty(), "{:?}", named.errors);
        let json = serde_json::to_value(&named.data).expect("data serializes");
        assert_eq!(json["deleteRuns"]["skipped"][0]["id"], "running");
        assert!(
            json["deleteRuns"]["skipped"][0]["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "a refusal says why"
        );
    })
    .await;
}

/// The refusals the export and the delete share with the listing.
///
/// Each is the listing's own rule reaching a caller that never asked for a page:
/// an export and a delete take the same filter, so they refuse the same shapes.
#[tokio::test]
async fn an_export_and_a_delete_refuse_what_the_listing_refuses() {
    crate::runstate::with_isolated_runs_dir_async("graphql-shared-refusals", |_d| async move {
        // A sort inside a combinator: the same shape the listing refuses,
        // refused here too rather than quietly ignored.
        let refused = mutate(
            no_daemon_client(),
            r#"mutation { bulkExportRuns(filter: { and: [{ sort: UPDATED_AT }] }) { id } }"#,
        )
        .await;
        assert_eq!(
            refused
                .errors
                .first()
                .expect("a refusal")
                .extensions
                .as_ref()
                .and_then(|e| e.get("code"))
                .map(ToString::to_string),
            Some("\"BAD_USER_INPUT\"".to_string())
        );

        // And more ids than one delete may name: the cap is the listing's, and
        // deleting is the act where exceeding it matters most.
        let many: Vec<String> = (0..300).map(|i| format!("\"run-{i}\"")).collect();
        let too_many = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ deleteRuns(ids: [{}]) {{ deleted }} }}",
                many.join(", ")
            ),
        )
        .await;
        let error = too_many.errors.first().expect("a refusal");
        assert!(
            error.message.contains("at most"),
            "it says what the cap is: {}",
            error.message
        );
    })
    .await;
}

/// Every input object round-trips through its own value form.
///
/// An input type is written for one direction and generated for both: the schema
/// reads one off the wire, and the executor writes one back when it reports a bad
/// value or fills a variable's default. A type whose two halves disagree would
/// report a rejected value as something the caller did not send.
#[test]
fn every_input_object_round_trips() {
    use async_graphql::InputType;

    let spawn = SpawnRunInput {
        blueprint: BlueprintInput {
            name: "coder".to_string(),
            digest: None,
        },
        task: "fix the parser".to_string(),
        model: Some("gpt-5.6".to_string()),
        max_depth: Some(3),
        workdir: Some("/work".to_string()),
        yolo: Some(true),
        yolo_profile: Some("cautious".to_string()),
        allow: Some(vec!["shell".to_string()]),
        no_seed_commands: Some(true),
        regions: Some(vec![RegionSeedInput {
            region: RegionInput {
                name: "plan".to_string(),
            },
            text: "start here".to_string(),
        }]),
        metadata: Some(vec![MetadataEntryInput {
            key: "ticket".to_string(),
            value: "42".to_string(),
        }]),
        output_format: Some("json".to_string()),
        output_instructions: Some("one object".to_string()),
        callback_url: Some("https://example.test/hook".to_string()),
        callback_secret: Some("shh".to_string()),
        capture_model_input: Some(true),
    };
    let Ok(read_back) = SpawnRunInput::parse(Some(spawn.to_value())) else {
        panic!("a spawn input reads back from its own value");
    };
    assert_eq!(read_back.blueprint.name, "coder");
    assert_eq!(read_back.max_depth, Some(3));
    assert_eq!(
        read_back.regions.as_ref().map(Vec::len),
        Some(1),
        "the nested inputs come with it"
    );
    assert_eq!(read_back.metadata.as_ref().map(Vec::len), Some(1));

    let answers = [
        AnswerInteractionInput::Choice(AnswerChoiceInput {
            request_id: "r1".to_string(),
            choice_index: 1,
        }),
        AnswerInteractionInput::Text(AnswerTextInput {
            request_id: "r2".to_string(),
            value: "the words".to_string(),
        }),
        AnswerInteractionInput::Approval(AnswerApprovalInput {
            request_id: "r3".to_string(),
            approved: false,
            scope: Some(ApprovalScope::Stage),
            feedback: Some("try the other file".to_string()),
        }),
    ];
    for answer in answers {
        let value = answer.to_value();
        let Ok(read_back) = AnswerInteractionInput::parse(Some(value)) else {
            panic!("an answer reads back from its own value");
        };
        assert_eq!(
            read_back.into_response().expect("a response").request_id,
            answer_id(&answer),
            "the same answer came back"
        );
    }

    let yolo = YoloTestInput {
        profile: "cautious".to_string(),
        tool: "shell".to_string(),
        command: Some("rm -r target".to_string()),
        arguments: Some(crate::commands::serve::graphql::scalars::Json(
            serde_json::json!({ "path": "." }),
        )),
        workdir: Some("/work".to_string()),
        configured: Some("ask".to_string()),
        kind: Some("builtin".to_string()),
        allowed: Some(true),
    };
    let Ok(read_back) = YoloTestInput::parse(Some(yolo.to_value())) else {
        panic!("a yolo test input reads back from its own value");
    };
    assert_eq!(read_back.tool, "shell");
    assert_eq!(read_back.command.as_deref(), Some("rm -r target"));
}

/// The request id one answer carries, whichever kind it is.
fn answer_id(answer: &AnswerInteractionInput) -> String {
    match answer {
        AnswerInteractionInput::Choice(choice) => choice.request_id.clone(),
        AnswerInteractionInput::Text(text) => text.request_id.clone(),
        AnswerInteractionInput::Approval(approval) => approval.request_id.clone(),
    }
}

/// Every input object in the schema refuses what it cannot read.
///
/// All of them together in one test, wherever their field lives: they are read
/// by the same derived code, and the question - does a wrong value fail the
/// request rather than land as a default - has one answer for all of them.
/// The one-of input is here too. It refuses a value that is not an object the
/// same way, on top of refusing two answers at once, which is the rule that
/// makes it a one-of.
#[test]
fn every_input_object_refuses_what_it_cannot_read() {
    use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

    /// One object with a single field set to `value`.
    fn one(field: &str, value: Value) -> Option<Value> {
        let mut map = IndexMap::new();
        map.insert(Name::new(field), value);
        Some(Value::Object(map))
    }
    let scalar = || Some(Value::String("nope".to_string()));
    let number = || Value::Number(7.into());
    /// A region argument, as the wire carries one.
    fn region_value(name: &str) -> Value {
        let mut map = IndexMap::new();
        map.insert(Name::new("name"), Value::String(name.to_string()));
        Value::Object(map)
    }
    /// A blueprint pointer, as the wire carries one.
    fn blueprint_value(name: &str) -> Value {
        let mut map = IndexMap::new();
        map.insert(Name::new("name"), Value::String(name.to_string()));
        Value::Object(map)
    }

    assert!(MetadataEntryInput::parse(scalar()).is_err());
    assert!(MetadataEntryInput::parse(None).is_err());
    assert!(MetadataEntryInput::parse(one("key", number())).is_err());
    assert!(RegionSeedInput::parse(scalar()).is_err());
    assert!(RegionSeedInput::parse(None).is_err());
    assert!(RegionSeedInput::parse(one("region", number())).is_err());
    assert!(SpawnRunInput::parse(scalar()).is_err());
    assert!(SpawnRunInput::parse(None).is_err());
    assert!(SpawnRunInput::parse(one("blueprint", number())).is_err());
    assert!(AnswerChoiceInput::parse(scalar()).is_err());
    assert!(AnswerChoiceInput::parse(None).is_err());
    assert!(AnswerChoiceInput::parse(one("requestId", number())).is_err());
    assert!(AnswerTextInput::parse(scalar()).is_err());
    assert!(AnswerTextInput::parse(None).is_err());
    assert!(AnswerTextInput::parse(one("requestId", number())).is_err());
    assert!(AnswerApprovalInput::parse(scalar()).is_err());
    assert!(AnswerApprovalInput::parse(None).is_err());
    assert!(AnswerApprovalInput::parse(one("requestId", number())).is_err());
    assert!(AnswerInteractionInput::parse(scalar()).is_err());
    assert!(AnswerInteractionInput::parse(None).is_err());
    assert!(
        AnswerInteractionInput::parse(one("text", number())).is_err(),
        "the chosen answer still has to be an answer"
    );
    assert!(YoloTestInput::parse(scalar()).is_err());
    assert!(YoloTestInput::parse(None).is_err());
    assert!(YoloTestInput::parse(one("profile", number())).is_err());

    // A first field that reads and a later one that does not. Each field is read
    // in turn, so a request that gets the last one wrong has to be refused just
    // as squarely as one that gets the first wrong.
    let text = |s: &str| Value::String(s.to_string());
    assert!(
        MetadataEntryInput::parse(one("key", text("k"))).is_err(),
        "no value"
    );
    assert!(
        RegionSeedInput::parse(one("region", region_value("plan"))).is_err(),
        "no text"
    );
    assert!(
        SpawnRunInput::parse(one("blueprint", blueprint_value("coder"))).is_err(),
        "no task"
    );
    assert!(
        SpawnRunInput::parse(one("task", text("t"))).is_err(),
        "no blueprint"
    );
    assert!(
        AnswerChoiceInput::parse(one("requestId", text("a"))).is_err(),
        "no choice"
    );
    assert!(
        AnswerTextInput::parse(one("requestId", text("a"))).is_err(),
        "no answer"
    );
    assert!(
        AnswerApprovalInput::parse(one("requestId", text("a"))).is_err(),
        "no verdict"
    );
    assert!(
        YoloTestInput::parse(one("profile", text("cautious"))).is_err(),
        "no tool"
    );
    // The one-of reads the answer it was given, so a broken answer inside it is
    // a broken request, whichever of the three it names.
    assert!(
        AnswerInteractionInput::parse(one("text", Value::Object(IndexMap::new()))).is_err(),
        "an answer with nothing in it"
    );
    assert!(
        AnswerInteractionInput::parse(one("approval", Value::Object(IndexMap::new()))).is_err(),
        "an approval with nothing in it"
    );
    assert!(
        AnswerInteractionInput::parse(one("choice", Value::Object(IndexMap::new()))).is_err(),
        "a choice with nothing in it"
    );

    // And the last field of each, which is read after every other one.
    let mut spawn = IndexMap::new();
    spawn.insert(Name::new("blueprint"), blueprint_value("coder"));
    spawn.insert(Name::new("task"), text("fix it"));
    spawn.insert(Name::new("callbackSecret"), number());
    assert!(SpawnRunInput::parse(Some(Value::Object(spawn))).is_err());

    let mut approval = IndexMap::new();
    approval.insert(Name::new("requestId"), text("a"));
    approval.insert(Name::new("approved"), Value::Boolean(true));
    approval.insert(Name::new("feedback"), number());
    assert!(AnswerApprovalInput::parse(Some(Value::Object(approval))).is_err());

    let mut yolo = IndexMap::new();
    yolo.insert(Name::new("profile"), text("cautious"));
    yolo.insert(Name::new("tool"), text("shell"));
    yolo.insert(Name::new("allowed"), number());
    assert!(YoloTestInput::parse(Some(Value::Object(yolo))).is_err());
}

/// A spawn input read as a field of another input object.
///
/// An input object on a field is read by the field's own type, which hands the
/// value to the object's reader. That is the path a client takes when it sends a
/// spawn inside a larger document, and it is a different path from an argument
/// read straight off the query, so it is worth its own assertion.
#[test]
fn a_spawn_input_reads_as_a_field_of_another_input() {
    use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

    /// One input object with a spawn input on a field.
    #[derive(async_graphql::InputObject)]
    struct SpawnProbe {
        /// The spawn being carried.
        input: SpawnRunInput,
    }

    let spawn = SpawnRunInput {
        blueprint: BlueprintInput {
            name: "coder".to_string(),
            digest: None,
        },
        task: "fix the parser".to_string(),
        model: None,
        max_depth: None,
        workdir: None,
        yolo: None,
        yolo_profile: None,
        allow: None,
        no_seed_commands: None,
        regions: None,
        metadata: None,
        output_format: None,
        output_instructions: None,
        callback_url: None,
        callback_secret: None,
        capture_model_input: None,
    };
    let mut carried = IndexMap::new();
    carried.insert(Name::new("input"), spawn.to_value());
    let Ok(probe) = SpawnProbe::parse(Some(Value::Object(carried))) else {
        panic!("a spawn input reads back as a carried field");
    };
    assert_eq!(probe.input.task, "fix the parser");
    assert_eq!(probe.input.blueprint.name, "coder");

    // And a carried value the reader refuses is refused through the field too.
    let mut broken = IndexMap::new();
    broken.insert(Name::new("input"), Value::String("coder".to_string()));
    assert!(SpawnProbe::parse(Some(Value::Object(broken))).is_err());
}

// ── waiting for an act to show in the record ──

/// Each act has its own idea of what landing looks like. A resume is the odd
/// one: what the run goes back to doing is its own business, so the only thing
/// the resume promises is that it is no longer parked.
#[test]
fn each_act_knows_what_landing_looks_like() {
    use super::has_landed;
    use crate::commands::serve::core::lifecycle::Action;
    assert!(has_landed(Action::Pause, &RunStatus::Paused));
    assert!(!has_landed(Action::Pause, &RunStatus::Running));
    assert!(has_landed(Action::Cancel, &RunStatus::Cancelled));
    assert!(!has_landed(Action::Cancel, &RunStatus::Running));
    assert!(has_landed(Action::Resume, &RunStatus::Running));
    assert!(has_landed(Action::Resume, &RunStatus::WaitingInput));
    assert!(!has_landed(Action::Resume, &RunStatus::Paused));
}

/// A record that already shows the act is answered on the first look, with no
/// waiting at all - which is what stops a pause on an already-paused run from
/// sitting out the whole window.
#[tokio::test]
async fn a_record_that_already_shows_the_act_is_answered_at_once() {
    use crate::commands::serve::core::lifecycle::Action;
    let looks = std::cell::Cell::new(0);
    let settled = super::settle(
        Action::Pause,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
        || {
            looks.set(looks.get() + 1);
            Ok(run_in("run-a", RunStatus::Paused))
        },
    )
    .await
    .expect("the record reads");
    assert_eq!(settled.status, RunStatus::Paused);
    assert_eq!(looks.get(), 1, "one look, no waiting");
}

/// The record catches up a moment later, which is the case the window exists
/// for: the first look still shows the status the run held when it was asked.
#[tokio::test]
async fn a_record_that_catches_up_is_waited_for() {
    use crate::commands::serve::core::lifecycle::Action;
    let looks = std::cell::Cell::new(0);
    let settled = super::settle(
        Action::Cancel,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
        || {
            looks.set(looks.get() + 1);
            Ok(match looks.get() {
                1 => run_in("run-a", RunStatus::Running),
                _ => run_in("run-a", RunStatus::Cancelled),
            })
        },
    )
    .await
    .expect("the record reads");
    assert_eq!(settled.status, RunStatus::Cancelled);
    assert_eq!(looks.get(), 2, "looked again once the first was stale");
}

/// A window that closes before the act shows answers with the record as it
/// stands rather than failing: the act was accepted, and the caller is told
/// what is there.
#[tokio::test]
async fn a_window_that_closes_answers_with_what_is_there() {
    use crate::commands::serve::core::lifecycle::Action;
    let settled = super::settle(
        Action::Pause,
        std::time::Instant::now() - std::time::Duration::from_millis(1),
        || Ok(run_in("run-a", RunStatus::Running)),
    )
    .await
    .expect("the record reads");
    assert_eq!(settled.status, RunStatus::Running);
}

/// A record that will not read is this server's problem, and it says so instead
/// of answering with a run it did not read.
#[tokio::test]
async fn a_record_that_will_not_read_is_reported() {
    use crate::commands::serve::core::error::ServeError;
    use crate::commands::serve::core::lifecycle::Action;
    let failure = super::settle(
        Action::Pause,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
        || {
            Err(ServeError::Internal(
                "the record would not read".to_string(),
            ))
        },
    )
    .await
    .expect_err("the read failed");
    assert_eq!(failure.code(), "INTERNAL");
}
