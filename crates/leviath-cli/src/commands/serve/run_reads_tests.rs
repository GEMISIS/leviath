//! `GET /api/runs/{id}` and everything under it, through the real route
//! table, against runs recorded the way the daemon records them.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use tower::ServiceExt;

use super::super::core::run_file::tests::{recorded, step};
use super::*;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};

/// A state that reaches `control`.
fn state_with(control: leviath_runtime::control_socket::ControlClient) -> AppState {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;
    state
}

/// Send `method uri` through the real route table, answering status and body.
async fn call_with(state: AppState, method: Method, uri: &str) -> (StatusCode, String) {
    let app = super::super::api_router().with_state(state);
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// `GET uri` with no daemon to ask.
async fn get(uri: &str) -> (StatusCode, String) {
    call_with(state_with(no_daemon_client()), Method::GET, uri).await
}

/// `GET uri`, as JSON, with the status it came with.
async fn get_json(uri: &str) -> (StatusCode, serde_json::Value) {
    let (status, body) = get(uri).await;
    (status, serde_json::from_str(&body).unwrap_or_default())
}

#[tokio::test]
async fn a_run_its_spec_and_its_graph_are_read_off_its_file() {
    crate::runstate::with_isolated_runs_dir_async("reads-run", |_d| async move {
        let run_id = recorded();
        let (status, run) = get_json(&format!("/api/runs/{run_id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run["run_id"], run_id.as_str());
        assert_eq!(get("/api/runs/ghost").await.0, StatusCode::NOT_FOUND);

        let (status, spec) = get_json(&format!("/api/runs/{run_id}/spec")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(spec["run_id"], run_id.as_str());
        assert_eq!(get("/api/runs/ghost/spec").await.0, StatusCode::NOT_FOUND);

        let (status, graph) = get_json(&format!("/api/runs/{run_id}/graph")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(graph["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(graph["edges"][0]["taken"], 0);
        assert_eq!(get("/api/runs/ghost/graph").await.0, StatusCode::NOT_FOUND);
    })
    .await;
}

#[tokio::test]
async fn a_runs_state_and_steps_are_read_at_any_point() {
    crate::runstate::with_isolated_runs_dir_async("reads-state", |_d| async move {
        let run_id = recorded();
        step(&run_id, 10, Vec::new(), |s| s.title = Some("one".into()));
        step(&run_id, 20, Vec::new(), |s| s.title = Some("two".into()));

        let (status, at_one) = get_json(&format!("/api/runs/{run_id}/state?at=1")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(at_one["title"], "one");
        let (status, now) = get_json(&format!("/api/runs/{run_id}/state")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(now["seq"], 2);
        assert_eq!(
            get(&format!("/api/runs/{run_id}/state?at=7")).await.0,
            StatusCode::RANGE_NOT_SATISFIABLE
        );
        assert_eq!(get("/api/runs/ghost/state").await.0, StatusCode::NOT_FOUND);

        // A run the daemon holds is read live.
        let (client, _dir, _task) = fake_daemon(|req| match req {
            ControlRequest::Inspect { .. } => {
                let mut state = leviath_runtime::state::RunState::initial(
                    leviath_runtime::spec::names::StageName::new("live").unwrap(),
                    Default::default(),
                    true,
                );
                state.title = Some("live".into());
                ControlResponse::State {
                    state: Box::new(state),
                }
            }
            other => panic!("unexpected {other:?}"),
        });
        let (status, body) = call_with(
            state_with(client),
            Method::GET,
            &format!("/api/runs/{run_id}/state"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"live\""), "{body}");

        let (status, deltas) = get_json(&format!("/api/runs/{run_id}/deltas")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(deltas.as_array().unwrap().len(), 2);
        let (_, tail) = get_json(&format!("/api/runs/{run_id}/deltas?from=2&to=2")).await;
        assert_eq!(tail[0]["seq"], 2);
        assert_eq!(
            get(&format!("/api/runs/{run_id}/deltas?from=2&to=1"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(get("/api/runs/ghost/deltas").await.0, StatusCode::NOT_FOUND);
    })
    .await;
}

#[tokio::test]
async fn a_runs_window_history_and_ledger_are_read_off_its_file() {
    crate::runstate::with_isolated_runs_dir_async("reads-window", |_d| async move {
        let run_id = recorded();
        let (status, window) = get_json(&format!("/api/runs/{run_id}/context")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!window["regions"].as_array().unwrap().is_empty());
        assert_eq!(
            get("/api/runs/ghost/context").await.0,
            StatusCode::NOT_FOUND
        );

        let (status, page) = get_json(&format!("/api/runs/{run_id}/context/history")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            get(&format!(
                "/api/runs/{run_id}/context/history?order=sideways"
            ))
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get("/api/runs/ghost/context/history").await.0,
            StatusCode::NOT_FOUND
        );

        let (status, stages) = get_json(&format!("/api/runs/{run_id}/stages")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stages["run_id"], run_id.as_str());
        assert!(stages["stages"].is_array());
        assert_eq!(get("/api/runs/ghost/stages").await.0, StatusCode::NOT_FOUND);

        let (status, children) = get_json(&format!("/api/runs/{run_id}/children")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(children.as_array().unwrap().is_empty());
        let mut child = crate::runstate::RunMeta::new(
            "child-run".into(),
            "worker".into(),
            "/agents/worker".into(),
            "a part of it".into(),
            None,
            "/tmp".into(),
            1,
        );
        child.parent_run_id = Some(run_id.clone());
        crate::runstate::create_run(&child).unwrap();
        let (_, children) = get_json(&format!("/api/runs/{run_id}/children")).await;
        assert_eq!(children[0]["run_id"], "child-run");
    })
    .await;
}

#[tokio::test]
async fn a_runs_logs_result_and_files_are_served() {
    crate::runstate::with_isolated_runs_dir_async("reads-logs", |_d| async move {
        let run_id = recorded();
        let (status, _) = get(&format!("/api/runs/{run_id}/logs?stage=all&stream=logs")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            get(&format!("/api/runs/{run_id}/logs?stage=x")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(&format!("/api/runs/{run_id}/logs?stream=x")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(get("/api/runs/ghost/logs").await.0, StatusCode::NOT_FOUND);

        let (status, result) = get_json(&format!("/api/runs/{run_id}/result")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["run_id"], run_id.as_str());
        assert_eq!(get("/api/runs/ghost/result").await.0, StatusCode::NOT_FOUND);

        // The run works in the system's temp directory.
        let name = format!("lev-reads-{run_id}.txt");
        let file = std::env::temp_dir().join(&name);
        std::fs::write(&file, "a report").unwrap();
        let (status, read) = get_json(&format!("/api/runs/{run_id}/files?path={name}")).await;
        std::fs::remove_file(&file).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(read["content"], "a report");
        let (status, listed) = get_json(&format!("/api/runs/{run_id}/files")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["kind"], "listing");
        let dir = format!("lev-reads-dir-{run_id}");
        std::fs::create_dir_all(std::env::temp_dir().join(&dir)).unwrap();
        let (status, inside) = get_json(&format!("/api/runs/{run_id}/files?path={dir}")).await;
        std::fs::remove_dir_all(std::env::temp_dir().join(&dir)).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(inside["kind"], "listing");
        assert_eq!(
            get(&format!("/api/runs/{run_id}/files?source=sideways"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(&format!("/api/runs/{run_id}/files?source=sideways&path=x"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(&format!(
                "/api/runs/{run_id}/files?source=workdir&path=../../etc"
            ))
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&format!(
                "/api/runs/{run_id}/files?path=lev-no-such-file-{run_id}"
            ))
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(get("/api/runs/ghost/files").await.0, StatusCode::NOT_FOUND);
    })
    .await;
}

/// Pause, resume and cancel each ask the daemon, and answer 204 when it
/// agrees.
#[tokio::test]
async fn a_run_is_paused_resumed_and_cancelled() {
    crate::runstate::with_isolated_runs_dir_async("reads-steer", |_d| async move {
        let run_id = recorded();
        for action in ["pause", "resume", "cancel"] {
            let (client, _dir, _task) = fake_daemon(|_| ControlResponse::Ok { ok: true });
            let (status, body) = call_with(
                state_with(client),
                Method::POST,
                &format!("/api/runs/{run_id}/{action}"),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{action}: {body}");
            let (status, _) = call_with(
                state_with(no_daemon_client()),
                Method::POST,
                &format!("/api/runs/{run_id}/{action}"),
            )
            .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{action}");
        }
    })
    .await;
}
