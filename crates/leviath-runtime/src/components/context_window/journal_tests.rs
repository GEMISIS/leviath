//! Tests for [`super`].
//!
//! A sibling file rather than an inline `mod tests`, which is what this repo
//! does with a test module whose own arms cannot all be driven: the
//! `unreachable!` guarding "the window sends nothing else" is one the lane
//! cannot produce, and llvm-cov excludes this layout by default
//! (see CONTRIBUTING, "Where a test module lives").

use super::*;
use leviath_core::{Region, RegionKind};

fn shape(entries: usize, tokens: usize) -> RegionShape {
    RegionShape { entries, tokens }
}

/// A plain append: one entry in, nothing out, tokens up.
#[test]
fn an_append_reports_one_added_and_nothing_removed() {
    let moved = RegionMove::between(shape(2, 20), shape(3, 35), 1);
    assert_eq!(moved.added, 1);
    assert_eq!(moved.removed, 0);
    assert_eq!(moved.token_delta, 15);
    assert!(!moved.is_still());
}

/// The case the arithmetic exists for: the write appended, and the region
/// evicted to make room. Three entries left, not one.
#[test]
fn an_append_that_evicted_reports_what_left() {
    let moved = RegionMove::between(shape(5, 90), shape(3, 40), 1);
    assert_eq!(moved.added, 1);
    assert_eq!(moved.removed, 3);
    assert_eq!(moved.token_delta, -50);
}

/// A replacement measured from before its own clear: everything that was
/// there left, and the one new entry arrived.
#[test]
fn a_replacement_reports_the_clear_it_did_first() {
    let moved = RegionMove::between(shape(4, 60), shape(1, 12), 1);
    assert_eq!(moved.added, 1);
    assert_eq!(moved.removed, 4);
    assert_eq!(moved.token_delta, -48);
}

/// A write nothing accepted moves nothing, and a journal of those is noise.
#[test]
fn a_refused_write_is_still() {
    assert!(RegionMove::between(shape(2, 20), shape(2, 20), 0).is_still());
}

/// Growing by more than the write pushed cannot happen today, and if it ever
/// did the record must not claim entries were removed to balance it.
#[test]
fn unaccountable_growth_reports_no_removals() {
    let moved = RegionMove::between(shape(1, 10), shape(4, 40), 1);
    assert_eq!(moved.removed, 0);
    assert_eq!(moved.added, 1);
    assert_eq!(moved.token_delta, 30);
}

fn window_with_region() -> ContextWindow {
    let mut window = ContextWindow::new(10_000);
    window.add_region(Region::new("plan".to_string(), RegionKind::Pinned, 1_000));
    window
}

/// A region's shape is read off the region itself, and a name the window
/// does not carry reads as empty rather than panicking.
#[test]
fn a_shape_is_the_regions_own_counts() {
    let mut window = window_with_region();
    assert_eq!(window.region_shape("plan"), shape(0, 0));
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");
    assert_eq!(window.region_shape("plan"), shape(1, 4));
    assert_eq!(window.region_shape("nowhere"), shape(0, 0));
}

/// The whole point of the handle: with one, a write lands in the run's
/// archive as a record naming its cause.
#[test]
fn an_attached_window_records_what_moved_and_why() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_region();
    // The stage is held for the whole test, as the world holds it for the whole
    // run: a window's handle on the lane is weak and writes nothing once the
    // lane's owner has let go.
    let stage = crate::pipeline::PersistenceStage(tx);
    window.attach_journal("run-c", Some(&stage));
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");

    let PersistMsg::Append { run_id, record, .. } = rx.try_recv().expect("one write, one record")
    else {
        unreachable!("the window sends nothing else")
    };
    assert_eq!(run_id, "run-c");
    let mut value = serde_json::to_value(&*record).expect("a record serializes");
    let fields = value["ContextChange"]
        .as_object_mut()
        .expect("the new variant");
    assert!(fields.remove("at").is_some(), "a change is stamped");
    assert_eq!(
        value,
        serde_json::json!({
            "ContextChange": {
                "region": "plan",
                "cause": "seed",
                "entries_added": 1,
                "entries_removed": 0,
                "token_delta": 4,
            }
        })
    );
}

/// A window with no journal is the common case in tests and in `lev test`,
/// and it has to be a no-op rather than a panic.
#[test]
fn a_detached_window_records_nothing() {
    let mut window = window_with_region();
    window.attach_journal("run-c", None);
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");
    assert!(window.journal.is_none());
}

/// A write the window could not place records nothing: the region is
/// untouched, and saying otherwise would put a change in the history that
/// never happened.
#[test]
fn a_write_that_moved_nothing_records_nothing() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_region();
    // The stage is held for the whole test, as the world holds it for the whole
    // run: a window's handle on the lane is weak and writes nothing once the
    // lane's owner has let go.
    let stage = crate::pipeline::PersistenceStage(tx);
    window.attach_journal("run-c", Some(&stage));
    window
        .add_to_region_caused(ContextCause::Seed, "nowhere", "lost".to_string(), 4)
        .expect_err("no such region");
    assert!(rx.try_recv().is_err(), "nothing moved, nothing recorded");

    // And a change that reached the journal with nothing to report is dropped
    // there too. A region hook may accept a write and store it unchanged, so
    // "the write succeeded" and "the region moved" are different facts, and a
    // record for the first would say a region changed when it did not.
    let before = window.region_shape("plan");
    window.journal_change(ContextCause::Hook, "plan", before, 0);
    assert!(
        rx.try_recv().is_err(),
        "a region that stood still is not a change"
    );
}

/// The invariant the weak handle exists for: a window that has been given a
/// journal must not keep the persistence lane open.
///
/// A clean shutdown closes the lane by dropping the world's sender and waiting
/// for the worker to finish draining what is queued. A window holding a live
/// sender keeps that wait going for ever, one per agent, so `lev daemon stop`
/// never returns and neither does any test that stands up a host.
#[tokio::test]
async fn an_attached_window_does_not_hold_the_lane_open() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_region();
    window.attach_journal(
        "run-c",
        Some(&crate::pipeline::PersistenceStage(tx.clone())),
    );

    // What a clean shutdown does: drop the world's own sender. The window's
    // handle is all that is left, and it must not count.
    drop(tx);
    assert!(rx.recv().await.is_none(), "the lane closed");

    // A write down a closed lane records nothing and is otherwise a normal write.
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "late".to_string(), 4)
        .expect("the write fits");
    assert_eq!(window.region_shape("plan").entries(), 1);
}
