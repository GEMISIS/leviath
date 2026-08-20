//! `GET /api/doctor` - the browser's "check my setup" button.
//!
//! Runs exactly the checks `lev doctor` runs
//! ([`crate::commands::doctor::run_checks`]), semantics preserved, and returns
//! them as data instead of a table. See that module for what each layer proves.

use axum::extract::State;
use axum::response::Json;

use super::types::*;
use crate::commands::doctor::{CheckStatus, DaemonTarget, DoctorArgs, run_checks};
use crate::commands::run::session::build_provider_registry_from_config;

/// `GET /api/doctor`: prove the provider wiring works, one layer at a time.
///
/// A failing check is an `ok: false` entry in a 200, never an HTTP error - the
/// endpoint answering at all is not the thing being diagnosed. The config is
/// re-read per request (not taken from [`AppState`]) so the button reflects an
/// edit the user just made.
///
/// Live-call behavior is `lev doctor`'s own, bounded by its own deadlines:
/// `inference` makes one real, billed provider call under the probe's 60s
/// request timeout, and `daemon` spawns a throwaway one-stage run (a second
/// billed call) through this server's control socket, waited on for at most
/// the doctor's 90s. A daemon that is not running reports as a failing
/// `daemon` check rather than skipping it.
pub(super) async fn run_doctor(State(state): State<AppState>) -> Json<DoctorResp> {
    let checks = run_checks(
        &DoctorArgs::default(),
        &build_provider_registry_from_config,
        DaemonTarget::Client(&state.control),
    )
    .await;
    Json(DoctorResp {
        checks: checks
            .into_iter()
            .map(|c| DoctorCheck {
                name: c.name.to_string(),
                // Keyed on "did not fail", so a degraded-but-working layer
                // reports the same verdict here as the CLI's exit code gives
                // it. The detail carries what is degraded.
                ok: c.status != CheckStatus::Fail,
                detail: c.detail,
                elapsed_ms: c.elapsed_ms,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::path::PathBuf;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::config::Config;

    /// Run `f` with the config path, data root, and runs directory all
    /// redirected into one fresh temp root, and every provider key cleared -
    /// the same combined-vars shape as the `lev doctor` tests, because
    /// `run_checks` reaches `Config::load()` and no test may make a billed
    /// call. One `temp_env` call, not two: it serializes process-wide and
    /// holds its lock across the future, so nesting
    /// `with_isolated_config_path_async` inside a second call would deadlock.
    async fn with_env<R, Fut>(f: impl FnOnce(PathBuf) -> Fut) -> R
    where
        Fut: std::future::Future<Output = R>,
    {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().to_path_buf();
        let mut vars = crate::config::config_isolation_vars(&root);
        vars.push(("LEVIATH_HOME", Some(root.clone().into_os_string())));
        vars.push(("LEVIATH_RUNS_DIR", Some(root.join("runs").into_os_string())));
        // The search check reads this, so a developer who has one exported
        // would otherwise see a different check list than CI does.
        vars.push(("BRAVE_API_KEY", None));
        temp_env::async_with_vars(vars, f(root)).await
    }

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel(64);
        AppState {
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: leviath_runtime::control_socket::ControlClient::new(
                leviath_runtime::control_socket::control_id(std::path::Path::new(
                    "/no/such/daemon",
                )),
            ),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            limits: Default::default(),
        }
    }

    /// The endpoint over the shared production route table, against an empty
    /// isolated config: `config` parses (ok), `search` warns (no key in the
    /// isolated environment), `resolve` fails (nothing
    /// registered answers to the default provider), and the chain stops there -
    /// before any billed call. The failure arrives as an `ok: false` entry in
    /// a 200, which is the endpoint's whole contract.
    #[tokio::test]
    async fn doctor_reports_failing_checks_as_data_not_http_errors() {
        with_env(|_root| async move {
            let app = super::super::api_router().with_state(test_state());
            let req = Request::builder()
                .uri("/api/doctor")
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);

            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let report: DoctorResp = serde_json::from_slice(&body).unwrap();

            assert_eq!(
                report.checks.len(),
                3,
                "config ok, search warn, resolve fail, stop"
            );
            let config_check = &report.checks[0];
            assert_eq!(config_check.name, "config");
            assert!(config_check.ok, "{}", config_check.detail);
            // A warning is not a failure over the API either.
            let search_check = &report.checks[1];
            assert_eq!(search_check.name, "search");
            assert!(search_check.ok, "{}", search_check.detail);
            let resolve_check = &report.checks[2];
            assert_eq!(resolve_check.name, "resolve");
            assert!(!resolve_check.ok);
            assert!(
                resolve_check.detail.contains("not configured"),
                "{}",
                resolve_check.detail
            );
            // The offline checks carry no timing.
            assert!(report.checks.iter().all(|c| c.elapsed_ms.is_none()));
        })
        .await;
    }
}
