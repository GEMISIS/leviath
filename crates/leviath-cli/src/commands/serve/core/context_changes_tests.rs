//! Tests for reading why a run's regions changed back out of its journal.

use leviath_core::ContextCause;
use leviath_core::run_archive::{self, RunIdentity, RunRecord};

use super::read;
use crate::runstate::{RunMeta, create_run};

/// A run to hang a journal off.
fn meta(run_id: &str) -> RunMeta {
    RunMeta::new(
        run_id.to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "move a few regions".to_string(),
        None,
        "/tmp".to_string(),
        1,
    )
}

/// One region change record.
fn changed(region: &str, cause: ContextCause, added: usize, removed: usize, at: i64) -> RunRecord {
    RunRecord::ContextChange {
        region: region.to_string(),
        cause,
        entries_added: added,
        entries_removed: removed,
        token_delta: 40,
        at,
    }
}

/// Write a journal of `records` for the run.
fn write_journal(run_id: &str, records: Vec<RunRecord>) {
    let meta = meta(run_id);
    let mut buf = Vec::new();
    run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION)
        .expect("a preamble");
    run_archive::write_record(
        &mut buf,
        &RunRecord::Header {
            identity: RunIdentity {
                run_id: meta.run_id.clone(),
                machine_id: "m".to_string(),
                world_id: "w".to_string(),
                created_at: 0,
            },
            meta: Box::new(meta.clone()),
        },
    )
    .expect("a header");
    for record in &records {
        run_archive::write_record(&mut buf, record).expect("a record");
    }
    std::fs::write(
        crate::runstate::run_dir(run_id).join(leviath_core::files::ARCHIVE_FILE),
        &buf,
    )
    .expect("the journal");
}

/// Every change comes back in the order it landed, carrying the cause the
/// journal recorded.
#[test]
fn the_changes_read_back_in_recorded_order() {
    crate::runstate::with_isolated_runs_dir("context-changes-order", |_dir| {
        create_run(&meta("did-change")).expect("run written");
        write_journal(
            "did-change",
            vec![
                changed("plan", ContextCause::Seed, 1, 0, 20),
                changed("conversation", ContextCause::ToolResult, 2, 0, 25),
                changed("plan", ContextCause::Compaction, 0, 3, 30),
            ],
        );

        let changes = read("did-change").expect("the journal reads");
        assert_eq!(changes.len(), 3);
        let first = &changes[0].record;
        assert_eq!(first.regions[0].region, "plan");
        assert_eq!(first.cause, ContextCause::Seed);
        assert_eq!(changes[1].record.cause, ContextCause::ToolResult);
        let last = &changes[2].record;
        assert_eq!(last.regions[0].entries_removed, 3);
        assert_eq!(last.at, 30);
        // Positions climb with the journal, which is what names a change for as
        // long as the run exists.
        assert!(changes[0].position < changes[2].position);
    });
}

/// A run whose writes named no cause has no changes, and says so with an
/// empty list rather than an error.
#[test]
fn a_run_with_no_recorded_causes_has_no_changes() {
    crate::runstate::with_isolated_runs_dir("context-changes-empty", |_dir| {
        create_run(&meta("quiet-run")).expect("run written");
        assert!(
            read("quiet-run")
                .expect("no journal is not a failure")
                .is_empty()
        );
    });
}

/// A journal that cannot be read is reported rather than read as a run whose
/// regions never moved.
#[test]
fn an_unreadable_journal_is_an_error() {
    crate::runstate::with_isolated_runs_dir("context-changes-corrupt", |_dir| {
        let dir = crate::runstate::run_dir("broken");
        std::fs::create_dir_all(&dir).expect("a run dir");
        std::fs::write(
            dir.join(leviath_core::files::ARCHIVE_FILE),
            b"not an archive",
        )
        .expect("a corrupt journal");
        let failed = read("broken").expect_err("an unreadable journal");
        assert_eq!(failed.code(), "INTERNAL");
        assert!(
            failed.to_string().contains("unreadable journal"),
            "{failed}"
        );
    });
}

/// One transaction that touched two regions reads back as one change naming
/// both, with the window it started from and the window it produced.
///
/// This is the shape that made the record a transaction: recorded region by
/// region, a compaction reads as two events that happen to share a second.
#[test]
fn a_transaction_reads_back_with_every_region_it_touched() {
    crate::runstate::with_isolated_runs_dir("context-changes-txn", |_dir| {
        create_run(&meta("compacted")).expect("run written");
        write_journal(
            "compacted",
            vec![RunRecord::ContextTransaction {
                revision_before: "cw1-before".to_string(),
                revision_after: "cw1-after".to_string(),
                cause: ContextCause::Compaction,
                regions: vec![
                    run_archive::RegionCommit {
                        region: "plan".to_string(),
                        digest_before: "rg1-full".to_string(),
                        digest_after: "rg1-empty".to_string(),
                        tokens_before: 400,
                        tokens_after: 0,
                        entries_before: 4,
                        entries_after: 0,
                        entries_added: 0,
                    },
                    run_archive::RegionCommit {
                        region: "plan_history".to_string(),
                        digest_before: "rg1-empty".to_string(),
                        digest_after: "rg1-summary".to_string(),
                        tokens_before: 0,
                        tokens_after: 30,
                        entries_before: 0,
                        entries_after: 1,
                        entries_added: 1,
                    },
                ],
                execution_id: String::new(),
                at: 90,
            }],
        );

        let changes = read("compacted").expect("the journal reads");
        assert_eq!(changes.len(), 1, "one transaction, one change");
        let record = &changes[0].record;
        assert_eq!(record.revision_before.as_deref(), Some("cw1-before"));
        assert_eq!(record.revision_after.as_deref(), Some("cw1-after"));
        assert_eq!(record.regions.len(), 2);
        assert_eq!(record.regions[0].token_delta, -400);
        assert_eq!(record.regions[0].entries_removed, 4);
        assert_eq!(record.regions[1].region, "plan_history");
        assert_eq!(record.regions[1].token_delta, 30);
    });
}

/// The changes one execution committed, and nothing else's.
#[test]
fn the_changes_one_execution_committed_are_its_own() {
    crate::runstate::with_isolated_runs_dir("context-changes-by-exec", |_dir| {
        create_run(&meta("attributed")).expect("run written");
        let committed = |execution_id: &str, at: i64| RunRecord::ContextTransaction {
            revision_before: format!("cw1-{at}"),
            revision_after: format!("cw1-{}", at + 1),
            cause: ContextCause::ContextTool,
            regions: vec![run_archive::RegionCommit {
                region: "plan".to_string(),
                digest_before: "rg1-a".to_string(),
                digest_after: "rg1-b".to_string(),
                tokens_before: 0,
                tokens_after: 10,
                entries_before: 0,
                entries_after: 1,
                entries_added: 1,
            }],
            execution_id: execution_id.to_string(),
            at,
        };
        write_journal(
            "attributed",
            vec![
                committed("x-one", 10),
                committed("x-two", 11),
                committed("", 12),
                committed("x-one", 13),
            ],
        );

        let mine = super::by_execution("attributed", "x-one").expect("the journal reads");
        let times: Vec<i64> = mine.iter().map(|held| held.record.at).collect();
        assert_eq!(times, vec![10, 13]);

        // An id nothing recorded matches nothing, and an empty one does not
        // collect every change that named no execution.
        assert!(
            super::by_execution("attributed", "x-nine")
                .expect("reads")
                .is_empty()
        );
        assert!(
            super::by_execution("attributed", "")
                .expect("reads")
                .is_empty()
        );
    });
}
