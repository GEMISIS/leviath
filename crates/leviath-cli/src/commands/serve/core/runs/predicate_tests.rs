//! Tests for the run tree a filter walks, and for the defaults a predicate
//! that reads nothing inherits.

use std::sync::Arc;

use super::{MatchContext, RunPredicate, RunTree, Verdict};
use crate::runstate::RunMeta;

/// A predicate that answers from the record alone, the way every listing with
/// a flat filter does: it implements `matches` and nothing else.
#[derive(Debug)]
struct ByName(&'static str);

impl RunPredicate for ByName {
    fn matches(&self, meta: &Arc<RunMeta>, _ctx: &MatchContext) -> bool {
        meta.agent_name == self.0
    }

    fn digest_part(&self) -> String {
        format!("name:{}", self.0)
    }
}

fn context() -> MatchContext {
    MatchContext::at(1_700_000_000)
}

fn meta(agent: &str) -> Arc<RunMeta> {
    Arc::new(RunMeta::new(
        "run-a".to_string(),
        agent.to_string(),
        "/agents/agent".to_string(),
        "task".to_string(),
        None,
        "/work".to_string(),
        1,
    ))
}

/// One run, with the run that started it.
fn child(id: &str, parent: Option<&str>) -> Arc<RunMeta> {
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

/// The default verdict is the file-free answer, so a predicate that reads
/// nothing is written once rather than twice.
#[test]
fn a_predicate_that_reads_nothing_answers_the_verdict_from_matches() {
    let predicate = ByName("builder");
    assert_eq!(
        predicate.verdict(&meta("builder"), &context()),
        Verdict::Keep
    );
    assert_eq!(
        predicate.verdict(&meta("someone-else"), &context()),
        Verdict::Drop
    );
}

/// Nothing calls it, because nothing says `NeedsIo`; it answers no rather than
/// guessing a run in.
#[tokio::test]
async fn the_default_confirmation_keeps_nothing() {
    let predicate = ByName("builder");
    assert!(!predicate.confirm(&meta("builder"), &context()).await);
}

/// A tree reports the chain above a run root first, so it reads as a
/// breadcrumb, and hands back the records on it.
#[test]
fn a_tree_walks_from_a_run_up_to_its_root() {
    let tree = RunTree::of(&[
        child("root", None),
        child("worker", Some("root")),
        child("grandchild", Some("worker")),
    ]);
    assert_eq!(tree.ancestors("grandchild"), ["root", "worker"]);
    assert_eq!(tree.ancestors("worker"), ["root"]);
    assert!(tree.ancestors("root").is_empty());
    assert!(tree.ancestors("nobody").is_empty());
    assert_eq!(
        tree.run("worker").map(|meta| meta.run_id.clone()),
        Some("worker".to_string())
    );
    assert!(tree.run("nobody").is_none());
}

/// A parent this tree does not hold is still named, because the run says so;
/// the chain simply stops there.
#[test]
fn a_parent_outside_the_tree_ends_the_chain() {
    let tree = RunTree::of(&[child("worker", Some("pruned"))]);
    assert_eq!(tree.ancestors("worker"), ["pruned"]);
}

/// A tree with nothing in it answers about nothing, which is what a listing
/// whose filter never asks gets.
#[test]
fn an_empty_tree_knows_nothing() {
    let tree = RunTree::default();
    assert!(tree.ancestors("anything").is_empty());
    assert!(tree.run("anything").is_none());
}

/// Records that name each other as parents stop the walk rather than spinning
/// it: a chain cannot be longer than the store.
#[test]
fn a_cycle_in_the_records_does_not_spin_the_walk() {
    let tree = RunTree::of(&[child("a", Some("b")), child("b", Some("a"))]);
    assert_eq!(tree.ancestors("a"), ["a", "b"]);
}
