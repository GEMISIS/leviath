//! Tests for starting and steering runs.
//!
//! The refusals are what matter here: each one is a decision the operator made
//! about what this server may be asked to do, and each is said beside every
//! problem the daemon finds with the same request.

use std::sync::{Arc, Mutex};

use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpecPath};
use leviath_runtime::spec::launch::{Callback, Unattended};
use leviath_runtime::spec::names::{BlueprintPath, BlueprintRef, HttpUrl, ToolName};
use leviath_runtime::spec::request::{SpawnRequest as Request, SpawnSource};
use leviath_runtime::spec::summary::SpawnSummary;

use super::{Verdict, answer_interaction, open_interactions, send_message, start, validate};
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};
use crate::commands::serve::types::{AppState, ServeLimits};

/// A request for the installed blueprint `name`.
fn named(name: &str) -> Request {
    Request::new(SpawnSource::Blueprint(BlueprintRef::parse(name).unwrap()))
}

/// A state reaching `control`, under `limits`.
fn state(control: leviath_runtime::control_socket::ControlClient, limits: ServeLimits) -> AppState {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;
    state.limits = Arc::new(limits);
    state
}

/// The limits of a hardened server: a workdir root, no unattended runs, no
/// seed commands, no webhooks at local addresses.
fn hardened(root: &std::path::Path) -> ServeLimits {
    ServeLimits {
        workdir_root: Some(root.to_path_buf()),
        no_remote_yolo: true,
        no_remote_seed_commands: true,
        ..ServeLimits::default()
    }
}

/// One issue the daemon would find.
fn daemon_issue() -> SpawnIssue {
    SpawnIssue::new(
        SpecPath::root().field("inputs").key("task"),
        IssueCode::Missing,
        "the blueprint needs a task",
    )
}

/// A fake daemon that records the request it was sent and answers `reply`.
fn recording(
    reply: ControlResponse,
) -> (
    leviath_runtime::control_socket::ControlClient,
    Arc<Mutex<Option<ControlRequest>>>,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let seen = Arc::new(Mutex::new(None));
    let held = seen.clone();
    let (client, dir, task) = fake_daemon(move |req| {
        *held.lock().unwrap() = Some(req);
        reply.clone()
    });
    (client, seen, dir, task)
}

/// The spawn request a recorded control request carried.
fn carried(seen: &Arc<Mutex<Option<ControlRequest>>>) -> Request {
    match seen.lock().unwrap().take() {
        Some(ControlRequest::Spawn { request })
        | Some(ControlRequest::ValidateSpawn { request }) => *request,
        other => panic!("not a spawn: {other:?}"),
    }
}

#[tokio::test]
async fn a_spawn_reaches_the_daemon_with_this_servers_defaults() {
    let (client, seen, _dir, _task) = recording(ControlResponse::Spawned {
        run_id: "run-1".into(),
        warnings: Default::default(),
    });
    let limits = ServeLimits {
        no_remote_seed_commands: true,
        ..ServeLimits::default()
    };
    let started = start(&state(client, limits), named("coder")).await.unwrap();
    assert!(matches!(started, Verdict::Accepted(ref s) if s.run_id == "run-1"));
    let sent = carried(&seen);
    assert_eq!(sent.workdir, Some(std::env::current_dir().unwrap()));
    assert!(
        !sent.launch.seed_commands,
        "the server refuses seed commands"
    );
}

#[tokio::test]
async fn a_daemons_refusal_or_failure_is_handed_back() {
    let rejected = ControlResponse::Rejected {
        issues: daemon_issue().into(),
    };
    let (client, _seen, _dir, _task) = recording(rejected);
    let answer = start(&state(client, ServeLimits::default()), named("coder"))
        .await
        .unwrap();
    assert!(matches!(answer, Verdict::Rejected(ref issues) if issues.len() == 1));

    let (client, _seen, _dir, _task) = recording(ControlResponse::Error {
        message: "shutting down".into(),
    });
    let failure = start(&state(client, ServeLimits::default()), named("coder"))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");

    let (client, _seen, _dir, _task) = recording(ControlResponse::Ok { ok: true });
    let failure = start(&state(client, ServeLimits::default()), named("coder"))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "INTERNAL");

    let failure = start(
        &state(no_daemon_client(), ServeLimits::default()),
        named("c"),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");
}

/// Four refusals of this server's and one of the daemon's, in one answer.
#[tokio::test]
async fn every_refusal_comes_back_at_once_with_the_daemons_own() {
    let root = tempfile::tempdir().unwrap();
    let (client, seen, _dir, _task) = recording(ControlResponse::Rejected {
        issues: daemon_issue().into(),
    });
    let mut request = named("coder");
    request.workdir = Some("/".into());
    request.launch.unattended = Unattended::All;
    request.launch.allow = vec![ToolName::new("bash").unwrap()];
    request.delivery.callback = Some(Callback {
        url: HttpUrl::new("http://169.254.169.254/latest").unwrap(),
        secret: None,
    });
    let answer = start(&state(client, hardened(root.path())), request)
        .await
        .unwrap();
    let Verdict::Rejected(issues) = answer else {
        panic!("refused");
    };
    let at: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(
        at,
        vec![
            "workdir",
            "launch.unattended",
            "launch.allow",
            "delivery.callback.url",
            "inputs.task",
        ]
    );
    // The daemon was asked to check, not to start.
    assert!(matches!(
        seen.lock().unwrap().as_ref(),
        Some(ControlRequest::ValidateSpawn { .. })
    ));
}

/// A refused request that the daemon otherwise likes is refused for this
/// server's reasons alone; a daemon that cannot answer is said so.
#[tokio::test]
async fn a_refusal_the_daemon_has_nothing_to_add_to_stands_alone() {
    let root = tempfile::tempdir().unwrap();
    let summary = ControlResponse::Valid {
        summary: Box::new(summary()),
    };
    let mut request = named("coder");
    request.workdir = Some("/".into());
    let (client, _seen, _dir, _task) = recording(summary);
    let answer = validate(&state(client, hardened(root.path())), request.clone())
        .await
        .unwrap();
    assert!(matches!(answer, Verdict::Rejected(ref issues) if issues.len() == 1));

    let (client, _seen, _dir, _task) = recording(ControlResponse::Error {
        message: "gone".into(),
    });
    let failure = validate(&state(client, hardened(root.path())), request.clone())
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");

    let failure = start(&state(no_daemon_client(), hardened(root.path())), request)
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");
}

/// A request that names no workdir works where the server started, which is
/// held to `--workdir-root` like any other, and the refusal says where the
/// directory came from rather than naming a path the caller never sent.
#[tokio::test]
async fn no_workdir_under_a_root_is_refused_saying_where_it_came_from() {
    let root = tempfile::tempdir().unwrap();
    let summary = ControlResponse::Valid {
        summary: Box::new(summary()),
    };
    let (client, _seen, _dir, _task) = recording(summary.clone());
    let limits = ServeLimits {
        workdir_root: Some(root.path().to_path_buf()),
        ..ServeLimits::default()
    };
    let answer = validate(&state(client, limits.clone()), named("coder"))
        .await
        .unwrap();
    let Verdict::Rejected(issues) = answer else {
        panic!("the server's own directory is outside the root")
    };
    assert_eq!(issues.0[0].code, IssueCode::NotAllowed);
    assert!(
        issues.0[0]
            .message
            .starts_with("the request names no workdir"),
        "{issues}"
    );
    // A workdir the caller sent is named as theirs.
    let mut request = named("coder");
    request.workdir = Some("/".into());
    let (client, _seen, _dir, _task) = recording(summary);
    let Verdict::Rejected(issues) = validate(&state(client, limits), request).await.unwrap() else {
        panic!("/ is outside the root")
    };
    assert!(issues.0[0].message.starts_with("workdir '/'"), "{issues}");
}

/// A blueprint read from a directory is refused over the network, and the
/// daemon is not asked to read it for its other issues.
#[tokio::test]
async fn a_blueprint_named_by_its_directory_is_refused_unread() {
    let root = tempfile::tempdir().unwrap();
    let mut request = Request::new(SpawnSource::BlueprintFile(
        BlueprintPath::new(root.path().to_string_lossy()).unwrap(),
    ));
    request.workdir = Some("/".into());
    let answer = start(&state(no_daemon_client(), hardened(root.path())), request)
        .await
        .unwrap();
    let Verdict::Rejected(issues) = answer else {
        panic!("refused");
    };
    let at: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(at, vec!["workdir", "source.blueprint_file"]);
}

/// What the daemon says a request would be.
fn summary() -> SpawnSummary {
    crate::runstate::with_isolated_runs_dir("spawn-summary", |_| {
        let run_id = super::super::run_file::tests::recorded();
        SpawnSummary::of(super::super::run_file::require(&run_id).unwrap().spec())
    })
}

#[tokio::test]
async fn a_dry_run_answers_with_the_run_or_every_reason_against_it() {
    let (client, seen, _dir, _task) = recording(ControlResponse::Valid {
        summary: Box::new(summary()),
    });
    let answer = validate(&state(client, ServeLimits::default()), named("coder"))
        .await
        .unwrap();
    assert!(matches!(answer, Verdict::Accepted(ref s) if !s.stages.is_empty()));
    assert!(carried(&seen).workdir.is_some());

    let (client, _seen, _dir, _task) = recording(ControlResponse::Rejected {
        issues: daemon_issue().into(),
    });
    let answer = validate(&state(client, ServeLimits::default()), named("coder"))
        .await
        .unwrap();
    assert!(matches!(answer, Verdict::Rejected(_)));

    let (client, _seen, _dir, _task) = recording(ControlResponse::Spawned {
        run_id: "x".into(),
        warnings: Default::default(),
    });
    let failure = validate(&state(client, ServeLimits::default()), named("coder"))
        .await
        .unwrap_err();
    assert_eq!(failure.code(), "INTERNAL");

    let failure = validate(
        &state(no_daemon_client(), ServeLimits::default()),
        named("c"),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code(), "DAEMON_UNAVAILABLE");
}

/// A blueprint this server lists from a configured directory, rather than the
/// installed ones, is sent as that directory. One pinned to a revision, or
/// one that is installed, is sent by name.
#[tokio::test]
async fn a_blueprint_from_a_configured_directory_is_sent_as_that_directory() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let elsewhere = home.path().join("elsewhere");
    for (root, name) in [(&agents, "inside"), (&elsewhere, "outside")] {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        crate::test_support::write_test_agent(&dir, crate::test_support::tiny_blueprint(name));
    }
    let sent_as = |name: &'static str| {
        let agents = agents.clone();
        let elsewhere = elsewhere.clone();
        async move {
            let (client, seen, _dir, _task) = recording(ControlResponse::Spawned {
                run_id: "r".into(),
                warnings: Default::default(),
            });
            let mut app = state(client, ServeLimits::default());
            app.config = crate::commands::serve::testutil::fixed_config(crate::config::Config {
                agent_paths: vec![elsewhere],
                ..Default::default()
            });
            crate::commands::serve::blueprints::TEST_AGENTS_DIR
                .scope(agents, start(&app, named(name)))
                .await
                .unwrap();
            carried(&seen).source
        }
    };
    assert!(matches!(
        sent_as("outside").await,
        SpawnSource::BlueprintFile(ref path) if path.path().ends_with("outside")
    ));
    assert!(matches!(sent_as("inside").await, SpawnSource::Blueprint(_)));
    assert!(matches!(
        sent_as("nowhere").await,
        SpawnSource::Blueprint(_)
    ));
    let pinned = format!("outside@{}", leviath_runtime::spec::names::Digest::of(b"x"));
    let pinned: &'static str = Box::leak(pinned.into_boxed_str());
    assert!(matches!(sent_as(pinned).await, SpawnSource::Blueprint(_)));
}

/// A whole graph is sent as it came: there is no blueprint to find.
#[tokio::test]
async fn a_whole_graph_is_sent_as_written() {
    let graph = crate::runstate::with_isolated_runs_dir("spawn-raw", |_| {
        let run_id = super::super::run_file::tests::recorded();
        super::super::run_file::require(&run_id)
            .unwrap()
            .spec()
            .graph
            .clone()
    });
    let (client, seen, _dir, _task) = recording(ControlResponse::Spawned {
        run_id: "raw-1".into(),
        warnings: Default::default(),
    });
    let request = Request::new(SpawnSource::Raw(Box::new(graph.clone())));
    let started = start(&state(client, ServeLimits::default()), request)
        .await
        .unwrap();
    assert!(matches!(started, Verdict::Accepted(_)));
    assert_eq!(carried(&seen).source, SpawnSource::Raw(Box::new(graph)));
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
        warnings: Default::default(),
    });
    state.control = control;
    let failure = send_message(&state, "run-a", "hi".to_string(), None, Vec::new())
        .await
        .expect_err("an answer to another question");
    assert_eq!(failure.code(), "INTERNAL");

    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Spawned {
        run_id: "x".to_string(),
        warnings: Default::default(),
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
        warnings: Default::default(),
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

/// An answer or a message the daemon refused is the caller's mistake, and says
/// what the mistake was: a 400 over REST, `BAD_USER_INPUT` over GraphQL.
#[tokio::test]
async fn a_refusal_from_the_daemon_is_a_bad_request_with_its_reason() {
    let refusal = |message: &str| {
        let message = message.to_string();
        move |_| ControlResponse::Error {
            message: message.clone(),
        }
    };
    let mut state = state_with_agent_paths(Vec::new());
    let (control, _socket, _srv) = fake_daemon(refusal("'ask-1' asks for a yes or no"));
    state.control = control;
    let failure = answer_interaction(
        &state,
        leviath_core::interaction::InteractionResponse::text("ask-1", "yes"),
    )
    .await
    .expect_err("text is no answer to a yes or no");
    assert_eq!(failure.code(), "BAD_USER_INPUT");
    assert!(
        failure.to_string().contains("asks for a yes or no"),
        "{failure}"
    );

    let (control, _socket, _srv) = fake_daemon(refusal("refusing to deliver an empty message"));
    state.control = control;
    let failure = send_message(&state, "run-a", String::new(), None, Vec::new())
        .await
        .expect_err("an empty message says nothing");
    assert_eq!(failure.code(), "BAD_USER_INPUT");
    assert!(failure.to_string().contains("empty message"), "{failure}");
}

/// A message the daemon will not deliver because of the run it names is not
/// the caller's mistake: no run by that name is a miss, a run that finished or
/// was cancelled is there and refuses it, and a run held off this machine says
/// what to put back. The daemon's own words are kept for each.
#[tokio::test]
async fn a_message_no_run_will_read_says_which_run_and_why() {
    use crate::commands::serve::core::held::{listing, seed_held};
    crate::runstate::with_isolated_runs_dir_async("core-message-refused", |_d| async move {
        let held_row = seed_held("held-1", "held-1-ask-1");
        crate::runstate::create_run(&crate::runstate::RunMeta {
            status: crate::runstate::RunStatus::Complete,
            ..crate::test_support::fixtures::run_meta("done-1")
        })
        .unwrap();
        // The daemon lists the finished run too, and not as held.
        let mut listed = held_row.clone();
        listed.run_id = "done-1".to_string();
        listed.wait_reason = None;
        let (control, _socket, _srv) =
            crate::commands::serve::testutil::busy_daemon(move |req| match req {
                ControlRequest::List => listing(vec![held_row.clone(), listed.clone()]),
                ControlRequest::Message { agent_id, .. } => ControlResponse::Error {
                    message: format!("run '{agent_id}' reads no more messages"),
                },
                other => panic!("a listing or a message, not {other:?}"),
            });
        let mut state = state_with_agent_paths(Vec::new());
        state.control = control;
        for (run, code) in [
            ("ghost", "NOT_FOUND"),
            ("done-1", "CONFLICT"),
            ("held-1", "RUN_HELD"),
        ] {
            let failure = send_message(&state, run, "hi".to_string(), None, Vec::new())
                .await
                .expect_err("the run reads no more messages");
            assert_eq!(failure.code(), code, "{run}");
            assert!(
                failure
                    .to_string()
                    .contains(&format!("run '{run}' reads no more")),
                "{run}"
            );
        }
    })
    .await;
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
