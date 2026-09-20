//! Exporting the run store, one JSONL file at a time.
//!
//! Paging ten thousand runs through a connection is two hundred requests, and a
//! client that wants everything wants it once. So an export is a job: the
//! request returns immediately, a worker writes the file, and a signed link
//! hands it over when it is done.
//!
//! The file is written a run at a time and never held whole in memory, which is
//! the difference between this and a very large response: the store can be
//! larger than the machine's memory, and one day will be.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::super::types::AppState;
use super::error::ServeError;

/// Where an export has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportStatus {
    /// Enqueued; the worker has not started it.
    Queued,
    /// Writing.
    Running,
    /// Written, and the file is there to fetch.
    Complete,
    /// It broke. `error` says how.
    Failed,
}

impl ExportStatus {
    /// The word this status goes out as.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

/// One export job.
#[derive(Debug, Clone)]
pub(crate) struct ExportJob {
    /// The job's id, which is also its file's name.
    pub(crate) id: String,
    /// Where it has got to.
    pub(crate) status: ExportStatus,
    /// How many runs have been written so far.
    pub(crate) written: usize,
    /// Why it failed, when it did.
    pub(crate) error: Option<String>,
    /// When it was started, unix seconds.
    pub(crate) started_at: i64,
}

/// How long a finished export's file is kept.
///
/// Long enough to fetch it, short enough that a forgotten export does not sit
/// in the data directory forever. A client that wants it again asks again;
/// the export is cheap and the store has moved on anyway.
pub(crate) const EXPORT_TTL_SECS: i64 = 3_600;

/// The exports this server has been asked for.
///
/// Records live in memory and files on disk, both swept on the same clock, so
/// a record never outlives its file or the other way round.
#[derive(Clone, Default)]
pub(crate) struct Exports {
    jobs: Arc<Mutex<HashMap<String, ExportJob>>>,
    /// Distinguishes two exports started in the same second.
    seq: Arc<AtomicU64>,
}

impl std::fmt::Debug for Exports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exports")
            .field("jobs", &leviath_core::sync::lock(&self.jobs).len())
            .finish_non_exhaustive()
    }
}

impl Exports {
    /// One job, by id.
    pub(crate) fn get(&self, id: &str) -> Option<ExportJob> {
        leviath_core::sync::lock(&self.jobs).get(id).cloned()
    }

    /// Record a new job, queued.
    fn enqueue(&self, now: i64) -> ExportJob {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let job = ExportJob {
            id: format!("export-{now}-{seq}"),
            status: ExportStatus::Queued,
            written: 0,
            error: None,
            started_at: now,
        };
        leviath_core::sync::lock(&self.jobs).insert(job.id.clone(), job.clone());
        job
    }

    /// Update one job in place.
    ///
    /// The edit is a trait object rather than a type parameter: a generic here is
    /// compiled once per call site, and no single copy sees both a job that is
    /// still listed and one the reaper has taken.
    fn update(&self, id: &str, edit: &mut dyn FnMut(&mut ExportJob)) {
        if let Some(job) = leviath_core::sync::lock(&self.jobs).get_mut(id) {
            edit(job);
        }
    }

    /// Drop the records and files of exports older than the window.
    ///
    /// Called when a new export starts rather than on a timer: an export is
    /// the only thing that makes these, so it is the only thing that needs to
    /// tidy them, and a server nobody exports from starts no threads.
    fn sweep(&self, dir: &std::path::Path, now: i64) {
        let mut jobs = leviath_core::sync::lock(&self.jobs);
        jobs.retain(|id, job| {
            let keep = now - job.started_at < EXPORT_TTL_SECS;
            if !keep {
                let _ = std::fs::remove_file(dir.join(format!("{id}.jsonl")));
            }
            keep
        });
    }
}

/// Where exports are written.
pub(crate) fn exports_dir() -> PathBuf {
    exports_dir_beside(&crate::runstate::runs_dir())
}

/// Where exports go, given where the runs are.
///
/// Beside the run store rather than inside it: a directory under `runs/` is a
/// run directory to every walk of the store, so the listing would stat it on
/// every request and a sweep by age would consider deleting it.
///
/// Split from [`exports_dir`] for the one case the real path never takes: a runs
/// directory with no parent, which is a filesystem root and has nowhere beside
/// it to put anything.
fn exports_dir_beside(runs: &std::path::Path) -> PathBuf {
    match runs.parent() {
        Some(data) => data.join("exports"),
        None => runs.join("exports"),
    }
}

/// The file one export writes.
pub(crate) fn export_path(id: &str) -> PathBuf {
    exports_dir().join(format!("{id}.jsonl"))
}

/// Start an export, and return the job straight away.
///
/// The filter is the run listing's own, so a client builds the predicate once
/// and uses it for both. `fields` narrows each row the way `?fields=` does on
/// the listing, and an unknown name is refused here rather than silently
/// dropped: a column that is quietly missing from an export is discovered
/// downstream, by somebody else.
pub(crate) async fn start(
    state: &AppState,
    spec: super::runs::RunSpec,
    known_fields: impl Fn() -> std::collections::HashSet<String>,
) -> Result<ExportJob, ServeError> {
    if let Some(fields) = spec.fields.as_ref() {
        let known = known_fields();
        if let Some(unknown) = fields.iter().find(|name| !known.contains(name.as_str())) {
            return Err(ServeError::BadRequest(format!(
                "Unknown field '{unknown}' in `fields`"
            )));
        }
    }

    let now = leviath_core::duration::now_secs();
    let dir = exports_dir();
    state.caches.exports.sweep(&dir, now);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ServeError::Internal(format!("Could not make the exports directory: {e}")))?;
    let job = state.caches.exports.enqueue(now);

    // The listing runs here, on the caller's task, so a filter that cannot be
    // answered fails the request rather than a file nobody is watching. The
    // writing is what moves off it.
    let listing = super::runs::list(state, &spec).await;
    let rows: Vec<serde_json::Value> = listing
        .hits
        .iter()
        .map(|hit| {
            let mut value = super::super::runs::run_json(&hit.meta, now);
            if let (Some(fields), serde_json::Value::Object(map)) = (&spec.fields, &mut value) {
                map.retain(|key, _| fields.contains(key));
            }
            value
        })
        .collect();

    let exports = state.caches.exports.clone();
    let id = job.id.clone();
    let path = export_path(&id);
    // One blocking hop for the whole file: a JSONL writer is a loop over a
    // buffered file, and doing it on the async runtime would hold a worker for
    // as long as the store is large.
    tokio::task::spawn_blocking(move || {
        exports.update(&id, &mut |job: &mut ExportJob| {
            job.status = ExportStatus::Running
        });
        match write_file(&path, &rows) {
            Ok(written) => exports.update(&id, &mut |job: &mut ExportJob| {
                job.status = ExportStatus::Complete;
                job.written = written;
            }),
            Err(e) => exports.update(&id, &mut |job: &mut ExportJob| {
                job.status = ExportStatus::Failed;
                job.error = Some(e.to_string());
            }),
        }
    });

    Ok(job)
}

/// Write the rows to a file as JSONL.
fn write_file(path: &std::path::Path, rows: &[serde_json::Value]) -> std::io::Result<usize> {
    let file = std::fs::File::create(path)?;
    write_rows(&mut std::io::BufWriter::new(file), rows)
}

/// Write the rows as JSONL, one run per line.
///
/// Line by line into whatever it is given: the file can be larger than memory,
/// and a reader can start on it before the writer has finished. Taking the
/// writer rather than opening one is what makes a failed write testable, which
/// matters because a client only ever learns about it from the job.
///
/// A trait object rather than a type parameter: a generic here is compiled once
/// per writer, and the real file never fails where the test doubles only fail,
/// so no copy would ever walk every arm.
fn write_rows(out: &mut dyn Write, rows: &[serde_json::Value]) -> std::io::Result<usize> {
    for row in rows {
        serde_json::to_writer(&mut *out, row)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(rows.len())
}

/// Record a job in whatever state a test needs, with its file beside it.
///
/// The route has an answer for every state a job can be in, and only the
/// complete one happens by running an export. Reaching the others by racing a
/// real worker would be a flaky test asserting a timing accident.
#[cfg(test)]
pub(crate) fn test_job(state: &AppState, status: ExportStatus, contents: &str) -> String {
    test_job_with(state, status, contents, Some("the disk went away"))
}

/// [`test_job`], with the reason a failure recorded, or none.
///
/// A failure with no reason is a real state rather than a contrivance: the
/// status and the reason are two writes, so a record read between them has the
/// first and not the second.
#[cfg(test)]
pub(crate) fn test_job_with(
    state: &AppState,
    status: ExportStatus,
    contents: &str,
    reason: Option<&str>,
) -> String {
    let now = leviath_core::duration::now_secs();
    let job = state.caches.exports.enqueue(now);
    state
        .caches
        .exports
        .update(&job.id, &mut |job: &mut ExportJob| {
            job.status = status;
            if status == ExportStatus::Failed {
                job.error = reason.map(str::to_string);
            }
        });
    if status == ExportStatus::Complete {
        std::fs::create_dir_all(exports_dir()).expect("the exports directory");
        std::fs::write(export_path(&job.id), contents).expect("the export's file");
    }
    job.id
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
