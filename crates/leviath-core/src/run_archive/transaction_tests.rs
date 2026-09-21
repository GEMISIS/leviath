//! What a transaction record has to say, and what an older journal's change
//! records must keep saying beside it.

use super::*;
use crate::run_archive::{RUN_ARCHIVE_VERSION, RunRecord, write_archive_start, write_record};

/// One region's part in a transaction: `entries_before` entries becoming
/// `entries_after`, with `entries_added` pushed.
fn commit(region: &str, entries_before: usize, entries_after: usize, added: usize) -> RegionCommit {
    RegionCommit {
        region: region.to_string(),
        digest_before: format!("rg1-{entries_before:032x}"),
        digest_after: format!("rg1-{entries_after:032x}"),
        tokens_before: entries_before * 10,
        tokens_after: entries_after * 10,
        entries_before,
        entries_after,
        entries_added: added,
    }
}

/// An archive holding `records`.
fn archive(records: Vec<RunRecord>) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_archive_start(&mut bytes, RUN_ARCHIVE_VERSION).expect("a Vec takes the preamble");
    for record in &records {
        write_record(&mut bytes, record).expect("a Vec takes a record");
    }
    bytes
}

/// One transaction over two regions reads back as one change naming both, with
/// the window it started from and the window it produced.
#[test]
fn a_transaction_over_two_regions_is_one_change() {
    let bytes = archive(vec![RunRecord::ContextTransaction {
        revision_before: "cw1-aaaa".to_string(),
        revision_after: "cw1-bbbb".to_string(),
        cause: ContextCause::Compaction,
        regions: vec![commit("plan", 4, 0, 0), commit("plan_history", 1, 2, 1)],
        execution_id: String::new(),
        at: 90,
    }]);
    let changes = read_archive_changes(&mut bytes.as_slice()).expect("it reads");
    assert_eq!(changes.len(), 1, "one transaction, one change");
    let change = &changes[0].record;
    assert_eq!(change.cause, ContextCause::Compaction);
    assert_eq!(change.revision_before.as_deref(), Some("cw1-aaaa"));
    assert_eq!(change.revision_after.as_deref(), Some("cw1-bbbb"));
    assert_eq!(change.at, 90);
    assert_eq!(change.regions.len(), 2);

    let emptied = &change.regions[0];
    assert_eq!(emptied.region, "plan");
    assert_eq!(emptied.tokens_before, Some(40));
    assert_eq!(emptied.tokens_after, Some(0));
    assert_eq!(emptied.token_delta, -40, "it shrank");
    assert_eq!(emptied.entries_removed, 4);
    assert_eq!(emptied.entries_added, 0);

    let summarised = &change.regions[1];
    assert_eq!(summarised.region, "plan_history");
    assert_eq!(summarised.token_delta, 10);
    assert_eq!(summarised.entries_added, 1);
    assert_eq!(summarised.entries_removed, 0);
}

/// A write that appended one entry into a full region evicted one, and the
/// counts either side are what say so.
#[test]
fn an_eviction_shows_up_in_the_counts_either_side() {
    let commit = commit("conv", 8, 8, 1);
    let transition = RegionTransition::from(commit);
    assert_eq!(transition.entries_added, 1);
    assert_eq!(transition.entries_removed, 1, "one in, one out");
    assert_eq!(transition.entries_before, Some(8));
    assert_eq!(transition.entries_after, Some(8));
}

/// Arithmetic that does not close reports no removal rather than an enormous
/// one. Nothing the runtime does produces this; a record from somewhere else
/// might.
#[test]
fn counts_that_do_not_close_report_no_removal() {
    let transition = RegionTransition::from(RegionCommit {
        entries_before: 1,
        entries_after: 9,
        entries_added: 1,
        ..commit("conv", 1, 9, 1)
    });
    assert_eq!(transition.entries_removed, 0);
}

/// A change record written one region at a time still reads back, and says
/// nothing it does not know: no revision, no digest, no counts either side.
///
/// This is the shape every journal written before transactions holds, and a
/// reader that invented a revision for one would send a client looking for a
/// window that was never named.
#[test]
fn a_journal_of_single_region_changes_still_reads() {
    let bytes = archive(vec![
        RunRecord::ContextChange {
            region: "plan".to_string(),
            cause: ContextCause::Seed,
            entries_added: 1,
            entries_removed: 0,
            token_delta: 40,
            at: 20,
        },
        RunRecord::ContextChange {
            region: "plan".to_string(),
            cause: ContextCause::Compaction,
            entries_added: 0,
            entries_removed: 3,
            token_delta: -120,
            at: 30,
        },
    ]);
    let changes = read_archive_changes(&mut bytes.as_slice()).expect("it reads");
    assert_eq!(changes.len(), 2);
    let seeded = &changes[0].record;
    assert_eq!(seeded.cause, ContextCause::Seed);
    assert_eq!(seeded.revision_before, None, "it named no window");
    assert_eq!(seeded.revision_after, None);
    assert_eq!(seeded.execution_id, None);
    assert_eq!(seeded.regions.len(), 1);
    assert_eq!(seeded.regions[0].region, "plan");
    assert_eq!(seeded.regions[0].entries_added, 1);
    assert_eq!(seeded.regions[0].token_delta, 40);
    assert_eq!(seeded.regions[0].digest_before, None, "it held no digest");
    assert_eq!(seeded.regions[0].tokens_before, None);
    assert_eq!(seeded.regions[0].entries_before, None);
    assert_eq!(changes[1].record.regions[0].entries_removed, 3);
    assert_eq!(changes[1].record.regions[0].token_delta, -120);
}

/// Both shapes in one journal read as one sequence, in the order they landed.
/// A run that was resumed by a newer build has exactly this.
#[test]
fn the_two_shapes_read_as_one_sequence() {
    let bytes = archive(vec![
        RunRecord::ContextChange {
            region: "conv".to_string(),
            cause: ContextCause::Message,
            entries_added: 1,
            entries_removed: 0,
            token_delta: 5,
            at: 10,
        },
        RunRecord::ContextTransaction {
            revision_before: "cw1-one".to_string(),
            revision_after: "cw1-two".to_string(),
            cause: ContextCause::ModelReply,
            regions: vec![commit("conv", 1, 2, 1)],
            execution_id: String::new(),
            at: 20,
        },
    ]);
    let changes = read_archive_changes(&mut bytes.as_slice()).expect("it reads");
    let causes: Vec<ContextCause> = changes.iter().map(|c| c.record.cause).collect();
    assert_eq!(
        causes,
        vec![ContextCause::Message, ContextCause::ModelReply]
    );
    assert!(changes[0].record.revision_after.is_none());
    assert!(changes[1].record.revision_after.is_some());
    assert!(
        changes[0].position < changes[1].position,
        "positions climb with the journal"
    );
}

/// The execution that committed a transaction is carried where one was known,
/// and is absent rather than blank where it was not.
#[test]
fn an_execution_is_carried_where_one_was_known() {
    let bytes = archive(vec![
        RunRecord::ContextTransaction {
            revision_before: "cw1-one".to_string(),
            revision_after: "cw1-two".to_string(),
            cause: ContextCause::ContextTool,
            regions: vec![commit("plan", 0, 1, 1)],
            execution_id: "x00-01".to_string(),
            at: 10,
        },
        RunRecord::ContextTransaction {
            revision_before: "cw1-two".to_string(),
            revision_after: "cw1-three".to_string(),
            cause: ContextCause::Framework,
            regions: vec![commit("conv", 0, 1, 1)],
            execution_id: String::new(),
            at: 11,
        },
    ]);
    let changes = read_archive_changes(&mut bytes.as_slice()).expect("it reads");
    assert_eq!(changes[0].record.execution_id.as_deref(), Some("x00-01"));
    assert_eq!(changes[1].record.execution_id, None);
}

/// Records about anything else are passed over, and a torn tail keeps what came
/// before it.
#[test]
fn other_records_are_passed_over_and_a_torn_tail_keeps_what_it_has() {
    let mut bytes = archive(vec![
        RunRecord::StatusChanged {
            status: crate::run_meta::RunStatus::Running,
            at: 1,
        },
        RunRecord::ContextTransaction {
            revision_before: "cw1-one".to_string(),
            revision_after: "cw1-two".to_string(),
            cause: ContextCause::Seed,
            regions: vec![commit("plan", 0, 1, 1)],
            execution_id: String::new(),
            at: 5,
        },
    ]);
    // Half a length prefix, which is what a crash mid-append leaves.
    bytes.extend_from_slice(&[0, 0, 0]);
    let changes = read_archive_changes(&mut bytes.as_slice()).expect("it reads");
    assert_eq!(changes.len(), 1);
}

/// A file that is not an archive is refused rather than read as a run with no
/// changes.
#[test]
fn something_that_is_not_an_archive_is_refused() {
    let mut bytes = b"not an archive at all".as_slice();
    assert!(read_archive_changes(&mut bytes).is_err());
}

/// A frame this build cannot read is stepped over, and the positions of
/// everything after it stay right.
#[test]
fn an_unreadable_frame_does_not_shift_later_positions() {
    let mut bytes = archive(vec![RunRecord::ContextTransaction {
        revision_before: "cw1-one".to_string(),
        revision_after: "cw1-two".to_string(),
        cause: ContextCause::Seed,
        regions: vec![commit("plan", 0, 1, 1)],
        execution_id: String::new(),
        at: 5,
    }]);
    let payload = br#"{"FromALaterBuild":{"whatever":1}}"#;
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(payload);
    let after = RunRecord::ContextChange {
        region: "conv".to_string(),
        cause: ContextCause::Message,
        entries_added: 1,
        entries_removed: 0,
        token_delta: 2,
        at: 9,
    };
    write_record(&mut bytes, &after).expect("a Vec takes a record");
    let changes = read_archive_changes(&mut bytes.as_slice()).expect("it reads");
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[1].record.at, 9);
}
