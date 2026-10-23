//! `POST /api/runs`, `POST /api/runs/validate`, the schema they take and a
//! blueprint's inputs, through the real route table.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpecPath};
use tower::ServiceExt;

use super::super::types::ServeLimits;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};

/// What a fake daemon was sent.
type Seen = Arc<Mutex<Option<ControlRequest>>>;

/// A state whose daemon answers `reply` and records what it was sent.
fn answering(reply: ControlResponse) -> (super::AppState, Seen, tempfile::TempDir) {
    let seen: Seen = Arc::new(Mutex::new(None));
    let held = seen.clone();
    let (client, dir, _task) = fake_daemon(move |req| {
        *held.lock().unwrap() = Some(req);
        reply.clone()
    });
    let mut state = state_with_agent_paths(Vec::new());
    state.control = client;
    (state, seen, dir)
}

/// A state with no daemon behind it.
fn offline() -> super::AppState {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = no_daemon_client();
    state
}

/// Send `request` through the real route table.
async fn send(state: super::AppState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let app = super::super::api_router().with_state(state);
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// A JSON `POST` of `body` to `uri`.
fn post(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A multipart `POST` to `uri` of `fields`: `(name, content type, bytes)`,
/// where a content type of `None` sends the field as text.
fn multipart(uri: &str, fields: &[(&str, Option<&str>, &[u8])]) -> Request<Body> {
    let boundary = "lev-test-boundary";
    let mut body = Vec::new();
    for (name, content_type, bytes) in fields {
        body.extend(format!("--{boundary}\r\n").as_bytes());
        match content_type {
            Some(t) => body.extend(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{name}.bin\"\r\n\
                     Content-Type: {t}\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend(*bytes);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// The request a recorded spawn or dry run carried.
fn carried(seen: &Seen) -> leviath_runtime::spec::request::SpawnRequest {
    match seen.lock().unwrap().take() {
        Some(ControlRequest::Spawn { request })
        | Some(ControlRequest::ValidateSpawn { request }) => *request,
        other => panic!("not a spawn: {other:?}"),
    }
}

const CODER: &str = r#"{"source": {"blueprint": {"name": "coder"}}, "inputs": {"task": "fix it"}}"#;

#[tokio::test]
async fn a_spawn_request_starts_a_run() {
    let (state, seen, _dir) = answering(ControlResponse::Spawned {
        run_id: "run-1".into(),
        warnings: Default::default(),
        deduplicated: false,
    });
    let before = crate::commands::serve::events::latest_seq();
    let (status, body) = send(state, post("/api/runs", CODER)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["run_id"], "run-1");
    assert_eq!(body["warnings"], serde_json::json!([]));
    // Where the run's frames start, and which process numbered that.
    let seq = body["seq"].as_u64().expect("a frame number");
    assert!(seq >= before, "{seq} is the bus's number when it was asked");
    assert_eq!(
        body["server_instance"],
        crate::commands::serve::events::server_instance()
    );
    assert_eq!(body["deduplicated"], false);
    assert!(carried(&seen).inputs.contains_key("task"));
}

/// A run that may never finish still starts, and the answer says why, each
/// warning in the shape a refusal's issues take.
#[tokio::test]
async fn a_spawn_that_may_never_finish_starts_and_says_so() {
    let warned = leviath_runtime::spec::issues::SpawnIssue::new(
        leviath_runtime::spec::issues::SpecPath::root()
            .field("graph")
            .field("edges"),
        leviath_runtime::spec::issues::IssueCode::MayNeverFinish,
        "this run can never finish",
    );
    let (state, _seen, _dir) = answering(ControlResponse::Spawned {
        run_id: "run-1".into(),
        warnings: warned.into(),
        deduplicated: false,
    });
    let (status, body) = send(state, post("/api/runs", CODER)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["warnings"][0]["at"], "graph.edges");
    assert_eq!(body["warnings"][0]["code"], "may_never_finish");
    assert_eq!(body["warnings"][0]["message"], "this run can never finish");
}

/// Four problems this server finds and one the daemon does, in one answer,
/// each with its path written out.
#[tokio::test]
async fn a_request_with_several_problems_hears_about_all_of_them() {
    let root = tempfile::tempdir().unwrap();
    let (mut state, seen, _dir) = answering(ControlResponse::Rejected {
        issues: SpawnIssue::new(
            SpecPath::root().field("inputs").key("task"),
            IssueCode::Missing,
            "the blueprint needs a task",
        )
        .into(),
    });
    state.limits = Arc::new(ServeLimits {
        workdir_roots: crate::commands::serve::workdir_roots::WorkdirRoots::resolve(&[root
            .path()
            .to_path_buf()])
        .unwrap(),
        no_remote_yolo: true,
        ..ServeLimits::default()
    });
    let body = r#"{
        "source": {"blueprint": {"name": "coder"}},
        "workdir": "/",
        "launch": {"unattended": "all", "allow": ["bash"]},
        "delivery": {"callback": {"url": "http://169.254.169.254/latest"}}
    }"#;
    let (status, refused) = send(state, post("/api/runs", body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let at: Vec<&str> = refused["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["at"].as_str().unwrap())
        .collect();
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
    assert_eq!(refused["issues"][0]["code"], "not_allowed");
    assert_eq!(refused["issues"][4]["code"], "missing");
    assert!(refused["issues"][4]["path"].is_array());
    assert!(matches!(
        seen.lock().unwrap().as_ref(),
        Some(ControlRequest::ValidateSpawn { .. })
    ));
}

#[tokio::test]
async fn a_body_that_is_not_a_request_is_refused_by_what_is_wrong_with_it() {
    let (status, body) = send(offline(), post("/api/runs", "not json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("not JSON"),
        "{body}"
    );

    let misspelled = r#"{"source": {"blueprint": {"name": "coder"}}, "input": {}}"#;
    let (status, body) = send(offline(), post("/api/runs", misspelled)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["issues"][0]["at"], "(request)");
    assert!(
        body["issues"][0]["message"]
            .as_str()
            .unwrap()
            .contains("unknown field `input`"),
        "{body}"
    );
    assert!(
        body["issues"][0]["hint"]
            .as_str()
            .unwrap()
            .contains("schema")
    );

    // Over the body limit the extractor holds.
    let huge = "x".repeat(3 * 1024 * 1024);
    let (status, _) = send(offline(), post("/api/runs", &huge)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_daemon_that_refuses_or_is_away_is_said_so() {
    let (state, _seen, _dir) = answering(ControlResponse::Rejected {
        issues: SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, "no").into(),
    });
    let (status, body) = send(state, post("/api/runs", CODER)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["issues"].as_array().unwrap().len(), 1);

    let (status, _) = send(offline(), post("/api/runs", CODER)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, _) = send(offline(), post("/api/runs/validate", CODER)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// Each file field becomes an attachment named after the field, typed by its
/// `Content-Type` unless that says only "bytes".
#[tokio::test]
async fn a_multipart_spawn_carries_its_files_as_attachments() {
    let (state, seen, _dir) = answering(ControlResponse::Spawned {
        run_id: "run-2".into(),
        warnings: Default::default(),
        deduplicated: false,
    });
    let request = multipart(
        "/api/runs",
        &[
            ("request", None, CODER.as_bytes()),
            ("report", Some("text/markdown"), b"# notes"),
            ("blob", Some("application/octet-stream"), b"\x00\x01"),
        ],
    );
    let (status, body) = send(state, request).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sent = carried(&seen);
    let names: Vec<&str> = sent.attachments.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["report", "blob"]);
    assert_eq!(
        sent.attachments[0].mime_type.as_ref().unwrap().as_str(),
        "text/markdown"
    );
    assert_eq!(sent.attachments[0].data.0, b"# notes".to_vec());
    assert!(sent.attachments[1].mime_type.is_none());
}

#[tokio::test]
async fn a_multipart_body_that_cannot_be_read_is_refused() {
    let no_request = multipart("/api/runs", &[("report", Some("text/plain"), b"x")]);
    let (status, body) = send(offline(), no_request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("`request`"),
        "{body}"
    );

    let not_json = multipart("/api/runs", &[("request", None, b"{")]);
    assert_eq!(send(offline(), not_json).await.0, StatusCode::BAD_REQUEST);

    // A file sent as a type no blueprint could name: refused, not sniffed.
    let bad_type = multipart(
        "/api/runs",
        &[
            ("request", None, CODER.as_bytes()),
            ("report", Some("text/x|y"), b"# notes"),
        ],
    );
    let (status, body) = send(offline(), bad_type).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("'text/x|y'"),
        "{body}"
    );

    let not_a_request = multipart("/api/runs", &[("request", None, br#"{"source": 1}"#)]);
    assert_eq!(
        send(offline(), not_a_request).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let request_bytes: &[u8] = b"\xff\xfe";
    let not_text = multipart("/api/runs", &[("request", None, request_bytes)]);
    assert_eq!(send(offline(), not_text).await.0, StatusCode::BAD_REQUEST);

    let no_boundary = Request::builder()
        .method("POST")
        .uri("/api/runs")
        .header(header::CONTENT_TYPE, "multipart/form-data")
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(
        send(offline(), no_boundary).await.0,
        StatusCode::BAD_REQUEST
    );

    // A body that ends in the middle of a field, a file or the request, and
    // one whose field has no headers to read.
    let torn = |body: &'static str| {
        Request::builder()
            .method("POST")
            .uri("/api/runs")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
            .body(Body::from(body))
            .unwrap()
    };
    for body in [
        "--b\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\nno end",
        "--b\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\nno end",
        "--b\r\nnot a header\r\n\r\nx\r\n--b--\r\n",
    ] {
        assert_eq!(
            send(offline(), torn(body)).await.0,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }

    let mut small = offline();
    small.limits = Arc::new(ServeLimits {
        request_limits: crate::commands::serve::request_limits::RequestLimits {
            max_upload_bytes: 4,
            ..Default::default()
        },
        ..ServeLimits::default()
    });
    let too_big = multipart(
        "/api/runs",
        &[
            ("request", None, CODER.as_bytes()),
            ("report", Some("text/plain"), b"far more than four bytes"),
        ],
    );
    assert_eq!(send(small, too_big).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_dry_run_answers_with_the_run_it_would_be() {
    let summary = crate::runstate::with_isolated_runs_dir("spawn-route-summary", |_| {
        let run_id = super::super::core::run_file::tests::recorded();
        leviath_runtime::spec::summary::SpawnSummary::of(
            super::super::core::run_file::require(&run_id)
                .unwrap()
                .spec(),
        )
    });
    let (state, seen, _dir) = answering(ControlResponse::Valid {
        summary: Box::new(summary.clone()),
    });
    let (status, body) = send(state, post("/api/runs/validate", CODER)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], summary.title.as_str());
    assert!(matches!(carried(&seen), request if request.workdir.is_some()));

    let (state, _seen, _dir) = answering(ControlResponse::Rejected {
        issues: SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, "no").into(),
    });
    let (status, _) = send(state, post("/api/runs/validate", CODER)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = send(offline(), post("/api/runs/validate", r#"{"x": 1}"#)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = send(offline(), post("/api/runs/validate", "{")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_request_schema_is_published() {
    let request = Request::builder()
        .uri("/api/schema/spawn-request")
        .body(Body::empty())
        .unwrap();
    let (status, schema) = send(offline(), request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(schema, leviath_runtime::runfile::spawn_request_schema());
}

#[tokio::test]
async fn a_blueprints_inputs_are_read_from_its_graph() {
    let dir = tempfile::tempdir().unwrap();
    for (name, manifest) in [
        ("coder", crate::test_support::inline_coder_manifest()),
        (
            "broken",
            crate::test_support::tiny_blueprint("broken")
                .replace("[graph]\n", "[graph]\nentry = \"nowhere\"\n"),
        ),
    ] {
        let agent = dir.path().join(name);
        std::fs::create_dir_all(&agent).unwrap();
        crate::test_support::write_test_agent(&agent, manifest);
    }
    let state = || {
        let mut state = state_with_agent_paths(vec![dir.path().to_path_buf()]);
        state.control = no_daemon_client();
        state
    };
    let get = |uri: &str| {
        Request::builder()
            .uri(uri.to_string())
            .body(Body::empty())
            .unwrap()
    };
    let (status, inputs) = send(state(), get("/api/blueprints/coder/inputs")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(inputs.is_array(), "{inputs}");
    let (status, _) = send(state(), get("/api/blueprints/nobody/inputs")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = send(state(), get("/api/blueprints/broken/inputs")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

/// A spawn under an idempotency key carries it to the daemon, and a repeat
/// the daemon answered with the first run says so.
#[tokio::test]
async fn a_keyed_spawn_carries_its_key_and_says_when_it_was_a_repeat() {
    let (state, seen, _dir) = answering(ControlResponse::Spawned {
        run_id: "run-1".into(),
        warnings: Default::default(),
        deduplicated: true,
    });
    let body = r#"{"source": {"blueprint": {"name": "coder"}}, "inputs": {"task": "fix it"},
                   "delivery": {"idempotency_key": "pw:task-1"}}"#;
    let (status, answer) = send(state, post("/api/runs", body)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(answer["run_id"], "run-1");
    assert_eq!(answer["deduplicated"], true);
    // A repeat still says where the stream stands now, above the first
    // spawn's frames.
    assert!(answer["seq"].is_u64(), "{answer}");
    let carried = carried(&seen);
    assert_eq!(
        carried
            .delivery
            .idempotency_key
            .map(String::from)
            .as_deref(),
        Some("pw:task-1")
    );
}

/// A key that is not one is refused at its own path, without asking the
/// daemon, and so is one that is not text at all. A `null` is no key.
#[tokio::test]
async fn a_key_that_is_not_one_is_refused_at_its_own_path() {
    let body = r#"{"source": {"blueprint": {"name": "coder"}},
                   "delivery": {"idempotency_key": "has space"}}"#;
    let (status, answer) = send(offline(), post("/api/runs", body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["issues"][0]["at"], "delivery.idempotency_key");
    assert_eq!(answer["issues"][0]["code"], "invalid");
    let body = r#"{"source": {"blueprint": {"name": "coder"}},
                   "delivery": {"idempotency_key": 7}}"#;
    let (status, answer) = send(offline(), post("/api/runs/validate", body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["issues"][0]["at"], "delivery.idempotency_key");
    assert_eq!(answer["issues"][0]["code"], "wrong_type");
    assert_eq!(answer["issues"][0]["got"], "7");
    let body = r#"{"source": {"blueprint": {"name": "coder"}},
                   "delivery": {"idempotency_key": null}}"#;
    let (status, answer) = send(offline(), post("/api/runs/validate", body)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{answer}");
}

/// A daemon that takes every request it is sent and answers none.
fn silent_daemon() -> (
    leviath_runtime::control_socket::ControlClient,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    use leviath_runtime::control_socket::{ControlClient, bind_control_listener, control_id};
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok(Some(stream)) = listener.accept().await {
            held.push(stream);
        }
    });
    (ControlClient::new(id), dir, task)
}

/// A state over a daemon that never answers, with a spawn deadline of
/// `secs` and the ordinary one left at its default.
fn waiting(secs: u64) -> (super::AppState, tempfile::TempDir) {
    let (client, dir, _task) = silent_daemon();
    let mut state = state_with_agent_paths(Vec::new());
    state.control = client;
    state.limits = Arc::new(ServeLimits {
        request_limits: crate::commands::serve::request_limits::RequestLimits {
            spawn_timeout_secs: secs,
            ..Default::default()
        },
        ..Default::default()
    });
    (state, dir)
}

/// A spawn past its own deadline answers 408, says its outcome is unknown,
/// and names the key to send it again under; one sent with no key says so.
#[tokio::test]
async fn a_spawn_past_its_deadline_says_its_outcome_is_unknown() {
    let (state, _dir) = waiting(1);
    let keyed = r#"{"source": {"blueprint": {"name": "coder"}}, "inputs": {"task": "fix it"},
                    "delivery": {"idempotency_key": "pw:task-1"}}"#;
    let (status, answer) = send(state.clone(), post("/api/runs", keyed)).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{answer}");
    assert_eq!(answer["spawn_outcome"], "unknown");
    assert_eq!(answer["idempotency_key"], "pw:task-1");
    let error = answer["error"].as_str().unwrap();
    assert!(error.contains("longer than 1 s"), "{error}");
    assert!(error.contains("; send it again with the same"), "{error}");
    let (status, answer) = send(state, post("/api/runs", CODER)).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(answer["idempotency_key"], serde_json::Value::Null);
}

/// A dry run past the spawn deadline is an ordinary timeout: it started
/// nothing, so there is no outcome to be unsure of.
#[tokio::test]
async fn a_dry_run_past_its_deadline_is_an_ordinary_timeout() {
    let (state, _dir) = waiting(1);
    let (status, answer) = send(state, post("/api/runs/validate", CODER)).await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(answer["error"], "request took longer than 1 s");
    assert_eq!(answer.get("spawn_outcome"), None);
}

/// A daemon that predates keys would refuse the whole request unread, so a
/// key sent to one is refused here, at its path, with what to do about it.
/// A daemon that keeps keys is announced as the `spawn.idempotency`
/// capability; one that does not, or has not said, is not.
#[tokio::test]
async fn a_key_is_refused_and_unannounced_when_the_daemon_predates_keys() {
    use leviath_runtime::control_socket::DaemonIdentity;
    let old = DaemonIdentity {
        idempotent_spawns: false,
        ..crate::test_support::same_code_daemon(7)
    };
    let daemon = crate::test_support::identified_daemon(old, 3, |req| match req {
        ControlRequest::Status { .. } => ControlResponse::Status { status: None },
        _ => ControlResponse::Rejected {
            issues: Default::default(),
        },
    });
    let mut state = state_with_agent_paths(Vec::new());
    state.control = daemon.client.clone();
    let get_config = || {
        Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap()
    };
    let (_, config) = send(state.clone(), get_config()).await;
    let announced = |config: &serde_json::Value| {
        config["capabilities"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("spawn.idempotency"))
    };
    assert!(!announced(&config), "no daemon has said who it is yet");
    daemon.client.status("anything").await.unwrap();
    let (_, config) = send(state.clone(), get_config()).await;
    assert!(!announced(&config), "this daemon predates keys");
    assert_eq!(config["limits"]["spawn_timeout_secs"], 330);

    let keyed = r#"{"source": {"blueprint": {"name": "coder"}}, "inputs": {"task": "fix it"},
                    "delivery": {"idempotency_key": "pw:task-1"}}"#;
    let (status, answer) = send(state, post("/api/runs", keyed)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["issues"][0]["at"], "delivery.idempotency_key");
    assert_eq!(answer["issues"][0]["code"], "not_allowed");

    let current =
        crate::test_support::identified_daemon(crate::test_support::same_code_daemon(8), 1, |_| {
            ControlResponse::Status { status: None }
        });
    let mut state = state_with_agent_paths(Vec::new());
    state.control = current.client.clone();
    current.client.status("anything").await.unwrap();
    let (_, config) = send(state, get_config()).await;
    assert!(announced(&config), "{config}");
}

/// A spawn's body has the ordinary deadline to arrive in: one still arriving
/// past it is an ordinary 408, on both routes, and nothing reaches the
/// daemon.
#[tokio::test(start_paused = true)]
async fn a_spawn_body_still_arriving_past_the_ordinary_deadline_is_a_408() {
    for uri in ["/api/runs", "/api/runs/validate"] {
        let mut state = offline();
        state.limits = Arc::new(ServeLimits {
            request_limits: crate::commands::serve::request_limits::RequestLimits {
                request_timeout_secs: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        let stuck = futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(stuck))
            .unwrap();
        let (status, answer) = send(state, request).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{uri}");
        assert_eq!(answer["error"], "request took longer than 1 s");
    }
}

#[tokio::test]
async fn a_graph_that_does_not_read_names_where() {
    let body = r#"{"source": {"raw": {"stages": "none"}}}"#;
    let (status, refused) = send(offline(), post("/api/runs", body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.to_string().contains("source.raw"), "{refused}");
}
