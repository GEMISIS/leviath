//! Tests for the two run fields a filter reads off the tree rather than the
//! record.

use std::sync::Arc;

use super::super::super::super::core::runs::predicate::RunTree;
use super::super::super::types::run::Run;
use super::{Ancestry, MatchCx, ancestor_ids, parent_of};
use crate::runstate::RunMeta;

/// One run, with the run that started it.
fn meta(id: &str, parent: Option<&str>) -> Arc<RunMeta> {
    let mut record = RunMeta::new(
        id.to_string(),
        "agent".to_string(),
        "/agents/agent".to_string(),
        "task".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    record.parent_run_id = parent.map(str::to_string);
    Arc::new(record)
}

/// A three-deep tree, and the run at the bottom of it.
fn tree() -> RunTree {
    RunTree::of(&[
        meta("root", None),
        meta("worker", Some("root")),
        meta("grandchild", Some("worker")),
    ])
}

/// The run object a filter is handed for one record.
fn run(record: Arc<RunMeta>) -> Run {
    Run {
        meta: record,
        now: 1_700_000_000,
    }
}

/// The tree the mirror reads is the listing's own, reached through the trait
/// the filter system asks with.
#[test]
fn the_run_tree_answers_the_filter_system() {
    let tree = tree();
    assert_eq!(Ancestry::ancestors(&tree, "grandchild"), ["root", "worker"]);
    assert_eq!(
        Ancestry::run(&tree, "worker").map(|meta| meta.run_id.clone()),
        Some("worker".to_string())
    );
}

/// The parent a filter reaches is the same run the field answers with, and
/// it carries the listing's clock rather than one of its own.
#[test]
fn a_filter_reaches_the_run_that_started_this_one() {
    let tree = tree();
    let cx = MatchCx::at(1_700_000_000).with_parents(&tree);
    let found = parent_of(&run(meta("grandchild", Some("worker"))), &cx).expect("a parent");
    assert_eq!(found.meta.run_id, "worker");
    assert_eq!(found.now, 1_700_000_000);
}

/// A run nobody started has no parent to reach, and neither has one whose
/// parent this listing never read.
#[test]
fn a_parent_that_is_not_there_is_null() {
    let tree = tree();
    let cx = MatchCx::at(1).with_parents(&tree);
    assert!(parent_of(&run(meta("root", None)), &cx).is_none());
    assert!(parent_of(&run(meta("orphan", Some("pruned"))), &cx).is_none());
}

/// The breadcrumb reads root first, and a listing with no tree behind it
/// reports none.
#[test]
fn the_ancestor_ids_read_root_first() {
    let tree = tree();
    let cx = MatchCx::at(1).with_parents(&tree);
    assert_eq!(
        ancestor_ids(&run(meta("grandchild", Some("worker"))), &cx),
        ["root", "worker"]
    );
    assert!(ancestor_ids(&run(meta("root", None)), &cx).is_empty());
    assert!(ancestor_ids(&run(meta("grandchild", Some("worker"))), &MatchCx::at(1)).is_empty());
}
