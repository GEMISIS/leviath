//! The shared, lazily-connected MCP pool (issue #97).
//!
//! Per-agent `[[mcp_servers]]` let a blueprint carry its own MCP tool
//! dependencies. Rather than spawn a connection per agent, all agents share one
//! [`leviath_mcp::ToolExecutor`] (the client store) fronted by this pool: a
//! server is connected **on first use**, deduped by its config signature, and
//! its tools reused by every agent that declares it. Connection is async and
//! happens in the daemon's spawn preprocessor (before the sync spawner runs), so
//! the pool is warm by the time an agent's tools are advertised.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use leviath_mcp::{MCPServerConfig, ToolDiscovery, ToolExecutor};
use leviath_providers::Tool;
use tokio::sync::Mutex;

/// A shared MCP connection pool over one executor, caching each connected
/// server's advertised tool defs by config signature.
pub struct McpPool {
    /// The shared client store; every agent dispatches MCP calls through it.
    shared: Arc<Mutex<ToolExecutor>>,
    /// Names reserved against MCP advertisement (built-in + sub-agent tools) so a
    /// server tool can't collide with a core tool.
    reserved: HashSet<String>,
    /// Signature → the server's advertised `Tool` defs (once connected). A `std`
    /// mutex (held only briefly, never across `.await`) so the sync spawner can
    /// read it from a runtime thread without `blocking_lock`'s panic.
    connected: StdMutex<HashMap<String, Vec<Tool>>>,
}

/// A stable dedup key for a server config: its full serialized form. Two
/// blueprints declaring an identical server share one connection; a difference in
/// name/command/url/args/env/headers is a distinct server.
fn signature(config: &MCPServerConfig) -> String {
    // Serializing a plain config never fails; fall back to an empty key rather
    // than carry a dead error closure.
    serde_json::to_string(config).unwrap_or_default()
}

impl McpPool {
    /// Build a pool over `shared`, reserving `reserved` names from advertisement.
    pub fn new(shared: Arc<Mutex<ToolExecutor>>, reserved: HashSet<String>) -> Self {
        Self {
            shared,
            reserved,
            connected: StdMutex::new(HashMap::new()),
        }
    }

    /// Seed the cache with an already-connected server's defs (used at startup for
    /// the global config servers, connected once by `ToolRegistry::build`).
    pub fn seed(&self, config: &MCPServerConfig, defs: Vec<Tool>) {
        self.connected
            .lock()
            .unwrap()
            .insert(signature(config), defs);
    }

    /// Ensure `config` is connected (idempotent by signature) and return its
    /// advertised tool defs. A connection failure logs and returns no defs (the
    /// agent simply doesn't get that server's tools); it is not cached, so a later
    /// spawn retries.
    pub async fn ensure(&self, config: &MCPServerConfig) -> Vec<Tool> {
        let sig = signature(config);
        if let Some(defs) = self.connected.lock().unwrap().get(&sig) {
            return defs.clone();
        }
        let mut discovery = ToolDiscovery::new();
        match discovery.discover_from_config(config).await {
            Ok((_metas, client)) => {
                let advertised = self.shared.lock().await.add_client_advertised(
                    config.name.clone(),
                    client,
                    &self.reserved,
                );
                let defs: Vec<Tool> = advertised
                    .into_iter()
                    .map(|m| Tool {
                        name: m.name,
                        description: m.description,
                        parameters: m.schema,
                    })
                    .collect();
                self.connected.lock().unwrap().insert(sig, defs.clone());
                // Pre-format the count so the tracing field carries no inline
                // method call (an uncoverable macro sub-region otherwise).
                let count = defs.len();
                tracing::info!(server = %config.name, tools = count, "connected per-agent MCP server");
                defs
            }
            Err(e) => {
                let err = e.to_string();
                tracing::warn!(server = %config.name, error = %err, "failed to connect per-agent MCP server");
                Vec::new()
            }
        }
    }

    /// The cached defs for every config in `configs` (pool must already be warm
    /// for them — call [`Self::ensure`] first). Unknown/unconnected configs
    /// contribute nothing. This is the sync read the spawner uses.
    pub fn cached_defs_for(&self, configs: &[MCPServerConfig]) -> Vec<Tool> {
        let cache = self.connected.lock().unwrap();
        configs
            .iter()
            .filter_map(|c| cache.get(&signature(c)))
            .flatten()
            .cloned()
            .collect()
    }
}

/// Parse a blueprint manifest's `[[mcp_servers]]` array (issue #97). Parsed
/// CLI-side because `leviath-core` cannot depend on `leviath-mcp` (that crate
/// already depends on core — a cycle). Returns an empty vec when the section is
/// absent or malformed; a malformed entry is skipped with a warning.
pub fn parse_blueprint_mcp_servers(manifest_toml: &str) -> Vec<MCPServerConfig> {
    let Ok(value) = manifest_toml.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(array) = value.get("mcp_servers").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in array {
        match entry.clone().try_into::<MCPServerConfig>() {
            Ok(cfg) => out.push(cfg),
            Err(e) => tracing::warn!(error = %e, "skipping malformed [[mcp_servers]] entry"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;

    /// A minimal stdio MCP server (python3) speaking initialize / tools/list /
    /// tools/call — mirrors the fixtures in `tools.rs`.
    const STUB: &str = r#"
import sys, json
def respond(i, r):
    sys.stdout.write(json.dumps({"jsonrpc":"2.0","id":i,"result":r})+"\n"); sys.stdout.flush()
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    req=json.loads(line); m=req.get("method",""); i=req.get("id")
    if m=="initialize": respond(i,{"capabilities":{"tools":{"listChanged":True}},"protocolVersion":"2024-11-05"})
    elif m=="notifications/initialized": pass
    elif m=="tools/list": respond(i,{"tools":[{"name":"echo","description":"e","inputSchema":{"type":"object","properties":{}}}]})
    elif m=="tools/call": respond(i,{"content":[{"type":"text","text":"ok"}],"isError":False})
    else: respond(i,{})
"#;

    fn stub_config(name: &str) -> MCPServerConfig {
        MCPServerConfig::stdio(name, "python3", vec!["-c".to_string(), STUB.to_string()])
    }

    fn pool() -> McpPool {
        McpPool::new(Arc::new(Mutex::new(ToolExecutor::new())), HashSet::new())
    }

    #[tokio::test]
    async fn ensure_connects_and_caches_by_signature() {
        with_tracing(|| {});
        let pool = pool();
        let cfg = stub_config("s");
        let defs = pool.ensure(&cfg).await;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
        // Second ensure of the same signature hits the cache (no reconnect).
        let again = pool.ensure(&cfg).await;
        assert_eq!(again.len(), 1);
    }

    #[tokio::test]
    async fn ensure_failure_returns_empty_and_is_not_cached() {
        with_tracing(|| {});
        let pool = pool();
        let bad = MCPServerConfig::stdio("bad", "definitely-not-a-binary-xyz", vec![]);
        assert!(pool.ensure(&bad).await.is_empty());
        // Not cached: cached_defs_for finds nothing for it.
        assert!(pool.cached_defs_for(std::slice::from_ref(&bad)).is_empty());
    }

    #[test]
    fn seed_then_cached_defs_for_reads_without_connecting() {
        let pool = pool();
        let cfg = stub_config("seeded");
        pool.seed(
            &cfg,
            vec![Tool {
                name: "seed_tool".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
        );
        let names: Vec<String> = pool
            .cached_defs_for(std::slice::from_ref(&cfg))
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["seed_tool".to_string()]);
    }

    #[test]
    fn parse_blueprint_mcp_servers_reads_array() {
        let toml = r#"
[agent]
name = "x"
[[mcp_servers]]
name = "search"
command = "leviath-search"
args = ["--provider", "brave"]
[[mcp_servers]]
name = "http-one"
url = "http://localhost:9/mcp"
"#;
        let servers = parse_blueprint_mcp_servers(toml);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "search");
        assert_eq!(servers[0].command.as_deref(), Some("leviath-search"));
        assert_eq!(servers[1].url.as_deref(), Some("http://localhost:9/mcp"));
    }

    #[test]
    fn parse_blueprint_mcp_servers_absent_or_malformed() {
        // No section → empty.
        assert!(parse_blueprint_mcp_servers("[agent]\nname='x'").is_empty());
        // Not even valid TOML → empty.
        assert!(parse_blueprint_mcp_servers("this is = = not toml").is_empty());
        // Section present but not an array of tables → empty (as_array is None).
        assert!(parse_blueprint_mcp_servers("mcp_servers = 5").is_empty());
        // A malformed entry (name is not a string) is skipped with a warning.
        with_tracing(|| {});
        let servers = parse_blueprint_mcp_servers("[[mcp_servers]]\nname = 5\n");
        assert!(servers.is_empty());
    }
}
