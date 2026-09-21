//! Tests for reading a run's provider attempts back out of its journal.

use leviath_core::run_archive::{
    self, AttemptOutcome, AttemptRecord, FailoverRecord, RequestDigest, Retry, RunIdentity,
    RunRecord,
};

use super::{INFERENCES_DEFAULT_LIMIT, INFERENCES_MAX_LIMIT, InferencesSpec, page};
use crate::commands::serve::cursor;
use crate::runstate::{RunMeta, create_run};

/// A run to hang a journal off.
fn meta(run_id: &str) -> RunMeta {
    RunMeta::new(
        run_id.to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "call a provider a few times".to_string(),
        None,
        "/tmp".to_string(),
        1,
    )
}

/// The digest every attempt in these tests sends.
fn digest() -> RequestDigest {
    RequestDigest {
        system_hash: 0x1234_5678_9abc_def0,
        messages: 4,
        tools: 7,
        max_tokens: 2048,
        temperature: 0.2,
    }
}

/// One attempt record.
fn attempt(stage: &str, n: u32, provider: &str, model: &str, outcome: AttemptOutcome) -> RunRecord {
    RunRecord::InferenceAttempt(AttemptRecord {
        id: format!("a{n:08x}"),
        stage: stage.to_string(),
        attempt: n,
        provider: provider.to_string(),
        model: model.to_string(),
        outcome,
        duration_ms: 1_200,
        backoff_ms: 400,
        digest: digest(),
        model_input: None,
        at: 200,
    })
}

/// One failover record.
fn failover(stage: &str, from: (&str, &str), to: (&str, &str)) -> RunRecord {
    RunRecord::InferenceFailover(FailoverRecord {
        stage: stage.to_string(),
        iteration: 3,
        from_provider: from.0.to_string(),
        from_model: from.1.to_string(),
        to_provider: to.0.to_string(),
        to_model: to.1.to_string(),
        reason: "credits_exhausted".to_string(),
        kind: "insufficient_credits".to_string(),
        at: 201,
    })
}

/// A failure that the loop reported rather than retried.
fn reported() -> AttemptOutcome {
    AttemptOutcome::Failed {
        kind: "insufficient_credits".to_string(),
        transient: false,
        capacity: false,
        next: Retry::Reported,
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
    let spec = InferencesSpec::resolve("run-a", None, None).expect("the defaults");
    assert_eq!(spec.limit, INFERENCES_DEFAULT_LIMIT);
    assert!(spec.after.is_none());

    let spec = InferencesSpec::resolve("run-a", Some(10_000), None).expect("clamped");
    assert_eq!(spec.limit, INFERENCES_MAX_LIMIT);

    let refused = InferencesSpec::resolve("run-a", Some(0), None).expect_err("zero");
    assert_eq!(refused.code(), "BAD_USER_INPUT");
}

/// A cursor this listing did not mint is ignored or refused, never followed.
#[test]
fn a_cursor_from_elsewhere_does_not_resume_this_listing() {
    let mine = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["inferences", "run-a"]),
        cursor::CursorKey::Int(3),
        "",
    );
    let spec = InferencesSpec::resolve("run-a", None, Some(&mine)).expect("its own cursor");
    assert_eq!(spec.after, Some(3));

    // A key of a kind this listing never mints: readable, and not usable.
    let lettered = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["inferences", "run-a"]),
        cursor::CursorKey::Text("seven".to_string()),
        "",
    );
    let spec = InferencesSpec::resolve("run-a", None, Some(&lettered)).expect("readable");
    assert!(spec.after.is_none(), "a key this listing cannot use");

    // A cursor minted for the interactions listing on the same run is refused:
    // the digest is namespaced per listing, not only per run.
    let interactions_cursor = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["interactions", "run-a"]),
        cursor::CursorKey::Int(3),
        "",
    );
    let refused = InferencesSpec::resolve("run-a", None, Some(&interactions_cursor))
        .expect_err("a cursor from the interactions listing");
    assert_eq!(refused.code(), "BAD_USER_INPUT");

    // A cursor minted for a different run is refused too.
    let elsewhere = cursor::encode(
        "index",
        "asc",
        &cursor::filter_digest(&["inferences", "run-b"]),
        cursor::CursorKey::Int(1),
        "",
    );
    let refused = InferencesSpec::resolve("run-a", None, Some(&elsewhere))
        .expect_err("a cursor from another run");
    assert_eq!(refused.code(), "BAD_USER_INPUT");
}

/// Every trip to a provider comes back in the order it was made, and the move
/// that followed one is on the attempt it followed.
#[test]
fn the_attempts_read_back_in_order_with_their_failover() {
    crate::runstate::with_isolated_runs_dir("inferences-order", |_dir| {
        create_run(&meta("did-call")).expect("run written");
        write_journal(
            "did-call",
            vec![
                attempt("plan", 1, "anthropic", "claude-sonnet-4-5", reported()),
                failover(
                    "plan",
                    ("anthropic", "claude-sonnet-4-5"),
                    ("openai", "gpt-5"),
                ),
                attempt("plan", 1, "openai", "gpt-5", AttemptOutcome::Succeeded),
            ],
        );

        let spec = InferencesSpec::resolve("did-call", None, None).expect("defaults");
        let paged = page("did-call", &spec).expect("the journal reads");
        assert_eq!(paged.total, 2, "a failover is not an attempt of its own");
        assert_eq!(paged.attempts[0].index, 0);
        assert_eq!(paged.attempts[0].attempt.record.provider, "anthropic");
        let moved = paged.attempts[0]
            .attempt
            .failover
            .as_ref()
            .expect("the move that followed it");
        assert_eq!(moved.to_provider, "openai");
        assert_eq!(moved.to_model, "gpt-5");
        assert_eq!(paged.attempts[1].index, 1);
        assert_eq!(paged.attempts[1].attempt.record.provider, "openai");
        assert!(
            paged.attempts[1].attempt.failover.is_none(),
            "nothing followed the call that worked"
        );
        assert!(paged.next_cursor.is_none(), "the only page");
    });
}

/// A move is paired with the call it left rather than with whatever was written
/// just before it, so an attempt from a lane with no stage of its own cannot
/// collect somebody else's failover.
#[test]
fn a_move_is_paired_with_the_call_it_left() {
    crate::runstate::with_isolated_runs_dir("inferences-pairing", |_dir| {
        create_run(&meta("two-lanes")).expect("run written");
        write_journal(
            "two-lanes",
            vec![
                attempt("plan", 1, "anthropic", "claude-sonnet-4-5", reported()),
                // The titling lane's own call, journaled in between.
                attempt("", 1, "openai", "gpt-5-mini", AttemptOutcome::Succeeded),
                failover(
                    "plan",
                    ("anthropic", "claude-sonnet-4-5"),
                    ("openai", "gpt-5"),
                ),
            ],
        );

        let spec = InferencesSpec::resolve("two-lanes", None, None).expect("defaults");
        let paged = page("two-lanes", &spec).expect("reads");
        assert!(
            paged.attempts[0].attempt.failover.is_some(),
            "the stage call that failed over"
        );
        assert!(
            paged.attempts[1].attempt.failover.is_none(),
            "the titling call moved nowhere"
        );
    });
}

/// A move naming a call that is not in the journal has nothing to hang on, and
/// is left out rather than attached to an unrelated attempt.
#[test]
fn a_move_with_no_attempt_of_its_own_is_left_out() {
    crate::runstate::with_isolated_runs_dir("inferences-orphan", |_dir| {
        create_run(&meta("orphan-move")).expect("run written");
        write_journal(
            "orphan-move",
            vec![
                attempt("plan", 1, "openai", "gpt-5", AttemptOutcome::Succeeded),
                failover("build", ("google", "gemini-3-pro"), ("openai", "gpt-5")),
            ],
        );

        let spec = InferencesSpec::resolve("orphan-move", None, None).expect("defaults");
        let paged = page("orphan-move", &spec).expect("reads");
        assert_eq!(paged.total, 1);
        assert!(paged.attempts[0].attempt.failover.is_none());
    });
}

/// A run that never called a provider has no attempts, and says so with an
/// empty page rather than an error.
#[test]
fn a_run_that_never_called_a_provider_has_no_attempts() {
    crate::runstate::with_isolated_runs_dir("inferences-empty", |_dir| {
        create_run(&meta("quiet-run")).expect("run written");
        let spec = InferencesSpec::resolve("quiet-run", None, None).expect("defaults");
        let paged = page("quiet-run", &spec).expect("no journal is not a failure");
        assert_eq!(paged.total, 0);
        assert!(paged.attempts.is_empty());
    });
}

/// The page carries on from its cursor, and the last page says it is the last.
#[test]
fn the_attempts_page_carries_on_from_its_cursor() {
    crate::runstate::with_isolated_runs_dir("inferences-paging", |_dir| {
        create_run(&meta("many-calls")).expect("run written");
        let records: Vec<RunRecord> = (0..5)
            .map(|i| attempt("plan", i + 1, "openai", "gpt-5", reported()))
            .collect();
        write_journal("many-calls", records);

        let spec = InferencesSpec::resolve("many-calls", Some(2), None).expect("first page");
        let first = page("many-calls", &spec).expect("reads");
        assert_eq!(first.total, 5);
        assert_eq!(first.attempts.len(), 2);
        assert_eq!(first.attempts[0].attempt.record.attempt, 1);
        let cursor = first.next_cursor.expect("more to come");

        let spec = InferencesSpec::resolve("many-calls", Some(10), Some(&cursor))
            .expect("the cursor resumes");
        let rest = page("many-calls", &spec).expect("reads");
        assert!(rest.next_cursor.is_none(), "that was the rest");
        let numbers: Vec<u32> = rest
            .attempts
            .iter()
            .map(|held| held.attempt.record.attempt)
            .collect();
        assert_eq!(numbers, vec![3, 4, 5], "no attempt read twice");
    });
}

/// A journal that cannot be read is reported rather than read as a run that
/// never called anything.
#[test]
fn an_unreadable_journal_is_an_error() {
    crate::runstate::with_isolated_runs_dir("inferences-corrupt", |_dir| {
        let dir = crate::runstate::run_dir("broken");
        std::fs::create_dir_all(&dir).expect("a run dir");
        std::fs::write(
            dir.join(leviath_core::files::ARCHIVE_FILE),
            b"not an archive",
        )
        .expect("a corrupt journal");
        let spec = InferencesSpec::resolve("broken", None, None).expect("defaults");
        let failed = page("broken", &spec).expect_err("an unreadable journal");
        assert_eq!(failed.code(), "INTERNAL");
        assert!(
            failed.to_string().contains("unreadable journal"),
            "{failed}"
        );
    });
}

/// One attempt by the id it was minted under, which is what a tool batch names.
///
/// A batch records the attempt whose answer asked for its calls, and nothing in
/// the timeline could stand in for the lookup: a failover means the answer came
/// from a provider the previous attempt did not go to.
#[test]
fn one_attempt_is_found_by_the_id_it_was_minted_under() {
    crate::runstate::with_isolated_runs_dir("inferences-by-id", |_dir| {
        create_run(&meta("named-attempts")).expect("run written");
        write_journal(
            "named-attempts",
            vec![
                attempt("plan", 1, "anthropic", "claude", AttemptOutcome::Succeeded),
                attempt("plan", 2, "openai", "gpt", AttemptOutcome::Succeeded),
            ],
        );

        // The fixture names each attempt after its number.
        let found = super::attempt("named-attempts", "a00000002")
            .expect("reads")
            .expect("the second attempt");
        assert_eq!(found.record.provider, "openai");
        assert_eq!(found.record.attempt, 2);

        // An id nothing was minted under matches nothing, and an empty one does
        // not match the attempts a journal recorded without identity.
        assert!(
            super::attempt("named-attempts", "a-from-another-build")
                .expect("reads")
                .is_none()
        );
        assert!(
            super::attempt("named-attempts", "")
                .expect("reads")
                .is_none()
        );
    });
}
