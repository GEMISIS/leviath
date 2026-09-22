//! What a filter knows besides the value in front of it.

use std::sync::Arc;

use super::{Ancestry, MatchCx, Unattached};
use crate::runstate::RunMeta;

/// An ancestry that answers from a fixed chain.
struct Known;

/// One run's record, for the lookup half of the trait.
fn record(id: &str) -> Arc<RunMeta> {
    Arc::new(RunMeta::new(
        id.to_string(),
        "agent".to_string(),
        "/agents/agent".to_string(),
        "task".to_string(),
        None,
        "/work".to_string(),
        1,
    ))
}

impl Ancestry for Known {
    fn ancestors(&self, id: &str) -> Vec<String> {
        match id {
            "child" => vec!["root".to_owned(), "parent".to_owned()],
            _ => Vec::new(),
        }
    }

    fn run(&self, id: &str) -> Option<Arc<RunMeta>> {
        (id == "parent").then(|| record(id))
    }
}

/// A context with nothing behind it still answers about ancestry.
#[test]
fn a_listing_with_no_run_tree_knows_no_ancestors() {
    let cx = MatchCx::at(17);
    assert_eq!(cx.now, 17);
    assert!(cx.parents.ancestors("child").is_empty());
    assert!(cx.parents.run("child").is_none());
    assert!(Unattached.ancestors("anything").is_empty());
    assert!(Unattached.run("anything").is_none());
}

/// A context lent an ancestry walks it, and reads the runs on it.
#[test]
fn an_ancestry_is_lent_to_every_test_in_the_request() {
    let known = Known;
    let cx = MatchCx::at(17).with_parents(&known);
    assert_eq!(cx.now, 17, "lending an ancestry keeps the clock");
    assert_eq!(cx.parents.ancestors("child"), ["root", "parent"]);
    assert!(cx.parents.ancestors("root").is_empty());
    assert_eq!(
        cx.parents.run("parent").map(|meta| meta.run_id.clone()),
        Some("parent".to_string())
    );
    assert!(cx.parents.run("root").is_none());
}

/// The context prints as what it is, which is one clock.
#[test]
fn a_context_prints_its_clock() {
    assert_eq!(format!("{:?}", MatchCx::at(5)), "MatchCx { now: 5, .. }");
}
