//! Tests for reading a run's interactions back out of its journal.

use leviath_core::interaction::{ApprovalScope, InteractionKind, Settlement};
use leviath_core::run_archive::{self, RunIdentity, RunRecord};

use super::{INTERACTIONS_DEFAULT_LIMIT, INTERACTIONS_MAX_LIMIT, InteractionsSpec, page};
use crate::commands::serve::cursor;
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

/// The page size is bounded at both ends, and a zero is refused rather than
/// read as "give me none".
#[test]
fn a_request_is_bounded_at_both_ends() {
    let spec = InteractionsSpec::resolve("run-a", None, None).expect("the defaults");
    assert_eq!(spec.limit, INTERACTIONS_DEFAULT_LIMIT);
    assert!(spec.after.is_none());

    let spec = InteractionsSpec::resolve("run-a", Some(10_000), None).expect("clamped");
    assert_eq!(spec.limit, INTERACTIONS_MAX_LIMIT);

    let refused = InteractionsSpec::resolve("run-a", Some(0), None).expect_err("zero");
    assert_eq!(refused.code(), "BAD_USER_INPUT");
}

/// A cursor this listing did not mint is ignored or refused, never followed.
#[test]
fn a_cursor_from_elsewhere_does_not_resume_this_listing() {
    let mine = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["interactions", "run-a"]),
        cursor::CursorKey::Int(3),
        "",
    );
    let spec = InteractionsSpec::resolve("run-a", None, Some(&mine)).expect("its own cursor");
    assert_eq!(spec.after, Some(3));

    // A key of a kind this listing never mints: readable, and not usable.
    let lettered = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["interactions", "run-a"]),
        cursor::CursorKey::Text("seven".to_string()),
        "",
    );
    let spec = InteractionsSpec::resolve("run-a", None, Some(&lettered)).expect("readable");
    assert!(spec.after.is_none(), "a key this listing cannot use");

    // A cursor minted for the executions listing on the same run is refused:
    // the digest is namespaced per listing, not only per run.
    let executions_cursor = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["run-a"]),
        cursor::CursorKey::Int(3),
        "",
    );
    let refused = InteractionsSpec::resolve("run-a", None, Some(&executions_cursor))
        .expect_err("a cursor from the executions listing");
    assert_eq!(refused.code(), "BAD_USER_INPUT");

    // A cursor minted for a different run is refused too.
    let elsewhere = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["interactions", "run-b"]),
        cursor::CursorKey::Int(1),
        "",
    );
    let refused = InteractionsSpec::resolve("run-a", None, Some(&elsewhere))
        .expect_err("a cursor from another run");
    assert_eq!(refused.code(), "BAD_USER_INPUT");
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

        let spec = InteractionsSpec::resolve("did-ask", None, None).expect("defaults");
        let paged = page("did-ask", &spec).expect("the journal reads");
        assert_eq!(paged.total, 3);
        assert_eq!(paged.interactions.len(), 3);
        assert_eq!(paged.interactions[0].index, 0);
        assert_eq!(paged.interactions[0].record.request_id, "r1");
        assert_eq!(paged.interactions[1].record.request_id, "r2");
        assert_eq!(
            paged.interactions[1].record.settlement,
            Settlement::TimedOut
        );
        assert_eq!(paged.interactions[2].index, 2);
        assert_eq!(
            paged.interactions[2].record.settlement,
            Settlement::Answered {
                approved: Some(true),
                scope: Some(ApprovalScope::Run),
                choice: None,
                text: None,
                feedback: None,
            }
        );
        assert!(paged.next_cursor.is_none(), "the only page");
    });
}

/// A run that never asked anybody anything has no interactions, and says so
/// with an empty page rather than an error.
#[test]
fn a_run_that_never_asked_has_no_interactions() {
    crate::runstate::with_isolated_runs_dir("interactions-empty", |_dir| {
        create_run(&meta("quiet-run")).expect("run written");
        let spec = InteractionsSpec::resolve("quiet-run", None, None).expect("defaults");
        let paged = page("quiet-run", &spec).expect("no journal is not a failure");
        assert_eq!(paged.total, 0);
        assert!(paged.interactions.is_empty());
    });
}

/// A run id that names nothing at all reads the same as one with no journal:
/// there is nothing here for this reader to distinguish, the caller reads the
/// run first for that.
#[test]
fn an_unknown_run_has_no_interactions() {
    crate::runstate::with_isolated_runs_dir("interactions-unknown", |_dir| {
        let spec = InteractionsSpec::resolve("no-such-run", None, None).expect("defaults");
        let paged = page("no-such-run", &spec).expect("no journal is not a failure");
        assert_eq!(paged.total, 0);
    });
}

/// The page carries on from its cursor, and the last page says it is the
/// last.
#[test]
fn the_interactions_page_carries_on_from_its_cursor() {
    crate::runstate::with_isolated_runs_dir("interactions-paging", |_dir| {
        create_run(&meta("many-asks")).expect("run written");
        let records: Vec<RunRecord> = (0..5)
            .map(|i| asked(&format!("r{i}"), "ok?", Settlement::TimedOut))
            .collect();
        write_journal("many-asks", records);

        let spec = InteractionsSpec::resolve("many-asks", Some(2), None).expect("first page");
        let first = page("many-asks", &spec).expect("reads");
        assert_eq!(first.total, 5);
        assert_eq!(first.interactions.len(), 2);
        assert_eq!(first.interactions[0].record.request_id, "r0");
        assert_eq!(first.interactions[1].record.request_id, "r1");
        let cursor = first.next_cursor.expect("more to come");

        let spec = InteractionsSpec::resolve("many-asks", Some(10), Some(&cursor))
            .expect("the cursor resumes");
        let rest = page("many-asks", &spec).expect("reads");
        assert!(rest.next_cursor.is_none(), "that was the rest");
        let ids: Vec<&str> = rest
            .interactions
            .iter()
            .map(|i| i.record.request_id.as_str())
            .collect();
        assert_eq!(ids, vec!["r2", "r3", "r4"], "no interaction read twice");
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
        let spec = InteractionsSpec::resolve("broken", None, None).expect("defaults");
        let failed = page("broken", &spec).expect_err("an unreadable journal");
        assert_eq!(failed.code(), "INTERNAL");
        assert!(
            failed.to_string().contains("unreadable journal"),
            "{failed}"
        );
    });
}
