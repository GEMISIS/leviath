//! Tests for [`super`].

use super::*;

use crate::runfile::RunFileWriter;
use crate::spec::issues::IssueCode;
use crate::state::RunStatus;

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).unwrap()
}

fn asked(text: &str, request: &[u8]) -> Idempotency {
    Idempotency {
        key: key(text),
        request: Digest::of(request),
    }
}

fn run(id: &str) -> RunId {
    RunId::new(id).unwrap()
}

fn spawned(id: &str) -> Spawned {
    Spawned {
        run_id: run(id),
        warnings: SpawnIssues::new(),
        deduplicated: false,
    }
}

/// A reply channel, and what was sent on it once it has been.
fn reply() -> (Reply, oneshot::Receiver<Result<Spawned, SpawnIssues>>) {
    oneshot::channel()
}

fn always(_: &RunId) -> bool {
    true
}

/// The one issue a refusal carries.
fn only_issue(answer: Result<Spawned, SpawnIssues>) -> SpawnIssue {
    let issues = answer.expect_err("a refusal");
    assert_eq!(issues.len(), 1, "{issues}");
    issues.0[0].clone()
}

/// The first request under a key goes ahead; the same one while it starts
/// waits and hears its answer, marked as a repeat; once the run is placed
/// the same request is answered with it at once.
#[test]
fn a_repeated_request_is_answered_with_the_first_ones_run() {
    let mut claims = Claims::default();
    let first = asked("k", b"req");
    let (tx, _first_rx) = reply();
    assert!(claims.admit(&first, tx, &always).is_some());

    let (tx, mut waiting) = reply();
    assert!(claims.admit(&first, tx, &always).is_none());
    assert!(waiting.try_recv().is_err(), "it waits for the first start");

    let mut placed = spawned("r-1");
    placed.warnings = crate::host::host_refusal("may never finish");
    claims.settle(&first.key, &Ok(placed.clone()));
    let heard = waiting.try_recv().unwrap().unwrap();
    assert_eq!(heard.run_id, run("r-1"));
    assert!(heard.deduplicated);
    assert_eq!(heard.warnings, placed.warnings);

    let (tx, mut again) = reply();
    assert!(claims.admit(&first, tx, &always).is_none());
    let heard = again.try_recv().unwrap().unwrap();
    assert_eq!(
        (heard.run_id, heard.deduplicated, heard.warnings),
        (run("r-1"), true, placed.warnings)
    );
}

/// A different request under a key that is held is refused, naming the run
/// that holds it, or saying its start is still under way.
#[test]
fn a_different_request_under_a_held_key_is_a_conflict() {
    let mut claims = Claims::default();
    let first = asked("pw:task-1", b"one");
    let other = asked("pw:task-1", b"two");
    let (tx, _rx) = reply();
    claims.admit(&first, tx, &always);

    let (tx, mut refused) = reply();
    assert!(claims.admit(&other, tx, &always).is_none());
    let issue = only_issue(refused.try_recv().unwrap());
    assert_eq!(issue.code, IssueCode::Conflict);
    assert_eq!(issue.path.to_string(), "delivery.idempotency_key");
    assert!(
        issue.message.contains("still starting"),
        "{}",
        issue.message
    );
    assert_eq!(issue.got.as_deref(), Some("pw:task-1"));

    claims.settle(&first.key, &Ok(spawned("r-1")));
    let (tx, mut refused) = reply();
    assert!(claims.admit(&other, tx, &always).is_none());
    let issue = only_issue(refused.try_recv().unwrap());
    assert_eq!(issue.code, IssueCode::Conflict);
    assert!(issue.message.contains("run 'r-1'"), "{}", issue.message);
}

/// A start that is refused frees its key, and whoever waited on it hears
/// the same refusal.
#[test]
fn a_refused_start_frees_its_key_and_says_why_to_whoever_waited() {
    let mut claims = Claims::default();
    let first = asked("k", b"req");
    let (tx, _rx) = reply();
    claims.admit(&first, tx, &always);
    let (tx, mut waiting) = reply();
    claims.admit(&first, tx, &always);

    let refusal = crate::host::host_refusal("bad blueprint");
    claims.settle(&first.key, &Err(refusal.clone()));
    assert_eq!(waiting.try_recv().unwrap(), Err(refusal));

    let (tx, _rx) = reply();
    assert!(
        claims.admit(&first, tx, &always).is_some(),
        "the key is free again"
    );
}

/// A key whose run's record has gone is free.
#[test]
fn a_deleted_run_frees_its_key() {
    let mut claims = Claims::default();
    let first = asked("k", b"req");
    let (tx, _rx) = reply();
    claims.admit(&first, tx, &always);
    claims.settle(&first.key, &Ok(spawned("r-1")));
    let (tx, _rx) = reply();
    assert!(claims.admit(&first, tx, &|_| false).is_some());
    // Settling a key nothing is starting under changes nothing.
    claims.settle(&key("never-asked"), &Ok(spawned("r-2")));
}

/// Recalled keys answer as if their runs had just started here, and a key
/// already in use stays with what holds it.
#[test]
fn recalled_keys_answer_for_their_runs() {
    let mut claims = Claims::default();
    let starting = asked("busy", b"now");
    let (tx, _rx) = reply();
    claims.admit(&starting, tx, &always);
    claims.recall(vec![
        HeldKey {
            idempotency: asked("k", b"req"),
            run_id: run("r-old"),
            warnings: SpawnIssues::new(),
        },
        HeldKey {
            idempotency: asked("busy", b"then"),
            run_id: run("r-other"),
            warnings: SpawnIssues::new(),
        },
    ]);
    let (tx, mut heard) = reply();
    assert!(claims.admit(&asked("k", b"req"), tx, &always).is_none());
    assert_eq!(heard.try_recv().unwrap().unwrap().run_id, run("r-old"));
    let (tx, mut waiting) = reply();
    assert!(claims.admit(&starting, tx, &always).is_none());
    assert!(waiting.try_recv().is_err(), "still waiting on the start");
}

#[test]
fn a_record_is_looked_for_only_where_runs_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    assert!(record_exists(None, &run("r-1")));
    assert!(!record_exists(Some(dir.path()), &run("r-1")));
    std::fs::create_dir_all(dir.path().join("r-1")).unwrap();
    std::fs::write(
        dir.path().join("r-1").join(leviath_core::files::RUN_FILE),
        b"x",
    )
    .unwrap();
    assert!(record_exists(Some(dir.path()), &run("r-1")));
}

/// Write a run file for the run `id` under `runs`, spawned under `kept`,
/// ending in `last` when given.
fn record(
    runs: &Path,
    id: &str,
    kept: Option<Idempotency>,
    last: Option<RunStatus>,
) -> crate::spec::run_spec::RunSpec {
    let mut spec = crate::spec::run_spec::tests::spec();
    spec.run_id = run(id);
    spec.delivery.idempotency = kept;
    let dir = runs.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let state = crate::insert::initial_state(&spec);
    let path = dir.join(leviath_core::files::RUN_FILE);
    let mut writer = RunFileWriter::create(
        &path,
        &spec,
        &Default::default(),
        &state,
        Default::default(),
    )
    .unwrap();
    if let Some(status) = last {
        let mut next = state.clone();
        next.status = status;
        writer.record(next, 1, Vec::new()).unwrap();
    }
    spec
}

/// The keys under a runs directory are the ones its runs' records keep:
/// not a run spawned without one, not a file that is no run file, and not a
/// run marked refused at its start. One that failed on its own, however
/// early, holds its key still.
#[test]
fn held_keys_are_read_from_the_runs_records() {
    let runs = tempfile::tempdir().unwrap();
    let spec = record(runs.path(), "a-keyed", Some(asked("k-1", b"one")), None);
    record(runs.path(), "b-unkeyed", None, None);
    record(
        runs.path(),
        "c-refused",
        Some(asked("k-2", b"two")),
        Some(RunStatus::Error("could not bind".into())),
    );
    mark_refused(
        &runs
            .path()
            .join("c-refused")
            .join(leviath_core::files::RUN_FILE),
    )
    .unwrap();
    let failed_later = record(
        runs.path(),
        "d-failed-later",
        Some(asked("k-3", b"three")),
        Some(RunStatus::Error("on its first step".into())),
    );
    std::fs::create_dir_all(runs.path().join("e-junk")).unwrap();
    std::fs::write(
        runs.path()
            .join("e-junk")
            .join(leviath_core::files::RUN_FILE),
        b"junk",
    )
    .unwrap();

    let held = held_keys(runs.path());
    assert_eq!(
        held,
        vec![
            HeldKey {
                idempotency: asked("k-1", b"one"),
                run_id: run("a-keyed"),
                warnings: spec.warnings(),
            },
            HeldKey {
                idempotency: asked("k-3", b"three"),
                run_id: run("d-failed-later"),
                warnings: failed_later.warnings(),
            },
        ]
    );
    assert!(held_keys(&runs.path().join("missing")).is_empty());
}

/// A key whose run's record is gone is dropped by a prune; one whose record
/// is there, and a start under way, are kept.
#[test]
fn a_prune_drops_the_keys_of_deleted_runs() {
    let runs = tempfile::tempdir().unwrap();
    let mut claims = Claims::default();
    for (key, run_id) in [("kept", "r-kept"), ("gone", "r-gone")] {
        let (tx, _rx) = reply();
        claims.admit(&asked(key, b"req"), tx, &always);
        claims.settle(&self::key(key), &Ok(spawned(run_id)));
    }
    let (tx, _rx) = reply();
    claims.admit(&asked("starting", b"req"), tx, &always);
    record(runs.path(), "r-kept", None, None);
    claims.prune(Some(runs.path()));
    assert_eq!(claims.0.len(), 2);
    assert!(claims.0.contains_key("kept") && claims.0.contains_key("starting"));
    claims.prune(None);
    assert_eq!(
        claims.0.len(),
        2,
        "with no runs directory nothing is deleted"
    );
}
