//! What a reader computes from a committed region change.

use super::*;

fn commit(entries_before: usize, entries_after: usize, added: usize) -> RegionCommit {
    RegionCommit {
        region: "conversation".to_string(),
        digest_before: "a".to_string(),
        digest_after: "b".to_string(),
        tokens_before: 100,
        tokens_after: 40,
        entries_before,
        entries_after,
        entries_added: added,
    }
}

/// A region that took one entry and lost two to its own eviction reads as
/// one added, two removed, and the tokens it shed.
#[test]
fn an_eviction_shows_up_in_the_counts_either_side() {
    let t = RegionTransition::from(commit(5, 4, 1));
    assert_eq!(t.entries_added, 1);
    assert_eq!(t.entries_removed, 2);
    assert_eq!(t.token_delta, -60);
    assert_eq!(t.digest_before.as_deref(), Some("a"));
    assert_eq!(t.entries_after, Some(4));
}

/// Counts that do not close report no removal rather than an enormous one.
#[test]
fn counts_that_do_not_close_report_no_removal() {
    assert_eq!(RegionTransition::from(commit(1, 9, 0)).entries_removed, 0);
}

/// An execution with no ending is in flight; one with an ending is not.
#[test]
fn an_execution_without_an_ending_is_unfinished() {
    let mut e = Execution {
        id: "x1".to_string(),
        call_id: "c1".to_string(),
        tool: "read_file".to_string(),
        arguments: "{}".to_string(),
        stage_index: 0,
        iteration: 0,
        visit_id: String::new(),
        requested_by: String::new(),
        artifacts: Vec::new(),
        dispatched_at: 1,
        position: 1,
        ended_at: None,
        result_position: None,
        outcome: None,
    };
    assert!(e.unfinished());
    e.ended_at = Some(2);
    assert!(!e.unfinished());
}
