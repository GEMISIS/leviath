//! The files a run keeps beside its run file, found through what the run file
//! names.
//!
//! A run's answer, its stage logs and its taint audits are files in its
//! directory, and its run file names each one: a path relative to the
//! directory, a size, and for a file written whole its digest (see
//! `leviath_runtime::state::files`). Every reader here (the dashboard, the
//! HTTP API and GraphQL, `lev result`, the REST search) finds them through
//! [`named_files`], and so reads exactly what the run file says it wrote.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use leviath_runtime::state::{FileRef, RunFiles, StageFile};

use super::{StatCache, run_file};

/// The files the run in `dir` keeps beside its run file, as its last step
/// names them. Empty for a directory with no run file that reads.
///
/// Read again only when the run file changed since the last call, so a
/// reader polling a run's logs pays a stat, not a read of the file.
pub(crate) fn named_files(dir: &Path) -> Arc<RunFiles> {
    static CACHE: OnceLock<Mutex<StatCache<RunFiles>>> = OnceLock::new();
    let path = run_file::path_in(dir);
    let mut cache = leviath_core::sync::lock(CACHE.get_or_init(Default::default));
    cache
        .get_reading(
            &path,
            || run_file::tail_in(dir).ok().map(|tail| tail.state.files),
            |_| std::time::Duration::ZERO,
        )
        .unwrap_or_default()
}

/// Where the file `named` is, in the run directory `dir`. `None` for a path
/// that leaves the directory, which is said in the log.
pub(crate) fn path_of(dir: &Path, named: &FileRef) -> Option<PathBuf> {
    named
        .path_in(dir)
        .inspect_err(|e| {
            let (why, run) = (e.to_string(), dir.display().to_string());
            tracing::warn!(dir = %run, why = %why, "a run file names a file outside its run");
        })
        .ok()
}

/// The bytes of the file `named` in `dir`. A file that is not as the run file
/// names it (changed, or shorter than it was written) is still read, and the
/// log says so; one that does not read is `None`.
pub(crate) fn read_named(dir: &Path, named: &FileRef) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path_of(dir, named)?).ok()?;
    warn_if_changed(named, bytes.len() as u64, Some(&bytes));
    Some(bytes)
}

/// The last `max_bytes` of the log `named` in `dir`, from a line boundary.
/// Empty when it does not read; a log shorter than the run wrote it is read,
/// and the log says so.
pub(crate) fn tail_named(dir: &Path, named: &FileRef, max_bytes: u64) -> String {
    let Some(path) = path_of(dir, named) else {
        return String::new();
    };
    if let Ok(meta) = std::fs::metadata(&path) {
        warn_if_changed(named, meta.len(), None);
    }
    super::tail_file(&path, max_bytes)
}

fn warn_if_changed(named: &FileRef, len: u64, bytes: Option<&[u8]>) {
    if let Err(e) = named.check(len, bytes) {
        let why = e.to_string();
        tracing::warn!(why = %why, "a file beside a run file is not as the run wrote it");
    }
}

/// Where the run in `dir` keeps its answer, when its run file names one.
pub(crate) fn final_output_path(dir: &Path) -> Option<PathBuf> {
    let files = named_files(dir);
    path_of(dir, files.final_output.as_ref()?)
}

/// Where the run in `dir` keeps the file `which` of the stage at `index`:
/// where its run file names it, or else where the run writes it once the
/// stage has something to write.
pub(crate) fn stage_file_path(dir: &Path, index: usize, which: StageFile) -> PathBuf {
    let index = u32::try_from(index).unwrap_or(u32::MAX);
    let files = named_files(dir);
    files
        .stage_file(index, which)
        .and_then(|named| path_of(dir, named))
        .unwrap_or(dir.join(which.path(index)))
}

/// The last `max_bytes` of the log `which` of the stage at `index` of the run
/// in `dir`, from a line boundary. Empty when its run file names none.
pub(crate) fn tail_stage_file(
    dir: &Path,
    index: usize,
    which: StageFile,
    max_bytes: u64,
) -> String {
    let files = named_files(dir);
    let named = u32::try_from(index)
        .ok()
        .and_then(|index| files.stage_file(index, which));
    match named {
        Some(named) => tail_named(dir, named, max_bytes),
        None => String::new(),
    }
}

#[cfg(test)]
#[path = "beside_tests.rs"]
mod tests;
