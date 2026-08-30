//! The shared, lazily-connected MCP pool.
//!
//! Per-agent `[[mcp_servers]]` let a blueprint carry its own MCP tool
//! dependencies. Rather than spawn a connection per agent, all agents share one
//! [`leviath_mcp::ToolExecutor`] (the client store) fronted by this pool: a
//! server is connected **on first use**, deduped by its config signature, and
//! its tools reused by every agent that declares it. Connection is async and is
//! driven from every spawn path: the spawn preprocessor for top-level and
//! sub-agent spawns (both run in the serve loop), `McpPool::warm_recovered` for
//! runs reloaded on daemon restart, and a detached warm task for fan-out workers.
//! So the pool is warm by the time an agent's tools are advertised.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

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
    /// The `[security]` settings that shape a *connection* rather than the
    /// pool: where MCP OAuth grants are kept, and which credential-shaped
    /// variables a server's `${VAR}` headers may interpolate.
    ///
    /// Behind a lock because they were boot copies, and the docs said
    /// otherwise: naming a variable in `allow_env_vars` was supposed to take
    /// effect on the next load, and for an MCP server it took effect on the
    /// next daemon restart. `mcp_reload` writes them on each config reconcile,
    /// and they are read when a server is connected.
    security: StdMutex<PoolSecurity>,
    /// Per-run leases on per-agent servers (see [`Self::lease_blueprint`]).
    /// Same `std` mutex discipline as `connected`: held briefly, never across
    /// an `.await`.
    leases: StdMutex<LeaseTable>,
    /// How long a per-agent server may sit with zero leasing runs before its
    /// connection (and, for stdio servers, its child process) is torn down.
    /// Zero disables disconnection - the pre-lease behavior, where every
    /// server any blueprint ever declared stayed connected for the daemon's
    /// life.
    idle_disconnect: std::time::Duration,
}

/// The connection-shaping half of `[security]`, as of the last config
/// reconcile.
struct PoolSecurity {
    /// Where MCP OAuth grants are read from and written back to. Defaults to
    /// the file store, which is also the config default - a pool built without
    /// being told reads `mcp-auth.json`.
    credential_store: leviath_core::CredentialStoreKind,
    /// `[security] allow_env_vars`.
    allow_env_vars: Vec<String>,
}

/// Which runs hold which per-agent servers open.
#[derive(Default)]
struct LeaseTable {
    /// Signature → the server's lease state.
    servers: HashMap<String, ServerLease>,
    /// Run id → the signatures it holds, so a reap releases them all.
    runs: HashMap<String, Vec<String>>,
    /// Signatures of the global config servers, seeded at startup: their
    /// lifecycle belongs to the daemon, never to a run, so they are exempt
    /// from idle disconnection.
    global: HashSet<String>,
}

/// What an idle-disconnect tick found.
#[derive(Debug, PartialEq, Eq)]
enum IdleOutcome {
    /// The server was idle: its client is taken and shut down.
    Disconnected,
    /// Something leased or released the server since the tick was scheduled,
    /// or there was no client to take. Nothing to do, now or later.
    Stale,
    /// A call still held the client, so the server was kept as it was; worth
    /// asking again after another grace window.
    Busy,
}

/// One per-agent server's lease state.
struct ServerLease {
    /// The server's name - the key the executor stores its client under.
    name: String,
    /// The runs currently holding it open.
    holders: HashSet<String>,
    /// Bumped on every lease and release, so a disconnect scheduled when the
    /// count hit zero is a no-op if anything touched the server since.
    generation: u64,
}

/// A stable dedup key for a server config: its full serialized form. Two
/// blueprints declaring an identical server share one connection; a difference in
/// name/command/url/args/env/headers is a distinct server.
pub(crate) fn signature(config: &MCPServerConfig) -> String {
    // Serializing a plain config never fails; fall back to an empty key rather
    // than carry a dead error closure.
    serde_json::to_string(config).unwrap_or_default()
}

/// Default for how long a per-agent MCP server may sit with zero leasing runs
/// before its connection is torn down. Long enough that back-to-back runs of
/// the same blueprint reuse the warm connection (and never re-trigger an OAuth
/// flow between them); short enough that a one-off run's servers do not hold
/// child processes and buffers for the daemon's remaining life.
pub const DEFAULT_MCP_IDLE_DISCONNECT_SECS: u64 = 60;

impl McpPool {
    /// Build a pool over `shared`, reserving `reserved` names from advertisement.
    pub(crate) fn new(shared: Arc<Mutex<ToolExecutor>>, reserved: HashSet<String>) -> Self {
        Self {
            shared,
            reserved,
            connected: StdMutex::new(HashMap::new()),
            security: StdMutex::new(PoolSecurity {
                credential_store: leviath_core::CredentialStoreKind::default(),
                allow_env_vars: Vec::new(),
            }),
            leases: StdMutex::new(LeaseTable::default()),
            idle_disconnect: std::time::Duration::from_secs(DEFAULT_MCP_IDLE_DISCONNECT_SECS),
        }
    }

    /// How long a per-agent server may sit unleased before disconnection.
    /// `0` disables it.
    pub(crate) fn with_idle_disconnect_secs(self, secs: u64) -> Self {
        self.with_idle_disconnect(std::time::Duration::from_secs(secs))
    }

    /// [`Self::with_idle_disconnect_secs`] at any resolution. Config only
    /// speaks in whole seconds; a test of the scheduled path wants a window
    /// short enough not to wait on the wall clock.
    pub(crate) fn with_idle_disconnect(mut self, grace: std::time::Duration) -> Self {
        self.idle_disconnect = grace;
        self
    }

    /// Allow these credential-shaped variables in MCP `${VAR}` headers.
    pub(crate) fn with_env_allowlist(self, allow: Vec<String>) -> Self {
        self.lock_security().allow_env_vars = allow;
        self
    }

    /// Read and write MCP grants through `kind`'s backend.
    pub(crate) fn with_credential_store(self, kind: leviath_core::CredentialStoreKind) -> Self {
        self.lock_security().credential_store = kind;
        self
    }

    /// Adopt `config`'s `[security]` for every connection opened from now on.
    ///
    /// Called from the config reconcile beside this, before anything is
    /// connected: a server whose `${VAR}` header the user has just allowed is
    /// reconnected there, and this is what makes the reconnect see the new
    /// allowlist.
    pub(crate) fn apply_security(&self, config: &crate::config::Config) {
        let mut security = self.lock_security();
        security.credential_store = config.security.credential_store;
        security
            .allow_env_vars
            .clone_from(&config.security.allow_env_vars);
    }

    fn lock_security(&self) -> std::sync::MutexGuard<'_, PoolSecurity> {
        self.security.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Build the daemon's shared pool over `shared_mcp`: reserve built-in and
    /// sub-agent tool names (so a server tool can't shadow a core one) and seed
    /// the already-connected global `config_servers` with empty defs, so a
    /// blueprint that re-declares one doesn't open a duplicate connection.
    pub fn for_daemon(
        shared_mcp: Arc<Mutex<ToolExecutor>>,
        config_servers: &[MCPServerConfig],
    ) -> Arc<Self> {
        Self::for_daemon_with(
            shared_mcp,
            config_servers,
            leviath_core::CredentialStoreKind::default(),
            Vec::new(),
            DEFAULT_MCP_IDLE_DISCONNECT_SECS,
        )
    }

    /// [`for_daemon`](Self::for_daemon) reading and writing MCP OAuth grants
    /// through `credential_store`'s backend.
    ///
    /// The pool refreshes lapsed tokens and writes them back, so it has to write
    /// them where the user asked for them to be kept - otherwise the first
    /// refresh after a keychain migration would put a fresh refresh token back
    /// on disk.
    pub(crate) fn for_daemon_with(
        shared_mcp: Arc<Mutex<ToolExecutor>>,
        config_servers: &[MCPServerConfig],
        credential_store: leviath_core::CredentialStoreKind,
        allow_env_vars: Vec<String>,
        idle_disconnect_secs: u64,
    ) -> Arc<Self> {
        let mut reserved: HashSet<String> =
            leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(std::env::temp_dir()))
                .names()
                .into_iter()
                .collect();
        reserved.extend(leviath_tools::BuiltinTools::subagent_tool_names());
        let pool = Arc::new(
            Self::new(shared_mcp, reserved)
                .with_credential_store(credential_store)
                .with_env_allowlist(allow_env_vars)
                .with_idle_disconnect_secs(idle_disconnect_secs),
        );
        for server in config_servers {
            pool.seed(server, Vec::new());
        }
        pool
    }

    /// Seed the cache with an already-connected server's defs (used at startup for
    /// the global config servers, connected once by `ToolRegistry::build`).
    /// Seeded servers are global: their lifecycle belongs to the daemon, so
    /// they are exempt from lease-driven idle disconnection.
    pub(crate) fn seed(&self, config: &MCPServerConfig, defs: Vec<Tool>) {
        let sig = signature(config);
        self.leases
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .global
            .insert(sig.clone());
        self.connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(sig, defs);
    }

    /// Seed every global server in `servers` with the defs it contributed to
    /// `defs`, worked out from `owners`.
    ///
    /// The alternative is [`seed`](Self::seed)'s empty vec, which is what the
    /// startup path used while the global servers' defs lived in a separate
    /// list beside the pool. Filing each server's own defs under its signature
    /// makes [`cached_defs_for`](Self::cached_defs_for) the single answer to
    /// "what does this set of servers advertise", for the global set as much as
    /// a blueprint's - which is what lets the global set change under a running
    /// daemon without a second list to keep in step.
    pub(crate) fn seed_all(
        &self,
        servers: &[MCPServerConfig],
        defs: &[Tool],
        owners: &leviath_runtime::pipeline::ToolOwners,
    ) {
        for server in servers {
            let mine: Vec<Tool> = defs
                .iter()
                .filter(|t| owners.get(&t.name).is_some_and(|o| *o == server.name))
                .cloned()
                .collect();
            self.seed(server, mine);
        }
    }

    /// [`ensure`](Self::ensure) for a server the *config* declares: the
    /// connection belongs to the daemon rather than to any run, so it is exempt
    /// from lease-driven idle disconnection, and a pending retirement from a
    /// moment ago (the user removed it and put it back) is cancelled.
    pub(crate) async fn ensure_global(&self, config: &MCPServerConfig) -> Vec<Tool> {
        let sig = signature(config);
        {
            let mut table = self.leases.lock().unwrap_or_else(PoisonError::into_inner);
            table.global.insert(sig.clone());
            // Dropping the row is what makes a scheduled disconnect a no-op:
            // it looks for the row before it touches anything.
            table.servers.remove(&sig);
        }
        self.ensure(config).await
    }

    /// Stop holding `config`'s server open on the daemon's behalf: it is no
    /// longer in `[[mcp_servers]]`.
    ///
    /// `at_once` tears the connection down before returning, for an *edited*
    /// entry - the replacement connects under the same name straight
    /// afterwards, and a delayed teardown would take that one down with it.
    /// Otherwise the server is handed to the same grace timer an unleased
    /// per-agent server gets, so a run part-way through a stage keeps the tool
    /// working for that window rather than losing it mid-call.
    pub(crate) async fn retire_global(self: &Arc<Self>, config: &MCPServerConfig, at_once: bool) {
        let sig = signature(config);
        let name = config.name.clone();
        let generation = {
            let mut table = self.leases.lock().unwrap_or_else(PoisonError::into_inner);
            table.global.remove(&sig);
            let entry = table
                .servers
                .entry(sig.clone())
                .or_insert_with(|| ServerLease {
                    name: name.clone(),
                    holders: HashSet::new(),
                    generation: 0,
                });
            entry.generation += 1;
            entry.generation
        };
        let handle = tokio::runtime::Handle::try_current().ok();
        match (at_once, handle) {
            // No runtime to schedule on (a sync test) is the immediate case too:
            // a deferred teardown that never runs is a leaked child process.
            (true, _) | (false, None) => {
                self.disconnect_if_still_idle(&sig, &name, generation).await;
            }
            (false, Some(handle)) => {
                let pool = Arc::clone(self);
                handle.spawn(async move {
                    tokio::time::sleep(pool.idle_disconnect).await;
                    pool.disconnect_if_still_idle(&sig, &name, generation).await;
                });
            }
        }
    }

    /// Record `run_id` as holding every per-agent server `blueprint_path`
    /// declares, so the connections stay up exactly as long as some run needs
    /// them. Global (seeded) servers are skipped. A missing or unreadable
    /// manifest leases nothing.
    ///
    /// Called from every path that brings a run into the world with a
    /// blueprint: the spawner, the restart reloader, and the fan-out worker
    /// spawner. The matching release is [`Self::release_run`], from the reap
    /// hook.
    pub(crate) fn lease_blueprint(&self, blueprint_path: &str, run_id: &str) {
        let Ok(toml) = std::fs::read_to_string(blueprint_path) else {
            return;
        };
        let mut table = self.leases.lock().unwrap_or_else(PoisonError::into_inner);
        for server in parse_blueprint_mcp_servers(&toml) {
            let sig = signature(&server);
            if table.global.contains(&sig) {
                continue;
            }
            let entry = table
                .servers
                .entry(sig.clone())
                .or_insert_with(|| ServerLease {
                    name: server.name.clone(),
                    holders: HashSet::new(),
                    generation: 0,
                });
            entry.generation += 1;
            if entry.holders.insert(run_id.to_string()) {
                table.runs.entry(run_id.to_string()).or_default().push(sig);
            }
        }
    }

    /// Release every lease `run_id` holds. Servers whose holder count reaches
    /// zero get an idle-disconnect scheduled (when a runtime is available and
    /// `idle_disconnect` is non-zero); a new lease during the grace window
    /// bumps the generation and turns the pending disconnect into a no-op.
    pub(crate) fn release_run(self: &Arc<Self>, run_id: &str) {
        let zeroed = self.release_run_bookkeeping(run_id);
        if self.idle_disconnect.is_zero() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return; // no runtime (a sync test): bookkeeping only
        };
        for (sig, name, generation) in zeroed {
            handle.spawn(Arc::clone(self).grace_disconnect(
                self.idle_disconnect,
                sig,
                name,
                generation,
            ));
        }
    }

    /// The grace timer: wait `grace`, then tear the server down if it is still
    /// idle. A server with a call in flight when the timer fires is left alone
    /// and the wait starts over, so an idle disconnect that loses the race to a
    /// late call is postponed, not forgotten.
    async fn grace_disconnect(
        self: Arc<Self>,
        grace: std::time::Duration,
        sig: String,
        name: String,
        generation: u64,
    ) {
        loop {
            tokio::time::sleep(grace).await;
            if self.disconnect_if_still_idle(&sig, &name, generation).await != IdleOutcome::Busy {
                return;
            }
        }
    }

    /// The synchronous half of [`Self::release_run`]: drop the run's leases and
    /// return the `(signature, name, generation)` of every server that now has
    /// zero holders.
    fn release_run_bookkeeping(&self, run_id: &str) -> Vec<(String, String, u64)> {
        let mut table = self.leases.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(sigs) = table.runs.remove(run_id) else {
            return Vec::new();
        };
        let mut zeroed = Vec::new();
        for sig in sigs {
            let Some(entry) = table.servers.get_mut(&sig) else {
                continue;
            };
            entry.holders.remove(run_id);
            entry.generation += 1;
            if entry.holders.is_empty() {
                zeroed.push((sig.clone(), entry.name.clone(), entry.generation));
            }
        }
        zeroed
    }

    /// Tear a server down if nothing touched it since `generation`: take its
    /// client out of the shared executor, forget its cached defs (so the next
    /// spawn reconnects lazily), and shut it down - which is what actually
    /// ends a stdio server's child process. A server kept because a call is
    /// in flight is [`IdleOutcome::Busy`], and the grace timer waits again for
    /// that one.
    ///
    /// The client is taken before any pool bookkeeping goes. The other order
    /// (forget the lease row and the cached defs, then ask the executor) lost
    /// the server for the daemon's life whenever the executor kept it: the
    /// next tick found no lease row and returned early, so nothing ever asked
    /// again, the child process never got `shutdown()`, and the tool stayed
    /// routable for a run that no longer leased it.
    async fn disconnect_if_still_idle(
        &self,
        sig: &str,
        name: &str,
        generation: u64,
    ) -> IdleOutcome {
        // The executor lock first, so a lease taken while waiting for it is
        // seen by the idle check below rather than after the client is gone.
        let mut executor = self.shared.lock().await;
        let client = {
            let mut table = self.leases.lock().unwrap_or_else(PoisonError::into_inner);
            let still_idle = table
                .servers
                .get(sig)
                .is_some_and(|e| e.holders.is_empty() && e.generation == generation);
            if !still_idle {
                return IdleOutcome::Stale;
            }
            let client = executor.remove_client(name);
            if client.is_none() && executor.has_client(name) {
                // A call still holds the client: keep the lease row and the
                // cached defs so the next tick finds the server and retries.
                return IdleOutcome::Busy;
            }
            table.servers.remove(sig);
            client
        };
        // Still under the executor lock: `ensure` inserts into the cache only
        // while holding that lock, so a spawn racing this tick sees either the
        // old connection whole or nothing, never cached defs with no client.
        self.connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(sig);
        drop(executor);
        match client {
            Some(mut client) => {
                let _ = client.shutdown().await;
                tracing::info!(server = %name, "disconnected idle per-agent MCP server");
                IdleOutcome::Disconnected
            }
            // Leased but never connected (or its connect failed): nothing to
            // shut down, and nothing left to retry.
            None => IdleOutcome::Stale,
        }
    }

    /// Whether the shared executor can still dispatch `tool`. The
    /// advertisement and the connection are separate records, and the bug this
    /// distinguishes is one going stale without the other.
    #[cfg(test)]
    pub(crate) async fn routes(&self, tool: &str) -> bool {
        self.shared.lock().await.route(tool).is_ok()
    }

    /// The signatures currently holding leases, for tests and diagnostics.
    #[cfg(test)]
    fn leased_holders(&self, config: &MCPServerConfig) -> usize {
        self.leases
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .servers
            .get(&signature(config))
            .map_or(0, |e| e.holders.len())
    }

    /// Ensure `config` is connected (idempotent by signature) and return its
    /// advertised tool defs. A connection failure logs and returns no defs (the
    /// agent simply doesn't get that server's tools); it is not cached, so a later
    /// spawn retries.
    pub(crate) async fn ensure(&self, config: &MCPServerConfig) -> Vec<Tool> {
        let sig = signature(config);
        if let Some(defs) = self
            .connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&sig)
        {
            return defs.clone();
        }
        // Resolve a stored OAuth bearer for an HTTP server (refreshing it
        // non-interactively if lapsed); `None` for stdio / unauthenticated /
        // static-header servers. Mirrors `ToolRegistry::build`.
        let oauth = leviath_mcp::OAuthClient::new();
        let store_path = leviath_mcp::AuthStore::default_path();
        // Read out before the connect: both sit behind a `std` mutex, and the
        // connect awaits.
        let (store_kind, allow_env) = {
            let security = self.lock_security();
            (security.credential_store, security.allow_env_vars.clone())
        };
        let credentials =
            crate::tools::credential_store_or_warn(crate::credentials::store_for(store_kind));
        let auth = match crate::tools::resolve_bearer(
            &oauth,
            &config.name,
            store_path.as_deref(),
            crate::tools::unix_now_secs(),
            credentials.as_deref(),
        )
        .await
        {
            Ok(header) => header,
            Err(e) => {
                let err = e.to_string();
                tracing::warn!(server = %config.name, error = %err, "MCP auth unavailable - skipping");
                return Vec::new();
            }
        };
        let auth_was_resolved = auth.is_some();
        let mut discovery = ToolDiscovery::new();
        match discovery
            .discover_from_config_with_auth(config, auth, &allow_env)
            .await
        {
            Ok((_metas, mut client)) => {
                // Attach a refresher so an OAuth-backed server that outlives its
                // access token re-auths on a 401 instead of failing every call.
                if auth_was_resolved && let Some(path) = store_path.clone() {
                    client.set_refresher(std::sync::Arc::new(
                        leviath_mcp::StoredTokenRefresher::new(config.name.clone(), path),
                    ));
                }
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
                self.connected
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(sig, defs.clone());
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

    /// Connect every server in `servers` (idempotent). Takes `Arc<Self>` + owned
    /// `servers` so it can be `tokio::spawn`ed directly as a detached warm task
    /// (e.g. by the fan-out spawner) without a wrapping closure.
    pub(crate) async fn ensure_all(self: Arc<Self>, servers: Vec<MCPServerConfig>) {
        for server in servers {
            self.ensure(&server).await;
        }
    }

    /// Warm the per-agent `[[mcp_servers]]` of every non-terminal persisted run in
    /// `runs_dir`, so a run reloaded on daemon restart can still *execute* its
    /// blueprint MCP tools (their advertisement is restored from the snapshot;
    /// only the shared connection is lost across a restart). Blueprint paths are
    /// collected synchronously, then connected - no fs iterator is held across an
    /// `.await`.
    pub(crate) async fn warm_recovered(&self, runs_dir: &std::path::Path) {
        use leviath_core::run_meta::RunStatus;
        let Ok(entries) = std::fs::read_dir(runs_dir) else {
            return;
        };
        let mut paths: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let Ok(text) =
                std::fs::read_to_string(entry.path().join(leviath_core::files::META_FILE))
            else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<leviath_core::run_meta::RunMeta>(&text) else {
                continue;
            };
            // Only runs that recovery will actually reload (non-terminal).
            if matches!(
                meta.status,
                RunStatus::Starting | RunStatus::Running | RunStatus::WaitingInput
            ) {
                paths.push(meta.agent_path);
            }
        }
        for path in paths {
            if let Ok(toml) = std::fs::read_to_string(&path) {
                for server in parse_blueprint_mcp_servers(&toml) {
                    self.ensure(&server).await;
                }
            }
        }
    }

    /// Which server advertises each cached tool, for the same `configs`.
    ///
    /// Derived from the same cache rather than stored separately: every def
    /// filed under a config's signature came from that config's server, so a
    /// second map would only be a chance for the two to disagree.
    pub(crate) fn cached_owners_for(
        &self,
        configs: &[MCPServerConfig],
    ) -> leviath_runtime::pipeline::ToolOwners {
        let cache = self
            .connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        configs
            .iter()
            .filter_map(|c| cache.get(&signature(c)).map(|defs| (c, defs)))
            .flat_map(|(c, defs)| defs.iter().map(|t| (t.name.clone(), c.name.clone())))
            .collect()
    }

    /// The cached defs for every config in `configs` (pool must already be warm
    /// for them - call [`Self::ensure`] first). Unknown/unconnected configs
    /// contribute nothing. This is the sync read the spawner uses.
    pub(crate) fn cached_defs_for(&self, configs: &[MCPServerConfig]) -> Vec<Tool> {
        let cache = self
            .connected
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        configs
            .iter()
            .filter_map(|c| cache.get(&signature(c)))
            .flatten()
            .cloned()
            .collect()
    }
}

/// Parse a blueprint manifest's `[[mcp_servers]]` array. Parsed
/// CLI-side because `leviath-core` cannot depend on `leviath-mcp` (that crate
/// already depends on core - a cycle). Returns an empty vec when the section is
/// absent or malformed; a malformed entry is skipped with a warning.
pub(crate) fn parse_blueprint_mcp_servers(manifest_toml: &str) -> Vec<MCPServerConfig> {
    // `toml::from_str`, not `manifest_toml.parse::<toml::Value>()`. In toml 1.x
    // `FromStr for Value` parses a single *value*, not a document - so a real
    // manifest starting with `[agent]` reads as an array literal followed by
    // junk and fails. It still compiles, so the change is silent; the tests are
    // what caught it.
    let Ok(value) = toml::from_str::<toml::Value>(manifest_toml) else {
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
    use crate::test_support::{McpStub, with_tracing};

    /// A minimal stdio MCP server (python3) speaking initialize / tools/list /
    /// tools/call - mirrors the fixtures in `tools.rs`.
    fn stub() -> McpStub {
        McpStub::new()
            .list_changed(true)
            .tool("echo", Some("e"))
            .input_schema(r#"{"type": "object", "properties": {}}"#)
            .replying("ok")
    }

    fn stub_config(name: &str) -> MCPServerConfig {
        MCPServerConfig::stdio(name, "python3", vec!["-c".to_string(), stub().source()])
    }

    fn pool() -> McpPool {
        McpPool::new(Arc::new(Mutex::new(ToolExecutor::new())), HashSet::new())
    }

    /// Run `body` with `LEVIATH_HOME` at a fresh temp dir so the OAuth auth store
    /// resolves to an empty, hermetic location rather than the real `~/.leviath`.
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
    async fn ensure_connects_and_caches_by_signature() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let pool = pool();
            let cfg = stub_config("s");
            // A stdio server has no OAuth bearer (the `None` auth path).
            let defs = pool.ensure(&cfg).await;
            assert_eq!(defs.len(), 1);
            assert_eq!(defs[0].name, "s__echo");
            // Second ensure of the same signature hits the cache (no reconnect).
            let again = pool.ensure(&cfg).await;
            assert_eq!(again.len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn ensure_all_connects_each_server() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let pool = Arc::new(pool());
            let cfg = stub_config("s");
            pool.clone().ensure_all(vec![cfg.clone()]).await;
            assert_eq!(pool.cached_defs_for(std::slice::from_ref(&cfg)).len(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn ensure_failure_returns_empty_and_is_not_cached() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let pool = pool();
            let bad = MCPServerConfig::stdio("bad", "definitely-not-a-binary-xyz", vec![]);
            assert!(pool.ensure(&bad).await.is_empty());
            // Not cached: cached_defs_for finds nothing for it.
            assert!(pool.cached_defs_for(std::slice::from_ref(&bad)).is_empty());
        })
        .await;
    }

    /// A minimal streamable-HTTP MCP server that lists one tool. Returns its base
    /// URL. Mirrors the `tools.rs` OAuth fixture.
    async fn mock_http_mcp_server() -> String {
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/mcp",
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
    async fn ensure_resolves_oauth_bearer_and_attaches_refresher() {
        // A live stored token → the auth-resolved branch + set_refresher.
        with_tracing(|| {});
        let base = mock_http_mcp_server().await;
        let defs = with_temp_home(|| async {
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
            let pool = pool();
            pool.ensure(&MCPServerConfig::http("remote", format!("{base}/mcp")))
                .await
        })
        .await;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "remote__remote_tool");
    }

    #[tokio::test]
    async fn ensure_returns_empty_when_bearer_cannot_be_resolved() {
        // An expired token with an unreachable refresh endpoint → resolve_bearer
        // errors → the auth `Err` arm returns no defs.
        with_tracing(|| {});
        let defs = with_temp_home(|| async {
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
            let pool = pool();
            pool.ensure(&MCPServerConfig::http("remote", "http://127.0.0.1:1/mcp"))
                .await
        })
        .await;
        assert!(defs.is_empty());
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

    /// Write a python MCP stub to a temp file; returns (tempdir, path).
    fn stub_py() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stub.py");
        std::fs::write(&path, stub().source()).unwrap();
        (dir, path)
    }

    /// Write a blueprint declaring one stdio `[[mcp_servers]]` → `stub_py`; returns
    /// its manifest path.
    fn blueprint_declaring(server: &str, stub: &std::path::Path) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            format!(
                // Single-quoted TOML literal so a Windows path's backslashes
                // aren't parsed as string escapes (`\\U…` → invalid unicode).
                "[agent]\nname = \"a\"\n\n[[mcp_servers]]\nname = \"{server}\"\ncommand = \"python3\"\nargs = ['{}']\n",
                stub.to_string_lossy()
            ),
        )
        .unwrap();
        (dir, manifest.to_string_lossy().to_string())
    }

    fn write_run_meta(
        runs_dir: &std::path::Path,
        run_id: &str,
        agent_path: &str,
        status: leviath_core::run_meta::RunStatus,
    ) {
        let dir = runs_dir.join(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut meta = leviath_core::run_meta::RunMeta::new(
            run_id.to_string(),
            "a".to_string(),
            agent_path.to_string(),
            "t".to_string(),
            None,
            std::env::temp_dir().to_string_lossy().to_string(),
            1,
        );
        meta.status = status;
        std::fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn warm_recovered_connects_only_nonterminal_run_blueprints() {
        use leviath_core::run_meta::RunStatus;
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd_live, live_bp) = blueprint_declaring("liveserver", &stub);
            let (_bd_done, done_bp) = blueprint_declaring("doneserver", &stub);
            let runs = tempfile::tempdir().unwrap();
            write_run_meta(runs.path(), "run-live", &live_bp, RunStatus::Running);
            write_run_meta(runs.path(), "run-done", &done_bp, RunStatus::Complete);
            // A non-terminal run whose blueprint file no longer exists → the
            // "unreadable manifest" arm (skipped, no panic).
            write_run_meta(
                runs.path(),
                "run-gone",
                "/no/such/agent.leviath",
                RunStatus::WaitingInput,
            );
            // A junk dir with no meta.json is skipped without error.
            std::fs::create_dir_all(runs.path().join("junk")).unwrap();
            // A dir with an unparseable meta.json is skipped (the parse-error arm).
            std::fs::create_dir_all(runs.path().join("garbled")).unwrap();
            std::fs::write(runs.path().join("garbled/meta.json"), "not json {{").unwrap();

            let pool = pool();
            pool.warm_recovered(runs.path()).await;

            // The non-terminal run's server is connected; the terminal one is not.
            let live_servers =
                parse_blueprint_mcp_servers(&std::fs::read_to_string(&live_bp).unwrap());
            let done_servers =
                parse_blueprint_mcp_servers(&std::fs::read_to_string(&done_bp).unwrap());
            assert_eq!(pool.cached_defs_for(&live_servers).len(), 1);
            assert!(pool.cached_defs_for(&done_servers).is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn warm_recovered_missing_runs_dir_is_noop() {
        let pool = pool();
        pool.warm_recovered(std::path::Path::new("/no/such/runs"))
            .await;
    }

    /// The lease lifecycle end to end: runs hold a server open, the last
    /// release zeroes it, and the idle disconnect tears the connection down so
    /// the next spawn reconnects lazily.
    #[tokio::test]
    async fn leases_hold_a_server_and_the_last_release_disconnects_it() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd, bp) = blueprint_declaring("leaseserver", &stub);
            let pool = Arc::new(pool().with_idle_disconnect_secs(1));
            let servers = parse_blueprint_mcp_servers(&std::fs::read_to_string(&bp).unwrap());
            let cfg = &servers[0];
            // Connect for real, so there is a live client to tear down.
            assert_eq!(pool.ensure(cfg).await.len(), 1);

            pool.lease_blueprint(&bp, "run-a");
            pool.lease_blueprint(&bp, "run-b");
            // Leasing twice from the same run holds once.
            pool.lease_blueprint(&bp, "run-b");
            assert_eq!(pool.leased_holders(cfg), 2);

            // Releasing one run leaves the server held (nothing zeroed, no
            // timer scheduled).
            pool.release_run("run-a");
            assert_eq!(pool.leased_holders(cfg), 1);
            assert!(!pool.cached_defs_for(&servers).is_empty());

            // The last release zeroes it; drive the disconnect directly (the
            // scheduled timer runs the same call after the grace window).
            let zeroed = pool.release_run_bookkeeping("run-b");
            assert_eq!(zeroed.len(), 1);
            let (sig, name, generation) = zeroed[0].clone();
            assert_eq!(
                pool.disconnect_if_still_idle(&sig, &name, generation).await,
                IdleOutcome::Disconnected
            );
            // Defs are forgotten, so the next spawn reconnects lazily...
            assert!(pool.cached_defs_for(&servers).is_empty());
            // ...and a replayed disconnect finds nothing to do.
            assert_eq!(
                pool.disconnect_if_still_idle(&sig, &name, generation).await,
                IdleOutcome::Stale
            );
        })
        .await;
    }

    /// A lease taken during the grace window outdates the scheduled
    /// disconnect: the generation moved, so the timer's callback is a no-op.
    #[tokio::test]
    async fn a_lease_during_the_grace_window_cancels_the_disconnect() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd, bp) = blueprint_declaring("graceserver", &stub);
            let pool = Arc::new(pool().with_idle_disconnect_secs(1));
            let servers = parse_blueprint_mcp_servers(&std::fs::read_to_string(&bp).unwrap());
            let cfg = &servers[0];
            assert_eq!(pool.ensure(cfg).await.len(), 1);

            pool.lease_blueprint(&bp, "run-a");
            let zeroed = pool.release_run_bookkeeping("run-a");
            let (sig, name, generation) = zeroed[0].clone();
            // A new run leases before the timer would have fired.
            pool.lease_blueprint(&bp, "run-b");
            assert_eq!(
                pool.disconnect_if_still_idle(&sig, &name, generation).await,
                IdleOutcome::Stale,
                "a stale generation must not tear down a re-leased server"
            );
            assert_eq!(pool.leased_holders(cfg), 1);
            assert!(!pool.cached_defs_for(&servers).is_empty());
        })
        .await;
    }

    /// A call in flight when the idle tick fires keeps the server exactly as
    /// it was: still leased, still cached, still routable. The next tick,
    /// once the call has let go, tears it down. Before the fix the first tick
    /// dropped the lease row and the cached defs and only then found the
    /// executor would not give the client up, so the second tick found no
    /// row, returned early, and the server (and its child process) lived for
    /// the daemon's life while its tool stayed routable.
    #[tokio::test]
    async fn an_in_flight_call_postpones_the_idle_disconnect() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd, bp) = blueprint_declaring("busyserver", &stub);
            let pool = Arc::new(pool());
            let servers = parse_blueprint_mcp_servers(&std::fs::read_to_string(&bp).unwrap());
            assert_eq!(pool.ensure(&servers[0]).await.len(), 1);
            pool.lease_blueprint(&bp, "run-a");
            let zeroed = pool.release_run_bookkeeping("run-a");
            let (sig, name, generation) = zeroed[0].clone();

            // A call holds the client the way `ToolExecutor::execute` does:
            // a clone of the shared `Arc`, taken by `route` and kept for the
            // duration of the call.
            let held = pool.shared.lock().await.route("busyserver__echo");
            let (held, _) = held.expect("the tool routes while connected");
            assert_eq!(
                pool.disconnect_if_still_idle(&sig, &name, generation).await,
                IdleOutcome::Busy,
                "a server with a call in flight is kept"
            );
            assert!(
                !pool.cached_defs_for(&servers).is_empty(),
                "the cached defs survive a refused disconnect"
            );
            assert!(
                pool.shared.lock().await.has_client(&name),
                "the executor still has the server"
            );
            assert!(
                pool.leases
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .servers
                    .contains_key(&sig),
                "the lease row survives, so a later tick can find the server"
            );

            drop(held);
            assert_eq!(
                pool.disconnect_if_still_idle(&sig, &name, generation).await,
                IdleOutcome::Disconnected,
                "once the call lets go, the next tick disconnects"
            );
            assert!(pool.cached_defs_for(&servers).is_empty());
            assert!(!pool.shared.lock().await.has_client(&name));
            assert!(
                pool.shared.lock().await.route("busyserver__echo").is_err(),
                "a disconnected server's tool no longer routes"
            );
        })
        .await;
    }

    /// The grace timer itself: finding the server busy, it waits another
    /// window and asks again, and disconnects once the call has let go.
    #[tokio::test]
    async fn the_grace_timer_waits_again_for_a_busy_server() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd, bp) = blueprint_declaring("retryserver", &stub);
            let pool = Arc::new(pool());
            let servers = parse_blueprint_mcp_servers(&std::fs::read_to_string(&bp).unwrap());
            assert_eq!(pool.ensure(&servers[0]).await.len(), 1);
            pool.lease_blueprint(&bp, "run-a");
            let zeroed = pool.release_run_bookkeeping("run-a");
            let (sig, name, generation) = zeroed[0].clone();

            let held = pool.shared.lock().await.route("retryserver__echo");
            let (held, _) = held.expect("routes");
            let grace = std::time::Duration::from_millis(20);
            let timer = tokio::spawn(Arc::clone(&pool).grace_disconnect(
                grace,
                sig,
                name.clone(),
                generation,
            ));
            // Several windows pass with the call in flight: the server stays.
            tokio::time::sleep(grace * 6).await;
            assert!(!pool.cached_defs_for(&servers).is_empty());
            assert!(pool.shared.lock().await.has_client(&name));

            // The call ends; the next window tears the server down and the
            // timer task finishes.
            drop(held);
            timer
                .await
                .expect("the timer task ends once it disconnected");
            assert!(pool.cached_defs_for(&servers).is_empty());
            assert!(!pool.shared.lock().await.has_client(&name));
        })
        .await;
    }

    /// Outside a runtime (a sync harness driving the reap hook directly), a
    /// release is bookkeeping only: there is nowhere to spawn the grace
    /// timer, and that must be a quiet no-op rather than a panic.
    #[test]
    fn release_run_without_a_runtime_is_bookkeeping_only() {
        let pool = Arc::new(pool());
        pool.release_run("no-runtime-run");
    }

    /// The scheduled path end to end: a real release on a live runtime spawns
    /// the grace timer, and after the window the server is gone. With the
    /// grace set to zero, releasing schedules nothing and the connection
    /// stays.
    ///
    /// The window is milliseconds and the teardown is awaited by polling with
    /// a generous ceiling, not by sleeping past a fixed slack: the python
    /// subprocess is real, so a paused clock is out, and a slow runner used to
    /// turn a 1 s timer plus 2.5 s of slack into the suite's one flake.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_run_schedules_the_grace_disconnect() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd, bp) = blueprint_declaring("timedserver", &stub);
            let grace = std::time::Duration::from_millis(20);
            let timed = Arc::new(pool().with_idle_disconnect(grace));
            let servers = parse_blueprint_mcp_servers(&std::fs::read_to_string(&bp).unwrap());
            assert_eq!(timed.ensure(&servers[0]).await.len(), 1);
            timed.lease_blueprint(&bp, "run-a");
            timed.release_run("run-a");
            // Within the grace window the connection survives...
            assert!(!timed.cached_defs_for(&servers).is_empty());
            // ...and after it, the timer has torn it down.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !timed.cached_defs_for(&servers).is_empty() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the grace timer never tore the server down"
                );
                tokio::time::sleep(grace).await;
            }

            // Grace zero: releasing disconnects nothing, ever. There is no
            // timer to wait out; a few windows' worth is enough to catch one
            // spawned by mistake.
            let keeper = Arc::new(pool().with_idle_disconnect_secs(0));
            assert_eq!(keeper.ensure(&servers[0]).await.len(), 1);
            keeper.lease_blueprint(&bp, "run-b");
            keeper.release_run("run-b");
            tokio::time::sleep(grace * 5).await;
            assert!(!keeper.cached_defs_for(&servers).is_empty());
        })
        .await;
    }

    /// The defensive arms: a lease that never connected disconnects to a
    /// no-op (no client in the executor), and a run entry pointing at a
    /// server the table no longer holds is skipped rather than panicking.
    #[tokio::test]
    async fn disconnect_without_a_client_and_a_dangling_lease_are_noops() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd, bp) = blueprint_declaring("neverconnected", &stub);
            let pool = Arc::new(pool());
            // Leased but never `ensure`d: nothing in the executor to remove.
            pool.lease_blueprint(&bp, "run-a");
            let zeroed = pool.release_run_bookkeeping("run-a");
            let (sig, name, generation) = zeroed[0].clone();
            assert_eq!(
                pool.disconnect_if_still_idle(&sig, &name, generation).await,
                IdleOutcome::Stale,
                "no client to remove is a no-op, not an error"
            );

            // A runs-map entry whose server row is gone (cannot happen through
            // the public API, which mutates both under one lock) is skipped.
            pool.lease_blueprint(&bp, "run-b");
            pool.leases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .servers
                .clear();
            assert!(pool.release_run_bookkeeping("run-b").is_empty());
        })
        .await;
    }

    /// Global (seeded) servers belong to the daemon: they are never leased,
    /// and releasing runs never schedules them for disconnection. A missing
    /// manifest and an unknown run are no-ops.
    #[tokio::test]
    async fn seeded_servers_are_exempt_and_bad_inputs_are_noops() {
        with_tracing(|| {});
        with_temp_home(|| async {
            let (_sd, stub) = stub_py();
            let (_bd, bp) = blueprint_declaring("globalserver", &stub);
            let pool = Arc::new(pool());
            let servers = parse_blueprint_mcp_servers(&std::fs::read_to_string(&bp).unwrap());
            pool.seed(&servers[0], Vec::new());

            pool.lease_blueprint(&bp, "run-a");
            assert_eq!(pool.leased_holders(&servers[0]), 0, "global: no lease");
            pool.release_run("run-a"); // nothing held → nothing zeroed
            assert!(pool.release_run_bookkeeping("never-leased").is_empty());
            pool.lease_blueprint("/no/such/agent.leviath", "run-b");
            assert!(pool.release_run_bookkeeping("run-b").is_empty());
        })
        .await;
    }

    #[test]
    fn for_daemon_reserves_core_names_and_seeds_globals() {
        let global =
            MCPServerConfig::stdio("g", "python3", vec!["-c".to_string(), "pass".to_string()]);
        let pool = McpPool::for_daemon(
            Arc::new(Mutex::new(ToolExecutor::new())),
            std::slice::from_ref(&global),
        );
        // The global server is seeded (cached with empty defs → deduped on a
        // re-declaration).
        assert!(
            pool.cached_defs_for(std::slice::from_ref(&global))
                .is_empty()
        );
        // Built-in names are reserved.
        assert!(pool.reserved.contains("read_file"));
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
