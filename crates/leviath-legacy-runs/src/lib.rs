//! Converts a run directory in the old layout into a single run file.
//!
//! Before the run file, a run was spread over several files in its directory:
//! an LVR1 journal (`run.lvr`), `meta.json`, `context.json`, `stages.json`,
//! `fanout.json`, `interactions.json` (or, from Leviath 0.1.0, the question
//! a worker asked in `pending.json`), the blueprint it ran
//! (`blueprint.leviath`), and stored parts under `blobs/`. [`convert`] reads
//! all of them and writes one LVR2 run file in their place: the run's
//! [`RunSpec`](leviath_runtime::spec::run_spec::RunSpec), its code, a state
//! at the start, one delta per journal step that maps onto one, and the
//! state the run was last in. The old files move into `legacy/` beside it
//! rather than being deleted, except the per-stage logs and audits
//! (`stages/`), the answer (`final_output`), the stored parts (`blobs/`) and
//! the list of files uploaded to providers (`provider-files.json`), which a
//! run in the new layout keeps in the same place and form: they stay where
//! they are, and the run file names the ones it names for a new run.
//!
//! A converted run lists as the release that wrote it listed it: what its
//! `meta.json` says, and the graph it ran. When the installed blueprint is
//! no longer that graph (a stage gone, added or moved), the run's graph is
//! the one it recorded, and a run that was not finished ends with the reason
//! rather than carrying on against stages it never had.
//!
//! An old run did not record everything a run file holds. Whatever the
//! conversion had to fill in is named in the [`ConvertReport`], with the value
//! it used and why, and the same lines go into the run's own log in the file.
//! One of them is always the environment fingerprint, which is left empty: an
//! old run never recorded what it relied on from the machine, so a resume
//! treats it as unknown and does not compare it.
//!
//! It also holds the reader for the old `agent.leviath` blueprint format,
//! and [`migrate()`] turns one into an `agent.toml`, which is what
//! `lev blueprint migrate` writes. The reader parses a manifest into a private
//! copy of the old parsed-blueprint types, kept here and nowhere else, and
//! reads that as the runtime's run graph.
//!
//! This crate is temporary. It exists to carry runs and blueprints over to
//! the new formats, only the CLI depends on it, and it is deleted before 1.0.

mod context;
mod error;
mod history;
pub mod journal;
mod legacy;
mod manifest;
mod migrate;
mod old;
mod plan;
mod recorded;
mod report;
mod spec;
mod state;
mod write;

use std::path::{Path, PathBuf};

use leviath_runtime::spec::env::{CodeFiles, ModelPlan, StageTools};
use leviath_runtime::spec::graph::{RunGraph, StageDef};
use leviath_runtime::spec::names::ModelRef;

pub use error::ConvertError;
pub use migrate::{Migrated, migrate, migrate_file, migrate_noted};
pub use report::{BlueprintSource, ConvertReport, Defaulted, Dropped};

/// Where the conversion looks for what an old run directory does not hold.
#[derive(Clone, Default)]
pub struct ConvertEnv<'a> {
    /// The installed blueprints, as `<dir>/<name>/agent.leviath`. Runs from
    /// before the blueprint snapshot existed are read against the installed
    /// copy, and the report says so.
    pub agents_dir: Option<PathBuf>,
    /// What this machine answers about a stage: the window of the model it
    /// runs and the tools it gets. Without it, a stage keeps the model its
    /// run named, sized to the context budget it last ran with, and no tools.
    pub stages: Option<&'a dyn StageLookup>,
}

impl std::fmt::Debug for ConvertEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConvertEnv")
            .field("agents_dir", &self.agents_dir)
            .field("stages", &self.stages.is_some())
            .finish()
    }
}

/// What the host answers about one stage of a run being converted, the way
/// it answers when it resolves a new run. An old run named its models and
/// its tools but kept neither a model's window nor a tool's definition, so a
/// converted run has both looked up here once and carries them from then on.
pub trait StageLookup: Send + Sync {
    /// The model `stage` runs on: `requested` when the run asked for one,
    /// else the stage's own choice over the operator's defaults.
    fn model(
        &self,
        graph: &RunGraph,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, String>;
    /// The tools `stage` gets, each with its schema, and the code of any
    /// script tool among them that `code` does not already hold. `base` is
    /// the directory of the installed agent the run came from, and `workdir`
    /// the run's own.
    fn tools(
        &self,
        graph: &RunGraph,
        stage: &StageDef,
        code: &CodeFiles,
        base: Option<&Path>,
        workdir: Option<&Path>,
    ) -> Result<StageTools, String>;
    /// How deep a tree of child runs goes for a run whose graph sets no
    /// limit: the operator's default, which a run that recorded none ran
    /// under.
    fn default_max_depth(&self, graph: &RunGraph) -> u8;
}

/// Whether `run_dir` holds a run in the old layout that [`convert`] would
/// convert: an LVR1 journal and its `meta.json`.
pub fn is_legacy(run_dir: &Path) -> bool {
    legacy::is_legacy(run_dir)
}

/// The metadata of the old run in `run_dir`, as its `meta.json` holds it, or
/// `None` when there is none that reads.
pub fn meta(run_dir: &Path) -> Option<leviath_core::run_meta::RunMeta> {
    legacy::meta(run_dir)
}

/// The run graph of the old run in `run_dir`, read as its conversion reads
/// it, so a host can make ready what the run needs (the MCP servers its
/// stages connect to) before converting it.
pub fn graph(run_dir: &Path, env: &ConvertEnv<'_>) -> Result<RunGraph, ConvertError> {
    let old = legacy::LegacyRun::read(run_dir, env)?;
    Ok(spec::graph(&old, &mut report::Report::default()).1)
}

/// Convert the run in `run_dir` into a single run file at
/// `<run_dir>/run.lvr`, moving the old files into `<run_dir>/legacy/` (all
/// but the per-stage logs and the answer, which stay where they are).
///
/// A directory that already holds a run file is refused with
/// [`ConvertError::AlreadyConverted`], so converting twice is harmless.
pub fn convert(run_dir: &Path, env: &ConvertEnv<'_>) -> Result<ConvertReport, ConvertError> {
    let old = legacy::LegacyRun::read(run_dir, env)?;
    let mut report = report::Report::default();
    let mut built = spec::build(&old, env.stages, &mut report)?;
    let (start, deltas, last) = history::build(&old, &built.spec, &mut report);
    built.spec.listed = Some(spec::listed(old.meta(), last.seq));
    let bytes = write::encode(&built, &start, &deltas, &last);
    let written = write::install(run_dir, &bytes)?;
    let blueprint_name = old.meta().agent_name.clone();
    let source = old.blueprint.source;
    let mut done = report.finish(built.spec.run_id, source, deltas.len(), written);
    done.blueprint_name = blueprint_name;
    Ok(done)
}
