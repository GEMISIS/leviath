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
//!
//! Each old run directory is saved in the home's backup (see
//! [`crate::home_backup`]) before it is converted, and one that cannot be
//! saved is left as it is. A run that does not convert is left as it is too,
//! and listed beside the runs directory, so the daemon tries it once per
//! release rather than at every start. A pass logs one line saying what it
//! did; what each conversion filled in is in the run's own log.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use leviath_runtime::spec::graph::RunGraph;
use serde::{Deserialize, Serialize};

use crate::daemon::resolve_env::DaemonEnv;
use crate::home_backup::Backup;

/// What the daemon resolves a run of a graph against.
pub(crate) type Envs<'a> = dyn Fn(&RunGraph) -> DaemonEnv + Sync + 'a;

/// The daemon's answers about a stage, for the converter.
///
/// What a lookup would log (a model no provider here serves, a script
/// provider that does not load) is left out of the daemon's log: a lookup
/// that fails is in the converted run's own log, with what it fell back to,
/// and a thousand old runs would otherwise log it a thousand times.
#[cfg(feature = "legacy-runs")]
struct Lookup<'a>(&'a Envs<'a>);

/// Run `f` with nothing it logs reaching the daemon's log.
#[cfg(feature = "legacy-runs")]
fn quietly<T>(f: impl FnOnce() -> T) -> T {
    tracing::dispatcher::with_default(&tracing::Dispatch::none(), f)
}

#[cfg(feature = "legacy-runs")]
impl leviath_legacy_runs::StageLookup for Lookup<'_> {
    fn model(
        &self,
        graph: &RunGraph,
        stage: &leviath_runtime::spec::graph::StageDef,
        requested: Option<&leviath_runtime::spec::names::ModelRef>,
    ) -> Result<leviath_runtime::spec::env::ModelPlan, String> {
        use leviath_runtime::spec::env::ResolveEnv;
        quietly(|| {
            let env = (self.0)(graph);
            crate::daemon::block_on::block_on(env.model(stage, requested))
                .map_err(|i| i.to_string())
        })
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
        quietly(|| {
            let env = (self.0)(graph);
            crate::daemon::block_on::block_on(env.tools(graph, stage, code, base, workdir))
                .map_err(|i| i.to_string())
        })
    }

    fn default_max_depth(&self, graph: &RunGraph) -> u8 {
        use leviath_runtime::spec::env::ResolveEnv;
        quietly(|| (self.0)(graph).limits().default_max_depth)
    }
}

/// The old runs that did not convert, kept beside the runs directory so that
/// a later start leaves them as they are rather than trying each again. The
/// list is kept per release: a new release tries every one again, and so
/// does a start after the file is deleted.
#[derive(Debug)]
pub(crate) struct Unconverted {
    path: PathBuf,
    runs: BTreeMap<String, String>,
    changed: bool,
}

/// What the list of runs that did not convert holds.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UnconvertedFile {
    /// The release that tried them.
    version: String,
    /// Why each did not convert, by run directory name.
    runs: BTreeMap<String, String>,
}

impl Unconverted {
    /// Where the list for `runs_dir` is kept: beside it, named after it.
    pub(crate) fn path_for(runs_dir: &Path) -> PathBuf {
        let name = runs_dir
            .file_name()
            .map_or_else(|| "runs".into(), |n| n.to_string_lossy().into_owned());
        runs_dir.with_file_name(format!("{name}.unconverted"))
    }

    /// The list for `runs_dir`. One another release wrote is read as empty,
    /// so every run in it is tried again.
    fn load(runs_dir: &Path) -> Self {
        let path = Self::path_for(runs_dir);
        let file = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<UnconvertedFile>(&bytes).ok());
        let changed = file.is_some();
        let runs = file
            .filter(|f| f.version == env!("CARGO_PKG_VERSION"))
            .map(|f| f.runs)
            .unwrap_or_default();
        Self {
            path,
            changed: changed && runs.is_empty(),
            runs,
        }
    }

    /// Why the run in the directory `name` did not convert, when it did not.
    fn why(&self, name: &str) -> Option<&str> {
        self.runs.get(name).map(String::as_str)
    }

    fn add(&mut self, name: String, why: String) {
        self.runs.insert(name, why);
        self.changed = true;
    }

    /// Write the list back when it changed, or remove it when it is empty.
    fn save(self) {
        if !self.changed {
            return;
        }
        let written = match self.runs.is_empty() {
            true => std::fs::remove_file(&self.path),
            false => {
                let file = UnconvertedFile {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    runs: self.runs,
                };
                let bytes =
                    serde_json::to_vec_pretty(&file).expect("a list of names is plain data");
                leviath_sys::perms::write_private(&self.path, &bytes)
            }
        };
        if let Err(e) = written {
            let (shown, why) = (self.path.display().to_string(), e.to_string());
            tracing::warn!(path = %shown, error = %why, "the list of old runs that did not convert could not be written");
        }
    }
}

/// What converting one directory came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Done {
    /// Not an old run directory.
    Nothing,
    /// Converted into a run file.
    Converted,
    /// Tried, and not converted.
    Failed,
    /// Left as it was: it did not convert before.
    Held,
}

/// What one pass over the old runs did, for its one line in the log.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pass {
    converted: usize,
    failed: usize,
    held: usize,
}

impl Pass {
    fn add(&mut self, done: Done) {
        match done {
            Done::Nothing => {}
            Done::Converted => self.converted += 1,
            Done::Failed => self.failed += 1,
            Done::Held => self.held += 1,
        }
    }

    /// Say in one line what the pass did, when it did anything.
    fn log(&self, runs_dir: &Path, backup: &Backup) {
        if *self == Self::default() {
            return;
        }
        let list = Unconverted::path_for(runs_dir).display().to_string();
        let saved = backup.dir().display().to_string();
        tracing::info!(
            converted = self.converted,
            failed = self.failed,
            left_as_they_were = self.held,
            backup = %saved,
            unconverted = %list,
            "old run directories: each converted one was saved in the backup first; one that does \
             not convert is left as it was and listed, and is tried again by the next release (or \
             after the list is deleted)"
        );
    }
}

/// Convert every run directory under `runs_dir` that is still in the older
/// layout, looking each stage up through `envs` when given. Each is saved in
/// the home's backup first, and one that did not convert before is left as
/// it is.
pub(crate) fn convert_all(runs_dir: &Path, agents_dir: Option<&Path>, envs: Option<&Envs<'_>>) {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return;
    };
    let mut unconverted = Unconverted::load(runs_dir);
    let backup = Backup::of_runs(runs_dir);
    let mut pass = Pass::default();
    for dir in entries.flatten().map(|e| e.path()) {
        pass.add(convert_in(
            &dir,
            agents_dir,
            envs,
            &backup,
            &mut unconverted,
        ));
    }
    unconverted.save();
    pass.log(runs_dir, &backup);
}

/// Convert the run in `dir` into a run file when it is in the older layout,
/// the way [`convert_all`] does for each run.
pub(crate) fn convert_one(dir: &Path, agents_dir: Option<&Path>, envs: Option<&Envs<'_>>) {
    let runs_dir = dir.parent().unwrap_or(dir);
    let mut unconverted = Unconverted::load(runs_dir);
    let backup = Backup::of_runs(runs_dir);
    let mut pass = Pass::default();
    pass.add(convert_in(dir, agents_dir, envs, &backup, &mut unconverted));
    unconverted.save();
    pass.log(runs_dir, &backup);
}

/// Convert the run in `dir` when it is an old run that has not failed to
/// convert before: saved in `backup` first (one that cannot be saved is not
/// converted, and is tried again at the next start), and listed in
/// `unconverted` when it does not convert. What each conversion filled in is
/// in the run's own log, and at debug level in the daemon's.
#[cfg(feature = "legacy-runs")]
fn convert_in(
    dir: &Path,
    agents_dir: Option<&Path>,
    envs: Option<&Envs<'_>>,
    backup: &Backup,
    unconverted: &mut Unconverted,
) -> Done {
    if !leviath_legacy_runs::is_legacy(dir) {
        return Done::Nothing;
    }
    let name = dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if unconverted.why(&name).is_some() {
        return Done::Held;
    }
    let shown = dir.display().to_string();
    if let Err(e) = backup.save_run(dir) {
        let why = e.to_string();
        tracing::warn!(dir = %shown, error = %why, "an old run directory could not be backed up, so it was not converted");
        return Done::Failed;
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
            let id = report.run_id.to_string();
            for line in report
                .defaulted
                .iter()
                .map(ToString::to_string)
                .chain(report.notes)
            {
                tracing::debug!(run_id = %id, "converted from the old layout: {line}");
            }
            Done::Converted
        }
        Err(e) => {
            let why = e.to_string();
            tracing::warn!(dir = %shown, error = %why, "an old run directory could not be converted; it is left as it was");
            unconverted.add(name, why);
            Done::Failed
        }
    }
}

/// Without the converter, a run directory with no run file this build reads
/// is only reported.
#[cfg(not(feature = "legacy-runs"))]
fn convert_in(
    dir: &Path,
    _agents_dir: Option<&Path>,
    _envs: Option<&Envs<'_>>,
    _backup: &Backup,
    _unconverted: &mut Unconverted,
) -> Done {
    if dir.is_dir() && !crate::runstate::run_file::is_run_file(dir) {
        let shown = dir.display();
        tracing::warn!(dir = %shown, "a run directory with no run file this build can read");
    }
    Done::Nothing
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
        leviath_legacy_runs::meta(dir)
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
