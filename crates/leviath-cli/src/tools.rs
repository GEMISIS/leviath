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
        let mut mcp_tool_defs = Vec::new();

        if !config.mcp_servers.is_empty() {
            let mut discovery = ToolDiscovery::new();
            let oauth = leviath_mcp::OAuthClient::new();
            let store_path = leviath_mcp::AuthStore::default_path();
            let now = unix_now_secs();
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
                )
                .await
                {
                    Ok(header) => header,
                    Err(e) => {
                        tracing::warn!(server = %server_cfg.name, error = %e, "MCP auth unavailable — skipping");
                        continue;
                    }
                };
                match discovery
                    .discover_from_config_with_auth(server_cfg, auth_header)
                    .await
                {
                    Ok((tool_metas, client)) => {
                        mcp_executor.add_client(server_cfg.name.clone(), client);
                        for meta in tool_metas {
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
                        tracing::warn!("Failed to connect MCP server — skipping");
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
/// somehow before the epoch — which reads every token as expired and forces a
/// refresh attempt, the safe direction.
fn unix_now_secs() -> u64 {
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
async fn resolve_bearer(
    oauth: &leviath_mcp::OAuthClient,
    server_name: &str,
    store_path: Option<&std::path::Path>,
    now: u64,
) -> anyhow::Result<Option<(String, String)>> {
    match store_path {
        Some(path) => oauth.authorization_header(server_name, path, now).await,
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
        // These tools ARE the human-in-the-loop mechanism — gating them behind
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

/// Resolve the effective policy for a tool call, narrowest scope first.
///
/// Precedence (first match wins):
/// 1. `launch_overrides` — from `--allow`/`--ask`/`--deny` / `--yolo` flags
/// 2. `stage_permissions` — `[stages.x.tool_permissions]` in agent.leviath
/// 3. `agent_permissions` — `[tool_permissions]` in agent.leviath
/// 4. `global_permissions` — `[tool_permissions]` in `~/.leviath/config.toml`
/// 5. Built-in defaults
pub fn resolve_policy(
    tool_name: &str,
    is_builtin: bool,
    launch_overrides: &HashMap<String, ToolPolicy>,
    stage_permissions: &HashMap<String, String>,
    agent_permissions: &HashMap<String, String>,
    global_permissions: &HashMap<String, ToolPolicy>,
) -> ToolPolicy {
    // 1. Launch overrides (highest priority)
    if let Some(p) = launch_overrides.get(tool_name) {
        return *p;
    }
    // Wildcard launch allow ("--yolo")
    if let Some(p) = launch_overrides.get("*") {
        return *p;
    }

    // 2. Stage-level (from blueprint string map "allow"/"ask"/"deny")
    if let Some(s) = stage_permissions.get(tool_name) {
        return parse_policy_str(s);
    }

    // 3. Agent-level
    if let Some(s) = agent_permissions.get(tool_name) {
        return parse_policy_str(s);
    }

    // 4. Global config
    if let Some(p) = global_permissions.get(tool_name) {
        return *p;
    }

    // 5. Built-in defaults
    default_tool_policy(tool_name, is_builtin)
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
    // test fixture -- a real (but fast, local, no-network) subprocess round
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
    async fn build_skips_mcp_server_that_fails_to_connect() {
        // A nonexistent command fails to spawn, exercising the `Err(e)` arm
        // ("Failed to connect MCP server -- skipping") instead of the
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

    #[tokio::test]
    async fn resolve_bearer_without_a_store_is_none() {
        let oauth = leviath_mcp::OAuthClient::new();
        let header = resolve_bearer(&oauth, "srv", None, 0).await.unwrap();
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
        // These tools ARE the human-in-the-loop mechanism — they must not
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

    #[test]
    fn test_resolve_policy_stage_beats_global() {
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

    #[test]
    fn test_resolve_policy_agent_beats_global() {
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
        assert_eq!(policy, ToolPolicy::Allow);
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

    #[test]
    fn test_resolve_policy_launch_overrides_stage() {
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
        assert_eq!(policy, ToolPolicy::Allow);
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

    #[test]
    fn test_resolve_policy_full_chain_launch_highest() {
        let mut launch = HashMap::new();
        launch.insert("bash".to_string(), ToolPolicy::Allow);
        let mut stage = HashMap::new();
        stage.insert("bash".to_string(), "deny".to_string());
        let mut agent = HashMap::new();
        agent.insert("bash".to_string(), "deny".to_string());
        let mut global = HashMap::new();
        global.insert("bash".to_string(), ToolPolicy::Deny);

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
