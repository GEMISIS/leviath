//! A change to a context window as the committed transaction it is.
//!
//! One write to one region is the smallest thing that happens and nearly never
//! the whole of what happened. A compaction writes a summary into one region and
//! empties another; a stage edge clears four; a resume rebuilds every region the
//! run had. Recorded region by region, those read as unrelated events that
//! happen to share a second, and the window they produced is not named at all -
//! so a reader cannot say which snapshot a change landed on, and a reader
//! looking at one region cannot see what moved with it.
//!
//! A transaction record fixes both. It names the window before and after by
//! [revision](crate::run_meta::revision), so a change is anchored to the exact
//! content it started from and produced, and it carries every region the change
//! touched, each with its contents digested before and after and its token count
//! either side.
//!
//! What it deliberately does not carry is content. The snapshot recorded beside
//! it already holds the text, and a digest is what tells a reader whether it has
//! to go and read that snapshot at all.

use serde::{Deserialize, Serialize};

use std::io::{self, Read};

use super::{Frame, Frames, RunRecord};
use crate::ContextCause;

/// One region's part in a committed transaction, as the journal writes it.
///
/// Every field is something the write path already had in hand: the shape of the
/// region either side of the change, and the digest of what it held. Nothing here
/// is derived, so nothing here can disagree with the snapshot beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionCommit {
    /// The region this part of the transaction touched.
    pub region: String,
    /// The digest of its contents before the change.
    pub digest_before: String,
    /// The digest of its contents after it.
    pub digest_after: String,
    /// What it held before, in tokens.
    pub tokens_before: usize,
    /// What it held after.
    pub tokens_after: usize,
    /// How many entries it held before.
    pub entries_before: usize,
    /// How many it held after.
    pub entries_after: usize,
    /// How many entries the change itself pushed.
    ///
    /// Not derivable from the counts either side: a write that appends one entry
    /// into a full sliding region leaves the count where it was, and the entry
    /// that left is the region's own eviction rather than part of the write.
    pub entries_added: usize,
}

/// One region's part in a committed transaction, as a reader gets it.
///
/// The fields a transaction record carries, plus the two a reader would
/// otherwise compute for itself. The `Option`s are what a change recorded
/// one region at a time, before transactions, simply did not write down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionTransition {
    /// The region this part of the transaction touched.
    pub region: String,
    /// The digest of its contents before the change. `None` when the record
    /// carries no digest.
    pub digest_before: Option<String>,
    /// The digest of its contents afterwards. `None` when the record carries no
    /// digest.
    pub digest_after: Option<String>,
    /// What it held before, in tokens. `None` when the record carries only the
    /// delta.
    pub tokens_before: Option<usize>,
    /// What it held afterwards. `None` on the same records.
    pub tokens_after: Option<usize>,
    /// How its token count moved; negative when it shrank.
    pub token_delta: i64,
    /// How many entries it held before. `None` when the record carries only the
    /// counts the change moved.
    pub entries_before: Option<usize>,
    /// How many it held afterwards. `None` on the same records.
    pub entries_after: Option<usize>,
    /// How many entries the change itself pushed.
    pub entries_added: usize,
    /// How many left, the eviction the change triggered included.
    pub entries_removed: usize,
}

impl From<RegionCommit> for RegionTransition {
    fn from(commit: RegionCommit) -> Self {
        Self {
            region: commit.region,
            digest_before: Some(commit.digest_before),
            digest_after: Some(commit.digest_after),
            tokens_before: Some(commit.tokens_before),
            tokens_after: Some(commit.tokens_after),
            token_delta: commit.tokens_after as i64 - commit.tokens_before as i64,
            entries_before: Some(commit.entries_before),
            entries_after: Some(commit.entries_after),
            entries_added: commit.entries_added,
            // What the region lost: everything it held plus everything the
            // change pushed, less what it ended with. Saturating, so a record
            // whose arithmetic does not close reports no removal rather than an
            // enormous one.
            entries_removed: (commit.entries_before + commit.entries_added)
                .saturating_sub(commit.entries_after),
        }
    }
}

/// One committed change to a run's context window, as folded out of the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextChangeRecord {
    /// What made the change.
    pub cause: ContextCause,
    /// The window's revision before the transaction. `None` when the record
    /// names no revision.
    pub revision_before: Option<String>,
    /// The window's revision after it. `None` on the same records.
    pub revision_after: Option<String>,
    /// The tool execution that committed it, where the runtime knew one. `None`
    /// where nothing recorded an execution.
    pub execution_id: Option<String>,
    /// Every region the transaction touched.
    pub regions: Vec<RegionTransition>,
    /// Unix seconds when it committed.
    pub at: i64,
}

/// One change with the position of the record that carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedChange {
    /// Where the record sits in the journal, as a byte offset. It only climbs
    /// within a run and never changes.
    pub position: u64,
    /// The change itself.
    pub record: ContextChangeRecord,
}

/// The folded form of one journal record, for the two records that carry a
/// change. Anything else answers `None`.
pub(super) fn change_of(record: &RunRecord) -> Option<ContextChangeRecord> {
    match record {
        RunRecord::ContextTransaction {
            revision_before,
            revision_after,
            cause,
            regions,
            execution_id,
            at,
        } => Some(ContextChangeRecord {
            cause: *cause,
            revision_before: Some(revision_before.clone()),
            revision_after: Some(revision_after.clone()),
            execution_id: Some(execution_id.clone()).filter(|id| !id.is_empty()),
            regions: regions
                .iter()
                .cloned()
                .map(RegionTransition::from)
                .collect(),
            at: *at,
        }),
        // A change recorded one region at a time. It named neither the window it
        // moved nor what the region held, so a reader gets the counts it does
        // carry and nothing invented around them.
        RunRecord::ContextChange {
            region,
            cause,
            entries_added,
            entries_removed,
            token_delta,
            at,
        } => Some(ContextChangeRecord {
            cause: *cause,
            revision_before: None,
            revision_after: None,
            execution_id: None,
            regions: vec![RegionTransition {
                region: region.clone(),
                digest_before: None,
                digest_after: None,
                tokens_before: None,
                tokens_after: None,
                token_delta: *token_delta,
                entries_before: None,
                entries_after: None,
                entries_added: *entries_added,
                entries_removed: *entries_removed,
            }],
            at: *at,
        }),
        _ => None,
    }
}

/// Read every context change an archive records, in the order they committed,
/// each with the position of its record.
///
/// Streams the file one frame at a time, the way the executions reader does: a
/// long run's journal is walked holding one record rather than the whole parsed
/// journal. A torn tail ends the walk with the changes read so far, which is
/// what a live run's journal looks like while the lane is mid-append.
pub fn read_archive_changes(r: &mut dyn Read) -> io::Result<Vec<IndexedChange>> {
    let (_, mut frames) = Frames::open(r)?;
    let mut changes = Vec::new();
    while let Ok(Some((position, frame))) = frames.next_frame() {
        let Frame::Record(record) = frame else {
            continue;
        };
        if let Some(record) = change_of(&record) {
            changes.push(IndexedChange { position, record });
        }
    }
    Ok(changes)
}

#[cfg(test)]
#[path = "transaction_tests.rs"]
mod tests;
