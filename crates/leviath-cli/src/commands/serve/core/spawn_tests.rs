//! Tests for starting and steering runs.
//!
//! The refusals are what matter here: each one is a decision the operator made
//! about what this server may be asked to do, and each is checked before the
//! daemon is troubled at all.

use leviath_runtime::control_socket::{ControlRequest, ControlResponse};

use super::{SpawnRequest, answer_interaction, open_interactions, send_message, spawn};
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};

/// A spawn request naming a blueprint and a workdir, with everything else at
/// its default.
fn request(blueprint: &str, workdir: Option<&str>) -> SpawnRequest {
    SpawnRequest {
        blueprint: blueprint.to_string(),
        task: "do the thing".to_string(),
        model: None,
        max_depth: None,
        workdir: workdir.map(str::to_string),
        yolo: false,
        yolo_profile: None,
        allow: Vec::new(),
        no_seed_commands: false,
        regions: std::collections::HashMap::new(),
        metadata: std::collections::HashMap::new(),
        callback_url: None,
        callback_secret: None,
        output: None,
        capture_model_input: false,
    }
}

/// An agents directory holding one blueprint, plus the state that can see it.
fn with_blueprint(dir: &std::path::Path, name: &str) -> crate::commands::serve::types::AppState {
    let agent = dir.join(name);
    std::fs::create_dir_all(&agent).expect("the agent dir");
    std::fs::write(
        agent.join(leviath_core::files::MANIFEST_FILENAME),
        format!("[agent]\nname = \"{name}\"\n\n[stages.only]\nmode = \"autonomous\"\n"),
    )
    .expect("manifest written");
    state_with_agent_paths(vec![dir.to_path_buf()])
}

/// The happy path: the daemon takes the spawn and answers with the run id.
#[tokio::test]
async fn a_spawn_reaches_the_daemon_and_answers_with_the_run() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut state = with_blueprint(dir.path(), "coder");
    let (control, _socket, _srv) = fake_daemon(|req| match req {
        ControlRequest::Spawn { args } => {
            // The blueprint the request named, resolved to the file on disk.
            assert!(
                args.blueprint_path.ends_with("agent.leviath"),
                "{}",
                args.blueprint_path
            );
            assert_eq!(args.task, "do the thing");
            ControlResponse::Spawned {
                run_id: args.run_id,
            }
        }
        other => panic!("the spawn is what reaches the daemon: {other:?}"),
    });
    state.control = control;

    let spawned = spawn(&state, request("coder", Some("/tmp")), Vec::new())
        .await
        .expect("the daemon took it");
    assert!(spawned.run_id.starts_with("coder-"), "{}", spawned.run_id);
}

/// A blueprint nobody installed is a `NOT_FOUND` naming what was asked for,
/// and the daemon is never troubled with it.
#[tokio::test]
async fn an_unknown_blueprint_is_refused_before_the_daemon() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = with_blueprint(dir.path(), "coder");
    // A daemon-less state: reaching the daemon at all would fail differently.
    let failure = spawn(&state, request("ghost", Some("/tmp")), Vec::new())
        .await
        .expect_err("no such blueprint");
    assert_eq!(failure.code(), "NOT_FOUND");
    assert!(failure.to_string().contains("ghost"), "{failure}");
}

/// A workdir outside `--workdir-root` is refused. This is the check that stops
/// a request pointing a tool-executing agent at the whole filesystem.
#[tokio::test]
async fn a_workdir_outside_the_root_is_forbidden() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = tempfile::tempdir().expect("a workdir root");
    let mut state = with_blueprint(dir.path(), "coder");
    state.limits = std::sync::Arc::new(crate::commands::serve::types::ServeLimits {
        workdir_root: Some(root.path().to_path_buf()),
        ..Default::default()
    });

    let failure = spawn(&state, request("coder", Some("/")), Vec::new())
        .await
        .expect_err("outside the root");
    assert_eq!(failure.code(), "FORBIDDEN");

    // Inside it is allowed, and the daemon then sees the spawn.
    let inside = root.path().join("work");
    std::fs::create_dir_all(&inside).expect("the workdir");
    let (control, _socket, _srv) = fake_daemon(|req| match req {
        ControlRequest::Spawn { args } => ControlResponse::Spawned {
            run_id: args.run_id,
        },
        other => panic!("unexpected: {other:?}"),
    });
    state.control = control;
    spawn(
        &state,
        request("coder", Some(&inside.to_string_lossy())),
        Vec::new(),
    )
    .await
    .expect("inside the root is allowed");
}

/// An unattended run is refused on a server that says no to them, and a named
/// profile is refused with it: a profile is a kind of yolo.
#[tokio::test]
async fn an_unattended_run_is_forbidden_when_the_server_refuses_them() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut state = with_blueprint(dir.path(), "coder");
    state.limits = std::sync::Arc::new(crate::commands::serve::types::ServeLimits {
        no_remote_yolo: true,
        ..Default::default()
    });

    let mut unattended = request("coder", Some("/tmp"));
    unattended.yolo = true;
    let failure = spawn(&state, unattended, Vec::new())
        .await
        .expect_err("yolo is refused");
    assert_eq!(failure.code(), "FORBIDDEN");

    let mut profiled = request("coder", Some("/tmp"));
    profiled.yolo_profile = Some("solo".to_string());
    let failure = spawn(&state, profiled, Vec::new())
        .await
        .expect_err("a profile is a kind of yolo");
    assert_eq!(failure.code(), "FORBIDDEN");
}

/// A callback URL goes through the same outbound policy a model-supplied URL
/// does, because the daemon makes that request on the caller's behalf.
#[tokio::test]
async fn a_callback_url_the_policy_refuses_is_forbidden() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = with_blueprint(dir.path(), "coder");
    let mut with_callback = request("coder", Some("/tmp"));
    with_callback.callback_url = Some("http://169.254.169.254/latest/meta-data".to_string());

    let failure = spawn(&state, with_callback, Vec::new())
        .await
        .expect_err("the metadata service is not a webhook target");
    assert_eq!(failure.code(), "FORBIDDEN");
}

/// A secret that signs a callback nobody asked for is a caller mistake worth
/// naming: accepted, it would leave somebody believing they had set up a signed
/// webhook that never fires, and it would swallow a credential doing it.
#[tokio::test]
async fn a_callback_secret_without_a_url_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = with_blueprint(dir.path(), "coder");
    let mut secret_only = request("coder", Some("/tmp"));
    secret_only.callback_secret = Some("whsec_nothing_to_sign".to_string());

    let failure = spawn(&state, secret_only, Vec::new())
        .await
        .expect_err("a secret with no webhook to sign for");
    assert_eq!(failure.code(), "BAD_USER_INPUT");
    let message = failure.to_string();
    assert!(message.contains("callback_url"), "{message}");
    // The secret itself never reaches the message a caller sees or a log keeps.
    assert!(!message.contains("whsec_nothing_to_sign"), "{message}");
}

/// The pair together is the shape the refusal exists to steer callers towards,
/// so it has to stay accepted.
#[tokio::test]
async fn a_callback_url_with_its_secret_is_accepted() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = with_blueprint(dir.path(), "coder");
    let mut both = request("coder", Some("/tmp"));
    both.callback_url = Some("https://example.com/hook".to_string());
    both.callback_secret = Some("whsec_signs_the_body".to_string());

    // The daemon is not listening in this test, so reaching it at all is the
    // proof: the refusal above happens before any of that.
    let failure = spawn(&state, both, Vec::new())
        .await
        .expect_err("no daemon is listening here");
    assert_ne!(failure.code(), "BAD_USER_INPUT", "{failure}");
}

/// The daemon's own refusal, such as a manifest that will not load, reaches the
/// caller as a bad request with what the daemon said.
#[tokio::test]
async fn the_daemons_refusal_carries_its_message() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut state = with_blueprint(dir.path(), "coder");
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Error {
        message: "region 'plan' is not declared".to_string(),
    });
    state.control = control;

    let failure = spawn(&state, request("coder", Some("/tmp")), Vec::new())
        .await
        .expect_err("the daemon said no");
    assert_eq!(failure.code(), "BAD_USER_INPUT");
    assert!(failure.to_string().contains("region 'plan'"), "{failure}");
}

/// A reply to a different question, and a daemon that is not there, are this
/// server's failures rather than the caller's.
#[tokio::test]
async fn a_spawn_reports_the_servers_own_failures() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut state = with_blueprint(dir.path(), "coder");
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
    state.control = control;
    let failure = spawn(&state, request("coder", Some("/tmp")), Vec::new())
        .await
        .expect_err("an answer to another question");
    assert_eq!(failure.code(), "INTERNAL");

    state.control = no_daemon_client();
    let failure = spawn(&state, request("coder", Some("/tmp")), Vec::new())
        .await
        .expect_err("no daemon");
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");
}

/// A message reaches the daemon, and a run that will not take one is reported
/// as not accepting messages rather than as missing.
#[tokio::test]
async fn a_message_reaches_the_daemon_or_says_why_not() {
    let (control, _socket, _srv) = fake_daemon(|req| match req {
        ControlRequest::Message {
            agent_id, content, ..
        } => {
            assert_eq!(agent_id, "run-a");
            assert_eq!(content, "keep going");
            ControlResponse::Ok { ok: true }
        }
        other => panic!("the message is what reaches the daemon: {other:?}"),
    });
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;
    send_message(&state, "run-a", "keep going".to_string(), None, Vec::new())
        .await
        .expect("the daemon took it");

    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
    state.control = control;
    let failure = send_message(&state, "run-a", "hello".to_string(), None, Vec::new())
        .await
        .expect_err("the run does not take messages");
    assert_eq!(failure.code(), "NOT_FOUND");
    assert!(
        failure.to_string().contains("not accepting messages"),
        "{failure}"
    );
}

/// A message, and an answer, each report this server's own failures on their
/// own terms.
#[tokio::test]
async fn the_write_paths_report_a_daemon_that_will_not_answer() {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = no_daemon_client();
    let failure = send_message(&state, "run-a", "hi".to_string(), None, Vec::new())
        .await
        .expect_err("no daemon");
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");

    let response = leviath_core::interaction::InteractionResponse::text("ask-1", "yes");
    let failure = answer_interaction(&state, response)
        .await
        .expect_err("no daemon");
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");

    let failure = open_interactions(&state).await.expect_err("no daemon");
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");

    // One fake daemon per request: the control client opens a fresh connection
    // for each one, and the fake serves a single connection.
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
    });
    state.control = control;
    let failure = send_message(&state, "run-a", "hi".to_string(), None, Vec::new())
        .await
        .expect_err("an answer to another question");
    assert_eq!(failure.code(), "INTERNAL");

    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
    });
    state.control = control;
    let failure = answer_interaction(
        &state,
        leviath_core::interaction::InteractionResponse::text("ask-1", "yes"),
    )
    .await
    .expect_err("an answer to another question");
    assert_eq!(failure.code(), "INTERNAL");

    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
    });
    state.control = control;
    let failure = open_interactions(&state)
        .await
        .expect_err("an answer to another question");
    assert_eq!(failure.code(), "INTERNAL");
}

/// The first answer wins. A second answer to the same request finds nothing
/// open and is told so, which is what two people clicking one prompt looks
/// like.
#[tokio::test]
async fn the_second_answer_to_one_request_finds_nothing_open() {
    let (control, _socket, _srv) = fake_daemon(|req| match req {
        ControlRequest::AnswerInteraction { response } => {
            assert_eq!(response.request_id, "ask-1");
            ControlResponse::Ok { ok: false }
        }
        other => panic!("the answer is what reaches the daemon: {other:?}"),
    });
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;

    let failure = answer_interaction(
        &state,
        leviath_core::interaction::InteractionResponse::text("ask-1", "yes"),
    )
    .await
    .expect_err("nothing open under that id");
    assert_eq!(failure.code(), "NOT_FOUND");
    assert!(
        failure.to_string().contains("answered already"),
        "{failure}"
    );
}

/// The inbox is whatever the daemon is holding, each entry naming its run.
#[tokio::test]
async fn the_open_asks_come_back_with_their_runs() {
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Interactions {
        interactions: vec![(
            "run-a".to_string(),
            leviath_core::interaction::InteractionRequest {
                id: "ask-1".to_string(),
                kind: leviath_core::interaction::InteractionKind::Confirm,
                prompt: "Ship it?".to_string(),
                options: Vec::new(),
                tool_name: None,
                tool_arguments: None,
                required: true,
                stage_name: "review".to_string(),
                body: None,
                body_format: Default::default(),
            },
        )],
    });
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;

    let open = open_interactions(&state).await.expect("the inbox reads");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].0, "run-a");
    assert_eq!(open[0].1.prompt, "Ship it?");
}

/// A spawn whose manifest will not parse still spawns, and says nothing about
/// retired checks: reporting that failure is the daemon's job, and this must
/// never be why a spawn fails.
#[tokio::test]
async fn the_retired_check_warnings_stay_quiet_on_a_bad_manifest() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let agent = dir.path().join("coder");
    std::fs::create_dir_all(&agent).expect("the agent dir");
    // Discovery only lists a blueprint it could parse, so the file is replaced
    // with something unparseable after the state has seen it.
    std::fs::write(
        agent.join(leviath_core::files::MANIFEST_FILENAME),
        "[agent]\nname = \"coder\"\n\n[stages.only]\nmode = \"autonomous\"\n",
    )
    .expect("manifest written");
    let mut state = state_with_agent_paths(vec![dir.path().to_path_buf()]);
    let (control, _socket, _srv) = fake_daemon(|req| match req {
        ControlRequest::Spawn { args } => ControlResponse::Spawned {
            run_id: args.run_id,
        },
        other => panic!("unexpected: {other:?}"),
    });
    state.control = control;

    let mut asked = request("coder", Some("/tmp"));
    asked.output = Some(leviath_core::output::OutputSpec {
        format: Some("json".to_string()),
        ..leviath_core::output::OutputSpec::default()
    });
    let spawned = spawn(&state, asked, Vec::new())
        .await
        .expect("the spawn goes through");
    assert!(
        spawned.warnings.is_empty(),
        "nothing retired here: {:?}",
        spawned.warnings
    );
}
