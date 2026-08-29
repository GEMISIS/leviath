//! MCP server management endpoints.
//!
//! Full CRUD plus login over HTTP, mirroring `lev mcp`. The paths, browser
//! opener, and clock live in [`McpAdmin`] so the handlers are unit-testable
//! without the real home directory or a browser.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::types::{AppState, err};
use crate::config::Config;
use leviath_mcp::{AuthStore, LoginOutcome, MCPClient, MCPServerConfig, OAuthClient};

/// Where this server reads and rewrites the operator's files.
///
/// Resolved from `LEVIATH_HOME` and `LEVIATH_CONFIG_PATH`, never from anything
/// in a request. The distinction matters to a taint scanner: everything
/// reachable from a handler's parameters, the shared state included, reads as
/// request data, and a file location that is request data is a path-injection
/// finding. So the handlers get these from [`admin_paths`], a plain function
/// over the environment, and not from a field on [`AppState`]. The update
/// route keeps its own locations in `UpdateEnv` for the same reason.
#[derive(Clone, Debug)]
pub(crate) struct AdminPaths {
    /// Config file to read and rewrite.
    pub config: std::path::PathBuf,
    /// OAuth token store.
    pub store: std::path::PathBuf,
}

/// The operator's file locations for this process.
///
/// Resolved on every call: the lookup is two environment reads, and a lazily
/// cached copy would pin the first test's home directory on the whole test
/// binary. In a test build a [`TEST_PATHS`] scope wins over the environment,
/// which is how the handler tests point at a temp dir.
pub(crate) fn admin_paths() -> AdminPaths {
    #[cfg(test)]
    if let Ok(paths) = TEST_PATHS.try_with(Clone::clone) {
        return paths;
    }
    AdminPaths {
        config: Config::config_path(),
        store: AuthStore::default_path().unwrap_or_default(),
    }
}

#[cfg(test)]
tokio::task_local! {
    /// Test override for [`admin_paths`]; see [`scoped`].
    pub(crate) static TEST_PATHS: AdminPaths;
}

/// Wrap a router so every request it serves sees `paths` from
/// [`admin_paths`]. Test-only: production resolves from the environment.
#[cfg(test)]
pub(crate) fn scoped(router: axum::Router, paths: AdminPaths) -> axum::Router {
    router.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let paths = paths.clone();
            async move { TEST_PATHS.scope(paths, next.run(req)).await }
        },
    ))
}

/// The seams the login flow needs: how to open a browser, and what time it
/// is. Cheap to clone (an `Arc` and a fn pointer).
#[derive(Clone)]
pub(crate) struct McpAdmin {
    /// How to open the browser during a login.
    pub opener: leviath_mcp::BrowserOpener,
    /// Current Unix time; a fn so a long-lived server stays current per request.
    pub clock: fn() -> u64,
}

/// Real Unix time in seconds.
fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Default for McpAdmin {
    fn default() -> Self {
        Self {
            opener: std::sync::Arc::new(leviath_sys::open_url),
            clock: system_now,
        }
    }
}

/// A server, as reported by the list/status endpoints.
#[derive(Serialize)]
pub(super) struct McpServerInfo {
    name: String,
    transport: String,
    endpoint: String,
    auth: String,
}

impl McpServerInfo {
    fn describe(server: &MCPServerConfig, store: &AuthStore, now: u64) -> Self {
        let (transport, endpoint) = match server.resolve() {
            Ok(leviath_mcp::ResolvedTransport::Stdio { command, .. }) => {
                ("stdio".to_string(), command.to_string())
            }
            Ok(leviath_mcp::ResolvedTransport::Http { url, .. }) => {
                ("http".to_string(), url.to_string())
            }
            Err(_) => ("invalid".to_string(), String::new()),
        };
        Self {
            name: server.name.clone(),
            transport,
            endpoint,
            auth: auth_status(server, store, now),
        }
    }
}

/// A one-word auth state for a server.
fn auth_status(server: &MCPServerConfig, store: &AuthStore, now: u64) -> String {
    let is_http = matches!(
        server.resolve(),
        Ok(leviath_mcp::ResolvedTransport::Http { .. })
    );
    if !is_http {
        return "n/a".to_string();
    }
    match store.get(&server.name) {
        Some(auth) if auth.is_expired_at(now) => "expired".to_string(),
        Some(_) => "authenticated".to_string(),
        // A configured `Authorization` header is a credential too, and calling
        // it "none" is what puts a login button in front of a server that needs
        // no login.
        None if server.has_auth_header() => "header".to_string(),
        None => "none".to_string(),
    }
}

/// `GET /api/mcp/servers` - list configured servers with their auth status.
pub(super) async fn list_servers(State(state): State<AppState>) -> impl IntoResponse {
    let admin = &state.mcp;
    let paths = admin_paths();
    let config = match Config::load_from_path_public(&paths.config) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let store = AuthStore::load(&paths.store).unwrap_or_default();
    let now = (admin.clock)();
    let servers: Vec<McpServerInfo> = config
        .mcp_servers
        .iter()
        .map(|s| McpServerInfo::describe(s, &store, now))
        .collect();
    Json(servers).into_response()
}

/// Body of `POST /api/mcp/servers`.
#[derive(Deserialize)]
pub(super) struct AddServerRequest {
    name: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
}

/// `POST /api/mcp/servers` - add a server.
pub(super) async fn add_server(Json(req): Json<AddServerRequest>) -> impl IntoResponse {
    let paths = admin_paths();
    let server = MCPServerConfig {
        name: req.name,
        command: req.command,
        url: req.url,
        args: req.args,
        headers: req.headers,
        ..Default::default()
    };
    if let Err(e) = server.validate() {
        return err(StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    let mut config = match Config::load_from_path_public(&paths.config) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    if config.mcp_servers.iter().any(|s| s.name == server.name) {
        return err(
            StatusCode::CONFLICT,
            format!("an MCP server named '{}' already exists", server.name),
        )
        .into_response();
    }
    config.mcp_servers.push(server.clone());
    if let Err(e) = config.save_to_path_public(&paths.config) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "name": server.name })),
    )
        .into_response()
}

/// `DELETE /api/mcp/servers/{name}` - remove a server and its credentials.
pub(super) async fn remove_server(AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    let paths = admin_paths();
    let mut config = match Config::load_from_path_public(&paths.config) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let before = config.mcp_servers.len();
    config.mcp_servers.retain(|s| s.name != name);
    if config.mcp_servers.len() == before {
        return err(
            StatusCode::NOT_FOUND,
            format!("no MCP server named '{name}'"),
        )
        .into_response();
    }
    if let Err(e) = config.save_to_path_public(&paths.config) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    if let Ok(mut store) = AuthStore::load(&paths.store)
        && store.remove(&name)
    {
        let _ = store.save(&paths.store);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/mcp/servers/{name}/login` - run the OAuth browser flow.
///
/// On the host running `lev serve` this opens the operator's browser and
/// completes the loopback redirect, the same flow `lev mcp login` uses.
pub(super) async fn login(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let admin = &state.mcp;
    let paths = admin_paths();
    let config = match Config::load_from_path_public(&paths.config) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let Some(server) = config.mcp_servers.iter().find(|s| s.name == name) else {
        return err(
            StatusCode::NOT_FOUND,
            format!("no MCP server named '{name}'"),
        )
        .into_response();
    };
    let url = match server.resolve() {
        Ok(leviath_mcp::ResolvedTransport::Http { url, .. }) => url.to_string(),
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("server '{name}' does not use HTTP transport and cannot log in"),
            )
            .into_response();
        }
    };

    let mut store = AuthStore::load(&paths.store).unwrap_or_default();
    let reuse = store.get(&name).map(|a| a.client_id.clone());
    let outcome = match OAuthClient::new()
        .login(
            &url,
            &server.headers,
            &config.security.allow_env_vars,
            admin.opener.clone(),
            (admin.clock)(),
            reuse.as_deref(),
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    // A server that answered the probe wants no OAuth, so there is nothing to
    // store. Reporting it as an error would be wrong: the caller asked whether a
    // login was needed, and the answer is no.
    let LoginOutcome::Authenticated(auth) = outcome else {
        return Json(serde_json::json!({ "status": "not_required", "server": name }))
            .into_response();
    };
    store.set(&name, *auth);
    if let Err(e) = store.save(&paths.store) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    Json(serde_json::json!({ "status": "authenticated", "server": name })).into_response()
}

/// `GET /api/mcp/servers/{name}/status` - one server's transport and auth state.
pub(super) async fn status(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let admin = &state.mcp;
    let paths = admin_paths();
    let config = match Config::load_from_path_public(&paths.config) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let Some(server) = config.mcp_servers.iter().find(|s| s.name == name) else {
        return err(
            StatusCode::NOT_FOUND,
            format!("no MCP server named '{name}'"),
        )
        .into_response();
    };
    let store = AuthStore::load(&paths.store).unwrap_or_default();
    Json(McpServerInfo::describe(server, &store, (admin.clock)())).into_response()
}

/// `POST /api/mcp/servers/{name}/test` - connect and report the tool count.
pub(super) async fn test_server(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    let admin = &state.mcp;
    let paths = admin_paths();
    let config = match Config::load_from_path_public(&paths.config) {
        Ok(config) => config,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let Some(server) = config.mcp_servers.iter().find(|s| s.name == name) else {
        return err(
            StatusCode::NOT_FOUND,
            format!("no MCP server named '{name}'"),
        )
        .into_response();
    };
    let auth_header = match OAuthClient::new()
        .authorization_header(&name, &paths.store, (admin.clock)())
        .await
    {
        Ok(header) => header,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let result = connect_and_list(server, auth_header, &config.security.allow_env_vars).await;
    match result {
        Ok(tools) => Json(serde_json::json!({ "server": name, "tools": tools })).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// Connect to `server` and return its tool names.
///
/// The client is shut down on EVERY path, not just success: `MCPClient` has no
/// `Drop` and a stdio transport's child process does not die with the handle,
/// so the early-return `?`s here each orphaned a spawned MCP server process
/// per failed test request.
async fn connect_and_list(
    server: &MCPServerConfig,
    auth_header: Option<(String, String)>,
    allow_env: &[String],
) -> anyhow::Result<Vec<String>> {
    // The allowlist has to come from the config, not be an empty slice: an
    // empty one refuses every `${VAR}` header, so testing a server whose token
    // comes from the environment failed here while the same server worked for
    // an agent.
    let mut client = MCPClient::from_config_with_auth(server, auth_header, allow_env).await?;
    let listed = async {
        client.connect().await?;
        client.list_tools().await
    }
    .await;
    let _ = client.shutdown().await;
    let tools = listed?;
    Ok(tools.into_iter().map(|t| t.name).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::serve::events::ServerEvent;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{delete, get, post};
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    fn never_opens(_: &str) -> bool {
        false
    }

    fn fixed_clock() -> u64 {
        1_000
    }

    /// An app state with a test browser opener and a fixed clock.
    fn state_at(opener: impl Fn(&str) -> bool + Send + Sync + 'static) -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(16);
        AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: McpAdmin {
                opener: Arc::new(opener),
                clock: fixed_clock,
            },
            limits: Default::default(),
        }
    }

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/api/mcp/servers", get(list_servers).post(add_server))
            .route("/api/mcp/servers/{name}", delete(remove_server))
            .route("/api/mcp/servers/{name}/status", get(status))
            .route("/api/mcp/servers/{name}/login", post(login))
            .route("/api/mcp/servers/{name}/test", post(test_server))
            .with_state(state)
    }

    /// The config and store a test keeps under `dir`.
    fn paths_in(dir: &std::path::Path) -> AdminPaths {
        AdminPaths {
            config: dir.join("config.toml"),
            store: dir.join("mcp-auth.json"),
        }
    }

    /// A router over [`state_at`] whose handlers read the files under `dir`.
    fn app_at(
        dir: &std::path::Path,
        opener: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Router {
        scoped(router(state_at(opener)), paths_in(dir))
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or(Body::empty()),
            )
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

    #[tokio::test]
    async fn add_list_status_and_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);

        // Empty to start.
        let (status_code, body) = send(&app, "GET", "/api/mcp/servers", None).await;
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 0);

        // Add an HTTP server.
        let (status_code, _) = send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "remote", "url": "https://e.com/mcp" })),
        )
        .await;
        assert_eq!(status_code, StatusCode::CREATED);

        // It lists, with auth "none".
        let (_, body) = send(&app, "GET", "/api/mcp/servers", None).await;
        assert_eq!(body[0]["name"], "remote");
        assert_eq!(body[0]["transport"], "http");
        assert_eq!(body[0]["auth"], "none");

        // Status for the one server.
        let (status_code, body) = send(&app, "GET", "/api/mcp/servers/remote/status", None).await;
        assert_eq!(status_code, StatusCode::OK);
        assert_eq!(body["endpoint"], "https://e.com/mcp");

        // Remove it.
        let (status_code, _) = send(&app, "DELETE", "/api/mcp/servers/remote", None).await;
        assert_eq!(status_code, StatusCode::NO_CONTENT);
        let (_, body) = send(&app, "GET", "/api/mcp/servers", None).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn add_rejects_a_malformed_server() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let (status_code, _) = send(
            &app,
            "POST",
            "/api/mcp/servers",
            // Neither url nor command.
            Some(serde_json::json!({ "name": "bad" })),
        )
        .await;
        assert_eq!(status_code, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn add_rejects_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let body = serde_json::json!({ "name": "x", "command": "npx" });
        send(&app, "POST", "/api/mcp/servers", Some(body.clone())).await;
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers", Some(body)).await;
        assert_eq!(status_code, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn remove_of_an_unknown_server_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let (status_code, _) = send(&app, "DELETE", "/api/mcp/servers/ghost", None).await;
        assert_eq!(status_code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn status_of_an_unknown_server_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let (status_code, _) = send(&app, "GET", "/api/mcp/servers/ghost/status", None).await;
        assert_eq!(status_code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn login_of_an_unknown_server_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/ghost/login", None).await;
        assert_eq!(status_code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn login_of_a_stdio_server_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "local", "command": "npx" })),
        )
        .await;
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/local/login", None).await;
        assert_eq!(status_code, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_of_an_unknown_server_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/ghost/test", None).await;
        assert_eq!(status_code, StatusCode::NOT_FOUND);
    }

    // ─── full login + test against a mock OAuth + MCP server ──────────────

    use axum::extract::State as AxumState;

    async fn mock_oauth_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let s = base.clone();
        let app = Router::new()
            .route(
                "/mcp",
                post(|AxumState(base): AxumState<String>| async move {
                    let hint = format!(
                        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
                    );
                    (
                        StatusCode::UNAUTHORIZED,
                        [(reqwest::header::WWW_AUTHENTICATE, hint)],
                    )
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                get(|AxumState(base): AxumState<String>| async move {
                    Json(serde_json::json!({
                        "resource": format!("{base}/mcp"),
                        "authorization_servers": [base],
                    }))
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(|AxumState(base): AxumState<String>| async move {
                    Json(serde_json::json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "registration_endpoint": format!("{base}/register"),
                        "scopes_supported": ["openid"],
                    }))
                }),
            )
            .route(
                "/register",
                post(|| async { Json(serde_json::json!({ "client_id": "rest-client" })) }),
            )
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "rest-access",
                        "refresh_token": "rest-refresh",
                        "expires_in": 3600,
                    }))
                }),
            )
            .with_state(s);
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
    }

    fn auto_consent(authorize_url: &str) -> bool {
        let url = reqwest::Url::parse(authorize_url).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let redirect = params["redirect_uri"].clone();
        let state = params["state"].clone();
        tokio::spawn(async move {
            let cb = format!("{redirect}?code=rest-code&state={state}");
            let _ = reqwest::Client::new().get(&cb).send().await;
        });
        true
    }

    #[tokio::test]
    async fn login_completes_and_status_reports_authenticated() {
        let base = mock_oauth_server().await;
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), auto_consent);

        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "navigator", "url": format!("{base}/mcp") })),
        )
        .await;

        let (status_code, body) =
            send(&app, "POST", "/api/mcp/servers/navigator/login", None).await;
        assert_eq!(status_code, StatusCode::OK, "login body: {body}");
        assert_eq!(body["status"], "authenticated");

        // Now status shows authenticated.
        let (_, body) = send(&app, "GET", "/api/mcp/servers/navigator/status", None).await;
        assert_eq!(body["auth"], "authenticated");
    }

    /// The website's login button on a header-authenticated server. It used to
    /// surface the discovery 404 as a bad gateway; the honest answer is that no
    /// login is needed.
    #[tokio::test]
    async fn login_reports_not_required_when_headers_already_satisfy_the_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Publishes no OAuth metadata, so an attempted discovery fails loudly.
        let mcp =
            axum::Router::new().route("/mcp", axum::routing::post(|| async { StatusCode::OK }));
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, mcp,
        )));

        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({
                "name": "hub",
                "url": format!("{base}/mcp"),
                "headers": { "Authorization": "Bearer configured-token" },
            })),
        )
        .await;

        let (status_code, body) = send(&app, "POST", "/api/mcp/servers/hub/login", None).await;
        assert_eq!(status_code, StatusCode::OK, "login body: {body}");
        assert_eq!(body["status"], "not_required");
        // `never_opens` would have failed the flow had discovery been attempted.

        // And the listing calls it credentialed, so no UI offers a login here.
        let (_, body) = send(&app, "GET", "/api/mcp/servers/hub/status", None).await;
        assert_eq!(body["auth"], "header");
    }

    #[tokio::test]
    async fn login_reports_a_bad_gateway_when_discovery_fails() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "dead", "url": "http://127.0.0.1:1/mcp" })),
        )
        .await;
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/dead/login", None).await;
        assert_eq!(status_code, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn test_endpoint_connects_and_lists_tools() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let stub = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); m = req.get("method",""); i = req.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"capabilities":{},"protocolVersion":"2024-11-05"}}), flush=True)
    elif m == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"tools":[{"name":"ping","inputSchema":{}}]}}), flush=True)
"#;
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(
                serde_json::json!({ "name": "local", "command": "python3", "args": ["-c", stub] }),
            ),
        )
        .await;
        let (status_code, body) = send(&app, "POST", "/api/mcp/servers/local/test", None).await;
        assert_eq!(status_code, StatusCode::OK, "body: {body}");
        assert_eq!(body["tools"][0], "ping");
    }

    #[tokio::test]
    async fn test_endpoint_reports_a_bad_gateway_on_connect_failure() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "dead", "url": "http://127.0.0.1:1/mcp" })),
        )
        .await;
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/dead/test", None).await;
        assert_eq!(status_code, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn test_reports_a_bad_gateway_when_the_token_cannot_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "remote", "url": "http://127.0.0.1:1/mcp" })),
        )
        .await;
        // Seed an expired token with a dead refresh endpoint.
        let mut store = AuthStore::default();
        store.set(
            "remote",
            leviath_mcp::ServerAuth {
                token_endpoint: "http://127.0.0.1:1/token".to_string(),
                refresh_token: Some("good".to_string()),
                expires_at: 1,
                ..Default::default()
            },
        );
        store.save(&dir.path().join("mcp-auth.json")).unwrap();
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/remote/test", None).await;
        assert_eq!(status_code, StatusCode::BAD_GATEWAY);
    }

    // ─── I/O failure arms ─────────────────────────────────────────────────

    /// A state whose config/store paths are directories, so reads fail.
    fn broken_state(dir: &std::path::Path) -> AppState {
        let cfg = dir.join("cfg-dir");
        let store = dir.join("store-dir");
        std::fs::create_dir(&cfg).unwrap();
        std::fs::create_dir(&store).unwrap();
        let (tx, _) = broadcast::channel::<ServerEvent>(16);
        AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: McpAdmin {
                opener: Arc::new(never_opens),
                clock: fixed_clock,
            },
            limits: Default::default(),
        }
    }

    #[tokio::test]
    async fn read_endpoints_surface_an_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let app = scoped(
            router(broken_state(dir.path())),
            AdminPaths {
                config: dir.path().join("cfg-dir"),
                store: dir.path().join("store-dir"),
            },
        );
        for (method, uri) in [
            ("GET", "/api/mcp/servers"),
            ("GET", "/api/mcp/servers/x/status"),
            ("POST", "/api/mcp/servers/x/login"),
            ("POST", "/api/mcp/servers/x/test"),
            ("DELETE", "/api/mcp/servers/x"),
        ] {
            let (status_code, _) = send(&app, method, uri, None).await;
            assert_eq!(
                status_code,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{method} {uri}"
            );
        }
    }

    #[tokio::test]
    async fn add_surfaces_an_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let app = scoped(
            router(broken_state(dir.path())),
            AdminPaths {
                config: dir.path().join("cfg-dir"),
                store: dir.path().join("store-dir"),
            },
        );
        let (status_code, _) = send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "x", "command": "npx" })),
        )
        .await;
        assert_eq!(status_code, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn add_surfaces_an_unwritable_config() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let (tx, _) = broadcast::channel::<ServerEvent>(16);
        let state = AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: McpAdmin {
                opener: Arc::new(never_opens),
                clock: fixed_clock,
            },
            limits: Default::default(),
        };
        let app = scoped(
            router(state),
            AdminPaths {
                config: file.join("config.toml"),
                store: dir.path().join("s.json"),
            },
        );
        let (status_code, _) = send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "x", "command": "npx" })),
        )
        .await;
        assert_eq!(status_code, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn remove_surfaces_an_unwritable_config() {
        // Config reads fine, add one server, then make the config file read-only.
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "x", "command": "npx" })),
        )
        .await;
        let cfg = dir.path().join("config.toml");
        let mut perms = std::fs::metadata(&cfg).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&cfg, perms).unwrap();
        let (status_code, _) = send(&app, "DELETE", "/api/mcp/servers/x", None).await;
        assert_eq!(status_code, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn login_surfaces_an_unwritable_store() {
        let base = mock_oauth_server().await;
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), auto_consent);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "navigator", "url": format!("{base}/mcp") })),
        )
        .await;
        // Make the store a read-only file so persisting the token fails.
        let store = dir.path().join("mcp-auth.json");
        AuthStore::default().save(&store).unwrap();
        let mut perms = std::fs::metadata(&store).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&store, perms).unwrap();
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/navigator/login", None).await;
        assert_eq!(status_code, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ─── McpAdmin::default ────────────────────────────────────────────────

    #[tokio::test]
    async fn list_and_status_describe_a_stdio_server() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "local", "command": "npx" })),
        )
        .await;
        let (_, body) = send(&app, "GET", "/api/mcp/servers", None).await;
        assert_eq!(body[0]["transport"], "stdio");
        assert_eq!(body[0]["endpoint"], "npx");
        assert_eq!(body[0]["auth"], "n/a");
        let (_, body) = send(&app, "GET", "/api/mcp/servers/local/status", None).await;
        assert_eq!(body["transport"], "stdio");
    }

    #[tokio::test]
    async fn remove_clears_stored_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "remote", "url": "https://e.com/mcp" })),
        )
        .await;
        // Seed a credential so removal has something to clear.
        let mut store = AuthStore::default();
        store.set("remote", leviath_mcp::ServerAuth::default());
        store.save(&dir.path().join("mcp-auth.json")).unwrap();

        let (status_code, _) = send(&app, "DELETE", "/api/mcp/servers/remote", None).await;
        assert_eq!(status_code, StatusCode::NO_CONTENT);
        let reloaded = AuthStore::load(&dir.path().join("mcp-auth.json")).unwrap();
        assert!(reloaded.get("remote").is_none());
    }

    #[tokio::test]
    async fn a_second_login_reuses_the_client_id() {
        let base = mock_oauth_server().await;
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), auto_consent);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "navigator", "url": format!("{base}/mcp") })),
        )
        .await;
        send(&app, "POST", "/api/mcp/servers/navigator/login", None).await;
        // Second login: store.get is Some, so the client_id is reused.
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/navigator/login", None).await;
        assert_eq!(status_code, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_endpoint_reports_a_spawn_failure() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({ "name": "x", "command": "definitely-not-a-real-binary-xyz" })),
        )
        .await;
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/x/test", None).await;
        assert_eq!(status_code, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn test_endpoint_reports_a_list_tools_failure() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        let stub = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    req = json.loads(line); m = req.get("method",""); i = req.get("id")
    if m == "initialize":
        print(json.dumps({"jsonrpc":"2.0","id":i,"result":{"capabilities":{},"protocolVersion":"2024-11-05"}}), flush=True)
    elif m == "tools/list":
        print(json.dumps({"jsonrpc":"2.0","id":i,"error":{"code":-32603,"message":"boom"}}), flush=True)
"#;
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(
                serde_json::json!({ "name": "local", "command": "python3", "args": ["-c", stub] }),
            ),
        )
        .await;
        let (status_code, _) = send(&app, "POST", "/api/mcp/servers/local/test", None).await;
        assert_eq!(status_code, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn never_opens_reports_no_browser() {
        assert!(!never_opens("https://x"));
    }

    #[test]
    fn default_admin_uses_real_paths() {
        // Constructing the default must not panic even with no LEVIATH_HOME; it
        // resolves the real config/store locations.
        let _admin = McpAdmin::default();
        assert!(
            admin_paths()
                .config
                .to_string_lossy()
                .contains("config.toml")
        );
    }

    #[test]
    fn system_now_advances_past_the_epoch() {
        assert!(system_now() > 1_600_000_000);
    }

    #[test]
    fn describe_marks_an_invalid_entry() {
        let bad = MCPServerConfig {
            name: "broken".to_string(),
            ..Default::default()
        };
        let info = McpServerInfo::describe(&bad, &AuthStore::default(), 0);
        assert_eq!(info.transport, "invalid");
        assert_eq!(info.auth, "n/a");
    }

    #[test]
    fn auth_status_reports_expired() {
        let http = MCPServerConfig::http("s", "https://e.com/mcp");
        let mut store = AuthStore::default();
        store.set(
            "s",
            leviath_mcp::ServerAuth {
                expires_at: 100,
                ..Default::default()
            },
        );
        assert_eq!(auth_status(&http, &store, 1_000), "expired");
    }
}
