//! `POST /api/runs/{id}/continue` and `GET /api/runs/{id}/segments`.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use tower::ServiceExt;

use super::*;
use crate::commands::serve::testutil::{fake_daemon, state_with_agent_paths, with_home};

fn app(control: ControlClient) -> Router {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;
    Router::new()
        .route("/api/runs/{id}/continue", post(continue_run))
        .route("/api/runs/{id}/segments", get(run_segments))
        .with_state(state)
}

/// Send `body` to the continue route of run `r-1`, answering `reply`.
async fn post_continue(
    body: serde_json::Value,
    reply: impl Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
) -> (StatusCode, serde_json::Value) {
    let (control, _socket, _srv) = fake_daemon(reply);
    let request = Request::builder()
        .method("POST")
        .uri("/api/runs/r-1/continue")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app(control).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn a_continuation_answers_202_and_a_fork_201() {
    with_home(|_| async {
        let reply = |request: ControlRequest| match request {
            ControlRequest::Continue {
                run_id,
                text,
                region,
                fork,
                ..
            } => {
                assert_eq!(text, "make it red");
                assert_eq!(region.as_deref(), Some("notes"));
                ControlResponse::Continued {
                    run_id: match fork {
                        true => "r-2".into(),
                        false => run_id.clone(),
                    },
                    segment: 2,
                    forked_from: fork.then_some(run_id),
                }
            }
            _ => ControlResponse::Ok { ok: false },
        };
        let body = serde_json::json!({"message": "make it red", "target_region": "notes"});
        let (status, answer) = post_continue(body.clone(), reply).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(answer["run_id"], "r-1");
        assert_eq!(answer["segment"], 2);
        assert!(answer.get("forked_from").is_none());
        assert!(answer["server_instance"].is_string());

        let mut forked = body;
        forked["fork"] = serde_json::json!(true);
        let (status, answer) = post_continue(forked, reply).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(answer["forked_from"], "r-1");
    })
    .await;
}

#[tokio::test]
async fn a_refused_continuation_answers_the_error_that_says_why() {
    with_home(|_| async {
        let (status, _) = post_continue(serde_json::json!({"message": "m"}), |_| {
            ControlResponse::Rejected {
                issues: leviath_runtime::continuation::refused(
                    leviath_runtime::spec::issues::IssueCode::Conflict,
                    leviath_runtime::spec::issues::SpecPath::root(),
                    "run 'r-1' has not finished",
                ),
            }
        })
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // A part named inside a workdir the server cannot see is a miss.
        let body = serde_json::json!({"message": "m", "parts": [{"path": "a.png"}]});
        let (status, _) = post_continue(body, |_| ControlResponse::Ok { ok: true }).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // A body that is not a continuation is the caller's mistake.
        let (status, _) = post_continue(serde_json::json!({"message": 3}), |_| {
            ControlResponse::Ok { ok: true }
        })
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

/// With no daemon to ask, a continuation fails as the daemon being away.
#[tokio::test]
async fn a_continuation_with_no_daemon_is_unavailable() {
    with_home(|_| async {
        let request = Request::builder()
            .method("POST")
            .uri("/api/runs/r-1/continue")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"message":"m"}"#))
            .unwrap();
        let response = app(crate::commands::serve::testutil::no_daemon_client())
            .oneshot(request)
            .await
            .unwrap();
        assert!(response.status().is_server_error(), "{}", response.status());
    })
    .await;
}

#[tokio::test]
async fn the_segments_of_a_run_are_listed_and_a_missing_run_is_a_miss() {
    with_home(|_| async {
        let get_segments = |id: &str| {
            Request::builder()
                .uri(format!("/api/runs/{id}/segments"))
                .body(Body::empty())
                .unwrap()
        };
        let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        let response = app(control.clone())
            .oneshot(get_segments("nope"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let mut meta = crate::test_fixtures::fixtures::run_meta("r-1");
        meta.status = leviath_core::run_meta::RunStatus::Complete;
        crate::runstate::create_run(&meta).unwrap();
        let response = app(control).oneshot(get_segments("r-1")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(listed[0]["index"], 1);
        assert_eq!(listed[0]["status"], "complete");
        assert_eq!(listed[0]["current"], true);
    })
    .await;
}

/// Every status a segment can stand in reads as the run's own word.
#[test]
fn every_segment_status_reads_as_a_run_status_word() {
    use leviath_runtime::state::RunStatus as S;
    let words: Vec<String> = [
        S::Idle,
        S::Active,
        S::Waiting,
        S::Paused,
        S::Complete,
        S::Error("x".into()),
        S::Cancelled,
    ]
    .into_iter()
    .map(|status| {
        let segment = leviath_runtime::runfile::Segment {
            index: 1,
            stage: leviath_runtime::spec::names::StageName::new("s").unwrap(),
            started_seq: 0,
            started_at: 0,
            forked_from: Some(leviath_runtime::spec::names::RunId::new("src").unwrap()),
            status,
            spend: Default::default(),
            tool_calls: 0,
            final_output: Some(leviath_runtime::state::FinalOutputState {
                bytes: 2,
                format: None,
                stage: leviath_runtime::spec::names::StageName::new("s").unwrap(),
                submitted_at: 1,
                truncated: false,
                artifacts: vec![leviath_runtime::state::journal::ArtifactState {
                    name: "cape.png".into(),
                    path: "out/cape.png".into(),
                    mime_type: "image/png".into(),
                    size: 3,
                    sha256: String::new(),
                }],
            }),
            file: None,
            last_seq: 0,
            current: true,
        };
        let shown = SegmentResp::from(SegmentRead {
            segment,
            answer: Some("ok".into()),
        });
        assert_eq!(shown.artifacts, ["cape.png"]);
        assert_eq!(shown.forked_from.as_deref(), Some("src"));
        shown.status
    })
    .collect();
    assert_eq!(
        words,
        [
            "starting",
            "running",
            "waiting_input",
            "paused",
            "complete",
            "error",
            "cancelled"
        ]
    );
}
