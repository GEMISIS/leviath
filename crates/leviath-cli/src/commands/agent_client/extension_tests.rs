//! The `_leviath/*` extension methods, driven over the JSON-RPC wire against
//! a scripted daemon.

use super::*;

use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::request::SpawnRequest;

/// Every request the scripted daemon was handed, in order.
type Seen = Arc<std::sync::Mutex<Vec<ControlRequest>>>;

/// A daemon that records every request and answers each with `answer`.
fn recording(
    events: Vec<WorldEvent>,
    answer: impl Fn(&ControlRequest) -> ControlResponse + Send + Sync + 'static,
) -> (ScriptedDaemon, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let daemon = ScriptedDaemon::new(events, move |req| {
        let reply = answer(&req);
        log.lock().unwrap().push(req);
        reply
    });
    (daemon, seen)
}

/// The spawn and validate requests among `seen`.
fn spawn_requests(seen: &Seen) -> Vec<SpawnRequest> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|req| match req {
            ControlRequest::Spawn { request } | ControlRequest::ValidateSpawn { request } => {
                Some((**request).clone())
            }
            _ => None,
        })
        .collect()
}

/// One issue the daemon has with every request.
fn daemon_issue() -> SpawnIssues {
    SpawnIssue::new(
        SpecPath::root().field("inputs").key("depth"),
        IssueCode::WrongType,
        "depth is a number",
    )
    .into()
}

/// An extension request line.
fn call(id: u32, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// A request for the installed `coder` blueprint with a task.
fn coder(extra: serde_json::Value) -> serde_json::Value {
    let mut request = serde_json::json!({
        "source": {"blueprint": {"name": "coder"}},
        "inputs": {"task": "fix the bug", "depth": 2}
    });
    for (key, value) in extra.as_object().unwrap() {
        request[key] = value.clone();
    }
    request
}

/// The paths of the issues an error carries as its data.
fn issue_paths(msg: &JsonRpcMessage) -> Vec<String> {
    let error = msg.error.as_ref().expect("an error");
    assert_eq!(error.code, error_codes::INVALID_PARAMS);
    let issues: SpawnIssues =
        serde_json::from_value(error.data.clone().expect("the issues ride as data")).unwrap();
    issues.iter().map(|i| i.path.to_string()).collect()
}

#[tokio::test]
async fn initialize_lists_the_extension_methods_in_meta() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    let result = h.recv().await.result.unwrap();
    assert_eq!(
        result["agentCapabilities"]["_meta"]["leviath"]["methods"],
        serde_json::json!(["_leviath/spawn", "_leviath/validate_spawn"])
    );
    h.close_input().await;
}

/// The protocol's rule for `_` methods: an unknown request is "method not
/// found", an unknown notification is ignored.
#[tokio::test]
async fn an_unknown_extension_is_not_found_and_its_notification_ignored() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(r#"{"jsonrpc":"2.0","method":"_leviath/nothing"}"#)
        .await;
    h.send(&call(2, "_leviath/nothing", serde_json::json!({})))
        .await;
    let reply = h.recv().await;
    assert_eq!(reply.id, Some(serde_json::json!(2)));
    assert_eq!(reply.error.unwrap().code, error_codes::METHOD_NOT_FOUND);
    h.close_input().await;
}

/// A spawn starts the run in a session of its own, working in the launch
/// directory when the request names none. An empty prompt on that session
/// follows the run without messaging it; one with text messages it first.
#[tokio::test]
async fn a_spawn_opens_a_session_bound_to_its_run() {
    let (daemon, seen) = recording(vec![status_event(), completed("complete")], |req| {
        spawn_ok(req.clone())
    });
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.write_output(0, "on it\n");
    h.send(&call(1, extensions::SPAWN, coder(serde_json::json!({}))))
        .await;
    let result = h.recv().await.result.expect("spawned");
    assert_eq!(result["runId"], RUN_ID);
    let session_id = result["sessionId"].as_str().unwrap().to_string();
    assert!(session_id.starts_with("coder"), "{session_id}");
    let sent = spawn_requests(&seen);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].workdir.as_deref(),
        Some(std::path::Path::new(HARNESS_DEFAULT_CWD))
    );
    assert_eq!(
        sent[0].inputs["depth"],
        leviath_runtime::spec::inputs::RawInput::Int(2)
    );

    // Following the run streams its output and ends with it.
    h.send(&call(
        2,
        "session/prompt",
        serde_json::json!({"sessionId": session_id, "prompt": []}),
    ))
    .await;
    let chunk = h
        .recv_until(|m| update_kind(m).as_deref() == Some("agent_message_chunk"))
        .await;
    assert_eq!(chunk.params.unwrap()["sessionId"], session_id.as_str());
    let end = h.recv_until(is_result).await;
    assert_eq!(end.result.unwrap()["stopReason"], "end_turn");
    let messaged = |seen: &Seen| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, ControlRequest::Message { .. }))
            .count()
    };
    assert_eq!(messaged(&seen), 0, "an empty prompt sends nothing");

    h.send(&call(
        3,
        "session/prompt",
        serde_json::json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "more"}]}),
    ))
    .await;
    let _ = h.recv_until(is_result).await;
    assert_eq!(messaged(&seen), 1);
    h.close_input().await;
}

/// A whole graph runs as the host wrote it, named after its title.
#[tokio::test]
async fn a_spawn_can_carry_a_graph_of_its_own() {
    let (daemon, seen) = recording(vec![], |req| spawn_ok(req.clone()));
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    let graph = super::super::extension::tests::a_graph();
    h.send(&call(
        1,
        extensions::SPAWN,
        serde_json::json!({"source": {"raw": graph}, "workdir": "/elsewhere"}),
    ))
    .await;
    let result = h.recv().await.result.expect("spawned");
    assert!(
        result["sessionId"].as_str().unwrap().starts_with("sketch"),
        "{result}"
    );
    let sent = spawn_requests(&seen);
    assert!(matches!(
        sent[0].source,
        leviath_runtime::spec::request::SpawnSource::Raw(_)
    ));
    assert_eq!(
        sent[0].workdir.as_deref(),
        Some(std::path::Path::new("/elsewhere"))
    );
    h.close_input().await;
}

/// A host relays what others wrote, so a blueprint read from a directory is
/// refused as it is over HTTP, and the daemon never sees the request.
#[tokio::test]
async fn a_blueprint_by_directory_is_refused_without_asking_the_daemon() {
    let (daemon, seen) = recording(vec![], |req| spawn_ok(req.clone()));
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    let dir = tempfile::tempdir().unwrap();
    let request = serde_json::json!({
        "source": {"blueprint_file": dir.path().to_string_lossy()},
        "launch": {"unattended": "all"}
    });
    for (id, method) in [(1, extensions::SPAWN), (2, extensions::VALIDATE_SPAWN)] {
        h.send(&call(id, method, request.clone())).await;
        let reply = h.recv().await;
        assert_eq!(
            issue_paths(&reply),
            ["launch.unattended", "source.blueprint_file"]
        );
    }
    assert!(spawn_requests(&seen).is_empty());
    h.close_input().await;
}

/// A request the server refuses for its own reasons hears the daemon's too,
/// all in one answer.
#[tokio::test]
async fn the_servers_refusals_and_the_daemons_arrive_together() {
    let (daemon, seen) = recording(vec![], |req| match req {
        ControlRequest::ValidateSpawn { .. } => ControlResponse::Rejected {
            issues: daemon_issue(),
        },
        other => spawn_ok(other.clone()),
    });
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(&call(
        1,
        extensions::SPAWN,
        coder(serde_json::json!({"launch": {"allow": ["bash"]}})),
    ))
    .await;
    let reply = h.recv().await;
    assert_eq!(issue_paths(&reply), ["launch.allow[0]", "inputs.depth"]);
    // Only the dry run reached the daemon; nothing spawned.
    let dry_runs = seen
        .lock()
        .unwrap()
        .iter()
        .all(|r| matches!(r, ControlRequest::ValidateSpawn { .. }));
    assert!(dry_runs);
    h.close_input().await;
}

/// A daemon with nothing to add leaves the server's own refusal standing.
#[tokio::test]
async fn the_servers_refusal_stands_alone_when_the_daemon_has_none() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(&call(
        1,
        extensions::VALIDATE_SPAWN,
        coder(serde_json::json!({"launch": {"unattended": "all"}})),
    ))
    .await;
    assert_eq!(issue_paths(&h.recv().await), ["launch.unattended"]);
    h.close_input().await;
}

/// The operator's `--allow` admits that tool, and `--no-seed-commands`
/// turns a request's seed commands off on its way through.
#[tokio::test]
async fn the_operator_flags_admit_and_adjust_a_request() {
    let (daemon, seen) = recording(vec![], |req| spawn_ok(req.clone()));
    let args = AgentClientArgs {
        allow: vec!["bash".to_string()],
        no_seed_commands: true,
        ..Default::default()
    };
    let mut h = Harness::start(daemon, args);
    h.send(&call(
        1,
        extensions::SPAWN,
        coder(serde_json::json!({"launch": {"allow": ["bash"]}})),
    ))
    .await;
    assert!(h.recv().await.result.is_some());
    let sent = spawn_requests(&seen);
    assert!(!sent[0].launch.seed_commands);
    h.close_input().await;
}

/// Params that are not a request are refused at the root, saying why.
#[tokio::test]
async fn params_that_do_not_read_are_refused_at_the_root() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"_leviath/spawn"}"#)
        .await;
    assert_eq!(issue_paths(&h.recv().await), ["(request)"]);
    h.send(&call(
        2,
        extensions::VALIDATE_SPAWN,
        serde_json::json!({"source": {"blueprint": {"name": "coder"}}, "input": {}}),
    ))
    .await;
    let reply = h.recv().await;
    assert_eq!(issue_paths(&reply), ["(request)"]);
    assert!(
        reply
            .error
            .unwrap()
            .message
            .contains("unknown field `input`"),
        "the message names the field"
    );
    h.close_input().await;
}

/// A dry run answers with the summary, and the daemon's refusal of either
/// method comes back as the same error.
#[tokio::test]
async fn a_dry_run_answers_with_the_summary_or_the_daemons_issues() {
    use leviath_runtime::spec::launch::{LaunchPolicy, LaunchRequest};
    let graph = super::super::extension::tests::a_graph();
    let valid = leviath_runtime::spec::summary::SpawnSummary {
        title: "coder".to_string(),
        origin: leviath_runtime::spec::run_spec::SpecOrigin::Raw,
        entry_stage: graph.stages[0].name.clone(),
        stages: Vec::new(),
        inputs: Default::default(),
        launch: LaunchPolicy::top_level(&LaunchRequest::default(), 3, true),
        workdir: "/w".into(),
    };
    let answer = valid.clone();
    let daemon = ScriptedDaemon::new(vec![], move |req| match req {
        ControlRequest::ValidateSpawn { request } if request.inputs.contains_key("task") => {
            ControlResponse::Valid {
                summary: Box::new(answer.clone()),
            }
        }
        _ => ControlResponse::Rejected {
            issues: daemon_issue(),
        },
    });
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(&call(
        1,
        extensions::VALIDATE_SPAWN,
        coder(serde_json::json!({})),
    ))
    .await;
    let result = h.recv().await.result.expect("valid");
    assert_eq!(result, serde_json::to_value(&valid).unwrap());
    h.send(&call(
        2,
        extensions::VALIDATE_SPAWN,
        serde_json::json!({"source": {"blueprint": {"name": "coder"}}}),
    ))
    .await;
    assert_eq!(issue_paths(&h.recv().await), ["inputs.depth"]);
    h.send(&call(
        3,
        extensions::SPAWN,
        serde_json::json!({"source": {"blueprint": {"name": "coder"}}}),
    ))
    .await;
    assert_eq!(issue_paths(&h.recv().await), ["inputs.depth"]);
    h.close_input().await;
}

/// A daemon that cannot answer is an internal error, whatever the reason.
#[tokio::test]
async fn a_daemon_that_cannot_answer_is_an_internal_error() {
    let daemon = ScriptedDaemon::new(vec![], |req| match req {
        ControlRequest::Spawn { .. } => ControlResponse::Error {
            message: "shutting down".to_string(),
        },
        _ => ControlResponse::Ok { ok: true },
    });
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(&call(1, extensions::SPAWN, coder(serde_json::json!({}))))
        .await;
    let error = h.recv().await.error.unwrap();
    assert_eq!(error.code, error_codes::INTERNAL_ERROR);
    assert!(error.message.contains("shutting down"), "{}", error.message);
    h.send(&call(
        2,
        extensions::VALIDATE_SPAWN,
        coder(serde_json::json!({})),
    ))
    .await;
    let error = h.recv().await.error.unwrap();
    assert!(
        error.message.contains("unexpected reply"),
        "{}",
        error.message
    );
    h.close_input().await;

    // No daemon at all.
    let gone = ControlClient::new(control_id(std::path::Path::new("/no/such/daemon")));
    let (to_server, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, from_server) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let _ = serve_over(
            BufReader::new(server_in),
            server_out,
            gone,
            AgentClientArgs::default(),
            std::env::temp_dir(),
            HARNESS_DEFAULT_CWD.to_string(),
        )
        .await;
    });
    let mut h = Harness {
        to_server,
        from_server: BufReader::new(from_server),
        runs_dir: tempfile::tempdir().unwrap(),
        _daemon: ScriptedDaemon::new(vec![], spawn_ok),
        _server: AbortOnDrop(server),
    };
    h.send(&call(1, extensions::SPAWN, coder(serde_json::json!({}))))
        .await;
    let error = h.recv().await.error.unwrap();
    assert_eq!(error.code, error_codes::INTERNAL_ERROR);
    h.close_input().await;
}

/// The `---region:<name>---` markers are gone: a prompt's whole text is the
/// task, and the run gets no other input from it.
#[tokio::test]
async fn a_prompt_is_the_task_and_nothing_else() {
    let (daemon, seen) = recording(vec![completed("complete")], |req| spawn_ok(req.clone()));
    let (mut h, _bp) = opened_session(daemon, false).await;
    let text = "build a parser\n---region:criteria---\nfocus on safety";
    h.send(&call(
        3,
        "session/prompt",
        serde_json::json!({"prompt": [{"type": "text", "text": text}]}),
    ))
    .await;
    let _ = h.recv_until(is_result).await;
    let sent = spawn_requests(&seen);
    assert_eq!(sent[0].inputs.len(), 1, "{:?}", sent[0].inputs);
    assert_eq!(
        sent[0].inputs["task"],
        leviath_runtime::spec::inputs::RawInput::Text(text.to_string())
    );
    h.close_input().await;
}
