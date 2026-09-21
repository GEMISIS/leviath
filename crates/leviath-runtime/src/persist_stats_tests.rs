//! Tests for the persistence lane's health counters.

use super::*;

/// A lane that has done nothing is well, and says nothing.
#[test]
fn a_lane_that_has_written_nothing_is_healthy() {
    let report = PersistLaneStats::new().report();
    assert_eq!(report, JournalHealth::default());
    assert!(report.is_healthy());
    assert_eq!(report.complaint(), None);
}

/// The two counters that say how much the lane has been asked to do.
#[test]
fn attempts_and_queue_depth_are_counted() {
    let stats = PersistLaneStats::new();
    stats.append_attempted();
    stats.append_attempted();
    stats.observe_queue(7);
    let report = stats.report();
    assert_eq!(report.appends_attempted, 2);
    assert_eq!(report.queue_depth, 7);
    assert!(report.is_healthy(), "attempting is not failing");
}

/// A lost journal record is counted, described, and named as a run to fail.
#[test]
fn a_lost_journal_record_names_its_run() {
    let stats = PersistLaneStats::new();
    stats.journal_append_failed(
        "run-1",
        Path::new("/runs/run-1/run.lvr"),
        "Permission denied",
    );

    let report = stats.report();
    assert_eq!(report.appends_failed, 1);
    assert!(!report.is_healthy());
    let last = report.last_error.expect("the failure is the latest one");
    assert_eq!(last.run_id, "run-1");
    assert_eq!(last.path, "/runs/run-1/run.lvr");
    assert_eq!(last.message, "Permission denied");
    assert!(last.at > 0, "stamped when it happened");

    let taken = stats.take_unwritable();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].run_id, "run-1");
    assert!(
        stats.take_unwritable().is_empty(),
        "a run is reported once, not on every tick after"
    );
}

/// A run failing every append it makes costs one entry, not one per write: the
/// table is keyed by run so it cannot grow while the world is between ticks.
#[test]
fn a_run_failing_repeatedly_is_named_once() {
    let stats = PersistLaneStats::new();
    for _ in 0..5 {
        stats.journal_append_failed("run-1", Path::new("/runs/run-1/run.lvr"), "No space left");
    }
    stats.journal_append_failed("run-2", Path::new("/runs/run-2/run.lvr"), "No space left");
    assert_eq!(stats.report().appends_failed, 6, "every loss is counted");
    let mut runs: Vec<String> = stats
        .take_unwritable()
        .into_iter()
        .map(|e| e.run_id)
        .collect();
    runs.sort();
    assert_eq!(runs, vec!["run-1".to_string(), "run-2".to_string()]);
}

/// A snapshot write that lost a file is reported, and is not a reason to fail
/// the run: the next snapshot rewrites the file whole.
#[test]
fn a_lost_snapshot_file_is_counted_but_fails_no_run() {
    let stats = PersistLaneStats::new();
    stats.snapshot_failed(
        "run-1",
        Path::new("/runs/run-1/meta.json"),
        "Read-only file system",
    );
    let report = stats.report();
    assert_eq!(report.snapshots_failed, 1);
    assert_eq!(report.appends_failed, 0);
    assert!(!report.is_healthy());
    assert!(
        stats.take_unwritable().is_empty(),
        "a run is never failed for a snapshot file"
    );
}

/// What an operator reads: the counts, the file, and the run it was for.
#[test]
fn the_complaint_names_the_file_and_the_run() {
    let stats = PersistLaneStats::new();
    stats.journal_append_failed(
        "run-1",
        Path::new("/runs/run-1/run.lvr"),
        "Permission denied",
    );
    stats.snapshot_failed(
        "run-1",
        Path::new("/runs/run-1/meta.json"),
        "Permission denied",
    );
    let complaint = stats.report().complaint().expect("something is wrong");
    assert!(complaint.contains("1 journal append(s)"), "{complaint}");
    assert!(complaint.contains("1 snapshot write(s)"), "{complaint}");
    assert!(complaint.contains("/runs/run-1/meta.json"), "{complaint}");
    assert!(complaint.contains("run-1"), "{complaint}");
    assert!(complaint.contains("Permission denied"), "{complaint}");
}

/// The reading crosses the control socket, so it has to survive the trip.
#[test]
fn a_reading_round_trips_through_json() {
    let stats = PersistLaneStats::new();
    stats.append_attempted();
    stats.observe_queue(3);
    stats.journal_append_failed(
        "run-1",
        Path::new("/runs/run-1/run.lvr"),
        "Permission denied",
    );
    let report = stats.report();
    let json = serde_json::to_string(&report).expect("a reading serializes");
    let back: JournalHealth = serde_json::from_str(&json).expect("and parses");
    assert_eq!(back, report);
}
