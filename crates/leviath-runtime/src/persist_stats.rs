//! Health counters for the persistence lane.
//!
//! Everything the lane writes is something a person reads back later: `lev ps`
//! reads `meta.json`, the dashboard reads `context.json`, and a post-mortem
//! reads the journal in `run.lvr`. A write the lane loses leaves a log line and
//! nothing else, so a daemon whose journal has been failing for an hour answers
//! every request and looks perfectly well. These counters are what makes that
//! legible: `lev ps`, `lev doctor` and the GraphQL schema all read the same
//! numbers.
//!
//! Modelled on [`ToolLaneStats`](crate::tool_bridge::ToolLaneStats), for the
//! same reason: the lane reads an **unbounded** queue, so nothing upstream ever
//! feels it fall behind, and counting is the only way to see it.
//!
//! The lane also names here the runs whose journal it could not write, because a
//! run that cannot record what it did must not keep doing things. The
//! `fail_runs_with_unwritable_journals` system drains those names each tick and
//! fails the runs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// One write the lane attempted and lost, with enough to act on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalError {
    /// The run whose write it was.
    pub run_id: String,
    /// The file that could not be written, as the lane resolved it.
    pub path: String,
    /// What the operating system said about it.
    pub message: String,
    /// When it happened, in unix seconds.
    pub at: i64,
}

/// What the persistence lane has written, and what it has lost.
///
/// Read from the daemon alongside the run listing, so "is this run stuck" and
/// "is anything this daemon says about its runs still true" are answered in the
/// same breath.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalHealth {
    /// Journal records the lane has tried to append.
    pub appends_attempted: u64,
    /// Those the lane could not get onto disk, retry included. Each one is a
    /// thing a run did that its journal does not mention.
    pub appends_failed: u64,
    /// Snapshot writes (`meta.json`, `context.json` and the files beside them)
    /// that lost at least one file.
    pub snapshots_failed: u64,
    /// How many messages the lane took in one go the last time it looked. One
    /// means it is keeping up; a large number means runs are queueing behind a
    /// disk that cannot take them as fast as they arrive. Zero until the lane
    /// has had anything to do.
    pub queue_depth: usize,
    /// The most recent write the lane lost, with the run and the file.
    pub last_error: Option<JournalError>,
}

impl JournalHealth {
    /// Whether every write the lane has attempted reached the disk.
    ///
    /// Sticky on purpose: a record that was lost does not become unlost, so
    /// this stays false for the life of the daemon once something has gone
    /// missing. The counters and `last_error` say how much and how recently.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.appends_failed == 0 && self.snapshots_failed == 0
    }

    /// One line on what is wrong, for a person. `None` while the lane is well.
    #[must_use]
    pub fn complaint(&self) -> Option<String> {
        let last = self.last_error.as_ref()?;
        Some(format!(
            "{} journal append(s) and {} snapshot write(s) lost; last: {} ({}) - {}",
            self.appends_failed, self.snapshots_failed, last.path, last.run_id, last.message
        ))
    }
}

/// Live health of the persistence lane, shared between the lane and the world.
///
/// The counters are atomics and the two tables are behind their own locks, so
/// the lane records a loss without ever waiting on a reader, and a reader never
/// waits on the lane.
#[derive(Debug, Default)]
pub struct PersistLaneStats {
    appends_attempted: AtomicU64,
    appends_failed: AtomicU64,
    snapshots_failed: AtomicU64,
    queued: AtomicUsize,
    /// The most recent loss of any kind.
    last: Mutex<Option<JournalError>>,
    /// The runs whose journal the lane could not write, by run id, waiting to
    /// be failed. Keyed by run rather than appended to, so a run failing every
    /// append costs one entry however long the world takes to notice.
    unwritable: Mutex<HashMap<String, JournalError>>,
}

impl PersistLaneStats {
    /// Counters for a lane that has done nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record how many messages were waiting when the lane last looked.
    pub(crate) fn observe_queue(&self, depth: usize) {
        self.queued.store(depth, Ordering::Relaxed);
    }

    /// Record a journal append about to be tried.
    pub(crate) fn append_attempted(&self) {
        self.appends_attempted.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a journal append that is lost for good, and name the run so it can
    /// be failed.
    ///
    /// Only a genuine write failure reaches here. A write the lane drops because
    /// the run directory is gone is not one: deleting a run is somebody saying
    /// they want it gone, and reporting that as a fault would fail runs for
    /// being deleted. The lane decides which it is before calling this - see
    /// `may_write` and `append_record` in
    /// [`persistence_bridge`](crate::persistence_bridge).
    pub(crate) fn journal_append_failed(&self, run_id: &str, path: &Path, message: &str) {
        self.appends_failed.fetch_add(1, Ordering::Relaxed);
        let error = self.remember(run_id, path, message);
        self.unwritable
            .lock()
            .expect("the failure table is never held across a panic")
            .insert(run_id.to_string(), error);
    }

    /// Record a snapshot write that lost one of its files.
    ///
    /// The run is not failed for this. A snapshot is rewritten whole every time
    /// the run changes, so the next one puts back what this one lost; the
    /// journal is append-only, which is why that half is treated differently.
    pub(crate) fn snapshot_failed(&self, run_id: &str, path: &Path, message: &str) {
        self.snapshots_failed.fetch_add(1, Ordering::Relaxed);
        self.remember(run_id, path, message);
    }

    /// Store a loss as the latest one, and hand it back for the caller to file.
    fn remember(&self, run_id: &str, path: &Path, message: &str) -> JournalError {
        let error = JournalError {
            run_id: run_id.to_string(),
            path: path.display().to_string(),
            message: message.to_string(),
            at: chrono::Utc::now().timestamp(),
        };
        *self
            .last
            .lock()
            .expect("the last-error slot is never held across a panic") = Some(error.clone());
        error
    }

    /// Take the runs whose journal could not be written, leaving the table
    /// empty.
    ///
    /// Taking rather than reading: the world fails each run it still holds and
    /// drops the rest, and a run that has already gone must not be reported for
    /// ever.
    pub(crate) fn take_unwritable(&self) -> Vec<JournalError> {
        std::mem::take(
            &mut *self
                .unwritable
                .lock()
                .expect("the failure table is never held across a panic"),
        )
        .into_values()
        .collect()
    }

    /// A point-in-time read of the counters.
    #[must_use]
    pub fn report(&self) -> JournalHealth {
        JournalHealth {
            appends_attempted: self.appends_attempted.load(Ordering::Relaxed),
            appends_failed: self.appends_failed.load(Ordering::Relaxed),
            snapshots_failed: self.snapshots_failed.load(Ordering::Relaxed),
            queue_depth: self.queued.load(Ordering::Relaxed),
            last_error: self
                .last
                .lock()
                .expect("the last-error slot is never held across a panic")
                .clone(),
        }
    }
}

#[cfg(test)]
#[path = "persist_stats_tests.rs"]
mod tests;
