//! Keeping the daemon's global `[[mcp_servers]]` in step with `config.toml`.
//!
//! `lev mcp add` and `POST /api/mcp/servers` write `[[mcp_servers]]` and stop
//! there, so this module reconciles the connected set against the reloaded
//! config before each spawn: a server added after boot is callable by the next
//! run, not by the next daemon. The pool beside it already knows how to connect
//! a server lazily, dedupe it by signature and tear an unused one down, so the
//! work here is deciding *what* changed and letting the pool do the rest.
//!
//! The advertised defs are read back off the pool's own cache rather than kept
//! in a second list here. Two lists of the same tools is how a def list falls
//! out of step, and one built from the config would claim a server that failed
//! to start.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use leviath_mcp::MCPServerConfig;
use leviath_providers::Tool;

use crate::config::Config;
use crate::daemon::mcp_pool::{McpPool, signature};

/// The global MCP set as it currently stands, and what it advertises.
struct State {
    /// The `[[mcp_servers]]` entries the defs below were built from.
    installed: Vec<MCPServerConfig>,
    /// `[security] allow_env_vars` as of that reconcile. A server's `${VAR}`
    /// headers are resolved at connect time, so a change here is a change to
    /// the connection even when the entry itself is untouched.
    allow_env_vars: Vec<String>,
    /// What those servers advertise, and which of them advertises each tool.
    defs: Vec<Tool>,
    owners: leviath_runtime::pipeline::ToolOwners,
}

/// Reconciles the daemon's global MCP servers with `config.toml`.
pub struct McpReload {
    pool: Arc<McpPool>,
    state: Mutex<State>,
}

impl McpReload {
    /// Start from the servers the daemon booted with and the tools they
    /// advertised.
    pub(crate) fn new(
        config: &Config,
        pool: Arc<McpPool>,
        defs: Vec<Tool>,
        owners: leviath_runtime::pipeline::ToolOwners,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            state: Mutex::new(State {
                installed: config.mcp_servers.clone(),
                allow_env_vars: config.security.allow_env_vars.clone(),
                defs,
                owners,
            }),
        })
    }

    /// What the global servers advertise right now, for the sync spawner.
    pub(crate) fn current(&self) -> (Vec<Tool>, leviath_runtime::pipeline::ToolOwners) {
        let state = self.lock();
        (state.defs.clone(), state.owners.clone())
    }

    /// Bring the connected global set in line with `config`, so the next run
    /// sees the servers the file names now.
    ///
    /// Connecting is lazy in the sense that matters: a server already up under
    /// the same signature is left alone, and a new one is connected here, once,
    /// rather than on the spawn that needs it. A connection that fails is not
    /// cached, so the next spawn tries again - a server whose command is not
    /// installed yet costs a run its tools, never its spawn.
    pub(crate) async fn refresh(&self, config: &Config) {
        // First, so every connection opened below - including a per-agent
        // server the blueprint warmer connects a moment later - is opened under
        // the `[security]` the file names now.
        self.pool.apply_security(config);
        let wanted = config.mcp_servers.clone();
        let allow = config.security.allow_env_vars.clone();
        let (installed, allow_before) = {
            let state = self.lock();
            (state.installed.clone(), state.allow_env_vars.clone())
        };
        // Compared by the pool's own signature, which is the key the
        // connections are stored under: two entries the pool would dedupe into
        // one connection are the same entry here too, whatever else differs.
        let wanted_sigs: HashSet<String> = wanted.iter().map(signature).collect();
        let installed_sigs: Vec<String> = installed.iter().map(signature).collect();
        let unchanged = installed_sigs.len() == wanted.len()
            && installed_sigs.iter().all(|sig| wanted_sigs.contains(sig));
        if unchanged && allow == allow_before {
            return;
        }
        let env_changed = allow != allow_before;
        let wanted_names: HashSet<&str> = wanted.iter().map(|s| s.name.as_str()).collect();
        for (old, sig) in installed.iter().zip(&installed_sigs) {
            // An entry whose exact form is still in the file stays connected,
            // unless the allowlist that resolved its headers has moved: the
            // header was interpolated at connect time and nothing revisits it.
            let kept = wanted_sigs.contains(sig);
            let stale_headers = env_changed && interpolates_env(old);
            if kept && !stale_headers {
                continue;
            }
            // Same name, different entry: the replacement connects under this
            // name in a moment, so the old connection has to be gone first.
            let replaced = wanted_names.contains(old.name.as_str());
            self.pool
                .retire_global(old, replaced || stale_headers)
                .await;
        }
        for server in &wanted {
            self.pool.ensure_global(server).await;
        }
        // Read back off the pool rather than accumulated above: what a run can
        // actually call is what the pool has connected, and a def list built
        // from the config would claim a server that failed to start.
        let defs = self.pool.cached_defs_for(&wanted);
        let owners = self.pool.cached_owners_for(&wanted);
        let count = defs.len();
        let servers = wanted.len();
        tracing::info!(
            servers,
            tools = count,
            "reconciled the global MCP servers with config.toml"
        );
        let mut state = self.lock();
        state.installed = wanted;
        state.allow_env_vars = allow;
        state.defs = defs;
        state.owners = owners;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether any of `config`'s headers reference an environment variable, and so
/// answer to `[security] allow_env_vars`. Only HTTP headers do; a stdio
/// server's environment is filtered by a different rule that the allowlist has
/// no part in.
fn interpolates_env(config: &MCPServerConfig) -> bool {
    config.headers.values().any(|v| v.contains("${"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stub MCP server over HTTP rather than a spawned script: every case
    /// here turns on *when* a connection is opened, and an in-process server
    /// can count that. It also keeps the whole module off `python3`, which is
    /// not a name Windows has.
    struct Stub {
        base: String,
        /// How many MCP handshakes this server has answered, which is how many
        /// times the pool has connected to it.
        connects: Arc<AtomicUsize>,
    }

    impl Stub {
        /// An entry naming this server, advertising one tool called `tool`
        /// unless an `X-Token` header says otherwise.
        fn entry(&self, name: &str, tool: &str) -> MCPServerConfig {
            MCPServerConfig::http(name, format!("{}/mcp/{tool}", self.base))
        }

        fn connects(&self) -> usize {
            self.connects.load(Ordering::Relaxed)
        }
    }

    /// Stand one up. `tools/list` answers with a single tool named after the
    /// `X-Token` header when it carries one, and after the URL's last segment
    /// otherwise - so the header the daemon actually sent is readable straight
    /// off the advertised tool list.
    async fn stub_server() -> Stub {
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        let connects = Arc::new(AtomicUsize::new(0));
        let counter = connects.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/mcp/{fallback}",
            post(
                move |axum::extract::Path(fallback): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap,
                      body: String| {
                    let counter = counter.clone();
                    async move {
                        let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));
                        let result = match req.get("method").and_then(|m| m.as_str()) {
                            Some("initialize") => {
                                counter.fetch_add(1, Ordering::Relaxed);
                                serde_json::json!({
                                    "capabilities": {}, "protocolVersion": "2024-11-05"
                                })
                            }
                            Some("tools/list") => {
                                let name = headers
                                    .get("x-token")
                                    .and_then(|v| v.to_str().ok())
                                    .filter(|v| !v.is_empty())
                                    .map_or(fallback, str::to_string);
                                serde_json::json!({"tools": [{"name": name, "inputSchema": {}}]})
                            }
                            _ => serde_json::json!({}),
                        };
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
                                .into_response()
                                .into_body(),
                        )
                            .into_response()
                    }
                },
            ),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        Stub { base, connects }
    }

    fn config_with(servers: Vec<MCPServerConfig>) -> Config {
        Config {
            mcp_servers: servers,
            ..Config::default()
        }
    }

    /// A pool whose retired servers wait `idle` seconds before the connection
    /// is torn down, the same window an unleased per-agent server gets.
    fn pool_with_idle(idle: u64) -> Arc<McpPool> {
        McpPool::for_daemon_with(
            Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
            Default::default(),
            Vec::new(),
            idle,
        )
    }

    fn pool() -> Arc<McpPool> {
        pool_with_idle(crate::daemon::mcp_pool::DEFAULT_MCP_IDLE_DISCONNECT_SECS)
    }

    /// A reload starting from a daemon that booted with no MCP servers at all.
    fn booted_empty_on(pool: Arc<McpPool>) -> Arc<McpReload> {
        McpReload::new(
            &config_with(Vec::new()),
            pool,
            Vec::new(),
            Default::default(),
        )
    }

    fn booted_empty() -> Arc<McpReload> {
        booted_empty_on(pool())
    }

    /// A reload as the daemon really boots one: `ToolRegistry::build` has
    /// connected `server` and the pool has been seeded with what it
    /// advertises, so the handle starts out holding that server's tools.
    async fn booted_with(pool: Arc<McpPool>, server: &MCPServerConfig) -> Arc<McpReload> {
        let defs = pool.ensure_global(server).await;
        let owners = pool.cached_owners_for(std::slice::from_ref(server));
        McpReload::new(&config_with(vec![server.clone()]), pool, defs, owners)
    }

    /// Wait until `pool` can no longer dispatch `tool`. The teardown of a
    /// retired server runs on its own task, so the assertion is about it
    /// having happened rather than about when the scheduler got to it.
    async fn until_disconnected(pool: &McpPool, tool: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while pool.routes(tool).await {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a server the config no longer names has to be disconnected");
    }

    /// The advertised tool names, sorted so an assertion does not depend on
    /// connection order.
    fn names(reload: &McpReload) -> Vec<String> {
        let mut out: Vec<String> = reload.current().0.into_iter().map(|t| t.name).collect();
        out.sort();
        out
    }

    /// `LEVIATH_HOME` at a fresh temp dir, so the OAuth store the connect path
    /// reads is empty rather than the real `~/.leviath`.
    async fn with_temp_home<F, Fut, T>(body: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let dir = tempfile::tempdir().unwrap();
        temp_env::async_with_vars(
            [("LEVIATH_HOME", Some(dir.path().to_str().unwrap()))],
            body(),
        )
        .await
    }

    /// A server written into `[[mcp_servers]]` after boot is connected and
    /// advertised on the next reconcile, so the next run can call it.
    #[tokio::test]
    async fn a_server_added_after_boot_is_callable_by_the_next_run() {
        with_tracing(|| {});
        let stub = stub_server().await;
        with_temp_home(|| async {
            let reload = booted_empty();
            assert!(names(&reload).is_empty());

            reload
                .refresh(&config_with(vec![stub.entry("added", "echo")]))
                .await;
            assert_eq!(
                names(&reload),
                vec!["added__echo".to_string()],
                "a server the user just added has to be advertised without a restart"
            );
            assert!(
                reload.pool.routes("added__echo").await,
                "and it has to actually route, not just appear in the list"
            );
        })
        .await;
    }

    /// And the other direction: a server taken out of the config stops being
    /// offered, rather than lingering for the daemon's life.
    #[tokio::test]
    async fn a_server_removed_from_the_config_is_gone_from_the_next_run() {
        with_tracing(|| {});
        let stub = stub_server().await;
        with_temp_home(|| async {
            let reload = booted_with(pool(), &stub.entry("gone", "echo")).await;
            assert_eq!(names(&reload), vec!["gone__echo".to_string()]);

            reload.refresh(&config_with(Vec::new())).await;
            assert!(
                names(&reload).is_empty(),
                "the entry the user deleted must stop being advertised"
            );
        })
        .await;
    }

    /// The connection outlives the advertisement by the pool's idle window,
    /// which is the point: a run part-way through a stage keeps calling the
    /// tool it was given rather than losing it mid-call, while the next run
    /// never sees it. The window is `[limits] mcp_idle_disconnect_secs`, the
    /// same one an unleased per-agent server gets; zero here so the test does
    /// not wait a minute for it.
    #[tokio::test]
    async fn a_removed_server_is_disconnected_after_its_grace_window() {
        with_tracing(|| {});
        let stub = stub_server().await;
        with_temp_home(|| async {
            let reload = booted_with(pool_with_idle(0), &stub.entry("gone", "echo")).await;
            assert!(reload.pool.routes("gone__echo").await);

            reload.refresh(&config_with(Vec::new())).await;
            until_disconnected(&reload.pool, "gone__echo").await;
        })
        .await;
    }

    /// A reconcile that changes nothing must not churn the connections. Every
    /// teardown costs a child process, and for an OAuth-backed server it risks
    /// a fresh interactive grant - on a path that runs before every spawn.
    #[tokio::test]
    async fn an_unchanged_config_leaves_the_connections_alone() {
        with_tracing(|| {});
        let stub = stub_server().await;
        with_temp_home(|| async {
            let config = config_with(vec![stub.entry("keep", "echo")]);
            let reload = booted_empty();
            reload.refresh(&config).await;
            assert_eq!(stub.connects(), 1);

            reload.refresh(&config).await;
            reload.refresh(&config).await;
            assert_eq!(stub.connects(), 1, "the same connection, not one per spawn");
            assert_eq!(names(&reload), vec!["keep__echo".to_string()]);
        })
        .await;
    }

    /// An *edited* entry keeps its name, so the old connection has to be gone
    /// before the new one opens: they are stored under one key, and a deferred
    /// teardown would take the replacement down with it a minute later.
    #[tokio::test]
    async fn an_edited_entry_replaces_its_connection_rather_than_racing_it() {
        with_tracing(|| {});
        let stub = stub_server().await;
        with_temp_home(|| async {
            let reload = booted_empty();
            reload
                .refresh(&config_with(vec![stub.entry("srv", "before")]))
                .await;
            assert_eq!(names(&reload), vec!["srv__before".to_string()]);

            reload
                .refresh(&config_with(vec![stub.entry("srv", "after")]))
                .await;
            assert_eq!(
                names(&reload),
                vec!["srv__after".to_string()],
                "the edited entry's tools, not the ones it replaced"
            );
            assert!(reload.pool.routes("srv__after").await);
            assert_eq!(stub.connects(), 2);
        })
        .await;
    }

    /// A server that cannot be reached costs the run its tools and nothing
    /// else, and the failure is not cached - fixing the URL and saving again
    /// is enough, with no restart in that loop either.
    #[tokio::test]
    async fn a_server_that_will_not_connect_costs_its_tools_and_not_the_spawn() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let reload = booted_empty();
            let dead = MCPServerConfig::http("dead", "http://127.0.0.1:1/mcp");
            reload.refresh(&config_with(vec![dead])).await;
            assert!(names(&reload).is_empty());
        })
        .await;
    }

    /// Naming a variable in `[security] allow_env_vars` reaches an MCP server's
    /// `${VAR}` header on the next load: the server reconnects because the
    /// allowlist that resolved its headers moved, even though its own entry did
    /// not.
    ///
    /// `MY_MCP_TOKEN` is credential-shaped, so it is refused until it is
    /// named - which is what makes the first half a real "before" rather than
    /// an unset variable.
    #[tokio::test]
    async fn a_newly_allowed_env_var_reaches_an_mcp_header() {
        with_tracing(|| {});
        let stub = stub_server().await;
        let mut server = stub.entry("hdr", "nothing");
        server
            .headers
            .insert("X-Token".to_string(), "${MY_MCP_TOKEN}".to_string());

        with_temp_home(|| async {
            temp_env::async_with_vars([("MY_MCP_TOKEN", Some("letmein"))], async {
                let denied = config_with(vec![server.clone()]);
                let reload = booted_empty();
                reload.refresh(&denied).await;
                assert_eq!(
                    names(&reload),
                    vec!["hdr__nothing".to_string()],
                    "a credential-shaped variable nobody allowed must not be sent"
                );

                let mut allowed = denied.clone();
                allowed.security.allow_env_vars = vec!["MY_MCP_TOKEN".to_string()];
                reload.refresh(&allowed).await;
                assert_eq!(
                    names(&reload),
                    vec!["hdr__letmein".to_string()],
                    "the variable the user just allowed has to reach the header, no restart"
                );
                assert_eq!(stub.connects(), 2, "the header is resolved at connect time");
            })
            .await;
        })
        .await;
    }

    /// The allowlist only reaches HTTP headers, so a change to it is no reason
    /// to tear down a server that interpolates nothing. Same reconcile, same
    /// connection.
    #[tokio::test]
    async fn an_allowlist_change_leaves_a_server_with_no_interpolation_alone() {
        with_tracing(|| {});
        let stub = stub_server().await;
        with_temp_home(|| async {
            let plain = config_with(vec![stub.entry("plain", "echo")]);
            let reload = booted_empty();
            reload.refresh(&plain).await;
            assert_eq!(stub.connects(), 1);

            let mut widened = plain.clone();
            widened.security.allow_env_vars = vec!["SOMETHING_ELSE_TOKEN".to_string()];
            reload.refresh(&widened).await;
            assert_eq!(stub.connects(), 1, "nothing about this server changed");
            assert_eq!(names(&reload), vec!["plain__echo".to_string()]);
        })
        .await;
    }

    /// Only HTTP headers answer to the allowlist, so only they mark an entry
    /// stale when it moves.
    #[test]
    fn only_a_header_that_interpolates_is_stale_when_the_allowlist_moves() {
        let mut http = MCPServerConfig::http("h", "https://example.test/mcp");
        assert!(!interpolates_env(&http));
        // A stdio server's environment is filtered by a different rule that
        // the allowlist has no part in, even when it names variables.
        let mut stdio = MCPServerConfig::stdio("s", "true", vec![]);
        stdio
            .env
            .insert("TOKEN".to_string(), "${MY_TOKEN}".to_string());
        assert!(!interpolates_env(&stdio));

        http.headers.insert(
            "Authorization".to_string(),
            "Bearer ${MY_TOKEN}".to_string(),
        );
        assert!(interpolates_env(&http));
        http.headers
            .insert("Authorization".to_string(), "Bearer literal".to_string());
        assert!(!interpolates_env(&http));
    }

    /// The boot path: the global servers `ToolRegistry::build` already
    /// connected are filed under their own signatures, each with the defs it
    /// contributed, so the pool alone can answer what the set advertises and
    /// there is no second list to fall out of step.
    #[test]
    fn seeding_files_each_global_servers_own_defs_under_it() {
        let a = MCPServerConfig::http("a", "https://a.test/mcp");
        let b = MCPServerConfig::http("b", "https://b.test/mcp");
        let defs: Vec<Tool> = ["a__one", "b__two"]
            .into_iter()
            .map(|name| Tool {
                name: name.to_string(),
                description: String::new(),
                parameters: serde_json::json!({}),
            })
            .collect();
        let owners: leviath_runtime::pipeline::ToolOwners = [
            ("a__one".to_string(), "a".to_string()),
            ("b__two".to_string(), "b".to_string()),
        ]
        .into_iter()
        .collect();
        let pool = pool();
        pool.seed_all(&[a.clone(), b.clone()], &defs, &owners);

        assert_eq!(
            pool.cached_defs_for(&[a.clone(), b.clone()])
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>(),
            vec!["a__one".to_string(), "b__two".to_string()],
            "the whole set reads back, in config order"
        );
        assert_eq!(
            pool.cached_owners_for(&[b]).get("b__two"),
            Some(&"b".to_string()),
            "and each tool still names the server it came from"
        );
        assert_eq!(pool.cached_defs_for(&[a]).len(), 1);
    }
}
