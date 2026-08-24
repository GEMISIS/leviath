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
pub(super) async fn get_update() -> Json<serde_json::Value> {
    // `--check` is implied: this route only ever plans. The other fields are
    // defaults, which is what a caller who is not choosing a channel means.
    let args = UpdateArgs {
        check: true,
        ..UpdateArgs::default()
    };
    Json(plan_json(
        &plan(&args, &UpdateEnv::for_planning()),
        API_VERSION,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt;

    /// The route answers, and answers with the shape the console reads: an
    /// install method it can name, and a command it can print.
    #[tokio::test]
    async fn get_update_reports_the_install_method_and_a_command() {
        let app = Router::new().route("/api/update", get(get_update));
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
        let from_cli = plan_json(&plan(&args, &env), API_VERSION);

        let app = Router::new().route("/api/update", get(get_update));
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
