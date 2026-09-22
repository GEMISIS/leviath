//! Tests for reading a run's interactions back out of its journal.

use leviath_core::interaction::{ApprovalScope, InteractionKind, Settlement};
use leviath_core::run_archive::{self, RunIdentity, RunRecord};

use super::read;
use crate::runstate::{RunMeta, create_run};

/// A run to hang a journal off.
fn meta(run_id: &str) -> RunMeta {
    RunMeta::new(
        run_id.to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "ask a few things".to_string(),
        None,
        "/tmp".to_string(),
        1,
    )
}

/// One settled interaction record.
fn asked(request_id: &str, prompt: &str, settlement: Settlement) -> RunRecord {
    RunRecord::Interaction {
        request_id: request_id.to_string(),
        kind: InteractionKind::Confirm,
        tool: None,
        prompt: prompt.to_string(),
        stage: "plan".to_string(),
        settlement,
        asked_at: 100,
        at: 101,
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

/// Every question a run asked comes back in the order it asked them, each
/// carrying the settlement the journal recorded.
#[test]
fn the_interactions_read_back_in_asked_order() {
    crate::runstate::with_isolated_runs_dir("interactions-order", |_dir| {
        create_run(&meta("did-ask")).expect("run written");
        write_journal(
            "did-ask",
            vec![
                asked(
                    "r1",
                    "proceed?",
                    Settlement::Answered {
                        approved: None,
                        scope: None,
                        choice: None,
                        text: Some("yes".to_string()),
                        feedback: None,
                    },
                ),
                asked("r2", "allow it?", Settlement::TimedOut),
                asked(
                    "r3",
                    "allow the run for good?",
                    Settlement::Answered {
                        approved: Some(true),
                        scope: Some(ApprovalScope::Run),
                        choice: None,
                        text: None,
                        feedback: None,
                    },
                ),
            ],
        );

        let interactions = read("did-ask").expect("the journal reads");
        assert_eq!(interactions.len(), 3);
        assert_eq!(interactions[0].request_id, "r1");
        assert_eq!(interactions[1].request_id, "r2");
        assert_eq!(interactions[1].settlement, Settlement::TimedOut);
        assert_eq!(
            interactions[2].settlement,
            Settlement::Answered {
                approved: Some(true),
                scope: Some(ApprovalScope::Run),
                choice: None,
                text: None,
                feedback: None,
            }
        );
    });
}

/// A run that never asked anybody anything has no interactions, and says so
/// with an empty list rather than an error.
#[test]
fn a_run_that_never_asked_has_no_interactions() {
    crate::runstate::with_isolated_runs_dir("interactions-empty", |_dir| {
        create_run(&meta("quiet-run")).expect("run written");
        assert!(
            read("quiet-run")
                .expect("no journal is not a failure")
                .is_empty()
        );
    });
}

/// A run id that names nothing at all reads the same as one with no journal:
/// there is nothing here for this reader to distinguish, the caller reads the
/// run first for that.
#[test]
fn an_unknown_run_has_no_interactions() {
    crate::runstate::with_isolated_runs_dir("interactions-unknown", |_dir| {
        assert!(
            read("no-such-run")
                .expect("no journal is not a failure")
                .is_empty()
        );
    });
}

/// A journal that cannot be read is reported rather than read as a run that
/// never asked anything.
#[test]
fn an_unreadable_journal_is_an_error() {
    crate::runstate::with_isolated_runs_dir("interactions-corrupt", |_dir| {
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
