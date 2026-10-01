//! Converts a run directory in the old layout into a single run file.
//!
//! Before the run file, a run was spread over several files in its directory:
//! an LVR1 journal (`run.lvr`), `meta.json`, `context.json`, `stages.json`,
//! `fanout.json`, `interactions.json`, the blueprint it ran
//! (`blueprint.leviath`), and stored parts under `blobs/`. [`convert`] reads
//! all of them and writes one LVR2 run file in their place: the run's
//! [`RunSpec`](leviath_runtime::spec::run_spec::RunSpec), its code and blobs,
//! a state at the start, one delta per journal step that maps onto one, and
//! the state the run was last in. The old files move into `legacy/` beside it
//! rather than being deleted.
//!
//! An old run did not record everything a run file holds. Whatever the
//! conversion had to fill in is named in the [`ConvertReport`], with the value
//! it used and why, and the same lines go into the run's own log in the file.
//! One of them is always the environment fingerprint, which is left empty: an
//! old run never recorded what it relied on from the machine, so a resume
//! treats it as unknown and does not compare it.
//!
//! This crate is temporary. It exists to carry runs over to the new format,
//! only the CLI depends on it, and it is deleted before 1.0.

mod context;
mod error;
mod history;
mod legacy;
mod report;
mod spec;
mod state;
mod write;

use std::path::{Path, PathBuf};

pub use error::ConvertError;
pub use report::{BlueprintSource, ConvertReport, Defaulted};

/// Where the conversion looks for what an old run directory does not hold.
#[derive(Debug, Clone, Default)]
pub struct ConvertEnv {
    /// The installed blueprints, as `<dir>/<name>/agent.leviath`. Runs from
    /// before the blueprint snapshot existed are read against the installed
    /// copy, and the report says so.
    pub agents_dir: Option<PathBuf>,
}

/// Whether `run_dir` holds a run in the old layout that [`convert`] would
/// convert: an LVR1 journal and its `meta.json`.
pub fn is_legacy(run_dir: &Path) -> bool {
    legacy::is_legacy(run_dir)
}

/// Convert the run in `run_dir` into a single run file at
/// `<run_dir>/run.lvr`, moving the old files into `<run_dir>/legacy/`.
///
/// A directory that already holds a run file is refused with
/// [`ConvertError::AlreadyConverted`], so converting twice is harmless.
pub fn convert(run_dir: &Path, env: &ConvertEnv) -> Result<ConvertReport, ConvertError> {
    let old = legacy::LegacyRun::read(run_dir, env)?;
    let mut report = report::Report::default();
    let built = spec::build(&old, &mut report)?;
    let (start, deltas, last) = history::build(&old, &built.spec, &mut report);
    let bytes = write::encode(&old, &built, &start, &deltas, &last);
    let written = write::install(run_dir, &bytes)?;
    let source = old.blueprint.source;
    Ok(report.finish(built.spec.run_id, source, deltas.len(), written))
}
