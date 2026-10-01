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
    });
    let (status, body) = send(state, post("/api/runs", CODER)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body, serde_json::json!({ "run_id": "run-1" }));
    assert!(carried(&seen).inputs.contains_key("task"));
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
        workdir_root: Some(root.path().to_path_buf()),
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
