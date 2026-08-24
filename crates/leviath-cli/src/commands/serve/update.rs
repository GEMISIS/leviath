//! `GET /api/update` - "am I current, and what do I type to fix it".
//!
//! Answers with exactly what `lev update --check --json` prints, from the same
//! [`crate::commands::update::plan`], so the console and the CLI can never
//! disagree about how this copy was installed.

use axum::response::Json;

use super::config_types::API_VERSION;
use crate::commands::update::{UpdateArgs, UpdateEnv, plan, plan_json};

/// `GET /api/update`: how this copy was installed, and the command that
/// upgrades it.
///
/// Exists because the browser console had no way to ask. It printed a single
/// hard-coded `brew upgrade leviath` at anyone whose Leviath was out of date,
/// which is an instruction a Windows user cannot carry out and has no
/// alternative offered for (issue #588) - while `lev update` on the very same
/// binary already knew to say `scoop update leviath`, or to re-run
/// `install.ps1`, or that a `cargo install` copy is theirs to rebuild.
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
pub(super) async fn get_update(
    axum::extract::State(state): axum::extract::State<super::types::AppState>,
) -> Json<serde_json::Value> {
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
    // Reports what is known now and, if that has gone stale, starts a lookup
    // for whoever asks next. Never waits on one.
    state
        .update_check
        .read_and_maybe_refresh(plan.method.channel(), API_VERSION);
    Json(plan_json(&plan, API_VERSION, &state.update_check.peek()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::update::latest::LatestCheck;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::routing::get;
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
            config: crate::commands::serve::testutil::fixed_config(crate::config::Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
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
    /// from the outside, which is what issue #600 is about.
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
}
