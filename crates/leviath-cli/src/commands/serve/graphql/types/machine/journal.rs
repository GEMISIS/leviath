//! What the daemon has recorded about its runs, and what it has lost.

use async_graphql::{ID, SimpleObject};
use leviath_graphql_derive::mirror;

use super::super::super::scalars::{BigInt, Timestamp};

/// One write the daemon attempted and lost.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct JournalWriteError {
    /// The run whose write it was.
    pub(crate) run_id: ID,
    /// The file that could not be written, as the daemon resolved it.
    pub(crate) path: String,
    /// What the operating system said about it.
    pub(crate) message: String,
    /// When it happened.
    pub(crate) at: Timestamp,
}

/// What the daemon has recorded about its runs, and what it has lost.
///
/// A daemon that cannot write a run's journal answers every request and reports
/// every lane as idle, so this is the only field that says so. A run whose
/// journal record cannot be written is failed, rather than carried on with a
/// history that cannot record what it did.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct JournalHealth {
    /// Whether every write the daemon has attempted reached the disk. Once
    /// something has been lost this stays false for the life of the daemon: a
    /// record that went missing does not come back.
    pub(crate) healthy: bool,
    /// Journal records the daemon has tried to append.
    pub(crate) appends_attempted: BigInt,
    /// Those it could not write, retry included. Each one is something a run did
    /// that its journal does not mention.
    pub(crate) appends_failed: BigInt,
    /// Snapshot writes that lost at least one file. A snapshot is rewritten
    /// whole whenever the run changes, so these cost freshness rather than
    /// history.
    pub(crate) snapshots_failed: BigInt,
    /// How many messages the persistence lane took in one go the last time it
    /// looked. A large number means runs are queueing behind the disk.
    pub(crate) queue_depth: i32,
    /// The most recent write the daemon lost, with the run and the file. Null
    /// while nothing has been lost.
    pub(crate) last_error: Option<JournalWriteError>,
}

impl JournalHealth {
    /// The daemon's reading, as the schema says it.
    pub(crate) fn of(health: &leviath_runtime::persist_stats::JournalHealth) -> Self {
        Self {
            healthy: health.is_healthy(),
            appends_attempted: BigInt(i64::try_from(health.appends_attempted).unwrap_or(i64::MAX)),
            appends_failed: BigInt(i64::try_from(health.appends_failed).unwrap_or(i64::MAX)),
            snapshots_failed: BigInt(i64::try_from(health.snapshots_failed).unwrap_or(i64::MAX)),
            queue_depth: i32::try_from(health.queue_depth).unwrap_or(i32::MAX),
            last_error: health.last_error.as_ref().map(|error| JournalWriteError {
                run_id: ID::from(error.run_id.clone()),
                path: error.path.clone(),
                message: error.message.clone(),
                at: Timestamp(error.at),
            }),
        }
    }
}
