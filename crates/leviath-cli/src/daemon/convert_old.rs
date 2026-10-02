//! Run directories in the older many-file layout become run files the first
//! time the daemon looks at them.
//!
//! An old run named its models and its tools but kept neither a model's
//! window nor a tool's definition. Each stage of a converted run has both
//! looked up here, against the same [`DaemonEnv`] a new run of its graph is
//! resolved against, so a converted run carries on with the model it was
//! launched with and the tools it had. The daemon converts at start, before
//! any run is brought back, and again whenever an unloaded run is paged in.
//! At start the MCP servers an unfinished old run's stages connect to are
//! connected first, so their tools are there to look up.

use std::path::Path;

use leviath_runtime::spec::graph::RunGraph;

use crate::daemon::resolve_env::DaemonEnv;

/// What the daemon resolves a run of a graph against.
pub(crate) type Envs<'a> = dyn Fn(&RunGraph) -> DaemonEnv + Sync + 'a;

/// The daemon's answers about a stage, for the converter.
#[cfg(feature = "legacy-runs")]
struct Lookup<'a>(&'a Envs<'a>);

#[cfg(feature = "legacy-runs")]
impl leviath_legacy_runs::StageLookup for Lookup<'_> {
    fn model(
        &self,
        graph: &RunGraph,
        stage: &leviath_runtime::spec::graph::StageDef,
        requested: Option<&leviath_runtime::spec::names::ModelRef>,
    ) -> Result<leviath_runtime::spec::env::ModelPlan, String> {
        use leviath_runtime::spec::env::ResolveEnv;
        let env = (self.0)(graph);
        crate::daemon::block_on::block_on(env.model(stage, requested)).map_err(|i| i.to_string())
    }

    fn tools(
        &self,
        graph: &RunGraph,
        stage: &leviath_runtime::spec::graph::StageDef,
        code: &leviath_runtime::spec::env::CodeFiles,
        base: Option<&Path>,
        workdir: Option<&Path>,
    ) -> Result<leviath_runtime::spec::env::StageTools, String> {
        use leviath_runtime::spec::env::ResolveEnv;
        let env = (self.0)(graph);
        crate::daemon::block_on::block_on(env.tools(graph, stage, code, base, workdir))
            .map_err(|i| i.to_string())
    }

    fn default_max_depth(&self, graph: &RunGraph) -> u8 {
        use leviath_runtime::spec::env::ResolveEnv;
        (self.0)(graph).limits().default_max_depth
    }
}

/// Convert every run directory under `runs_dir` that is still in the older
/// layout, looking each stage up through `envs` when given.
pub(crate) fn convert_all(runs_dir: &Path, agents_dir: Option<&Path>, envs: Option<&Envs<'_>>) {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return;
    };
    for dir in entries.flatten().map(|e| e.path()) {
        convert_one(&dir, agents_dir, envs);
    }
}

/// Convert the run in `dir` into a run file when it is in the older layout,
/// logging what the conversion had to fill in. A directory that cannot be
/// converted is left as it is and said so.
#[cfg(feature = "legacy-runs")]
pub(crate) fn convert_one(dir: &Path, agents_dir: Option<&Path>, envs: Option<&Envs<'_>>) {
    if !leviath_legacy_runs::is_legacy(dir) {
        return;
    }
    let lookup = envs.map(Lookup);
    let env = leviath_legacy_runs::ConvertEnv {
        agents_dir: agents_dir.map(Path::to_path_buf),
        stages: lookup
            .as_ref()
            .map(|l| l as &dyn leviath_legacy_runs::StageLookup),
    };
    match leviath_legacy_runs::convert(dir, &env) {
        Ok(report) => {
            tracing::info!(
                run_id = %report.run_id,
                deltas = report.deltas,
                "converted an old run directory into a run file"
            );
            for filled in &report.defaulted {
                tracing::info!(run_id = %report.run_id, "converted from the old layout: {filled}");
            }
            for note in &report.notes {
                tracing::info!(run_id = %report.run_id, "converted from the old layout: {note}");
            }
        }
        Err(e) => {
            let shown = dir.display();
            tracing::warn!(dir = %shown, error = %e, "an old run directory could not be converted");
        }
    }
}

/// Without the converter, a run in the older layout is only reported.
#[cfg(not(feature = "legacy-runs"))]
pub(crate) fn convert_one(dir: &Path, _agents_dir: Option<&Path>, _envs: Option<&Envs<'_>>) {
    let old = dir.join(leviath_core::files::META_FILE).is_file()
        && !dir.join(leviath_core::files::RUN_FILE).is_file();
    if old {
        let shown = dir.display();
        tracing::warn!(dir = %shown, "an old run directory, and this build cannot convert it");
    }
}

/// What the daemon has at start for converting old runs, before its host
/// exists.
pub(crate) struct AtStart<'a> {
    pub(crate) config: &'a crate::config::Config,
    pub(crate) registry: leviath_runtime::ProviderRegistry,
    pub(crate) agents_dir: Option<&'a Path>,
    /// The tools the connected global MCP servers advertise, and which
    /// server advertises each.
    pub(crate) mcp_defs: &'a [leviath_providers::Tool],
    pub(crate) mcp_owners: &'a leviath_runtime::pipeline::ToolOwners,
    pub(crate) shared_mcp: std::sync::Arc<tokio::sync::Mutex<leviath_mcp::ToolExecutor>>,
    pub(crate) pool: &'a crate::daemon::mcp_pool::McpPool,
}

impl AtStart<'_> {
    /// What a run of `graph` is resolved against: this machine's providers,
    /// its built-in and script tools, the global MCP servers' tools and the
    /// tools of the graph's own servers that are connected.
    fn env(&self, graph: &RunGraph) -> DaemonEnv {
        let servers = crate::daemon::starter::mcp_configs(graph);
        let mut mcp_defs = self.mcp_defs.to_vec();
        mcp_defs.extend(self.pool.cached_defs_for(&servers));
        let mut mcp_owners = self.mcp_owners.clone();
        mcp_owners.extend(self.pool.cached_owners_for(&servers));
        DaemonEnv {
            mime: std::sync::Arc::new(self.config.mime_registry_or_defaults()),
            config: std::sync::Arc::new(self.config.clone()),
            registry: self.registry.clone(),
            agents_dir: self.agents_dir.map(Path::to_path_buf),
            workdir_root: None,
            mcp_defs,
            mcp_owners,
            shared_mcp: self.shared_mcp.clone(),
            // Looking a stage up never binds a run, so nothing is registered
            // with these.
            tool_service: std::sync::Arc::new(crate::daemon::tool_service::CliToolService::new()),
            hub: leviath_runtime::interaction_hub::InteractionHub::new(),
            subagent_tx: tokio::sync::mpsc::unbounded_channel().0,
            blob_store: std::sync::Arc::new(leviath_core::mime::MemoryBlobStore::new()),
            mcp_overrides: Default::default(),
        }
    }
}

/// Convert every old run under `runs_dir` at daemon start: first connect the
/// MCP servers each unfinished one's stages use, then convert them all,
/// looking each stage up on this machine.
pub(crate) async fn convert_at_start(runs_dir: &Path, start: AtStart<'_>) {
    for server in servers_of_unfinished(runs_dir, start.agents_dir) {
        start.pool.ensure(&server).await;
    }
    let envs = |graph: &RunGraph| start.env(graph);
    convert_all(runs_dir, start.agents_dir, Some(&envs));
}

/// The MCP servers the stages of every unfinished old run under `runs_dir`
/// connect to.
#[cfg(feature = "legacy-runs")]
fn servers_of_unfinished(
    runs_dir: &Path,
    agents_dir: Option<&Path>,
) -> Vec<leviath_mcp::MCPServerConfig> {
    let env = leviath_legacy_runs::ConvertEnv {
        agents_dir: agents_dir.map(Path::to_path_buf),
        stages: None,
    };
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let unfinished = |dir: &Path| {
        std::fs::read_to_string(dir.join(leviath_core::files::META_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<leviath_core::run_meta::RunMeta>(&text).ok())
            .is_some_and(|meta| !crate::runstate::is_terminal_status(&meta.status))
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|dir| leviath_legacy_runs::is_legacy(dir) && unfinished(dir))
        .filter_map(|dir| leviath_legacy_runs::graph(&dir, &env).ok())
        .flat_map(|graph| crate::daemon::starter::mcp_configs(&graph))
        .collect()
}

/// Without the converter there are no old runs to connect servers for.
#[cfg(not(feature = "legacy-runs"))]
fn servers_of_unfinished(
    _runs_dir: &Path,
    _agents_dir: Option<&Path>,
) -> Vec<leviath_mcp::MCPServerConfig> {
    Vec::new()
}

#[cfg(test)]
#[path = "convert_old_tests.rs"]
mod tests;
