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
//! connected first, so their tools are there to look up, and the runs are
//! converted in a child process that asks the daemon about each stage (see
//! [`crate::daemon::convert_child`]), so the memory converting takes is not
//! the daemon's to keep.
//!
//! Each old run directory is saved in the home's backup (see
//! [`crate::home_backup`]) before it is converted, and one that cannot be
//! saved is left as it is. A run that does not convert is left as it is too,
//! and listed beside the runs directory, so the daemon tries it once per
//! release rather than at every start. A pass logs one line saying what it
//! did, and adds it to the summary of the upgrade (see
//! [`crate::daemon::upgrade`]), where each key a run's blueprint dropped is a
//! warning; what each conversion filled in is in the run's own log. At start
//! a pass says how far along it is on the daemon's start-up board.
//!
//! The same pass upgrades each run file an alpha build wrote in binary
//! layout 2, which this build cannot read, to this build's layout in place
//! (see [`leviath_legacy_runs::upgrade`]). It is taken in the same order and
//! the same way: saved in the backup first, listed when it does not upgrade,
//! and counted in the summary. The file as it was stays in the run's
//! `legacy/` directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use leviath_runtime::spec::graph::RunGraph;
use serde::{Deserialize, Serialize};

use leviath_runtime::control_socket::StartupBoard;

use crate::daemon::resolve_env::DaemonEnv;
use crate::daemon::upgrade::Upgrade;
use crate::home_backup::Backup;

/// What the daemon resolves a run of a graph against.
pub(crate) type Envs<'a> = dyn Fn(&RunGraph) -> DaemonEnv + Sync + 'a;

/// What looks each stage of a resumable old run up as it converts: the
/// daemon's own answers, in the daemon or asked of it from a converting
/// child.
#[cfg(feature = "legacy-runs")]
pub(crate) type Stages<'a> = dyn leviath_legacy_runs::StageLookup + 'a;

/// Without the converter no stage is looked up.
#[cfg(not(feature = "legacy-runs"))]
pub(crate) type Stages<'a> = dyn Sync + 'a;

/// Where a pass over the old runs says how far along it is: the daemon's
/// start-up board, or the output of a child converting for the daemon.
pub(crate) trait Progress {
    /// A step of `total` items begins (`0` when it cannot be counted).
    fn begin(&self, step: &str, total: u64);
    /// A line under the step.
    fn detail(&self, detail: String);
    /// `done` items of the step are done, and the pass has done `so_far`.
    fn done(&self, done: u64, so_far: &Upgrade);
}

impl Progress for StartupBoard {
    fn begin(&self, step: &str, total: u64) {
        StartupBoard::begin(self, step, total);
    }

    fn detail(&self, detail: String) {
        StartupBoard::detail(self, detail);
    }

    fn done(&self, done: u64, _so_far: &Upgrade) {
        StartupBoard::done(self, done);
    }
}

/// The daemon's answers about a stage, for the converter.
///
/// What a lookup would log (a model no provider here serves, a script
/// provider that does not load) is left out of the daemon's log: a lookup
/// that fails is in the converted run's own log, with what it fell back to,
/// and a thousand old runs would otherwise log it a thousand times.
#[cfg(feature = "legacy-runs")]
pub(crate) struct Lookup<'a>(pub(crate) &'a Envs<'a>);

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
    #[cfg(feature = "legacy-runs")]
    fn why(&self, name: &str) -> Option<&str> {
        self.runs.get(name).map(String::as_str)
    }

    #[cfg(feature = "legacy-runs")]
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
    #[cfg(feature = "legacy-runs")]
    Converted,
    /// A layout-2 run file, upgraded in place.
    #[cfg(feature = "legacy-runs")]
    Upgraded,
    /// Tried, and not converted.
    #[cfg(feature = "legacy-runs")]
    Failed,
    /// Left as it was: it did not convert before.
    #[cfg(feature = "legacy-runs")]
    Held,
}

/// What one pass over the old runs did, for its one line in the log.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pass {
    converted: usize,
    upgraded: usize,
    failed: usize,
    held: usize,
}

impl Pass {
    fn add(&mut self, done: Done) {
        match done {
            Done::Nothing => {}
            #[cfg(feature = "legacy-runs")]
            Done::Converted => self.converted += 1,
            #[cfg(feature = "legacy-runs")]
            Done::Upgraded => self.upgraded += 1,
            #[cfg(feature = "legacy-runs")]
            Done::Failed => self.failed += 1,
            #[cfg(feature = "legacy-runs")]
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
            upgraded = self.upgraded,
            failed = self.failed,
            left_as_they_were = self.held,
            backup = %saved,
            unconverted = %list,
            "old run directories and layout-2 run files: each converted or upgraded one was saved \
             in the backup first; one that does not convert is left as it was and listed, and is \
             tried again by the next release (or after the list is deleted)"
        );
    }
}

/// Convert every run directory under `runs_dir` that is still in the older
/// layout, looking each stage up through `envs` when given, and say how far
/// along it is on `board`. Each is saved in the home's backup first, and one
/// that did not convert before is left as it is. Returns what it did.
#[cfg(feature = "legacy-runs")]
pub(crate) fn convert_all(
    runs_dir: &Path,
    agents_dir: Option<&Path>,
    envs: Option<&Envs<'_>>,
    board: &dyn Progress,
) -> Upgrade {
    let lookup = envs.map(Lookup);
    convert_each(
        runs_dir,
        agents_dir,
        lookup.as_ref().map(|l| l as &Stages<'_>),
        board,
    )
}

/// Without the converter there is nothing to look a stage up for.
#[cfg(not(feature = "legacy-runs"))]
pub(crate) fn convert_all(
    runs_dir: &Path,
    agents_dir: Option<&Path>,
    _envs: Option<&Envs<'_>>,
    board: &dyn Progress,
) -> Upgrade {
    convert_each(runs_dir, agents_dir, None, board)
}

/// [`convert_all`], looking each stage up through `stages`.
pub(crate) fn convert_each(
    runs_dir: &Path,
    agents_dir: Option<&Path>,
    stages: Option<&Stages<'_>>,
    board: &dyn Progress,
) -> Upgrade {
    let mut upgrade = Upgrade {
        unconverted: Some(Unconverted::path_for(runs_dir)),
        ..Upgrade::default()
    };
    let dirs = old_runs(runs_dir);
    let mut unconverted = Unconverted::load(runs_dir);
    let backup = Backup::of_runs(runs_dir);
    if !dirs.is_empty() {
        board.begin("converting runs", dirs.len() as u64);
        board.detail(crate::blueprint_upgrade::saving_to(&backup));
    }
    let mut pass = Pass::default();
    for (i, dir) in dirs.iter().enumerate() {
        pass.add(convert_in(
            dir,
            agents_dir,
            stages,
            (&backup, &mut unconverted),
            &mut upgrade,
        ));
        board.done(i as u64 + 1, &upgrade);
    }
    unconverted.save();
    pass.log(runs_dir, &backup);
    upgrade
}

/// Convert the run in `dir` into a run file when it is in the older layout,
/// the way [`convert_all`] does for each run, and say what it did the way an
/// upgrade at start does.
pub(crate) fn convert_one(dir: &Path, agents_dir: Option<&Path>, envs: Option<&Envs<'_>>) {
    #[cfg(feature = "legacy-runs")]
    let lookup = envs.map(Lookup);
    #[cfg(feature = "legacy-runs")]
    let stages = lookup.as_ref().map(|l| l as &Stages<'_>);
    #[cfg(not(feature = "legacy-runs"))]
    let stages = envs.and(None);
    let runs_dir = dir.parent().unwrap_or(dir);
    let mut unconverted = Unconverted::load(runs_dir);
    let backup = Backup::of_runs(runs_dir);
    let mut pass = Pass::default();
    let mut upgrade = Upgrade {
        unconverted: Some(Unconverted::path_for(runs_dir)),
        ..Upgrade::default()
    };
    pass.add(convert_in(
        dir,
        agents_dir,
        stages,
        (&backup, &mut unconverted),
        &mut upgrade,
    ));
    unconverted.save();
    pass.log(runs_dir, &backup);
    upgrade.finish(&backup);
}

/// The old runs under `runs_dir` waiting to be converted: those that did
/// not fail to convert before.
#[cfg(feature = "legacy-runs")]
fn to_convert(runs_dir: &Path) -> Vec<PathBuf> {
    let held = Unconverted::load(runs_dir);
    old_runs(runs_dir)
        .into_iter()
        .filter(|dir| held.why(&dir_name(dir)).is_none())
        .collect()
}

/// How many of `waiting` hold a run file in this build's layout now, whoever
/// converted or upgraded them.
#[cfg(feature = "legacy-runs")]
fn done_of(waiting: &[PathBuf]) -> usize {
    waiting
        .iter()
        .filter(|dir| {
            crate::runstate::run_file::is_run_file(dir) && !leviath_legacy_runs::needs_upgrade(dir)
        })
        .count()
}

/// The name of the run directory `dir`, as the list of runs that did not
/// convert names it.
#[cfg(feature = "legacy-runs")]
fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Every directory under `runs_dir` that holds an old run or a layout-2 run
/// file, in name order. A conversion that was stopped part way is put back
/// first, so the run is converted again.
#[cfg(feature = "legacy-runs")]
fn old_runs(runs_dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(runs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .inspect(|dir| put_back(dir))
        .filter(|dir| {
            leviath_legacy_runs::is_legacy(dir) || leviath_legacy_runs::needs_upgrade(dir)
        })
        .collect();
    dirs.sort();
    dirs
}

/// Put back the old run in `dir` when its conversion was stopped part way,
/// saying so in the log.
#[cfg(feature = "legacy-runs")]
fn put_back(dir: &Path) {
    let shown = dir.display().to_string();
    match leviath_legacy_runs::put_back(dir) {
        Ok(false) => {}
        Ok(true) => {
            tracing::warn!(dir = %shown, "an old run whose conversion was stopped part way was put back, to be converted again");
        }
        Err(e) => {
            let why = e.to_string();
            tracing::warn!(dir = %shown, error = %why, "an old run whose conversion was stopped part way could not be put back; its files are in its legacy directory and the backup");
        }
    }
}

/// Without the converter every run directory is looked at, so one this
/// build cannot read is reported.
#[cfg(not(feature = "legacy-runs"))]
fn old_runs(runs_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(runs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// Convert the run in `dir` when it is an old run, or upgrade its run file
/// when it is in layout 2, unless it failed to before: saved in `backup`
/// first (one that cannot be saved is left as it is, and is tried again at
/// the next start), and listed in `unconverted` when it does not convert.
/// What each conversion filled in is in the run's own log, and at debug
/// level in the daemon's; what it did is added to `upgrade`.
#[cfg(feature = "legacy-runs")]
fn convert_in(
    dir: &Path,
    agents_dir: Option<&Path>,
    stages: Option<&Stages<'_>>,
    (backup, unconverted): (&Backup, &mut Unconverted),
    upgrade: &mut Upgrade,
) -> Done {
    let layout2 = leviath_legacy_runs::needs_upgrade(dir);
    if !layout2 && !leviath_legacy_runs::is_legacy(dir) {
        return Done::Nothing;
    }
    let name = dir_name(dir);
    if unconverted.why(&name).is_some() {
        return Done::Held;
    }
    let shown = dir.display().to_string();
    if let Err(e) = backup.save_run(dir) {
        let why = e.to_string();
        tracing::warn!(dir = %shown, error = %why, "an old run directory could not be backed up, so it was not converted");
        upgrade.failed += 1;
        return Done::Failed;
    }
    if layout2 {
        return upgrade_in(dir, (name, unconverted), upgrade);
    }
    let env = leviath_legacy_runs::ConvertEnv {
        agents_dir: agents_dir.map(Path::to_path_buf),
        stages,
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
            for dropped in &report.dropped {
                upgrade.dropped_in_run(&report.blueprint_name, dropped.to_string());
            }
            upgrade.converted += 1;
            Done::Converted
        }
        Err(e) => {
            let why = e.to_string();
            tracing::warn!(dir = %shown, error = %why, "an old run directory could not be converted; it is left as it was");
            unconverted.add(name, why);
            upgrade.failed += 1;
            Done::Failed
        }
    }
}

/// Upgrade the layout-2 run file in `dir`, the directory `name`, already
/// saved in the backup, listing it in `unconverted` when it does not
/// upgrade, and add what it did to `upgrade`.
#[cfg(feature = "legacy-runs")]
fn upgrade_in(
    dir: &Path,
    (name, unconverted): (String, &mut Unconverted),
    upgrade: &mut Upgrade,
) -> Done {
    match leviath_legacy_runs::upgrade(dir) {
        Ok(report) => {
            let (id, kept) = (
                report.run_id.to_string(),
                report.original.display().to_string(),
            );
            tracing::debug!(run_id = %id, original = %kept, torn_bytes_left_out = report.cut, "upgraded a run file from layout 2");
            upgrade.upgraded += 1;
            Done::Upgraded
        }
        Err(e) => {
            let (shown, why) = (dir.display().to_string(), e.to_string());
            tracing::warn!(dir = %shown, error = %why, "a run file in layout 2 could not be upgraded; it is left as it was");
            unconverted.add(name, why);
            upgrade.failed += 1;
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
    _stages: Option<&Stages<'_>>,
    _kept: (&Backup, &mut Unconverted),
    _upgrade: &mut Upgrade,
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
    /// The child that converts the runs, so the memory converting takes
    /// leaves with it; `None` converts them in the daemon.
    #[cfg(feature = "legacy-runs")]
    pub(crate) child: Option<ChildCmd>,
}

/// How the daemon starts the child that converts its old runs (see
/// [`crate::daemon::convert_child`]).
#[cfg(feature = "legacy-runs")]
#[derive(Debug, Clone)]
pub(crate) struct ChildCmd {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<std::ffi::OsString>,
    /// Variables set for the child on top of the daemon's own environment,
    /// which it inherits.
    pub(crate) env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// How long the child may say nothing before it is taken to be stuck,
    /// stopped, and its runs converted in the daemon.
    pub(crate) quiet_limit: std::time::Duration,
}

#[cfg(feature = "legacy-runs")]
impl ChildCmd {
    /// How long a child may say nothing. It says something after every run,
    /// and the largest old run converts in seconds.
    pub(crate) const QUIET_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

    /// `lev daemon convert-runs` of the executable `program` (this one), for
    /// the runs under `runs_dir` and the blueprints in `agents_dir`, logging
    /// at debug level when this process does.
    pub(crate) fn lev(program: PathBuf, runs_dir: &Path, agents_dir: Option<&Path>) -> Self {
        let mut args: Vec<std::ffi::OsString> = vec![
            "daemon".into(),
            "convert-runs".into(),
            "--runs-dir".into(),
            runs_dir.into(),
            "--build".into(),
            crate::daemon::setup::CURRENT_BUILD.into(),
        ];
        args.extend(
            agents_dir
                .into_iter()
                .flat_map(|dir| ["--agents-dir".into(), dir.into()]),
        );
        args.extend(crate::logging::verbose().then_some("--verbose".into()));
        Self {
            program,
            args,
            env: Vec::new(),
            quiet_limit: Self::QUIET_LIMIT,
        }
    }
}

impl AtStart<'_> {
    /// What a run of `graph` is resolved against: this machine's providers,
    /// its built-in and script tools, the global MCP servers' tools and the
    /// tools of the graph's own servers that are connected.
    pub(crate) fn env(&self, graph: &RunGraph) -> DaemonEnv {
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
            secrets: None,
        }
    }
}

/// Convert every old run under `runs_dir` at daemon start, in the child
/// `start` names when it names one: first connect the MCP servers each
/// unfinished one's stages use, then convert them all, looking each stage up
/// on this machine. A child that cannot start, or stops part way, leaves the
/// rest to the daemon. Returns what it did.
pub(crate) async fn convert_at_start(
    runs_dir: &Path,
    start: AtStart<'_>,
    board: &StartupBoard,
) -> Upgrade {
    #[cfg(feature = "legacy-runs")]
    let (old, layout2): (Vec<PathBuf>, Vec<PathBuf>) = to_convert(runs_dir)
        .into_iter()
        .partition(|dir| leviath_legacy_runs::is_legacy(dir));
    #[cfg(feature = "legacy-runs")]
    let waiting = old.len() + layout2.len();
    #[cfg(feature = "legacy-runs")]
    if let Some(cmd) = start.child.as_ref().filter(|_| waiting > 0) {
        match crate::daemon::convert_child::convert(cmd, runs_dir, &start, board).await {
            Ok(upgrade) => return upgrade,
            Err(so_far) => {
                // A run the child converted and died before it reported is
                // a run file now, and the daemon's pass skips it: what is on
                // disk counts the runs converted and upgraded, whoever did it.
                let rest = in_daemon(runs_dir, &start, board).await;
                let done = (done_of(&old), done_of(&layout2));
                return crate::daemon::convert_child::then(so_far, rest, done);
            }
        }
    }
    in_daemon(runs_dir, &start, board).await
}

/// [`convert_at_start`] in the daemon's own process.
async fn in_daemon(runs_dir: &Path, start: &AtStart<'_>, board: &StartupBoard) -> Upgrade {
    let servers = servers_of_unfinished(runs_dir, start.agents_dir);
    if !servers.is_empty() {
        board.begin("connecting the MCP servers of unfinished old runs", 0);
    }
    for server in servers {
        start.pool.ensure(&server).await;
    }
    let envs = |graph: &RunGraph| start.env(graph);
    convert_all(runs_dir, start.agents_dir, Some(&envs), board)
}

/// The MCP servers the stages of every unfinished old run under `runs_dir`
/// connect to.
#[cfg(feature = "legacy-runs")]
pub(crate) fn servers_of_unfinished(
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
pub(crate) fn servers_of_unfinished(
    _runs_dir: &Path,
    _agents_dir: Option<&Path>,
) -> Vec<leviath_mcp::MCPServerConfig> {
    Vec::new()
}

#[cfg(test)]
#[path = "convert_old_tests.rs"]
mod tests;
