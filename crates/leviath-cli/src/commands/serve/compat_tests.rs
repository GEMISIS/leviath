//! The `/api/agents` routes older clients call, through the real route table.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpecPath};
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use tower::ServiceExt;

use super::super::types::{AppState, ServeLimits};
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};

/// A blueprint with three inputs: `task`, `subject` (which fills the `query`
/// region) and `count` (which fills `notes` through a template).
const PROBE: &str = r#"[blueprint]
name = "compat-probe"
version = "1.0.0"

[graph]
stages = [{ name = "main", system_prompt = "work" }]
layout = { total_budget_tokens = 4000, regions = [
  { name = "task", kind = "pinned", budget = 1000 },
  { name = "query", kind = "pinned", budget = 1000 },
  { name = "notes", kind = "pinned", budget = 1000 },
] }
inputs = [
  { name = "task", type = "text", required = true, binds = [{ region = "task" }] },
  { name = "subject", type = "text", binds = [{ region = "query" }] },
  { name = "count", type = "int", binds = [{ region = "notes", template = "count={count}" }] },
]
"#;

/// What a fake daemon was sent.
type Seen = Arc<Mutex<Vec<ControlRequest>>>;

/// A directory of blueprints holding [`PROBE`] and one whose graph does not
/// hold together, and a state that lists it, whose daemon answers `reply`.
fn served(
    reply: Option<ControlResponse>,
) -> (AppState, Seen, tempfile::TempDir, Option<tempfile::TempDir>) {
    let blueprints = tempfile::tempdir().unwrap();
    for (name, manifest) in [
        ("compat-probe", PROBE.to_string()),
        (
            "compat-broken",
            crate::test_support::tiny_blueprint("compat-broken")
                .replace("[graph]\n", "[graph]\nentry = \"nowhere\"\n"),
        ),
    ] {
        let dir = blueprints.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        crate::test_support::write_test_agent(&dir, manifest);
    }
    let mut state = state_with_agent_paths(vec![blueprints.path().to_path_buf()]);
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let socket = reply.map(|reply| {
        let held = seen.clone();
        let (client, dir, _task) = fake_daemon(move |req| {
            held.lock().unwrap().push(req);
            reply.clone()
        });
        state.control = client;
        dir
    });
    if socket.is_none() {
        state.control = no_daemon_client();
    }
    (state, seen, blueprints, socket)
}

/// Send `request` through the real route table, with the installed
/// blueprints in an empty directory of their own.
async fn send(state: AppState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let installed = tempfile::tempdir().unwrap();
    let app = super::super::api_router().with_state(state);
    let response = super::super::blueprints::TEST_AGENTS_DIR
        .scope(installed.path().to_path_buf(), app.oneshot(request))
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// A JSON `POST /api/agents` of `body`.
fn spawn(body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/agents")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The spawn a fake daemon was sent.
fn spawned(seen: &Seen) -> SpawnRequest {
    seen.lock()
        .unwrap()
        .iter()
        .find_map(|req| match req {
            ControlRequest::Spawn { request } => Some((**request).clone()),
            _ => None,
        })
        .expect("a spawn reached the daemon")
}

#[tokio::test]
async fn the_old_body_starts_a_run_and_answers_in_the_old_shape() {
    let (state, seen, _bp, _sock) = served(Some(ControlResponse::Spawned {
        run_id: "compat-probe-1".into(),
        warnings: Default::default(),
    }));
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("brief.txt"), "the brief").unwrap();
    let body = serde_json::json!({
        "blueprint": "compat-probe",
        "task": "summarise @brief.txt",
        "regions": {"query": "what to look up", "notes": "free text"},
        "workdir": work.path(),
        "model": "openai/gpt-mock",
        "yolo_profile": "careful",
        "allow": ["read_file"],
        "max_depth": 2,
        "no_seed_commands": true,
        "capture_model_input": true,
        "metadata": {"k": "v"},
        "callback_url": "https://example.com/hook",
        "callback_secret": "s3cret",
        "output_format": "markdown",
        "parts": [{"path": "brief.txt", "region": "query"}],
    });
    let (status, answer) = send(state, spawn(body)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer,
        serde_json::json!({"agent_id": "compat-probe-1", "run_id": "compat-probe-1"})
    );
    let request = spawned(&seen);
    // Listed from a configured directory rather than installed, so the
    // daemon is pointed at that directory.
    let SpawnSource::BlueprintFile(path) = &request.source else {
        panic!("{:?}", request.source)
    };
    assert!(path.to_string().ends_with("compat-probe"), "{path}");
    // `query` is the region `subject` fills; `notes` is filled only through
    // a template, so it keeps its own name and validation names it.
    let names: Vec<&str> = request.inputs.keys().map(String::as_str).collect();
    assert_eq!(names, ["notes", "subject", "task"]);
    assert_eq!(
        request.inputs["subject"],
        RawInput::Text("what to look up".into())
    );
    assert_eq!(request.attachments.len(), 2, "the part and the @path");
    assert_eq!(
        request.launch.unattended,
        Unattended::Profile(leviath_runtime::spec::names::ProfileName::new("careful").unwrap())
    );
    assert!(!request.launch.seed_commands);
    assert!(request.launch.capture_model_input);
    assert_eq!(request.launch.max_depth, Some(2));
    assert_eq!(request.delivery.metadata["k"], "v");
    assert!(request.delivery.callback.is_some());
    assert_eq!(
        request.output.and_then(|o| o.format).as_deref(),
        Some("markdown")
    );
}

#[tokio::test]
async fn a_plain_body_asks_for_nothing_it_did_not_name() {
    let (state, seen, _bp, _sock) = served(Some(ControlResponse::Spawned {
        run_id: "r".into(),
        warnings: Default::default(),
    }));
    let (status, _) = send(
        state,
        spawn(serde_json::json!({"blueprint": "compat-probe", "task": "go", "yolo": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request = spawned(&seen);
    assert_eq!(request.launch.unattended, Unattended::All);
    assert!(request.output.is_none());
}

#[tokio::test]
async fn a_refusal_is_one_message_under_the_old_statuses() {
    let missing = ControlResponse::Rejected {
        issues: SpawnIssue::new(
            SpecPath::root().field("inputs").key("task"),
            IssueCode::Missing,
            "the blueprint needs a task",
        )
        .into(),
    };
    // Only the daemon's refusal: a 400 naming it.
    let (state, _seen, _bp, _sock) = served(Some(missing.clone()));
    let (status, body) = send(
        state,
        spawn(serde_json::json!({"blueprint": "compat-probe", "task": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("inputs.task"),
        "{body}"
    );
    // Something this server does not allow as well: a 403 naming both.
    let (mut state, _seen, _bp, _sock) = served(Some(missing));
    state.limits = Arc::new(ServeLimits {
        no_remote_yolo: true,
        ..ServeLimits::default()
    });
    let (status, body) = send(
        state,
        spawn(serde_json::json!({"blueprint": "compat-probe", "task": "", "yolo": true})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("launch.unattended") && message.contains("inputs.task"),
        "{message}"
    );
}

/// A region no input of the blueprint fills is left out, as older servers
/// left it, and the answer says so; the rest of the body still starts the
/// run. A file named in that text is not read.
#[tokio::test]
async fn a_region_no_input_fills_is_left_out_with_a_warning() {
    let (state, seen, _bp, _sock) = served(Some(ControlResponse::Spawned {
        run_id: "compat-probe-2".into(),
        warnings: Default::default(),
    }));
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("notes.txt"), "a note").unwrap();
    let body = serde_json::json!({
        "blueprint": "compat-probe",
        "task": "go",
        "workdir": work.path(),
        "regions": {"query": "what to look up", "scratch": "see @notes.txt"},
    });
    let (status, answer) = send(state, spawn(body)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["run_id"], "compat-probe-2");
    let warnings = answer["warnings"].as_array().expect("a warning");
    assert_eq!(warnings.len(), 1, "{answer}");
    let said = warnings[0].as_str().unwrap();
    assert!(said.contains("regions.scratch"), "{said}");
    let request = spawned(&seen);
    let names: Vec<&str> = request.inputs.keys().map(String::as_str).collect();
    assert_eq!(names, ["subject", "task"]);
    assert!(request.attachments.is_empty());
}

#[tokio::test]
async fn a_body_that_cannot_be_a_run_is_refused_before_the_daemon() {
    let cases = [
        // Not the old body at all.
        (serde_json::json!({"task": "x"}), StatusCode::BAD_REQUEST),
        // Not a blueprint name.
        (
            serde_json::json!({"blueprint": "", "task": "x"}),
            StatusCode::BAD_REQUEST,
        ),
        // Not installed anywhere.
        (
            serde_json::json!({"blueprint": "compat-nobody", "task": "x"}),
            StatusCode::NOT_FOUND,
        ),
        // Installed, but its graph does not hold together.
        (
            serde_json::json!({"blueprint": "compat-broken", "task": "x"}),
            StatusCode::BAD_REQUEST,
        ),
        // A flag that does not read.
        (
            serde_json::json!({"blueprint": "compat-probe", "task": "x", "callback_url": "nope"}),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (body, want) in cases {
        let (state, seen, _bp, _sock) = served(Some(ControlResponse::Spawned {
            run_id: "never".into(),
            warnings: Default::default(),
        }));
        let (status, answer) = send(state, spawn(body.clone())).await;
        assert_eq!(status, want, "{body} -> {answer}");
        assert!(answer["error"].is_string(), "{answer}");
        assert!(seen.lock().unwrap().is_empty(), "{body}");
    }
}

#[tokio::test]
async fn a_file_over_the_ceiling_is_refused_wherever_it_is_named() {
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("big.txt"), "x".repeat(64)).unwrap();
    let bodies = [
        serde_json::json!({"parts": [{"path": "big.txt"}]}),
        serde_json::json!({"task": "read @big.txt"}),
        serde_json::json!({"regions": {"query": "read @big.txt"}}),
    ];
    for extra in bodies {
        let (mut state, seen, _bp, _sock) = served(Some(ControlResponse::Spawned {
            run_id: "never".into(),
            warnings: Default::default(),
        }));
        let mut limits = ServeLimits::default();
        limits.request_limits.max_upload_bytes = 8;
        state.limits = Arc::new(limits);
        let mut body = serde_json::json!({
            "blueprint": "compat-probe", "task": "x", "workdir": work.path()
        });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, _) = send(state, spawn(body.clone())).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
        assert!(seen.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_daemon_that_is_not_there_is_a_503() {
    let (state, _seen, _bp, _sock) = served(None);
    let (status, _) = send(
        state,
        spawn(serde_json::json!({"blueprint": "compat-probe", "task": "go"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn a_region_is_named_as_the_input_that_fills_it() {
    let inputs = leviath_blueprint::BlueprintFile::parse(PROBE)
        .unwrap()
        .run_graph()
        .inputs;
    assert_eq!(
        super::input_for_region(&inputs, "task").as_deref(),
        Some("task")
    );
    assert_eq!(
        super::input_for_region(&inputs, "query").as_deref(),
        Some("subject")
    );
    assert_eq!(
        super::input_for_region(&inputs, "notes").as_deref(),
        Some("notes")
    );
    assert_eq!(super::input_for_region(&inputs, "nowhere"), None);
    // Two inputs filling one region: neither is the region's.
    let mut twice = inputs.clone();
    twice.push(inputs[1].clone());
    twice[3].name = leviath_runtime::spec::names::InputName::new("other").unwrap();
    assert_eq!(
        super::input_for_region(&twice, "query").as_deref(),
        Some("query")
    );
}

#[tokio::test]
async fn the_old_listing_is_every_run_in_one_array() {
    crate::runstate::with_isolated_runs_dir_async("compat_listing", |_d| async move {
        let mut done = crate::runstate::RunMeta::new(
            "compat-done".into(),
            "a".into(),
            String::new(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        done.status = crate::runstate::RunStatus::Complete;
        crate::runstate::create_run(&done).unwrap();
        let running = crate::runstate::RunMeta::new(
            "compat-running".into(),
            "a".into(),
            String::new(),
            "t".into(),
            None,
            "/w".into(),
            1,
        );
        crate::runstate::create_run(&running).unwrap();
        let get = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let ids = |body: &serde_json::Value| -> Vec<String> {
            let mut ids: Vec<String> = body
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["run_id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        let (status, all) = send(state_with_agent_paths(Vec::new()), get("/api/agents")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ids(&all), ["compat-done", "compat-running"]);
        let (_, done) = send(
            state_with_agent_paths(Vec::new()),
            get("/api/agents?status=complete,waiting_input"),
        )
        .await;
        assert_eq!(ids(&done), ["compat-done"]);
    })
    .await;
}
