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
//! Whoever lists the runs saves what changed. The daemon also looks at the
//! run files every couple of seconds while it runs, and brings the index up
//! to date when one changed, so a listing usually finds every entry current
//! and only stats.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        // A file an earlier release wrote under the same name is told apart
        // by its first bytes, not read whole.
        let read = runstate::run_file::is_run_file(dir)
            .then(|| runstate::read_meta_from(dir).ok())
            .flatten();
        let Some(run) = read else {
            self.forget(&name);
            return None;
        };
        let entry = Entry {
            stamp,
            run: run.clone(),
        };
        self.runs.insert(name, entry);
        self.changed = true;
        Some(run)
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

/// Every run under `runs_dir` and its directory, through its index, which
/// is brought up to date and saved.
fn indexed(runs_dir: &Path) -> Vec<(PathBuf, RunMeta)> {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    let mut index = RunIndex::load(runs_dir);
    let runs = dirs
        .iter()
        .filter_map(|d| index.run(d).map(|run| (d.clone(), run)))
        .collect();
    index.keep_only(&dirs.into_iter().collect());
    index.save();
    runs
}

/// Every run under `runs_dir`, newest first, through its index, which is
/// brought up to date and saved.
pub(crate) fn list(runs_dir: &Path) -> Vec<RunMeta> {
    let mut runs: Vec<RunMeta> = indexed(runs_dir).into_iter().map(|(_, run)| run).collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs
}

/// Every run directory under `runs_dir` whose run has not finished, through
/// its index: what a daemon brings back when it starts, found without
/// reading the file of every run that finished.
pub(crate) fn unfinished(runs_dir: &Path) -> Vec<PathBuf> {
    unfinished_at_a_glance(runs_dir).unwrap_or_else(|| {
        indexed(runs_dir)
            .into_iter()
            .filter(|(_, run)| !runstate::is_terminal_status(&run.status))
            .map(|(dir, _)| dir)
            .collect()
    })
}

/// One run in the index, with only what says whether it finished.
#[derive(Deserialize)]
struct Glance {
    stamp: Stamp,
    run: GlanceRun,
}

#[derive(Deserialize)]
struct GlanceRun {
    status: runstate::RunStatus,
}

/// The index, with only what says whether each run finished.
#[derive(Deserialize)]
struct GlanceFile {
    version: u32,
    runs: BTreeMap<String, Glance>,
}

/// [`unfinished`], answered from each run's status in the index, when the
/// index is up to date: every run file under `runs_dir` is as it last read,
/// and it holds no other. `None` when it is not, or does not read.
///
/// A summary of a run is a few kilobytes. Making one for each of a thousand
/// finished runs, only to drop them again, leaves a daemon holding megabytes
/// of freed memory between what it keeps, for as long as it runs.
fn unfinished_at_a_glance(runs_dir: &Path) -> Option<Vec<PathBuf>> {
    // Read through a small buffer rather than whole: the index of a thousand
    // runs is megabytes, and nearly all of it is passed over.
    let index = std::fs::File::open(path_for(runs_dir)).ok()?;
    let file = serde_json::from_reader::<_, GlanceFile>(std::io::BufReader::new(index))
        .ok()
        .filter(|file| file.version == VERSION)?;
    let mut dirs: Vec<(PathBuf, String)> = std::fs::read_dir(runs_dir)
        .ok()?
        .flatten()
        .map(|e| (e.path(), e.file_name().to_string_lossy().into_owned()))
        .collect();
    dirs.sort();
    let mut indexed = 0;
    let mut open = Vec::new();
    for (dir, name) in dirs {
        let Some(stamp) = Stamp::of(&dir) else {
            continue;
        };
        let entry = file.runs.get(&name).filter(|e| e.stamp == stamp)?;
        indexed += 1;
        if !runstate::is_terminal_status(&entry.run.status) {
            open.push(dir);
        }
    }
    (indexed == file.runs.len()).then_some(open)
}

/// How the run files under `runs_dir` look from outside: one number made
/// from the directory name, size and modification time of each. It changes
/// whenever a run file is added, removed or written, and costs a stat per
/// run to make.
fn look_of(runs_dir: &Path) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return 0;
    };
    // Summed rather than hashed in turn, so the order a directory lists its
    // entries in does not matter.
    entries.flatten().fold(0u64, |look, entry| {
        let Some(stamp) = Stamp::of(&entry.path()) else {
            return look;
        };
        let mut hasher = DefaultHasher::new();
        entry.file_name().hash(&mut hasher);
        stamp.hash(&mut hasher);
        look.wrapping_add(hasher.finish())
    })
}

/// Bring the index of `runs_dir` up to date, unless its run files still
/// look the way they did at `seen` (see [`look_of`]). Returns how they look
/// now.
///
/// An index is a few kilobytes per run, so reading it is far from free in a
/// home of a thousand runs. A daemon sitting idle leaves every run file as it
/// was, and this reads nothing but the directory.
fn refresh(runs_dir: &Path, seen: u64) -> u64 {
    let look = look_of(runs_dir);
    if look != seen {
        indexed(runs_dir);
    }
    look
}

/// Keep the index of `runs_dir` up to date for as long as `runtime` runs, off
/// its worker threads: every [`REFRESH_EVERY`] the run files are looked at,
/// and the index is read and brought up to date when one has changed since
/// the last look. The first look is taken now, so the index is taken to be up
/// to date already, as the daemon's start leaves it.
pub(crate) fn keep_fresh(runtime: &tokio::runtime::Handle, runs_dir: PathBuf) {
    keep_fresh_every(runtime, runs_dir, REFRESH_EVERY);
}

/// [`keep_fresh`], checking every `every`.
fn keep_fresh_every(runtime: &tokio::runtime::Handle, runs_dir: PathBuf, every: Duration) {
    let mut seen = look_of(&runs_dir);
    runtime.spawn(async move {
        let start = tokio::time::Instant::now() + every;
        let mut tick = tokio::time::interval_at(start, every);
        loop {
            tick.tick().await;
            let dir = runs_dir.clone();
            seen = tokio::task::spawn_blocking(move || refresh(&dir, seen))
                .await
                .unwrap_or(seen);
        }
    });
}

#[cfg(test)]
#[path = "run_index_tests.rs"]
mod tests;
