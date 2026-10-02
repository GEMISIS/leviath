//! The run index: a summary of every run, kept in one file beside the runs
//! directory, so that listing runs reads that file and a stat per run rather
//! than decoding every run file.
//!
//! A run's file is the truth about it; the index only remembers what each one
//! said. Every entry carries the size and modification time of the run file it
//! was read from, and an entry whose file has moved on since is read again from
//! the file. A run with no entry is read and added, and an entry whose run is
//! gone is dropped. So the index can be deleted at any time, and the next
//! listing builds it again; and any writer of a run file (the daemon, a
//! command that edits one) can leave it stale without anyone reading a wrong
//! summary.
//!
//! Whoever lists the runs saves what changed. The daemon also brings it up to
//! date every couple of seconds while it runs, so a listing usually finds
//! every entry current and only stats.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::runstate::{self, RunMeta};

/// The index format. An index written in any other is read as empty and
/// rebuilt.
const VERSION: u32 = 1;

/// How often the daemon brings the index up to date.
pub(crate) const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// A run file's size and modification time, which change whenever it is
/// written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    len: u64,
    secs: u64,
    nanos: u32,
}

impl Stamp {
    /// The stamp of the run file in `dir`, when there is one.
    fn of(dir: &Path) -> Option<Self> {
        let meta = std::fs::metadata(runstate::run_file::path_in(dir)).ok()?;
        // A filesystem that keeps no modification time leaves the size to
        // tell a rewrite apart.
        let modified = meta
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Some(Self {
            len: meta.len(),
            secs: modified.as_secs(),
            nanos: modified.subsec_nanos(),
        })
    }
}

/// One run, as its file last read.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    run: RunMeta,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    /// By run directory name.
    runs: BTreeMap<String, Entry>,
}

/// The index of the runs under one runs directory, loaded.
#[derive(Debug)]
pub(crate) struct RunIndex {
    path: PathBuf,
    runs: BTreeMap<String, Entry>,
    changed: bool,
}

/// Where the index of `runs_dir` is kept: beside it, named after it.
pub(crate) fn path_for(runs_dir: &Path) -> PathBuf {
    let name = runs_dir
        .file_name()
        .map_or_else(|| "runs".into(), |n| n.to_string_lossy().into_owned());
    runs_dir.with_file_name(format!("{name}.index"))
}

impl RunIndex {
    /// The index of `runs_dir`. One that is missing, does not parse or is in
    /// another format is empty, and fills as runs are read.
    pub(crate) fn load(runs_dir: &Path) -> Self {
        let path = path_for(runs_dir);
        let runs = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<IndexFile>(&bytes).ok())
            .filter(|file| file.version == VERSION)
            .map(|file| file.runs)
            .unwrap_or_default();
        Self {
            path,
            runs,
            changed: false,
        }
    }

    /// The run in `dir`: the index's summary when its run file has not
    /// changed since, else read from the file and remembered. `None` when the
    /// directory holds no run file that reads, which is forgotten.
    pub(crate) fn run(&mut self, dir: &Path) -> Option<RunMeta> {
        let name = dir.file_name()?.to_string_lossy().into_owned();
        let Some(stamp) = Stamp::of(dir) else {
            self.forget(&name);
            return None;
        };
        if let Some(entry) = self.runs.get(&name).filter(|e| e.stamp == stamp) {
            return Some(entry.run.clone());
        }
        match runstate::read_meta_from(dir) {
            Ok(run) => {
                let entry = Entry {
                    stamp,
                    run: run.clone(),
                };
                self.runs.insert(name, entry);
                self.changed = true;
                Some(run)
            }
            Err(_) => {
                self.forget(&name);
                None
            }
        }
    }

    fn forget(&mut self, name: &str) {
        self.changed |= self.runs.remove(name).is_some();
    }

    /// Forget every run whose directory is not among `dirs`.
    pub(crate) fn keep_only(&mut self, dirs: &HashSet<PathBuf>) {
        let names: HashSet<String> = dirs
            .iter()
            .filter_map(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        let before = self.runs.len();
        self.runs.retain(|name, _| names.contains(name));
        self.changed |= self.runs.len() != before;
    }

    /// Write the index back when anything in it changed. Best effort: an
    /// index that cannot be written is built again by the next listing.
    pub(crate) fn save(self) {
        if !self.changed {
            return;
        }
        let file = IndexFile {
            version: VERSION,
            runs: self.runs,
        };
        let bytes = serde_json::to_vec(&file).expect("a run summary is plain data");
        if let Err(e) = leviath_sys::perms::write_private(&self.path, &bytes) {
            // Formatted outside the macro, so the text is made whether or not
            // a subscriber reads the fields.
            let (shown, why) = (self.path.display().to_string(), e.to_string());
            tracing::debug!(path = %shown, error = %why, "the run index could not be saved");
        }
    }
}

/// Every run under `runs_dir`, newest first, through its index, which is
/// brought up to date and saved.
pub(crate) fn list(runs_dir: &Path) -> Vec<RunMeta> {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    let mut index = RunIndex::load(runs_dir);
    let mut runs: Vec<RunMeta> = dirs.iter().filter_map(|d| index.run(d)).collect();
    index.keep_only(&dirs.into_iter().collect());
    index.save();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs
}

/// Bring the index of `runs_dir` up to date every [`REFRESH_EVERY`] for as
/// long as `runtime` runs, off its worker threads.
pub(crate) fn keep_fresh(runtime: &tokio::runtime::Handle, runs_dir: PathBuf) {
    runtime.spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_EVERY);
        loop {
            tick.tick().await;
            let dir = runs_dir.clone();
            let _ = tokio::task::spawn_blocking(move || list(&dir)).await;
        }
    });
}

#[cfg(test)]
#[path = "run_index_tests.rs"]
mod tests;
