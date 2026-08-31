//! The provider routes, driven end to end against a local issuer.
//!
//! The sign-in is real: a mock issuer, the stub browser the login flow already
//! had, and port zero instead of the two registered ones. Nothing opens and
//! nothing reaches OpenAI, and the path the Lair will drive is the path under
//! test rather than a mocked stand-in for it.

use super::*;
use crate::commands::auth::codex::tests::{browser_that_redirects, id_token};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::{get, post};
use leviath_providers::ProviderGrant;
use leviath_providers::codex::ProviderAuthStore;
use tokio::sync::broadcast;
use tower::ServiceExt as _;

use crate::config::Config;

/// A fixed clock, so a recorded timestamp is an assertion rather than a
/// reading of the wall.
fn fixed_now() -> u64 {
    1_700_000_000
}

/// An authorizer pointed at `issuer`, writing into `dir`, with `opener`
/// playing the browser.
fn authorizer(
    dir: &std::path::Path,
    issuer: String,
    opener: leviath_mcp::BrowserOpener,
) -> LiveAuthorizer {
    LiveAuthorizer {
        opener,
        store_path: Some(dir.join("provider-auth.json")),
        credential_store: Ok(None),
        client: reqwest::Client::new(),
        issuer,
        // The registered ports are the production value; a test that bound
        // them would fight whatever the developer has signed in.
        ports: vec![0],
    }
}

/// App state carrying `admin`, with the rest stubbed.
fn state_with(admin: ProviderAdmin, config: Config) -> AppState {
    let (tx, _) = broadcast::channel(16);
    AppState {
        update_check: Default::default(),
        update_jobs: Default::default(),
        config: crate::commands::serve::testutil::fixed_config(config),
        event_tx: tx,
        control: crate::commands::serve::testutil::no_daemon_client(),
        mcp: crate::commands::serve::mcp::McpAdmin::default(),
        providers: admin,
        limits: Default::default(),
    }
}

fn admin_over(authorizer: LiveAuthorizer) -> ProviderAdmin {
    ProviderAdmin {
        authorizer: Arc::new(authorizer),
        in_flight: Arc::new(Mutex::new(HashMap::new())),
        now: fixed_now,
        usage_url: None,
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/providers", get(list_providers))
        .route("/api/providers/{name}/login", post(login))
        .route("/api/providers/{name}/logout", post(logout))
        .route("/api/providers/{name}/check", post(check))
        .with_state(state)
}

async fn send(app: &Router, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// The issuer's token response, for a sign-in that is meant to succeed.
async fn mock_issuer() -> String {
    let body = serde_json::json!({
        "access_token": "at-1",
        "refresh_token": "rt-1",
        "id_token": id_token(),
        "expires_in": 3600,
    });
    leviath_testkit::spawn_mock_server(200, "OK", body.to_string().into_bytes()).await
}

/// Write a grant straight into the store, for the routes that read one.
fn store_grant(dir: &std::path::Path) {
    let mut store = ProviderAuthStore::default();
    store.set(
        "codex",
        ProviderGrant {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            email: Some("someone@example.com".to_string()),
            plan_type: Some("plus".to_string()),
            ..Default::default()
        },
    );
    store.save(&dir.join("provider-auth.json")).unwrap();
}

/// The listing separates "turned on" from "signed in", which is the whole
/// point: either can be true alone, and a console needs to tell them apart to
/// know which button to offer.
#[tokio::test]
async fn the_listing_reports_enabled_and_signed_in_separately() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.providers.codex_enabled = true;
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| true),
        )),
        config,
    ));

    let (status, body) = send(&app, "GET", "/api/providers").await;
    assert_eq!(status, StatusCode::OK);
    let first = &body["providers"][0];
    assert_eq!(first["id"], "codex");
    assert_eq!(first["enabled"], true);
    assert_eq!(first["signed_in"], false, "enabled is not signed in");
    assert!(first["account"].is_null());
    assert!(first["signin"].is_null(), "nothing is in flight");

    store_grant(dir.path());
    let (_, body) = send(&app, "GET", "/api/providers").await;
    let first = &body["providers"][0];
    assert_eq!(first["signed_in"], true);
    assert_eq!(first["account"], "someone@example.com");
    assert_eq!(first["plan"], "plus");
}

/// The listing carries no token. It is open to any caller holding the bearer,
/// which is a wider audience than the admin routes, so what it discloses is
/// worth pinning.
#[tokio::test]
async fn the_listing_never_carries_a_token() {
    let dir = tempfile::tempdir().unwrap();
    store_grant(dir.path());
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| true),
        )),
        Config::default(),
    ));

    let (_, body) = send(&app, "GET", "/api/providers").await;
    let text = body.to_string();
    for secret in ["at", "rt"] {
        assert!(
            !text.contains(&format!("\"{secret}\"")),
            "a token reached the listing: {text}"
        );
    }
}

/// The whole flow the Lair drives: start it, get a URL back without waiting
/// for the browser, and watch the listing turn over to signed in.
#[tokio::test]
async fn a_login_returns_a_url_at_once_and_the_listing_settles() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            mock_issuer().await,
            browser_that_redirects(Arc::clone(&seen)),
        )),
        Config::default(),
    ));

    let (status, body) = send(&app, "POST", "/api/providers/codex/login").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "waiting");
    let url = body["authorize_url"].as_str().expect("a URL to open");
    assert!(url.contains("code_challenge"), "{url}");

    // The flow is still running behind the response, so the listing settles a
    // moment later rather than immediately.
    let mut signed_in = false;
    for _ in 0..100 {
        let (_, body) = send(&app, "GET", "/api/providers").await;
        if body["providers"][0]["signed_in"] == true {
            assert_eq!(body["providers"][0]["account"], "someone@example.com");
            assert!(
                body["providers"][0]["signin"].is_null(),
                "the finished flow left a state behind: {body}"
            );
            signed_in = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(signed_in, "the sign-in never landed");
}

/// A flow that cannot reach its issuer reports why, and leaves the reason on
/// the listing rather than only in the response nobody kept.
#[tokio::test]
async fn a_failed_login_is_reported_and_then_visible_on_the_listing() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            // Nothing listens on port 1, so the code exchange fails.
            "http://127.0.0.1:1".to_string(),
            browser_that_redirects(Arc::clone(&seen)),
        )),
        Config::default(),
    ));

    let (status, _) = send(&app, "POST", "/api/providers/codex/login").await;
    assert_eq!(status, StatusCode::ACCEPTED, "it started before it failed");

    let mut failed = false;
    for _ in 0..100 {
        let (_, body) = send(&app, "GET", "/api/providers").await;
        if body["providers"][0]["signin"]["state"] == "failed" {
            assert!(
                !body["providers"][0]["signin"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty()
            );
            assert_eq!(body["providers"][0]["signed_in"], false);
            failed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(failed, "the failure never surfaced");
}

/// A second sign-in while one is waiting is refused with the URL of the one
/// already running, so a console that lost the response can pick it back up.
#[tokio::test]
async fn a_second_login_is_refused_with_the_url_of_the_first() {
    let dir = tempfile::tempdir().unwrap();
    // A browser that never redirects, so the first flow stays waiting.
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| false),
        )),
        Config::default(),
    ));

    let (status, first) = send(&app, "POST", "/api/providers/codex/login").await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, second) = send(&app, "POST", "/api/providers/codex/login").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(second["authorize_url"], first["authorize_url"]);
}

/// A name nothing signs in with is a 404 on every route, rather than being
/// handed to a flow written for one provider.
#[tokio::test]
async fn an_unknown_provider_is_a_404_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| panic!("no browser may open")),
        )),
        Config::default(),
    ));

    for route in [
        "/api/providers/anthropic/login",
        "/api/providers/anthropic/logout",
    ] {
        let (status, _) = send(&app, "POST", route).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
    }
}

/// Signing out forgets the grant and leaves `config.toml` alone, the same as
/// `lev auth logout`.
#[tokio::test]
async fn signing_out_forgets_the_grant_and_not_the_setting() {
    let dir = tempfile::tempdir().unwrap();
    store_grant(dir.path());
    let mut config = Config::default();
    config.providers.codex_enabled = true;
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| panic!("no browser may open")),
        )),
        config,
    ));

    let (status, body) = send(&app, "POST", "/api/providers/codex/logout").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "signed_out");

    let (_, body) = send(&app, "GET", "/api/providers").await;
    assert_eq!(body["providers"][0]["signed_in"], false);
    assert_eq!(
        body["providers"][0]["enabled"], true,
        "signing out turned the provider off"
    );
}

/// A sign-out that cannot read the store says so rather than reporting that
/// there was nothing to forget.
#[tokio::test]
async fn a_sign_out_that_cannot_read_the_store_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("provider-auth.json"), "{ not json").unwrap();
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| panic!("no browser may open")),
        )),
        Config::default(),
    ));

    let (status, _) = send(&app, "POST", "/api/providers/codex/logout").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// A login whose flow ends before it has a URL - no home to write into -
/// reports the reason rather than the timeout.
#[tokio::test]
async fn a_login_that_never_starts_reports_why() {
    let mut authorizer = authorizer(
        std::path::Path::new("/unused"),
        "http://127.0.0.1:1".to_string(),
        Arc::new(|_: &str| panic!("no browser may open")),
    );
    authorizer.store_path = None;
    let app = router(state_with(admin_over(authorizer), Config::default()));

    let (status, body) = send(&app, "POST", "/api/providers/codex/login").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("LEVIATH_HOME"),
        "{body}"
    );
}

/// The timestamps come from the injected clock, so a recorded `started_at` is
/// something a test can assert rather than read off the wall.
#[tokio::test]
async fn a_waiting_sign_in_records_when_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| false),
        )),
        Config::default(),
    ));

    send(&app, "POST", "/api/providers/codex/login").await;
    let (_, body) = send(&app, "GET", "/api/providers").await;
    assert_eq!(body["providers"][0]["signin"]["state"], "waiting");
    assert_eq!(body["providers"][0]["signin"]["started_at"], fixed_now());
}

/// A grant that names nobody still reports the account and plan, because the
/// id token carries them. The stored fields are a cache of those claims, and
/// an older grant was written before they existed.
#[tokio::test]
async fn the_account_falls_back_to_the_id_token() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = ProviderAuthStore::default();
    store.set(
        "codex",
        ProviderGrant {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            id_token: id_token(),
            ..Default::default()
        },
    );
    store.save(&dir.path().join("provider-auth.json")).unwrap();

    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| true),
        )),
        Config::default(),
    ));

    let (_, body) = send(&app, "GET", "/api/providers").await;
    assert_eq!(body["providers"][0]["account"], "someone@example.com");
    assert_eq!(body["providers"][0]["plan"], "plus");
}

/// The check reaches the network. A store with no grant in it cannot answer,
/// and that is a 502 carrying the reason rather than a cheerful model list.
#[tokio::test]
async fn checking_without_a_sign_in_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| panic!("no browser may open")),
        )),
        Config::default(),
    ));

    let (status, body) = send(&app, "POST", "/api/providers/codex/check").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        !body["error"].as_str().unwrap_or_default().is_empty(),
        "{body}"
    );
}

/// And the check is refused for a provider that does not sign in at all,
/// rather than being handed to the codex registry arm.
#[tokio::test]
async fn checking_an_unknown_provider_is_a_404() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state_with(
        admin_over(authorizer(
            dir.path(),
            "http://127.0.0.1:1".to_string(),
            Arc::new(|_: &str| panic!("no browser may open")),
        )),
        Config::default(),
    ));

    let (status, _) = send(&app, "POST", "/api/providers/openai/check").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The default admin resolves this machine rather than being handed paths.
#[test]
fn the_default_admin_points_at_this_machine() {
    let admin = ProviderAdmin::default();
    assert!(admin.authorizer.store_path.is_some(), "a home resolves");
    assert!(
        leviath_core::sync::lock(&admin.in_flight).is_empty(),
        "nothing is in flight before anything is asked"
    );
}

/// The whole point of the check: it asks the account. A store with a grant in
/// it and a quota route that answers gives back the models *this plan* can
/// reach, not the compiled table.
#[tokio::test]
async fn a_check_reports_the_models_the_plan_can_reach() {
    let dir = tempfile::tempdir().unwrap();
    store_grant(dir.path());
    let usage = leviath_testkit::spawn_mock_server(
        200,
        "OK",
        br#"{"plan_type":"plus","rate_limit":{}}"#.to_vec(),
    )
    .await;
    let mut admin = admin_over(authorizer(
        dir.path(),
        "http://127.0.0.1:1".to_string(),
        Arc::new(|_: &str| panic!("no browser may open")),
    ));
    admin.usage_url = Some(usage);
    let app = router(state_with(admin, Config::default()));

    let (status, body) = send(&app, "POST", "/api/providers/codex/check").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    let models = body["models"].as_array().expect("a model list");
    assert!(!models.is_empty());
    assert!(
        !models.iter().any(|m| m == "gpt-5.3-codex-spark"),
        "a pro-only model reached a plus account: {body}"
    );
}
