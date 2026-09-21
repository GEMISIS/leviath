//! Tests for the doctor's journal check.

use super::*;
use crate::commands::doctor::CheckStatus;
use leviath_runtime::persist_stats::{JournalError, JournalHealth};

/// A daemon that has written `written` records and lost nothing.
fn well(written: u64) -> DaemonHealth {
    DaemonHealth {
        journal: JournalHealth {
            appends_attempted: written,
            ..JournalHealth::default()
        },
        ..DaemonHealth::default()
    }
}

/// A daemon that cannot write one run's journal.
fn failing() -> DaemonHealth {
    DaemonHealth {
        journal: JournalHealth {
            appends_attempted: 9,
            appends_failed: 2,
            last_error: Some(JournalError {
                run_id: "run-1".to_string(),
                path: "/runs/run-1/run.lvr".to_string(),
                message: "Permission denied".to_string(),
                at: 1_700_000_000,
            }),
            ..JournalHealth::default()
        },
        ..DaemonHealth::default()
    }
}

/// With no daemon asked there is nothing to report, and the doctor says nothing
/// rather than guessing.
#[test]
fn no_daemon_means_no_check() {
    assert!(journal_check(None).is_none());
}

/// A daemon that has written everything it tried passes, and says how much.
#[test]
fn a_daemon_that_has_lost_nothing_passes() {
    let check = journal_check(Some(&well(42))).expect("a daemon was asked");
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(check.detail.contains("42 record(s)"), "{}", check.detail);
}

/// The failure is the whole point: it names the file, the run, the error, and
/// what to look at.
#[test]
fn a_daemon_that_has_lost_a_record_fails() {
    let check = journal_check(Some(&failing())).expect("a daemon was asked");
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(
        check.detail.contains("/runs/run-1/run.lvr"),
        "{}",
        check.detail
    );
    assert!(check.detail.contains("run-1"), "{}", check.detail);
    assert!(
        check.detail.contains("Permission denied"),
        "{}",
        check.detail
    );
    assert!(check.detail.contains("read-only mount"), "{}", check.detail);
}

/// A reply that is not a listing, and an unreachable daemon, both leave the
/// health unknown rather than looking well.
#[test]
fn only_a_listing_reports_health() {
    assert!(
        reported_health(Ok(ControlResponse::Ok { ok: true })).is_none(),
        "another reply says nothing about the journal"
    );
    assert!(
        reported_health(Err(std::io::Error::other("no daemon"))).is_none(),
        "an unreachable daemon says nothing either"
    );
    let listing = ControlResponse::List {
        runs: vec![],
        finished: vec![],
        health: Box::new(failing()),
    };
    let health = reported_health(Ok(listing)).expect("a listing carries it");
    assert_eq!(health.journal.appends_failed, 2);
}
