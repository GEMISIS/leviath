//! MCP management screen: state operations and the async action background loop.
//!
//! The screen lists configured servers with their auth status and supports the
//! same operations as `lev mcp`. Add/remove are synchronous file writes done
//! inline; login and test are long-running (a browser flow, a network round
//! trip) so they are dispatched to [`mcp_background_loop`] over a channel and
//! their results drain back as toasts - the same shape the daemon-command lane
//! already uses.

use tokio::sync::mpsc;

use super::state::Dashboard;
use super::types::{ConfirmAction, McpCommand, McpContext, McpOutcome, McpRow, ToastLevel};
use crate::config::Config;
use leviath_mcp::{AuthStore, MCPClient, MCPServerConfig, OAuthClient};

impl Dashboard {
    /// Re-read the config + token store and rebuild the MCP row list, clamping
    /// the selection. Cheap file I/O, called when the screen opens and after any
    /// change so it always reflects disk.
    pub(super) fn refresh_mcp_rows(&mut self) {
        let ctx = &self.mcp_ctx;
        let servers = Config::load_from_path_public(&ctx.config_path)
            .map(|c| c.mcp_servers)
            .unwrap_or_default();
        let store = AuthStore::load(&ctx.store_path).unwrap_or_default();
        let now = (ctx.clock)();
        self.mcp_rows = servers
            .iter()
            .map(|s| describe_row(s, &store, now))
            .collect();
        if self.mcp_selected >= self.mcp_rows.len() {
            self.mcp_selected = self.mcp_rows.len().saturating_sub(1);
        }
    }

    /// Add a server from the add-form line and persist it.
    ///
    /// The line is `<name> <url-or-command> [args…]`: a second token starting
    /// with `http` is an HTTP `url`, anything else is a stdio `command` with the
    /// rest as its arguments. Returns whether the add succeeded (a message is
    /// logged either way).
    pub(super) fn mcp_add_from_line(&mut self, line: &str) -> bool {
        let server = match parse_add_line(line) {
            Ok(server) => server,
            Err(e) => {
                self.toast(e, ToastLevel::Error);
                return false;
            }
        };
        let ctx = &self.mcp_ctx;
        let mut config = match Config::load_from_path_public(&ctx.config_path) {
            Ok(config) => config,
            Err(e) => {
                self.toast(format!("Could not read config: {e}"), ToastLevel::Error);
                return false;
            }
        };
        if config.mcp_servers.iter().any(|s| s.name == server.name) {
            self.toast(
                format!("A server named '{}' already exists", server.name),
                ToastLevel::Error,
            );
            return false;
        }
        let name = server.name.clone();
        config.mcp_servers.push(server);
        if let Err(e) = config.save_to_path_public(&ctx.config_path) {
            self.toast(format!("Could not save config: {e}"), ToastLevel::Error);
            return false;
        }
        self.toast(format!("Added MCP server '{name}'"), ToastLevel::Info);
        self.refresh_mcp_rows();
        true
    }

    /// Open the remove confirmation for the selected server.
    pub(super) fn mcp_request_remove(&mut self) {
        use crate::tui::widgets::confirm::Confirm;
        use ratatui::text::Line;
        let Some(row) = self.mcp_rows.get(self.mcp_selected) else {
            return;
        };
        let name = row.name.clone();
        let dialog = Confirm::new(
            "Remove MCP server?",
            vec![Line::from(format!(
                "Remove '{name}' from the config (and forget its login)?"
            ))],
            "Remove",
            "Cancel",
        )
        .danger();
        self.pending_confirm = Some((ConfirmAction::McpRemove { name }, dialog));
    }

    /// Remove `name` from the config and the auth store. Runs after its
    /// confirmation dialog; keyed by name, not the current selection.
    pub(super) fn mcp_remove_named(&mut self, name: &str) {
        let name = name.to_string();
        let ctx = &self.mcp_ctx;
        let mut config = match Config::load_from_path_public(&ctx.config_path) {
            Ok(config) => config,
            Err(e) => {
                self.toast(format!("Could not read config: {e}"), ToastLevel::Error);
                return;
            }
        };
        config.mcp_servers.retain(|s| s.name != name);
        if let Err(e) = config.save_to_path_public(&ctx.config_path) {
            self.toast(format!("Could not save config: {e}"), ToastLevel::Error);
            return;
        }
        if let Ok(mut store) = AuthStore::load(&ctx.store_path)
            && store.remove(&name)
        {
            let _ = store.save(&ctx.store_path);
        }
        self.toast(format!("Removed MCP server '{name}'"), ToastLevel::Info);
        self.refresh_mcp_rows();
    }

    /// Dispatch a login for the selected server to the background loop.
    pub(super) fn mcp_login_selected(&mut self) {
        if let Some(row) = self.mcp_rows.get(self.mcp_selected) {
            let name = row.name.clone();
            self.toast(format!("Logging in to '{name}'…"), ToastLevel::Info);
            let _ = self.mcp_cmd_tx.send(McpCommand::Login { name });
        }
    }

    /// Dispatch a connectivity test for the selected server to the background loop.
    pub(super) fn mcp_test_selected(&mut self) {
        if let Some(row) = self.mcp_rows.get(self.mcp_selected) {
            let name = row.name.clone();
            self.toast(format!("Testing '{name}'…"), ToastLevel::Info);
            let _ = self.mcp_cmd_tx.send(McpCommand::Test { name });
        }
    }

    /// Drain any completed background actions into toasts and refresh the rows.
    ///
    /// Called each tick while the dashboard is open, mirroring
    /// [`Self::sync_interactions`].
    pub(super) fn drain_mcp_outcomes(&mut self) {
        let mut changed = false;
        while let Ok(outcome) = self.mcp_outcome_rx.try_recv() {
            let level = if outcome.ok {
                ToastLevel::Info
            } else {
                ToastLevel::Error
            };
            self.toast(outcome.message, level);
            changed = true;
        }
        // A completed login/test may have changed stored auth, so refresh the
        // status column - but only while the screen is open, to avoid needless
        // disk reads every tick.
        if changed && self.mcp_screen {
            self.refresh_mcp_rows();
        }
    }
}

/// Build a display row for one server.
fn describe_row(server: &MCPServerConfig, store: &AuthStore, now: u64) -> McpRow {
    let (transport, endpoint) = match server.resolve() {
        Ok(leviath_mcp::ResolvedTransport::Stdio { command, .. }) => {
            ("stdio".to_string(), command.to_string())
        }
        Ok(leviath_mcp::ResolvedTransport::Http { url, .. }) => {
            ("http".to_string(), url.to_string())
        }
        Err(_) => ("invalid".to_string(), String::new()),
    };
    McpRow {
        name: server.name.clone(),
        transport,
        endpoint,
        auth: auth_status(server, store, now),
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
        // A configured `Authorization` header is a credential too, and "none"
        // in this column is what sends someone to press login on a server that
        // needs none.
        None if server.has_auth_header() => "header".to_string(),
        None => "none".to_string(),
    }
}

/// Parse an add-form line into a server config.
fn parse_add_line(line: &str) -> Result<MCPServerConfig, String> {
    let mut tokens = line.split_whitespace();
    let name = tokens
        .next()
        .ok_or_else(|| "Enter: <name> <url-or-command> [args…]".to_string())?;
    let target = tokens
        .next()
        .ok_or_else(|| format!("Server '{name}' needs a url or command"))?;

    // A name + a target always yields a valid http or stdio config, so no
    // further validation can fail here.
    let server = if target.starts_with("http://") || target.starts_with("https://") {
        MCPServerConfig::http(name, target)
    } else {
        let args = tokens.map(str::to_string).collect();
        MCPServerConfig::stdio(name, target, args)
    };
    Ok(server)
}

/// Background task running MCP logins and tests off the UI loop.
///
/// Mirrors `daemon_background_loop`: it owns the receiving end of the command
/// channel and reports each result back over `outcome_tx`, which the dashboard
/// drains into toasts. A dropped `outcome_tx` (dashboard gone) just ends the
/// task.
pub(super) async fn mcp_background_loop(
    ctx: McpContext,
    mut cmd_rx: mpsc::UnboundedReceiver<McpCommand>,
    outcome_tx: mpsc::UnboundedSender<McpOutcome>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        let outcome = match cmd {
            McpCommand::Login { name } => run_login(&ctx, &name).await,
            McpCommand::Test { name } => run_test(&ctx, &name).await,
        };
        if outcome_tx.send(outcome).is_err() {
            return; // dashboard dropped the receiver
        }
    }
}

/// Load the configured server by name, or an error outcome.
/// The server entry plus the `${VAR}` allowlist that goes with it.
///
/// Both come out of the same config read. Returning only the server is what
/// left the callers passing an empty allowlist, which refuses every `${VAR}`
/// header and made a server that works for an agent fail here.
fn find_server(ctx: &McpContext, name: &str) -> Result<(MCPServerConfig, Vec<String>), McpOutcome> {
    let config = Config::load_from_path_public(&ctx.config_path)
        .map_err(|e| fail(format!("Could not read config: {e}")))?;
    let allow_env = config.security.allow_env_vars.clone();
    config
        .mcp_servers
        .into_iter()
        .find(|s| s.name == name)
        .map(|server| (server, allow_env))
        .ok_or_else(|| fail(format!("No MCP server named '{name}'")))
}

/// Run the OAuth browser login for `name`.
async fn run_login(ctx: &McpContext, name: &str) -> McpOutcome {
    let (server, allow_env) = match find_server(ctx, name) {
        Ok(found) => found,
        Err(outcome) => return outcome,
    };
    let url = match server.resolve() {
        Ok(leviath_mcp::ResolvedTransport::Http { url, .. }) => url.to_string(),
        _ => return fail(format!("'{name}' uses stdio transport and cannot log in")),
    };
    let mut store = AuthStore::load(&ctx.store_path).unwrap_or_default();
    let reuse = store.get(name).map(|a| a.client_id.clone());
    match OAuthClient::new()
        .login(
            &url,
            &server.headers,
            &allow_env,
            ctx.opener.clone(),
            (ctx.clock)(),
            reuse.as_deref(),
        )
        .await
    {
        Ok(leviath_mcp::LoginOutcome::Authenticated(auth)) => {
            store.set(name, *auth);
            match store.save(&ctx.store_path) {
                Ok(()) => ok(format!("Authenticated with '{name}'")),
                Err(e) => fail(format!("Login succeeded but saving failed: {e}")),
            }
        }
        Ok(leviath_mcp::LoginOutcome::NotRequired) => ok(format!(
            "'{name}' needs no login: it accepted the configured request"
        )),
        Err(e) => fail(format!("Login failed for '{name}': {e}")),
    }
}

/// Connect to `name` and report its tool count.
async fn run_test(ctx: &McpContext, name: &str) -> McpOutcome {
    let (server, allow_env) = match find_server(ctx, name) {
        Ok(found) => found,
        Err(outcome) => return outcome,
    };
    let auth_header = match OAuthClient::new()
        .authorization_header(name, &ctx.store_path, (ctx.clock)())
        .await
    {
        Ok(header) => header,
        Err(e) => return fail(format!("Auth failed for '{name}': {e}")),
    };
    match connect_and_count(&server, auth_header, &allow_env, ctx.connect_timeout).await {
        Ok(count) => ok(format!("'{name}' connected · {count} tool(s)")),
        Err(e) => fail(format!("'{name}' failed: {e}")),
    }
}

/// Connect and return the tool count.
async fn connect_and_count(
    server: &MCPServerConfig,
    auth_header: Option<(String, String)>,
    allow_env: &[String],
    connect_timeout: std::time::Duration,
) -> anyhow::Result<usize> {
    let mut client = MCPClient::from_config_with_auth(server, auth_header, allow_env)
        .await?
        .with_connect_timeout(connect_timeout);
    client.connect().await?;
    let tools = client.list_tools().await?;
    let _ = client.shutdown().await;
    Ok(tools.len())
}

fn ok(message: String) -> McpOutcome {
    McpOutcome { message, ok: true }
}

fn fail(message: String) -> McpOutcome {
    McpOutcome { message, ok: false }
}

#[cfg(test)]
impl Dashboard {
    /// The retained command receiver, for asserting dispatched login/test.
    pub(super) fn mcp_cmd_rx_for_test(&mut self) -> &mut mpsc::UnboundedReceiver<McpCommand> {
        &mut self
            .mcp_bg_ends
            .as_mut()
            .expect("background ends retained in tests")
            .0
    }

    /// Inject an outcome as if the background loop had produced it.
    pub(super) fn inject_mcp_outcome_for_test(&self, outcome: McpOutcome) {
        self.mcp_bg_ends
            .as_ref()
            .expect("background ends retained in tests")
            .1
            .send(outcome)
            .expect("outcome receiver is alive");
    }

    /// The current toast messages, for assertions.
    pub(super) fn toast_messages_for_test(&self) -> Vec<String> {
        self.toasts.iter().map(|t| t.message.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;

    // ─── parse_add_line ───────────────────────────────────────────────────

    #[test]
    fn parse_add_line_reads_an_http_server() {
        let server = parse_add_line("remote https://e.com/mcp").unwrap();
        assert_eq!(server.name, "remote");
        assert_eq!(server.url.as_deref(), Some("https://e.com/mcp"));
    }

    #[test]
    fn parse_add_line_reads_a_stdio_server_with_args() {
        let server = parse_add_line("local npx -y @scope/server").unwrap();
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, vec!["-y", "@scope/server"]);
    }

    #[test]
    fn parse_add_line_needs_a_name() {
        assert!(parse_add_line("   ").is_err());
    }

    #[test]
    fn parse_add_line_needs_a_target() {
        let err = parse_add_line("lonely").unwrap_err();
        assert!(err.contains("needs a url or command"), "got: {err}");
    }

    // ─── describe_row / auth_status ───────────────────────────────────────

    #[test]
    fn describe_row_covers_each_transport() {
        let store = AuthStore::default();
        let http = describe_row(&MCPServerConfig::http("h", "https://e.com/mcp"), &store, 0);
        assert_eq!(http.transport, "http");
        assert_eq!(http.auth, "none");

        let stdio = describe_row(&MCPServerConfig::stdio("s", "npx", vec![]), &store, 0);
        assert_eq!(stdio.transport, "stdio");
        assert_eq!(stdio.auth, "n/a");

        let bad = describe_row(
            &MCPServerConfig {
                name: "b".to_string(),
                ..Default::default()
            },
            &store,
            0,
        );
        assert_eq!(bad.transport, "invalid");
    }

    #[test]
    fn auth_status_reports_authenticated_and_expired() {
        let http = MCPServerConfig::http("s", "https://e.com/mcp");
        let mut store = AuthStore::default();
        store.set(
            "s",
            leviath_mcp::ServerAuth {
                expires_at: 10_000,
                ..Default::default()
            },
        );
        assert_eq!(auth_status(&http, &store, 1_000), "authenticated");
        assert_eq!(auth_status(&http, &store, 20_000), "expired");
    }

    // ─── screen state operations ──────────────────────────────────────────

    fn dash_at(dir: &std::path::Path) -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.mcp_ctx.config_path = dir.join("config.toml");
        dash.mcp_ctx.store_path = dir.join("mcp-auth.json");
        dash
    }

    #[test]
    fn add_refresh_and_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());

        assert!(dash.mcp_add_from_line("remote https://e.com/mcp"));
        dash.refresh_mcp_rows();
        assert_eq!(dash.mcp_rows.len(), 1);
        assert_eq!(dash.mcp_rows[0].name, "remote");
        assert_eq!(dash.mcp_rows[0].auth, "none");

        dash.mcp_selected = 0;
        dash.mcp_remove_named("remote");
        assert!(dash.mcp_rows.is_empty());
    }

    #[test]
    fn add_rejects_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        assert!(dash.mcp_add_from_line("x npx"));
        assert!(
            !dash.mcp_add_from_line("x npx"),
            "duplicate must be rejected"
        );
    }

    #[test]
    fn add_rejects_a_malformed_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        assert!(!dash.mcp_add_from_line("lonely"));
        assert!(dash.mcp_rows.is_empty());
    }

    #[test]
    fn add_surfaces_an_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = make_test_dashboard();
        // config path is a directory → load fails.
        std::fs::create_dir(dir.path().join("cfg")).unwrap();
        dash.mcp_ctx.config_path = dir.path().join("cfg");
        dash.mcp_ctx.store_path = dir.path().join("s.json");
        assert!(!dash.mcp_add_from_line("x npx"));
    }

    #[test]
    fn add_surfaces_an_unwritable_config() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let mut dash = make_test_dashboard();
        dash.mcp_ctx.config_path = file.join("config.toml");
        dash.mcp_ctx.store_path = dir.path().join("s.json");
        assert!(!dash.mcp_add_from_line("x npx"));
    }

    #[test]
    fn remove_with_no_selection_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        // No rows loaded; requesting a removal opens nothing.
        dash.mcp_request_remove();
        assert!(dash.pending_confirm.is_none());
        assert!(dash.mcp_rows.is_empty());
    }

    #[test]
    fn remove_clears_stored_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.mcp_add_from_line("remote https://e.com/mcp");
        let mut store = AuthStore::default();
        store.set("remote", leviath_mcp::ServerAuth::default());
        store.save(&dash.mcp_ctx.store_path).unwrap();
        dash.refresh_mcp_rows();

        dash.mcp_selected = 0;
        dash.mcp_remove_named("remote");
        assert!(
            AuthStore::load(&dash.mcp_ctx.store_path)
                .unwrap()
                .get("remote")
                .is_none()
        );
    }

    #[test]
    fn remove_surfaces_an_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.mcp_add_from_line("x npx");
        dash.refresh_mcp_rows();
        // Replace the config file with a directory so the reload fails.
        std::fs::remove_file(&dash.mcp_ctx.config_path).unwrap();
        std::fs::create_dir(&dash.mcp_ctx.config_path).unwrap();
        dash.mcp_selected = 0;
        dash.mcp_remove_named("x");
        // Rows unchanged (still the pre-remove snapshot is fine); the point is
        // it did not panic and toasted an error.
    }

    #[test]
    fn remove_surfaces_an_unwritable_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.mcp_add_from_line("x npx");
        dash.refresh_mcp_rows();
        let mut perms = std::fs::metadata(&dash.mcp_ctx.config_path)
            .unwrap()
            .permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dash.mcp_ctx.config_path, perms).unwrap();
        dash.mcp_selected = 0;
        dash.mcp_remove_named("x");
    }

    #[test]
    fn refresh_clamps_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.mcp_add_from_line("a npx");
        dash.mcp_add_from_line("b npx");
        dash.refresh_mcp_rows();
        dash.mcp_selected = 5;
        dash.refresh_mcp_rows();
        assert_eq!(dash.mcp_selected, 1, "clamped to last row");
    }

    #[test]
    fn login_and_test_dispatch_commands() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.mcp_add_from_line("remote https://e.com/mcp");
        dash.refresh_mcp_rows();
        dash.mcp_selected = 0;

        dash.mcp_login_selected();
        dash.mcp_test_selected();
        // Both commands were queued for the background loop.
        assert_eq!(
            dash.mcp_cmd_rx_for_test().try_recv().unwrap(),
            McpCommand::Login {
                name: "remote".to_string()
            }
        );
        assert_eq!(
            dash.mcp_cmd_rx_for_test().try_recv().unwrap(),
            McpCommand::Test {
                name: "remote".to_string()
            }
        );
    }

    #[test]
    fn login_and_test_with_no_selection_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        // No rows: nothing queued.
        dash.mcp_login_selected();
        dash.mcp_test_selected();
        assert!(dash.mcp_cmd_rx_for_test().try_recv().is_err());
    }

    #[test]
    fn draining_outcomes_toasts_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.mcp_screen = true;
        dash.inject_mcp_outcome_for_test(McpOutcome {
            message: "done".to_string(),
            ok: true,
        });
        dash.inject_mcp_outcome_for_test(McpOutcome {
            message: "boom".to_string(),
            ok: false,
        });
        dash.drain_mcp_outcomes();
        // Two toasts, one info one error.
        assert!(dash.toast_messages_for_test().iter().any(|m| m == "done"));
        assert!(dash.toast_messages_for_test().iter().any(|m| m == "boom"));
    }

    #[test]
    fn draining_with_nothing_pending_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.drain_mcp_outcomes();
        assert!(dash.toast_messages_for_test().is_empty());
    }

    // ─── background loop ──────────────────────────────────────────────────

    fn ctx_at(
        dir: &std::path::Path,
        opener: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> McpContext {
        McpContext {
            config_path: dir.join("config.toml"),
            store_path: dir.join("mcp-auth.json"),
            opener: std::sync::Arc::new(opener),
            clock: || 1_000,
            connect_timeout: TEST_CONNECT_TIMEOUT,
        }
    }

    /// The handshake deadline these tests use, and why it is not the 30s a
    /// person gets.
    ///
    /// The warm-up below removes the interpreter's cold start; this removes
    /// the machine. On 2026-08-21 a `windows-latest` job stopped executing for
    /// 159 seconds - zero tests completed, the binary took 241s against a
    /// normal 40s - with all four of this suite's subprocess tests in flight.
    /// A wall-clock deadline inside a process that is not running measures the
    /// runner, not the server. Bounded rather than removed, so a genuinely
    /// wedged server still fails instead of hanging CI.
    const TEST_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    fn no_browser(_: &str) -> bool {
        false
    }

    /// Pay an interpreter's cold-start cost before a test's own clock starts.
    ///
    /// `connect()` allows 30s for `initialize`. That is generous for an MCP
    /// server already resident in the page cache and tight for one that is
    /// not: on a loaded CI runner a first `python3` can spend most of that
    /// budget in the loader, and on Windows in the virus scanner, before it
    /// reaches its first read. That is a property of the machine, not of the
    /// code under test, and it has failed this suite in CI.
    ///
    /// Running a trivial program first leaves the image cached, so the
    /// handshake these tests actually measure starts warm. A machine with no
    /// `python3` is unaffected: the warm-up fails, and the test that follows
    /// reports the same spawn failure it always did.
    async fn warm_interpreter() {
        let _ = tokio::process::Command::new("python3")
            .args(["-c", ""])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }

    fn write_config(ctx: &McpContext, server: MCPServerConfig) {
        let mut config = Config::default();
        config.mcp_servers.push(server);
        config.save_to_path_public(&ctx.config_path).unwrap();
    }

    use axum::extract::State as AxumState;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};

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
                post(|| async { Json(serde_json::json!({ "client_id": "tui-client" })) }),
            )
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "tui-access",
                        "refresh_token": "tui-refresh",
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
            let cb = format!("{redirect}?code=tui-code&state={state}");
            let _ = reqwest::Client::new().get(&cb).send().await;
        });
        true
    }

    #[test]
    fn no_browser_reports_no_browser() {
        assert!(!no_browser("https://x"));
    }

    #[tokio::test]
    async fn background_login_succeeds_and_stores_the_token() {
        let base = mock_oauth_server().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), auto_consent);
        write_config(
            &ctx,
            MCPServerConfig::http("navigator", format!("{base}/mcp")),
        );
        let outcome = run_login(&ctx, "navigator").await;
        assert!(outcome.ok, "got: {}", outcome.message);
        assert!(
            outcome.message.contains("Authenticated"),
            "got: {}",
            outcome.message
        );
        let store = AuthStore::load(&ctx.store_path).unwrap();
        assert_eq!(store.get("navigator").unwrap().access_token, "tui-access");
    }

    /// Pressing login in the dashboard on a header-authenticated server reports
    /// that none is needed, rather than a discovery failure it cannot act on.
    #[tokio::test]
    async fn background_login_says_so_when_no_login_is_needed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Publishes no OAuth metadata, so an attempted discovery fails loudly.
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|| async { axum::http::StatusCode::OK }),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));

        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        let mut server = MCPServerConfig::http("hub", format!("{base}/mcp"));
        server.headers.insert(
            "Authorization".to_string(),
            "Bearer configured-token".to_string(),
        );
        write_config(&ctx, server);

        let outcome = run_login(&ctx, "hub").await;
        assert!(outcome.ok, "got: {}", outcome.message);
        assert!(
            outcome.message.contains("needs no login"),
            "got: {}",
            outcome.message
        );
        let store = AuthStore::load(&ctx.store_path).unwrap();
        assert!(store.get("hub").is_none());

        // The screen's own column agrees, so nobody presses login again.
        let config = Config::load_from_path_public(&ctx.config_path).unwrap();
        assert_eq!(auth_status(&config.mcp_servers[0], &store, 0), "header");
    }

    #[tokio::test]
    async fn background_login_reports_a_store_write_failure() {
        let base = mock_oauth_server().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), auto_consent);
        write_config(
            &ctx,
            MCPServerConfig::http("navigator", format!("{base}/mcp")),
        );
        // Read-only store file: login succeeds but persisting the token fails.
        AuthStore::default().save(&ctx.store_path).unwrap();
        let mut perms = std::fs::metadata(&ctx.store_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ctx.store_path, perms).unwrap();
        let outcome = run_login(&ctx, "navigator").await;
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("saving failed"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_loop_handles_a_login_command() {
        let base = mock_oauth_server().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), auto_consent);
        write_config(
            &ctx,
            MCPServerConfig::http("navigator", format!("{base}/mcp")),
        );
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(mcp_background_loop(ctx, cmd_rx, out_tx));
        cmd_tx
            .send(McpCommand::Login {
                name: "navigator".to_string(),
            })
            .unwrap();
        let outcome = out_rx.recv().await.unwrap();
        assert!(outcome.ok, "got: {}", outcome.message);
        drop(cmd_tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn background_login_of_an_unknown_server_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        let outcome = run_login(&ctx, "ghost").await;
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("No MCP server named"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_login_of_a_stdio_server_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        write_config(&ctx, MCPServerConfig::stdio("local", "npx", vec![]));
        let outcome = run_login(&ctx, "local").await;
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("cannot log in"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_login_reports_a_discovery_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        write_config(
            &ctx,
            MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp"),
        );
        let outcome = run_login(&ctx, "remote").await;
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("Login failed"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_login_reuses_a_prior_client_id() {
        let base = mock_oauth_server().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), auto_consent);
        write_config(
            &ctx,
            MCPServerConfig::http("navigator", format!("{base}/mcp")),
        );
        // First login stores a client_id; the second reuses it (store.get Some).
        assert!(run_login(&ctx, "navigator").await.ok);
        assert!(run_login(&ctx, "navigator").await.ok);
    }

    #[tokio::test]
    async fn background_test_reports_a_spawn_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        write_config(
            &ctx,
            MCPServerConfig::stdio("x", "definitely-not-a-real-binary-xyz", vec![]),
        );
        assert!(
            !run_test(&ctx, "x").await.ok,
            "spawn failure must be reported"
        );
    }

    #[tokio::test]
    async fn background_test_reports_a_list_tools_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
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
        write_config(
            &ctx,
            MCPServerConfig::stdio("local", "python3", vec!["-c".to_string(), stub.to_string()]),
        );
        warm_interpreter().await;
        let outcome = run_test(&ctx, "local").await;
        assert!(!outcome.ok, "a tools/list error must be reported");
        // Not merely `!ok`: this server fails to start on a machine with no
        // `python3`, and that also reports `!ok`. Naming the server's own error
        // is what distinguishes "tools/list was answered with an error" from
        // "nothing ever ran", which is the only thing this test is about.
        assert!(
            outcome.message.contains("boom"),
            "the failure must be the server's tools/list error, not a spawn \
             or handshake failure; got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_test_of_an_unknown_server_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        let outcome = run_test(&ctx, "ghost").await;
        assert!(!outcome.ok);
    }

    #[tokio::test]
    async fn background_test_reports_a_connect_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        write_config(
            &ctx,
            MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp"),
        );
        let outcome = run_test(&ctx, "remote").await;
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("failed"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_test_reports_an_unrefreshable_token() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        write_config(
            &ctx,
            MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp"),
        );
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
        store.save(&ctx.store_path).unwrap();
        let outcome = run_test(&ctx, "remote").await;
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("Auth failed"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_test_lists_tools_of_a_live_stdio_server() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
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
        write_config(
            &ctx,
            MCPServerConfig::stdio("local", "python3", vec!["-c".to_string(), stub.to_string()]),
        );
        warm_interpreter().await;
        let outcome = run_test(&ctx, "local").await;
        assert!(outcome.ok, "got: {}", outcome.message);
        assert!(
            outcome.message.contains("1 tool"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn find_server_reports_an_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        std::fs::create_dir(&ctx.config_path).unwrap();
        let outcome = find_server(&ctx, "x").expect_err("dir config must fail");
        assert!(
            outcome.message.contains("Could not read config"),
            "got: {}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn background_loop_processes_a_command_and_reports_back() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(mcp_background_loop(ctx, cmd_rx, out_tx));

        cmd_tx
            .send(McpCommand::Test {
                name: "ghost".to_string(),
            })
            .unwrap();
        let outcome = out_rx.recv().await.unwrap();
        assert!(!outcome.ok);

        // Dropping the command sender ends the loop.
        drop(cmd_tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn background_loop_stops_when_the_outcome_receiver_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_at(dir.path(), no_browser);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<McpOutcome>();
        drop(out_rx); // dashboard gone
        let handle = tokio::spawn(mcp_background_loop(ctx, cmd_rx, out_tx));
        cmd_tx
            .send(McpCommand::Test {
                name: "ghost".to_string(),
            })
            .unwrap();
        // The loop tries to send the outcome, finds the receiver gone, and ends.
        handle.await.unwrap();
    }
}
