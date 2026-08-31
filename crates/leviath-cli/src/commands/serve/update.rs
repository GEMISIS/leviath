//! The update routes: what an update would do, and doing it.
//!
//! `GET /api/update` answers with exactly what `lev update --check --json`
//! prints, from the same [`crate::commands::update::plan`], so the console and
//! the CLI can never disagree about how this copy was installed. `POST
//! /api/update` carries that plan out, behind `--allow-admin`; the machinery
//! for it is in [`super::update_job`].

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};

use super::config_types::API_VERSION;
use super::types::{AppState, err};
use super::update_job::{ApplyRequest, parse_request};
use crate::commands::update::{UpdateArgs, UpdateEnv, plan, plan_json};

/// `GET /api/update`: how this copy was installed, and the command that
/// upgrades it.
///
/// Exists so a browser console never has to guess. A hard-coded
/// `brew upgrade leviath` is an instruction a Windows user cannot carry out
/// and has no alternative offered for, while `lev update` on the very same
/// binary knows to say `scoop update leviath`, or to re-run `install.ps1`, or
/// that a `cargo install` copy is theirs to rebuild.
///
/// Read-only, and so not behind `--allow-admin`: [`plan`] works out what the
/// command *would* do and does none of it, the environment it is handed cannot
/// run anything ([`UpdateEnv::for_planning`]), and no network call is made. The
/// auth token is the boundary here, the same as for every other read route.
///
/// The body is `plan_json` unchanged rather than a trimmed "just the command"
/// shape: one shape means one thing to document, and no drift the day the CLI
/// gains a field.
///
/// Cannot fail, which is why it returns a bare `Json`. Every step of the
/// discovery degrades to worse detection rather than to an error - a copy this
/// build cannot place is `unknown` with advice, an answer rather than a 500.
pub(super) async fn get_update(State(state): State<AppState>) -> Json<serde_json::Value> {
    // `--check` is implied: this route only ever plans. The other fields are
    // defaults, which is what a caller who is not choosing a channel means.
    let args = UpdateArgs {
        check: true,
        ..UpdateArgs::default()
    };
    // `for_planning_offline` rather than `for_planning`: planning never fetches,
    // and handing this path a fetcher it is trusted not to call is how a later
    // edit turns a fast route into a slow one with nothing to catch it.
    let plan = plan(&args, &UpdateEnv::for_planning_offline());
    // Reports what is known now and, if that has gone stale, starts a lookup for
    // whoever asks next. Never waits on one.
    //
    // The config is read per request rather than at startup, so turning the
    // check off takes effect on the next page load instead of on the next
    // restart - the same way every other setting this server reads behaves.
    if state.current_config().update_check {
        state
            .update_check
            .read_and_maybe_refresh(plan.method.channel(), API_VERSION);
    }
    Json(plan_json(&plan, API_VERSION, &state.update_check.peek()))
}

/// `POST /api/update`: carry the plan out.
///
/// Answers `202 Accepted` with a job id and lets the work run behind it. The
/// alternative is a request held open for a `brew update && brew upgrade`,
/// which is a download and an install: a minute on a good day, and a console
/// showing "downloading" for a request that has not returned is a console
/// showing a spinner it made up. Every step change goes out on `/ws` as
/// `update_progress`, the last as `update_finished`, and
/// [`get_update_job`] answers the same record for a client that would rather
/// poll.
///
/// Behind `--allow-admin`, which is the line this route crosses and
/// `GET /api/update` does not: that one plans and acts on nothing, this one
/// runs a package manager and rewrites the agents directory and the config.
///
/// `409` while another update is going. Two package-manager upgrades of the
/// same binary racing each other is not a state to debug, and a double-clicked
/// button means one update.
pub(super) async fn post_update(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let req = match parse_request(&body) {
        Ok(req) => req,
        Err(message) => return err(StatusCode::BAD_REQUEST, message).into_response(),
    };
    match state.update_jobs.spawn(req, &state.event_tx) {
        Ok(job_id) => (StatusCode::ACCEPTED, Json(started(&job_id, req))).into_response(),
        Err(running) => err(
            StatusCode::CONFLICT,
            format!("update {running} is already running"),
        )
        .into_response(),
    }
}

/// The `202` body: the id to watch, and the request as it was read.
///
/// Echoing the three flags back is not decoration. Every one defaults to on, so
/// a caller that sent nothing, or sent a body this route read differently than
/// it meant, learns which update it actually started at the moment it starts -
/// rather than from the steps that turn out to be `skipped`.
fn started(job_id: &str, req: ApplyRequest) -> serde_json::Value {
    serde_json::json!({
        "job_id": job_id,
        "status": "running",
        "applying": {
            "binary": req.binary,
            "agents": req.agents,
            "migrations": req.migrations,
        },
    })
}

/// `GET /api/update/jobs/{id}`: where one update run got to.
///
/// The websocket says it sooner; this is for a client that connected late,
/// dropped a frame, or does not hold a socket open at all. Same record either
/// way, so there is no second shape to keep in step.
///
/// The last few runs are kept, so an operator who reads back after the fact
/// finds the job rather than a 404 that means nothing in particular.
pub(super) async fn get_update_job(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> axum::response::Response {
    match state.update_jobs.get(&id) {
        Some(job) => Json(job).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("no update job {id}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::update::latest::LatestCheck;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    /// A state with nothing behind it but the update cache, which is all this
    /// route reads. The cache declines to look anything up, so these tests
    /// assert on the route's own shape and never reach the network.
    /// Stands in for the network in these tests. A refusal rather than a no-op
    /// that would quietly answer: a route test that reached a real release feed
    /// would pass or fail on what GitHub happened to be serving.
    fn declines(_: &str) -> Result<String, String> {
        Err("no network in tests".to_string())
    }

    fn test_state() -> super::super::types::AppState {
        let (tx, _) = tokio::sync::broadcast::channel(64);
        super::super::types::AppState {
            update_check: super::super::update_cache::UpdateCheckCache::with_fetcher(
                std::sync::Arc::new(declines),
            ),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(crate::config::Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        }
    }

    /// The state these tests serve from must not be able to reach the network,
    /// so the thing standing in for it is a refusal rather than a no-op that
    /// would quietly answer.
    #[test]
    fn the_test_state_cannot_reach_the_network() {
        let state = test_state();
        assert_eq!(state.update_check.peek(), Default::default());
        assert!(declines("https://example.invalid").is_err());
    }

    /// With the check switched off in config, the route still answers - it just
    /// never looks anything up.
    ///
    /// The whole route degrading to a 500, or to a missing key, would be worse
    /// than the guessing this replaced: a client cannot tell "switched off" from
    /// "this daemon is too old to ask" unless the keys are there and `null`.
    #[tokio::test]
    async fn a_config_that_turns_the_check_off_still_answers() {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = {
            let asked = std::sync::Arc::clone(&asked);
            std::sync::Arc::new(move |_: &str| {
                asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(r#"{"name": "9.9.9"}"#.to_string())
            })
        };
        // Called once here so the counter starts from a known, non-zero place.
        // A fetcher that is never called at all leaves its body unexecuted,
        // which reads as untested code rather than as the point of the test -
        // and this way the assertion below is that the count did not MOVE,
        // which is the same claim without that gap.
        assert!(counting("https://example.invalid/probe").is_ok());
        let baseline = asked.load(std::sync::atomic::Ordering::SeqCst);

        let (tx, _) = tokio::sync::broadcast::channel(64);
        let state = super::super::types::AppState {
            update_check: super::super::update_cache::UpdateCheckCache::with_fetcher(counting),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(crate::config::Config {
                update_check: false,
                ..crate::config::Config::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        };

        let app = Router::new()
            .route("/api/update", get(get_update))
            .with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/update")
                    .body(Body::empty())
                    .expect("a GET with no body always builds"),
            )
            .await
            .expect("the router is infallible");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("the body is a small JSON document");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the route answers with JSON");

        assert_eq!(
            asked.load(std::sync::atomic::Ordering::SeqCst),
            baseline,
            "switched off means the route looked nothing up"
        );
        for key in ["latest", "update_available", "checked_at"] {
            assert_eq!(
                body.get(key),
                Some(&serde_json::Value::Null),
                "`{key}` is present and null, which is an answer a client can render"
            );
        }
        assert!(
            body.get("install_method").is_some(),
            "the rest of the answer is unaffected"
        );
    }

    /// The route answers, and answers with the shape the console reads: an
    /// install method it can name, and a command it can print.
    #[tokio::test]
    async fn get_update_reports_the_install_method_and_a_command() {
        let app = Router::new()
            .route("/api/update", get(get_update))
            .with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/update")
                    .body(Body::empty())
                    .expect("a GET with no body always builds"),
            )
            .await
            .expect("the router is infallible");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("the body is a small JSON document");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the handler serializes a plan");

        // The version is this build's, so a console can compare without a
        // second request.
        assert_eq!(body["version"], API_VERSION);
        // Whatever this test machine is, the method is one of the five the CLI
        // knows and the binary step is one of the two shapes it produces.
        // Asserting on membership rather than a value is the point: enumerating
        // "it is homebrew here" would pass on a laptop and fail on a runner.
        let method = body["install_method"]
            .as_str()
            .expect("install_method is always a string");
        assert!(["homebrew", "scoop", "cargo", "script", "unknown"].contains(&method));
        let action = body["binary"]["action"]
            .as_str()
            .expect("binary.action is always a string");
        assert!(["run", "advise"].contains(&action));
    }

    /// The three fields a console needs to decide whether to say anything are
    /// present and answerable, even before any check has run.
    ///
    /// Absent keys and `null` keys are different things to a client: `null` is
    /// "cannot tell", which it already knows how to render, while a missing key
    /// is indistinguishable from an older daemon and sends it back to guessing
    /// from the outside.
    #[tokio::test]
    async fn the_update_check_fields_are_always_present_even_when_unanswered() {
        let app = Router::new()
            .route("/api/update", get(get_update))
            .with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/update")
                    .body(Body::empty())
                    .expect("a GET with no body always builds"),
            )
            .await
            .expect("the router is infallible");
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("the body is a small JSON document");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the route answers with JSON");

        for key in ["latest", "update_available", "checked_at"] {
            assert!(
                body.get(key).is_some(),
                "`{key}` is missing, which a client cannot tell from an older daemon"
            );
        }
    }

    /// The console reads this to decide what to print, so it has to be the same
    /// answer `lev update --json` gives on the same machine. A route that
    /// drifted from the CLI would send people a command their own terminal
    /// disagrees with.
    #[tokio::test]
    async fn get_update_agrees_with_what_the_cli_would_print() {
        let env = UpdateEnv::for_planning();
        let args = UpdateArgs {
            check: true,
            ..UpdateArgs::default()
        };
        let from_cli = plan_json(&plan(&args, &env), API_VERSION, &LatestCheck::default());

        let app = Router::new()
            .route("/api/update", get(get_update))
            .with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/update")
                    .body(Body::empty())
                    .expect("a GET with no body always builds"),
            )
            .await
            .expect("the router is infallible");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("the body is a small JSON document");
        let from_route: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the handler serializes a plan");
        assert_eq!(from_route, from_cli);
    }

    // ─── POST /api/update, and reading a job back ────────────────────────────

    /// A state whose update jobs act on a temp directory, so a route test
    /// cannot install a blueprint into the developer's own agents directory or
    /// run a package manager.
    fn applying_state() -> (tempfile::TempDir, super::super::types::AppState) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (agents_dir, config_path) = (dir.path().join("agents"), dir.path().join("config.toml"));
        let jobs = super::super::update_job::UpdateJobs::with_env(std::sync::Arc::new(move || {
            crate::commands::update::UpdateEnv {
                // Nowhere an installer writes, so the binary step is advice and
                // no command is ever handed to the runner.
                exe: std::path::PathBuf::from("/nowhere/at/all/lev"),
                agents_dir: agents_dir.clone(),
                config_path: config_path.clone(),
                ..crate::commands::update::UpdateEnv::for_planning()
            }
        }));
        let state = super::super::types::AppState {
            update_jobs: jobs,
            ..test_state()
        };
        (dir, state)
    }

    /// The two routes as production mounts them, so a test cannot pass against
    /// a path production does not serve.
    fn update_app(state: super::super::types::AppState) -> Router {
        Router::new()
            .route("/api/update", post(post_update))
            .route("/api/update/jobs/{id}", get(get_update_job))
            .with_state(state)
    }

    /// The status and body of one request.
    async fn call(app: Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("the body is a small JSON document");
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// A `POST` with a body.
    fn post_with(body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/update")
            .body(Body::from(body))
            .expect("a POST with a body always builds")
    }

    /// The route answers at once with an id to watch, and the request as it
    /// read it - so a caller learns which update it started rather than
    /// inferring it from the steps that turn out skipped.
    #[tokio::test]
    async fn post_update_answers_202_with_a_job_id_and_what_it_is_applying() {
        let (_dir, state) = applying_state();
        let jobs = state.update_jobs.clone();
        let (status, body) = call(update_app(state), post_with(r#"{"agents": false}"#)).await;

        assert_eq!(status, StatusCode::ACCEPTED);
        let id = body["job_id"].as_str().expect("a job id is a string");
        assert_eq!(body["status"], "running");
        assert_eq!(body["applying"]["binary"], true);
        assert_eq!(body["applying"]["agents"], false);
        assert_eq!(body["applying"]["migrations"], true);
        assert!(jobs.get(id).is_some(), "the job is readable straight away");
    }

    /// An empty body is the whole plan, which is what a console's plain
    /// "update" button sends.
    #[tokio::test]
    async fn post_update_with_no_body_applies_everything() {
        let (_dir, state) = applying_state();
        let (status, body) = call(update_app(state), post_with("")).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        for part in ["binary", "agents", "migrations"] {
            assert_eq!(body["applying"][part], true, "{part}");
        }
    }

    /// A body this route cannot read is a 400 that says so, rather than a
    /// silent default that would run an update the caller did not ask for.
    #[tokio::test]
    async fn post_update_refuses_a_body_it_cannot_read() {
        let (_dir, state) = applying_state();
        let (status, body) = call(update_app(state), post_with(r#"{"agent": false}"#)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains("could not read the request body")),
            "{body}"
        );
    }

    /// One update at a time, and the refusal names the one already going so a
    /// console can watch that instead of starting a second package manager over
    /// the same binary.
    #[tokio::test]
    async fn post_update_refuses_a_second_while_one_is_running() {
        let (_dir, state) = applying_state();
        let running = state
            .update_jobs
            .start()
            .expect("nothing is running to begin with");
        let (status, body) = call(update_app(state), post_with("")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains(&running) && e.contains("already running")),
            "{body}"
        );
    }

    /// The poll route answers the same record the finish frame carries.
    #[tokio::test]
    async fn a_job_can_be_read_back_by_id() {
        let (_dir, state) = applying_state();
        let id = state.update_jobs.start().expect("nothing is running");
        let request = Request::builder()
            .uri(format!("/api/update/jobs/{id}"))
            .body(Body::empty())
            .expect("a GET with no body always builds");
        let (status, body) = call(update_app(state), request).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], id);
        assert_eq!(body["status"], "running");
        assert_eq!(body["restart_required"], false);
        assert_eq!(body["finished_at"], serde_json::Value::Null);
        let steps: Vec<&str> = body["steps"]
            .as_array()
            .expect("steps is a list")
            .iter()
            .map(|step| step["step"].as_str().expect("a step names itself"))
            .collect();
        assert_eq!(steps, vec!["binary", "agents", "migrations"]);
    }

    /// An id nobody minted is a 404 that names it, not an empty 200 a client
    /// would render as a job that did nothing.
    #[tokio::test]
    async fn an_unknown_job_id_is_a_404() {
        let (_dir, state) = applying_state();
        let request = Request::builder()
            .uri("/api/update/jobs/update-never-1")
            .body(Body::empty())
            .expect("a GET with no body always builds");
        let (status, body) = call(update_app(state), request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains("update-never-1")),
            "{body}"
        );
    }
}
