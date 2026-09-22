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
    /// Provider sign-in grants, for the `/api/providers` routes.
    pub grants: std::path::PathBuf,
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
        grants: leviath_providers::oauth::ProviderAuthStore::default_path().unwrap_or_default(),
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

/// Real Unix time in seconds. Shared with the provider routes, which
/// track sign-in timestamps the same way.
pub(super) fn system_now() -> u64 {
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
    pub(super) name: String,
    pub(super) transport: String,
    pub(super) endpoint: String,
    pub(super) auth: String,
    /// Why the configuration does not resolve to a transport, when it does not.
    ///
    /// `transport: "invalid"` on its own says a server is broken and nothing
    /// about how to fix it, which leaves reading the config file by hand as the
    /// only way to find out. Null for a server that resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) config_error: Option<String>,
    /// The program a stdio server is, as the entry spells it.
    ///
    /// Skipped on the wire, with the four below. `GET /api/mcp/servers`
    /// answers `endpoint`, one string for either transport, and a client
    /// reading that field would be shown two spellings of one server if these
    /// joined it. GraphQL reads them off this struct instead.
    #[serde(skip)]
    pub(super) command: Option<String>,
    /// Where an HTTP server is, as the entry spells it.
    #[serde(skip)]
    pub(super) url: Option<String>,
    /// The arguments a stdio server is spawned with, in order.
    #[serde(skip)]
    pub(super) args: Vec<String>,
    /// The names of the headers an HTTP server is sent, sorted, without their
    /// values: a header is where a credential goes.
    #[serde(skip)]
    pub(super) header_names: Vec<String>,
    /// The names of the variables a stdio server is spawned with, sorted,
    /// without their values, for the same reason.
    #[serde(skip)]
    pub(super) env_names: Vec<String>,
}

/// The keys of a map, sorted, without their values.
fn names_of(map: &std::collections::HashMap<String, String>) -> Vec<String> {
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
}

impl McpServerInfo {
    fn describe(server: &MCPServerConfig, store: &AuthStore, now: u64) -> Self {
        let (transport, endpoint, config_error) = match server.resolve() {
            Ok(leviath_mcp::ResolvedTransport::Stdio { command, .. }) => {
                ("stdio".to_string(), command.to_string(), None)
            }
            Ok(leviath_mcp::ResolvedTransport::Http { url, .. }) => {
                ("http".to_string(), url.to_string(), None)
            }
            Err(e) => ("invalid".to_string(), String::new(), Some(e.to_string())),
        };
        Self {
            name: server.name.clone(),
            transport,
            endpoint,
            auth: auth_status(server, store, now),
            config_error,
            command: server.command.clone(),
            url: server.url.clone(),
            args: server.args.clone(),
            header_names: names_of(&server.headers),
            env_names: names_of(&server.env),
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
    match server_infos(&state) {
        Ok(servers) => Json(servers).into_response(),
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// Every MCP server the config declares, with its auth state. Both surfaces
/// read the config file here rather than from `AppState`, because the admin
/// routes write it and a stale copy would report a server that was just
/// removed.
pub(super) fn server_infos(
    state: &AppState,
) -> Result<Vec<McpServerInfo>, super::core::error::ServeError> {
    let paths = admin_paths();
    let config = Config::load_from_path_public(&paths.config)
        .map_err(|e| super::core::error::ServeError::Internal(e.to_string()))?;
    let store = AuthStore::load(&paths.store).unwrap_or_default();
    let now = (state.mcp.clock)();
    Ok(config
        .mcp_servers
        .iter()
        .map(|server| McpServerInfo::describe(server, &store, now))
        .collect())
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
    match install_server(
        req.name,
        req.command,
        req.url,
        req.args,
        std::collections::HashMap::new(),
        req.headers,
    ) {
        Ok(written) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "name": written.name })),
        )
            .into_response(),
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// Write an MCP server into the config, and hand back the entry that was
/// written.
///
/// The entry rather than its name: a caller that has to describe what it wrote
/// would otherwise read the file back and have a miss to invent an answer for,
/// when the entry it is asking about is the one in its hand.
///
/// Remote code execution by construction: the command written here is what
/// Leviath spawns, for this run and every future one. Both surfaces gate the
/// act behind `--allow-admin`; this is what the act itself is.
pub(super) fn install_server(
    name: String,
    command: Option<String>,
    url: Option<String>,
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
    headers: std::collections::HashMap<String, String>,
) -> Result<MCPServerConfig, super::core::error::ServeError> {
    use super::core::error::ServeError;

    let paths = admin_paths();
    let server = checked(name, command, url, args, env, headers)?;
    let mut config = Config::load_from_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    if config.mcp_servers.iter().any(|s| s.name == server.name) {
        return Err(ServeError::Conflict(format!(
            "an MCP server named '{}' already exists",
            server.name
        )));
    }
    config.mcp_servers.push(server.clone());
    config
        .save_to_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    Ok(server)
}

/// One server's state, described from an entry the caller is already holding.
///
/// The counterpart of [`server_infos`] for a caller that has just written or
/// just resolved the entry: there is no name to look up, and so no miss to
/// report about a server it is looking at.
pub(super) fn described(state: &AppState, server: &MCPServerConfig) -> McpServerInfo {
    let store = AuthStore::load(&admin_paths().store).unwrap_or_default();
    McpServerInfo::describe(server, &store, (state.mcp.clock)())
}

/// Replace an MCP server's entry, whole, and hand back what now stands there.
///
/// Whole rather than field by field: the entry is what gets spawned, and an
/// edit that left half of a previous transport behind would describe a server
/// nobody wrote. A name nothing is configured under is a miss, because
/// creating one here would turn a typo into a second server.
pub(super) fn update_server(
    name: String,
    command: Option<String>,
    url: Option<String>,
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
    headers: std::collections::HashMap<String, String>,
) -> Result<MCPServerConfig, super::core::error::ServeError> {
    use super::core::error::ServeError;

    let paths = admin_paths();
    let server = checked(name, command, url, args, env, headers)?;
    let mut config = Config::load_from_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    let Some(at) = config
        .mcp_servers
        .iter()
        .position(|s| s.name == server.name)
    else {
        return Err(ServeError::NotFound(format!(
            "no MCP server named '{}'",
            server.name
        )));
    };
    config.mcp_servers[at] = server.clone();
    config
        .save_to_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    Ok(server)
}

/// The entry a write describes, refused here when it describes nothing
/// reachable, so neither writer touches the file with a server that could not
/// be spawned.
fn checked(
    name: String,
    command: Option<String>,
    url: Option<String>,
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
    headers: std::collections::HashMap<String, String>,
) -> Result<MCPServerConfig, super::core::error::ServeError> {
    let server = MCPServerConfig {
        name,
        command,
        url,
        args,
        env,
        headers,
        ..Default::default()
    };
    server
        .validate()
        .map_err(|e| super::core::error::ServeError::BadRequest(e.to_string()))?;
    Ok(server)
}

pub(super) async fn remove_server(AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    match uninstall_server(&name) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// Take an MCP server out of the config, and its stored credential with it.
pub(super) fn uninstall_server(name: &str) -> Result<(), super::core::error::ServeError> {
    use super::core::error::ServeError;

    let paths = admin_paths();
    let mut config = Config::load_from_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    let before = config.mcp_servers.len();
    config.mcp_servers.retain(|server| server.name != name);
    if config.mcp_servers.len() == before {
        return Err(ServeError::NotFound(format!(
            "no MCP server named '{name}'"
        )));
    }
    config
        .save_to_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    // The credential goes with the server it was for. Left behind, it would be
    // silently reused by a later server that happened to take the same name.
    if let Ok(mut store) = AuthStore::load(&paths.store)
        && store.remove(name)
    {
        let _ = store.save(&paths.store);
    }
    Ok(())
}

/// `POST /api/mcp/servers/{name}/login` - run the OAuth browser flow.
///
/// On the host running `lev serve` this opens the operator's browser and
/// completes the loopback redirect, the same flow `lev mcp login` uses.
pub(super) async fn login(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    match signed_in(&state, &name).await {
        Ok((status, _server)) => {
            Json(serde_json::json!({ "status": status.wire(), "server": name })).into_response()
        }
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// What a sign-in to an MCP server ended as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoginStatus {
    /// A grant was obtained and stored.
    Authenticated,
    /// The server wants no OAuth, so there was nothing to store. Not a failure:
    /// the caller asked whether a login was needed, and the answer is no.
    NotRequired,
}

impl LoginStatus {
    /// The word this status goes out as.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::Authenticated => "authenticated",
            Self::NotRequired => "not_required",
        }
    }
}

/// Sign in to one MCP server, for whichever surface asked, and hand back the
/// entry it signed in to.
///
/// The entry travels with the answer for the reason [`install_server`] gives:
/// the caller that has to describe the server is holding it already.
///
/// Opens a browser on the host, which is why it is an act rather than a read: a
/// server reached over SSH cannot do this, and the refusal says so.
pub(super) async fn signed_in(
    state: &AppState,
    name: &str,
) -> Result<(LoginStatus, MCPServerConfig), super::core::error::ServeError> {
    use super::core::error::ServeError;

    let admin = &state.mcp;
    let paths = admin_paths();
    let config = Config::load_from_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    let server = config
        .mcp_servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| ServeError::NotFound(format!("no MCP server named '{name}'")))?
        .clone();
    let url = match server.resolve() {
        Ok(leviath_mcp::ResolvedTransport::Http { url, .. }) => url.to_string(),
        _ => {
            return Err(ServeError::BadRequest(format!(
                "server '{name}' does not use HTTP transport and cannot log in"
            )));
        }
    };

    let mut store = AuthStore::load(&paths.store).unwrap_or_default();
    let reuse = store.get(name).map(|a| a.client_id.clone());
    let outcome = OAuthClient::new()
        .login(
            &url,
            &server.headers,
            &config.security.allow_env_vars,
            admin.opener.clone(),
            (admin.clock)(),
            reuse.as_deref(),
        )
        .await
        .map_err(|e| ServeError::Upstream(e.to_string()))?;
    let LoginOutcome::Authenticated(auth) = outcome else {
        return Ok((LoginStatus::NotRequired, server));
    };
    store.set(name, *auth);
    store
        .save(&paths.store)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    Ok((LoginStatus::Authenticated, server))
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
    match tools_of(&state, &name).await {
        Ok((tools, _server)) => {
            Json(serde_json::json!({ "server": name, "tools": tools })).into_response()
        }
        Err(e) => super::core::error::as_api_error(&e).into_response(),
    }
}

/// What one MCP server advertises, for whichever surface asked, and the entry
/// that was asked.
///
/// Connects and lists, which is the only honest answer to "does this server
/// work": a config that parses proves nothing about a program that will not
/// start. The entry travels with the answer for the reason
/// [`install_server`] gives.
pub(super) async fn tools_of(
    state: &AppState,
    name: &str,
) -> Result<(Vec<String>, MCPServerConfig), super::core::error::ServeError> {
    use super::core::error::ServeError;

    let admin = &state.mcp;
    let paths = admin_paths();
    let config = Config::load_from_path_public(&paths.config)
        .map_err(|e| ServeError::Internal(e.to_string()))?;
    let server = config
        .mcp_servers
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| ServeError::NotFound(format!("no MCP server named '{name}'")))?
        .clone();
    let auth_header = OAuthClient::new()
        .authorization_header(name, &paths.store, (admin.clock)())
        .await
        .map_err(|e| ServeError::Upstream(e.to_string()))?;
    let tools = connect_and_list(&server, auth_header, &config.security.allow_env_vars)
        .await
        .map_err(|e| ServeError::Upstream(e.to_string()))?;
    Ok((tools, server))
}

/// The tools `server` advertises, for a caller with no request to answer:
/// the dashboard's agent editor asks this for every configured server, off
/// its loop, so its tools chooser can offer them by name.
pub(crate) async fn list_mcp_tools(
    config: Config,
    server: MCPServerConfig,
) -> Result<Vec<String>, String> {
    let paths = admin_paths();
    let auth_header = OAuthClient::new()
        .authorization_header(&server.name, &paths.store, system_now())
        .await
        .map_err(|e| e.to_string())?;
    connect_and_list(&server, auth_header, &config.security.allow_env_vars)
        .await
        .map_err(|e| e.to_string())
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
        let (tx, _) = broadcast::channel(16);
        AppState {
            caches: Default::default(),
            signer: Default::default(),
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: McpAdmin {
                opener: Arc::new(opener),
                clock: fixed_clock,
            },
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
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
            grants: dir.join("provider-auth.json"),
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

    /// `GET /api/mcp/servers` carries exactly the keys it always has.
    ///
    /// `McpServerInfo` grew five fields for GraphQL to read a server back the
    /// way it was written. This route answers `endpoint`, one string for
    /// either transport, and a client reading it key by key has never been
    /// sent the other spelling, so the five are skipped on the wire and this
    /// is what holds them there.
    #[tokio::test]
    async fn the_server_listing_carries_no_new_keys() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_at(dir.path(), never_opens);
        send(
            &app,
            "POST",
            "/api/mcp/servers",
            Some(serde_json::json!({
                "name": "remote",
                "url": "https://e.com/mcp",
                "headers": { "Authorization": "Bearer t" },
            })),
        )
        .await;
        let (_, body) = send(&app, "GET", "/api/mcp/servers", None).await;
        // Sorted, because the JSON is read back through a map that sorts: what
        // is being held here is the set of keys, not their order on the wire.
        let keys: Vec<&String> = body[0].as_object().expect("an object").keys().collect();
        assert_eq!(
            keys,
            vec!["auth", "endpoint", "name", "transport"],
            "the server's JSON is what it has always been"
        );
    }

    /// Replacing a server writes the new entry whole, and a name nothing is
    /// configured under is a miss rather than a second server.
    #[tokio::test]
    async fn update_replaces_an_entry_and_refuses_an_unknown_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::write(&paths.config, "").unwrap();
        TEST_PATHS.sync_scope(paths, || {
            install_server(
                "docs".to_string(),
                Some("/bin/echo".to_string()),
                None,
                vec!["one".to_string()],
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            )
            .expect("the server is written");

            update_server(
                "docs".to_string(),
                None,
                Some("https://docs.example/mcp".to_string()),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer t".to_string(),
                )]),
            )
            .expect("the server is replaced");

            let config = Config::load_from_path_public(&admin_paths().config).unwrap();
            assert_eq!(config.mcp_servers.len(), 1, "replaced, not added beside");
            let server = &config.mcp_servers[0];
            assert_eq!(server.url.as_deref(), Some("https://docs.example/mcp"));
            assert!(
                server.command.is_none() && server.args.is_empty(),
                "the previous transport is gone, whole"
            );

            let missing = update_server(
                "ghost".to_string(),
                Some("/bin/echo".to_string()),
                None,
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            )
            .expect_err("nothing is configured under that name");
            assert_eq!(missing.code(), "NOT_FOUND");

            let nowhere = update_server(
                "docs".to_string(),
                None,
                None,
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            )
            .expect_err("neither a command nor a URL reaches anything");
            assert_eq!(nowhere.code(), "BAD_USER_INPUT");
        });
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

    /// The editor's listing reaches a stdio server and names its tools, and
    /// says why when it cannot.
    #[tokio::test]
    async fn list_mcp_tools_answers_for_the_dashboard() {
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
        let server = MCPServerConfig {
            name: "local".to_string(),
            command: Some("python3".to_string()),
            args: vec!["-c".to_string(), stub.to_string()],
            ..Default::default()
        };
        let tools = list_mcp_tools(Config::default(), server).await.unwrap();
        assert_eq!(tools, vec!["ping"]);
        let dead = MCPServerConfig {
            name: "dead".to_string(),
            command: Some("/nonexistent/mcp-server-binary".to_string()),
            ..Default::default()
        };
        let err = list_mcp_tools(Config::default(), dead).await.unwrap_err();
        assert!(!err.is_empty());
        // A token store that will not load is the answer too, before any
        // server is spoken to.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::write(&paths.store, "not json").unwrap();
        let broken = MCPServerConfig::http("remote", "https://example.invalid/mcp");
        let err = TEST_PATHS
            .scope(paths, list_mcp_tools(Config::default(), broken))
            .await
            .unwrap_err();
        assert!(!err.is_empty());
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
        let (tx, _) = broadcast::channel(16);
        AppState {
            caches: Default::default(),
            signer: Default::default(),
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: McpAdmin {
                opener: Arc::new(never_opens),
                clock: fixed_clock,
            },
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
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
                grants: dir
                    .path()
                    .join("store-dir")
                    .with_file_name("provider-auth.json"),
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
                grants: dir
                    .path()
                    .join("store-dir")
                    .with_file_name("provider-auth.json"),
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
        let (tx, _) = broadcast::channel(16);
        let state = AppState {
            caches: Default::default(),
            signer: Default::default(),
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: McpAdmin {
                opener: Arc::new(never_opens),
                clock: fixed_clock,
            },
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        };
        let app = scoped(
            router(state),
            AdminPaths {
                config: file.join("config.toml"),
                store: dir.path().join("s.json"),
                grants: dir
                    .path()
                    .join("s.json")
                    .with_file_name("provider-auth.json"),
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

    /// Replacing an entry surfaces both file failures it can meet: a config it
    /// cannot read, and one it can read and cannot write back.
    ///
    /// Two different answers for a caller: the first is a machine that cannot
    /// be configured at all, the second is an edit refused after the entry it
    /// names was found.
    #[tokio::test]
    async fn update_surfaces_a_config_it_cannot_read_or_write() {
        let dir = tempfile::tempdir().unwrap();

        // A directory where the config file belongs, so loading it fails.
        let unreadable = dir.path().join("cfg-dir");
        std::fs::create_dir(&unreadable).unwrap();
        TEST_PATHS.sync_scope(
            AdminPaths {
                config: unreadable,
                store: dir.path().join("s.json"),
                grants: dir.path().join("g.json"),
            },
            || {
                let failed = update_server(
                    "docs".to_string(),
                    Some("/bin/echo".to_string()),
                    None,
                    Vec::new(),
                    std::collections::HashMap::new(),
                    std::collections::HashMap::new(),
                )
                .expect_err("a config that will not load");
                assert_eq!(failed.code(), "INTERNAL");
            },
        );

        // And a config that reads fine, with the entry in it, that cannot be
        // written back.
        let paths = paths_in(dir.path());
        std::fs::write(&paths.config, "").unwrap();
        TEST_PATHS.sync_scope(paths, || {
            install_server(
                "docs".to_string(),
                Some("/bin/echo".to_string()),
                None,
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            )
            .expect("the server is written");

            let config = admin_paths().config;
            let mut perms = std::fs::metadata(&config).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&config, perms).unwrap();

            let failed = update_server(
                "docs".to_string(),
                None,
                Some("https://docs.example/mcp".to_string()),
                Vec::new(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            )
            .expect_err("a config that will not save");
            assert_eq!(failed.code(), "INTERNAL");
        });
    }
}
