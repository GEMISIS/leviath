//! Tests for [`super`].
//!
//! A sibling file rather than an inline `mod tests`, which is what this repo
//! does with a test module whose own arms cannot all be driven: the
//! `unreachable!` guarding "the window sends nothing else" is one the lane
//! cannot produce, and llvm-cov excludes this layout by default
//! (see CONTRIBUTING, "Where a test module lives").

use super::*;
use leviath_core::{Region, RegionKind};

/// A window with one pinned region, and nothing recorded.
fn window_with_region() -> ContextWindow {
    let mut window = ContextWindow::new(10_000);
    window.add_region(Region::new("plan".to_string(), RegionKind::Pinned, 1_000));
    window
}

/// A window with two regions, for the transactions that touch both.
fn window_with_two_regions() -> ContextWindow {
    let mut window = window_with_region();
    window.add_region(Region::new("notes".to_string(), RegionKind::Pinned, 1_000));
    window
}

/// A window recording into `tx`, with the stage the caller must hold for as
/// long as it wants writes to land.
fn attached(
    tx: tokio::sync::mpsc::UnboundedSender<PersistMsg>,
) -> crate::pipeline::PersistenceStage {
    crate::pipeline::PersistenceStage(tx)
}

/// The one transaction the lane received, as the record's own fields.
fn one_transaction(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistMsg>,
) -> (String, RunRecord) {
    let PersistMsg::Append { run_id, record, .. } = rx.try_recv().expect("one record") else {
        unreachable!("the window sends nothing else")
    };
    (run_id, *record)
}

/// The parts of a transaction record a test asserts on.
fn parts(record: RunRecord) -> (String, String, ContextCause, Vec<RegionCommit>, String) {
    match record {
        RunRecord::ContextTransaction {
            revision_before,
            revision_after,
            cause,
            regions,
            execution_id,
            ..
        } => (
            revision_before,
            revision_after,
            cause,
            regions,
            execution_id,
        ),
        other => unreachable!("the window records transactions, not {other:?}"),
    }
}

/// How many entries a push reports for a region that went from `before` to
/// `after` entries.
#[test]
fn what_each_kind_of_push_reports() {
    assert_eq!(Pushed::Nothing.into_region(4, 1), 0);
    assert_eq!(Pushed::Into(2).into_region(4, 6), 2);
    assert_eq!(Pushed::Everything.into_region(0, 7), 7);
    // A keyed write that took a new key grew the region; one that replaced a key
    // where it stood did not, and nothing the caller holds tells them apart.
    assert_eq!(Pushed::Upsert.into_region(3, 4), 1);
    assert_eq!(Pushed::Upsert.into_region(3, 3), 0);
}

/// The whole point of the handle: with one, a write lands in the run's archive
/// as a transaction naming its cause, the window either side of it, and what
/// the region it touched held either side.
#[test]
fn an_attached_window_records_the_transaction_a_write_committed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_region();
    // The stage is held for the whole test, as the world holds it for the whole
    // run: a window's handle on the lane is weak and writes nothing once the
    // lane's owner has let go.
    let stage = attached(tx);
    window.attach_journal("run-c", Some(&stage));
    let empty = window.revision_now();
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");

    let (run_id, record) = one_transaction(&mut rx);
    assert_eq!(run_id, "run-c");
    let (before, after, cause, regions, execution) = parts(record);
    assert_eq!(cause, ContextCause::Seed);
    assert_eq!(before, empty, "it started from the window that was there");
    assert_eq!(
        after,
        window.revision_now(),
        "and produced the one that is there now"
    );
    assert_ne!(before, after);
    assert!(execution.is_empty(), "no call was being handled");
    assert_eq!(regions.len(), 1);
    let plan = &regions[0];
    assert_eq!(plan.region, "plan");
    assert_eq!(plan.entries_before, 0);
    assert_eq!(plan.entries_after, 1);
    assert_eq!(plan.entries_added, 1);
    assert_eq!(plan.tokens_before, 0);
    assert_eq!(plan.tokens_after, 4);
    assert_ne!(
        plan.digest_before, plan.digest_after,
        "the region holds something it did not hold"
    );
}

/// A revision a transaction records is one a reader can find: the window's own
/// revision matches the one computed from the snapshot the lane writes.
///
/// This is the join the whole debugger rests on. Two spellings of the rule would
/// drift, and the first person to notice would be someone whose revision did not
/// resolve.
#[test]
fn a_recorded_revision_is_the_snapshots_own() {
    let mut window = window_with_region();
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");
    let snapshot = crate::persistence::build_context_snapshot(&window, "gather");
    assert_eq!(
        window.revision_now(),
        leviath_core::run_meta::revision::context_revision(&snapshot)
    );
}

/// An entry's sensitivity is part of the window's identity, and the live window
/// reads it from the region the way the snapshot writer does.
///
/// Two spellings of that rule would drift, and a run whose region tracks taint
/// would then record revisions no reader could resolve. The same content at two
/// sensitivities is also not the same window: a region that was re-classified has
/// changed, whatever its text says.
#[test]
fn an_entrys_sensitivity_is_part_of_the_windows_identity() {
    let mut window = window_with_region();
    window
        .get_region_mut("plan")
        .expect("it is there")
        .taint
        .get_or_insert_default();
    window
        .add_tainted_to_region(
            ContextCause::Seed,
            "plan",
            "the plan".to_string(),
            4,
            leviath_core::TaintLevel::Private,
        )
        .expect("the write fits");

    // The snapshot writer takes the entry's level off the region, and the live
    // window has to agree with it.
    let snapshot = crate::persistence::build_context_snapshot(&window, "gather");
    assert_eq!(
        snapshot.regions[0].entries[0].taint,
        leviath_core::TaintLevel::Private
    );
    assert_eq!(
        window.revision_now(),
        leviath_core::run_meta::revision::context_revision(&snapshot)
    );

    // And the level counts: the same text at another sensitivity is another
    // window.
    let mut public = window_with_region();
    public
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");
    assert_ne!(window.revision_now(), public.revision_now());
}

/// One transaction over two regions is one record naming both, which is what a
/// compaction and a stage edge do.
#[test]
fn a_transaction_over_two_regions_is_one_record() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_two_regions();
    let stage = attached(tx);
    window.attach_journal("run-c", Some(&stage));
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");
    rx.try_recv().expect("the seed's own transaction");

    let both = window.begin_changes(["plan", "notes"]);
    window.get_region_mut("plan").expect("it is there").clear();
    window
        .get_region_mut("notes")
        .expect("it is there")
        .add_entry("a summary".to_string(), 3)
        .expect("it fits");
    window.current_tokens = window.calculate_tokens();
    window.commit_change(ContextCause::Compaction, both, Pushed::Upsert);

    let (_, record) = one_transaction(&mut rx);
    let (_, _, cause, regions, _) = parts(record);
    assert_eq!(cause, ContextCause::Compaction);
    assert_eq!(regions.len(), 2, "both halves, in one record");
    assert_eq!(regions[0].region, "plan");
    assert_eq!(regions[0].entries_after, 0);
    assert_eq!(regions[0].entries_added, 0, "nothing arrived in it");
    assert_eq!(regions[1].region, "notes");
    assert_eq!(regions[1].entries_added, 1);
}

/// A region a transaction names but nothing wrote to is still in the record,
/// with the same digest either side. A reader can then see that a stage edge
/// looked at it and left it alone, which is a different fact from not knowing.
#[test]
fn a_region_the_transaction_left_alone_says_so() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_two_regions();
    let stage = attached(tx);
    window.attach_journal("run-c", Some(&stage));

    let both = window.begin_changes(["plan", "notes"]);
    window
        .get_region_mut("plan")
        .expect("it is there")
        .add_entry("only here".to_string(), 2)
        .expect("it fits");
    window.current_tokens = window.calculate_tokens();
    window.commit_change(ContextCause::Transform, both, Pushed::Upsert);

    let (_, record) = one_transaction(&mut rx);
    let (_, _, _, regions, _) = parts(record);
    assert_eq!(regions[1].region, "notes");
    assert_eq!(regions[1].digest_before, regions[1].digest_after);
    assert_eq!(regions[1].entries_added, 0);
    assert_eq!(regions[1].tokens_before, regions[1].tokens_after);
}

/// A region the window does not carry measures as an empty one, which is also
/// what a write to it would find.
#[test]
fn a_region_that_is_not_there_measures_as_empty() {
    let window = window_with_region();
    let plan = window.region_before("plan");
    let nowhere = window.region_before("nowhere");
    assert_eq!(nowhere.entries, 0);
    assert_eq!(nowhere.tokens, 0);
    assert_eq!(
        nowhere.digest, plan.digest,
        "an empty region and an absent one hold the same nothing"
    );
    assert_eq!(nowhere.name, "nowhere");
}

/// The execution being handled is carried on the transactions committed while it
/// is, and nothing is carried once it is cleared.
#[test]
fn a_transaction_carries_the_execution_being_handled() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_region();
    let stage = attached(tx);
    window.attach_journal("run-c", Some(&stage));

    window.attribute_to("x0123-0001");
    window
        .add_to_region_caused(ContextCause::ContextTool, "plan", "written".to_string(), 3)
        .expect("the write fits");
    let (_, record) = one_transaction(&mut rx);
    let (_, _, _, _, execution) = parts(record);
    assert_eq!(execution, "x0123-0001");

    window.attribute_to("");
    window
        .add_to_region_caused(ContextCause::Framework, "plan", "a nudge".to_string(), 2)
        .expect("the write fits");
    let (_, record) = one_transaction(&mut rx);
    let (_, _, _, _, execution) = parts(record);
    assert!(execution.is_empty(), "no call is being handled any more");
}

/// Attributing on a window with no journal is a no-op rather than a panic: the
/// dispatcher does it for every call, and most worlds keep no history.
#[test]
fn attributing_a_detached_window_changes_nothing() {
    let mut window = window_with_region();
    window.attach_journal("run-c", None);
    window.attribute_to("x0123-0001");
    assert!(window.journal.is_none());
}

/// A window with no journal is the common case in tests and in `lev test`, and
/// it has to be a no-op rather than a panic - including the measuring, which is
/// skipped entirely so a run that keeps no history pays nothing for one.
#[test]
fn a_detached_window_records_nothing_and_measures_nothing() {
    let mut window = window_with_region();
    window.attach_journal("run-c", None);
    let txn = window.begin_change("plan");
    assert!(txn.opened.is_none(), "nothing to record, nothing measured");
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "the plan".to_string(), 4)
        .expect("the write fits");
    assert!(window.journal.is_none());
    // And committing one is still a no-op.
    window.commit_change(ContextCause::Seed, txn, Pushed::Into(1));
}

/// A write the window could not place records nothing: the region is untouched,
/// and saying otherwise would put a change in the history that never happened.
#[test]
fn a_write_that_moved_nothing_records_nothing() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut window = window_with_region();
    let stage = attached(tx);
    window.attach_journal("run-c", Some(&stage));
    window
        .add_to_region_caused(ContextCause::Seed, "nowhere", "lost".to_string(), 4)
        .expect_err("no such region");
    assert!(rx.try_recv().is_err(), "nothing moved, nothing recorded");

    // And a transaction that reached the journal with nothing to report is
    // dropped there too. A region hook may accept a write and store it
    // unchanged, so "the write succeeded" and "the window moved" are different
    // facts, and a record for the first would say a region changed when it did
    // not. The window's revision either side is what answers it.
    let txn = window.begin_change("plan");
    window.commit_change(ContextCause::Hook, txn, Pushed::Nothing);
    assert!(
        rx.try_recv().is_err(),
        "a window that stood still is not a change"
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
    window.attach_journal("run-c", Some(&attached(tx.clone())));

    // What a clean shutdown does: drop the world's own sender. The window's
    // handle is all that is left, and it must not count.
    drop(tx);
    assert!(rx.recv().await.is_none(), "the lane closed");

    // A write down a closed lane records nothing and is otherwise a normal write.
    window
        .add_to_region_caused(ContextCause::Seed, "plan", "late".to_string(), 4)
        .expect("the write fits");
    assert_eq!(window.region_before("plan").entries, 1);
}
