//! Starting runs on the daemon.
//!
//! Every run the daemon starts goes through [`DaemonStarter`]: a person's
//! (the control socket's `spawn`, whichever front door sent it), a child run
//! an agent asked for, and a fan-out worker. The steps are always the same,
//! and all of them happen off the world:
//!
//! 1. bring the machine up to date for the run: the global MCP servers and
//!    the provider set as `config.toml` names them now, the MCP servers the
//!    run's own graph declares (and those of the blueprints its fan-outs
//!    start), and the models its stages name;
//! 2. [`resolve`] the request against a [`DaemonEnv`] built from that;
//! 3. write the run's file: its spec, its code, its attached files, and the
//!    state it starts in;
//! 4. [`bind`](leviath_runtime::bind::bind) the spec.
//!
//! The host then places the result with `insert`. [`spawn`] and [`validate`]
//! are the two calls every front door ends at.
//!
//! [`spawn`]: DaemonStarter::spawn
//! [`validate`]: DaemonStarter::validate

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leviath_mcp::{MCPServerConfig, MCPTransport};
use leviath_runtime::host::{PreparedRun, RunStarter, SubAgentOp};
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::resolve::{ResolveMode, Resolved, resolve};
use leviath_runtime::secret_store::SecretStore;
use leviath_runtime::spec::env::Caller;
use leviath_runtime::spec::graph::{McpServerDef, McpTransport, RunGraph, StageMode, WorkerSource};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use leviath_runtime::spec::summary::SpawnSummary;
use leviath_runtime::state::RunState;
use tokio::sync::mpsc::UnboundedSender;

use crate::daemon::resolve_env::DaemonEnv;
use crate::daemon::tool_service::CliToolService;

/// How long the models a run names get to say what they are before it starts
/// anyway, per provider. A ceiling, not a wait: a model already loaded answers
/// in a fraction of a second, and one that does not answer in time is sized
/// from the table compiled into this build.
const MODEL_WARM_TIMEOUT_SECS: u64 = 90;

/// Everything the daemon starts runs with.
pub(crate) struct DaemonStarter {
    /// `config.toml`, as it stands for each start.
    pub config: Arc<crate::daemon::config_reload::ConfigReloader>,
    /// The provider set, kept in step with the config.
    pub providers: Arc<crate::daemon::provider_reload::ProviderReload>,
    /// The taint policy files.
    pub policy: Arc<crate::daemon::policy_reload::PolicyReload>,
    /// The telemetry exporter the config names.
    pub telemetry: Arc<crate::daemon::telemetry_reload::TelemetryReload>,
    /// The `[limits]` the world runs on.
    pub limits: Arc<crate::daemon::live_limits::LiveLimits>,
    /// The global `[[mcp_servers]]`, kept in step with the config.
    pub mcp_global: Arc<crate::daemon::mcp_reload::McpReload>,
    /// The connections to every MCP server a run declares itself.
    pub mcp_pool: Arc<crate::daemon::mcp_pool::McpPool>,
    /// The MCP connections every run's tools go through.
    pub shared_mcp: Arc<tokio::sync::Mutex<leviath_mcp::ToolExecutor>>,
    /// The tool dispatcher a bound run is registered with.
    pub tool_service: Arc<CliToolService>,
    /// Where a run's prompts are parked.
    pub hub: InteractionHub,
    /// Where a run's own sub-agent tools send.
    pub subagent_tx: UnboundedSender<SubAgentOp>,
    /// Where runs are kept, one directory each.
    pub runs_dir: PathBuf,
    /// Where installed blueprints live.
    pub agents_dir: Option<PathBuf>,
    /// The store a run's files go to, shared with the world.
    pub blob_store: Arc<dyn leviath_core::mime::BlobStore>,
}

/// An issue at the top of a request, for what goes wrong around resolving it.
fn refusal(code: IssueCode, message: impl Into<String>) -> SpawnIssues {
    SpawnIssue::new(SpecPath::root(), code, message).into()
}

/// The MCP servers a graph declares, as the pool connects them.
pub(crate) fn mcp_configs(graph: &RunGraph) -> Vec<MCPServerConfig> {
    graph
        .mcp_servers
        .iter()
        .cloned()
        .map(server_config)
        .collect()
}

/// One server a graph declares, as the pool's `[[mcp_servers]]` entry for it.
///
/// Every field is named on both sides, so a field added to either struct does
/// not compile until it has somewhere to go.
fn server_config(def: McpServerDef) -> MCPServerConfig {
    let McpServerDef {
        name,
        transport,
        command,
        url,
        args,
        env,
        headers,
    } = def;
    MCPServerConfig {
        name: name.into(),
        transport: transport.map(|transport| match transport {
            McpTransport::Stdio => MCPTransport::Stdio,
            McpTransport::Http => MCPTransport::Http,
        }),
        command,
        url,
        args,
        env: env.into_iter().collect(),
        headers: headers.into_iter().collect(),
    }
}

/// An error as the text a refusal carries.
fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Every model a graph's stages name, bare and without repeats.
fn model_names(graph: &RunGraph) -> Vec<String> {
    let mut names: Vec<String> = graph
        .stages
        .iter()
        .flat_map(|s| s.model.models.iter().map(|m| m.model.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

impl DaemonStarter {
    /// The graph a request runs, for warming what it needs: the blueprint's,
    /// or the caller's own. `None` when the blueprint cannot be loaded, which
    /// resolving reports properly a moment later.
    pub(crate) fn graph_of(&self, request: &SpawnRequest) -> Option<RunGraph> {
        match &request.source {
            SpawnSource::Blueprint(reference) => {
                crate::daemon::resolve_env::load_installed(self.agents_dir.as_deref(), reference)
                    .ok()
                    .map(|loaded| loaded.graph)
            }
            SpawnSource::BlueprintFile(path) => crate::daemon::resolve_env::load_file(path)
                .ok()
                .map(|loaded| loaded.graph),
            SpawnSource::Raw(graph) => Some((**graph).clone()),
        }
    }

    /// The MCP servers declared by the blueprints `graph`'s fan-outs start,
    /// so the first worker of each already has their tools.
    fn worker_servers(&self, graph: &RunGraph) -> Vec<MCPServerConfig> {
        graph
            .stages
            .iter()
            .filter_map(|s| match &s.mode {
                StageMode::FanOut(fan) => Some(&fan.worker),
                _ => None,
            })
            .filter_map(|worker| {
                let installed = |reference: &_| {
                    crate::daemon::resolve_env::load_installed(
                        self.agents_dir.as_deref(),
                        reference,
                    )
                    .ok()
                };
                match worker {
                    WorkerSource::Blueprint(reference) => installed(reference),
                    WorkerSource::BlueprintFile(path) => {
                        crate::daemon::resolve_env::load_file(path).ok()
                    }
                    WorkerSource::Query(query) => crate::daemon::fanout_spawner::find_installed(
                        self.agents_dir.as_deref(),
                        query,
                    )
                    .ok()
                    .and_then(|reference| installed(&reference)),
                    WorkerSource::Stage(_) => None,
                }
            })
            .flat_map(|loaded| mcp_configs(&loaded.graph))
            .collect()
    }

    /// Bring this machine up to date for `request`: the global MCP set and
    /// the providers as the config names them now, then the run's own MCP
    /// servers and its fan-out workers', then its models.
    async fn warm(&self, request: &SpawnRequest, config: &crate::config::Config) {
        self.mcp_global.refresh(config).await;
        self.providers.refresh_and_prime(config).await;
        // A gateway list that could not be read when the daemon started is
        // asked for again, so this spawn resolves against it if it is back.
        self.providers.prime_unread(config).await;
        self.providers.refresh_retention(config).await;
        let Some(graph) = self.graph_of(request) else {
            return;
        };
        for server in mcp_configs(&graph)
            .into_iter()
            .chain(self.worker_servers(&graph))
        {
            self.mcp_pool.ensure(&server).await;
        }
        self.providers
            .registry()
            .warm_models(
                &model_names(&graph),
                std::time::Duration::from_secs(MODEL_WARM_TIMEOUT_SECS),
                Some(config.default_provider.as_str()),
            )
            .await;
    }

    /// What this machine answers for `request`, as it stands now: the
    /// global MCP tools and those of every server the request's graph
    /// declares that the pool has connected.
    pub(crate) fn env_for(
        &self,
        request: &SpawnRequest,
        config: Arc<crate::config::Config>,
    ) -> DaemonEnv {
        self.env_with(self.graph_of(request).as_ref(), config)
    }

    /// What this machine answers for a run of `graph`, to bind it.
    pub(crate) fn env_for_graph(
        &self,
        graph: &RunGraph,
        config: Arc<crate::config::Config>,
    ) -> DaemonEnv {
        self.env_with(Some(graph), config)
    }

    fn env_with(&self, graph: Option<&RunGraph>, config: Arc<crate::config::Config>) -> DaemonEnv {
        let (mut mcp_defs, mut mcp_owners) = self.mcp_global.current();
        if let Some(graph) = graph {
            let servers = mcp_configs(graph);
            mcp_defs.extend(self.mcp_pool.cached_defs_for(&servers));
            mcp_owners.extend(self.mcp_pool.cached_owners_for(&servers));
        }
        self.policy.refresh();
        DaemonEnv {
            mime: Arc::new(config.mime_registry_or_defaults()),
            config,
            registry: self.providers.registry(),
            agents_dir: self.agents_dir.clone(),
            workdir_root: None,
            mcp_defs,
            mcp_owners,
            shared_mcp: self.shared_mcp.clone(),
            tool_service: self.tool_service.clone(),
            hub: self.hub.clone(),
            subagent_tx: self.subagent_tx.clone(),
            blob_store: self.blob_store.clone(),
            mcp_overrides: self.policy.current().mcp_overrides,
            secrets: Some(self.secrets()),
        }
    }

    /// The secret store this daemon keeps its runs' secrets in.
    pub(crate) fn secrets(&self) -> SecretStore {
        SecretStore::of_runs(&self.runs_dir)
    }

    /// Put a resolved run's attached files in its blob directory and its
    /// secrets in the secret store, then write its file, holding its spec,
    /// its code and the state it starts in. The file names the attached
    /// files and the secrets and never holds either, so one that cannot be
    /// stored refuses the run.
    fn record(&self, resolved: &Resolved, state: &RunState) -> Result<(), SpawnIssues> {
        let spec = &resolved.spec;
        let dir = self.runs_dir.join(spec.run_id.as_str());
        let path = dir.join(leviath_core::files::RUN_FILE);
        let secrets = self.secrets();
        leviath_sys::perms::create_private_dir_all(&dir)
            .and_then(|()| {
                store_blobs(
                    self.blob_store.as_ref(),
                    spec.run_id.as_str(),
                    &resolved.blobs,
                )
            })
            .and_then(|()| secrets.keep_all(&resolved.secrets))
            .map_err(text)
            .and_then(|()| {
                leviath_runtime::runfile::RunFileWriter::create(
                    &path,
                    spec,
                    &resolved.code,
                    state,
                    Default::default(),
                )
                .map(drop)
                .map_err(text)
            })
            .map_err(|e| {
                // A run that is refused here never runs, so nothing will
                // ever sign with its secrets.
                secrets.forget_run(spec.run_id.as_str());
                refusal(
                    IssueCode::Unavailable,
                    format!("the run's file could not be written: {e}"),
                )
            })
    }

    /// Record on a run's file that it could not be bound, and why, so the
    /// run ends there rather than looking like one that never moved.
    fn record_refusal(&self, resolved: &Resolved, state: &RunState, issues: &SpawnIssues) {
        let path = self
            .runs_dir
            .join(resolved.spec.run_id.as_str())
            .join(leviath_core::files::RUN_FILE);
        if let Err(e) = record_failed(&path, state, issues) {
            tracing::warn!(run_id = %resolved.spec.run_id, error = %e, "could not record why a run did not start");
        }
    }

    /// Resolve, record and bind `request` for `caller` against `env`, as the
    /// machine stands: nothing is warmed first. What a fan-out worker gets,
    /// from inside the world.
    pub(crate) async fn start_with(
        &self,
        env: DaemonEnv,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<PreparedRun, SpawnIssues> {
        let resolved = resolve(&request, &caller, &env, ResolveMode::Spawn).await?;
        let state = leviath_runtime::insert::initial_state(&resolved.spec);
        self.record(&resolved, &state)?;
        let bindings = match leviath_runtime::bind::bind(&resolved.spec, &resolved.code, &env).await
        {
            Ok(bindings) => bindings,
            Err(issues) => {
                self.record_refusal(&resolved, &state, &issues);
                return Err(issues);
            }
        };
        let servers = mcp_configs(&resolved.spec.graph);
        let lease = self
            .mcp_pool
            .lease_servers(&servers, resolved.spec.run_id.as_str());
        Ok(PreparedRun {
            spec: Arc::new(resolved.spec),
            // The servers it holds go on the run, for the reap to hand back.
            bindings: bindings.with(lease),
            state,
        })
    }

    /// Start a run: warm what it needs, then resolve, record and bind it.
    pub(crate) async fn spawn(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<PreparedRun, SpawnIssues> {
        let config = self.config.current();
        self.warm(&request, &config).await;
        let env = self.env_for(&request, config);
        self.start_with(env, request, caller).await
    }

    /// Resolve a run without starting it.
    pub(crate) async fn validate(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<SpawnSummary, SpawnIssues> {
        let config = self.config.current();
        self.warm(&request, &config).await;
        let env = self.env_for(&request, config);
        let resolved = resolve(&request, &caller, &env, ResolveMode::Check).await?;
        Ok(SpawnSummary::of(&resolved.spec))
    }

    /// Bring the settings that live on the world up to date with the config
    /// before a run is placed: the provider set its stages resolved against,
    /// the taint policy, the telemetry exporter and the limits.
    pub(crate) fn refresh_world(&self, world: &mut leviath_runtime::PipelineWorld) {
        let config = self.config.current();
        self.providers.install(world);
        self.policy.refresh_into(world);
        self.telemetry.refresh_into(world, &config.observability);
        self.limits.apply(&config, world);
    }
}

#[async_trait::async_trait]
impl RunStarter for DaemonStarter {
    async fn start(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<PreparedRun, SpawnIssues> {
        self.spawn(request, caller).await
    }

    async fn check(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<SpawnSummary, SpawnIssues> {
        self.validate(request, caller).await
    }

    fn before_insert(
        &self,
        world: &mut leviath_runtime::PipelineWorld,
        _spec: &leviath_runtime::spec::run_spec::RunSpec,
    ) {
        self.refresh_world(world);
    }
}

/// Put a run's attached files in `store`, by digest, where its tools and
/// every reader of the run find them. The bytes are stored as they are: they
/// were checked against their types when the run was resolved.
pub(crate) fn store_blobs(
    store: &dyn leviath_core::mime::BlobStore,
    run_id: &str,
    blobs: &std::collections::BTreeMap<leviath_runtime::spec::names::Digest, Vec<u8>>,
) -> std::io::Result<()> {
    let registry = leviath_core::mime::MimeRegistry::builtin();
    blobs.values().try_for_each(|bytes| {
        let blob = leviath_core::mime::Blob::new(leviath_core::mime::octet_stream(), bytes.clone());
        store.put(run_id, &blob, &registry).map(drop)
    })
}

/// Append to the run file at `path` that the run failed with `issues`.
pub(crate) fn record_failed(
    path: &Path,
    state: &RunState,
    issues: &SpawnIssues,
) -> Result<(), leviath_runtime::runfile::RunFileError> {
    let mut writer = leviath_runtime::runfile::RunFileWriter::open(path, Default::default())?;
    let mut failed = state.clone();
    failed.status = leviath_runtime::state::RunStatus::Error(issues.to_string());
    failed.phase = leviath_runtime::state::PipelinePhase::Done;
    failed.settle_ledger();
    let events = vec![leviath_runtime::state::RunEvent::Log(issues.to_string())];
    writer
        .record(failed, chrono::Utc::now().timestamp(), events)
        .map(drop)
}

#[cfg(test)]
#[path = "starter_tests.rs"]
mod tests;

#[cfg(test)]
mod steps_tests;

#[cfg(test)]
pub(crate) mod testing;
