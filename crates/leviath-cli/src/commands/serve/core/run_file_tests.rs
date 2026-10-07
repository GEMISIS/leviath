//! Opening and walking a run's file, and the fixtures the other serve tests
//! share: a run recorded the way the daemon records one, and a step added to
//! it.

use std::ops::ControlFlow;
use std::sync::Arc;

use leviath_runtime::runfile::RunFileWriter;
use leviath_runtime::state::{RunEvent, RunState};

use super::*;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, run_on_disk};

/// A run of the inline coder blueprint, recorded under the runs directory in
/// force (a test's isolated one) the way the daemon records a new run: its
/// spec and the state it starts in, with no step taken. Returns its id.
pub(crate) fn recorded() -> String {
    let agent = tempfile::tempdir().unwrap().keep();
    let manifest = manifest_in(&agent, &crate::test_support::inline_coder_manifest());
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(crate::test_support::FakeProvider::new().context_window(100_000)),
    );
    run_on_disk(
        Config::default(),
        registry,
        &runstate::runs_dir(),
        &manifest,
    )
}

/// Record one more step of `run_id`: its last state with `edit` made to it,
/// stamped `at`, carrying `events`. Returns the step's number.
pub(crate) fn step(
    run_id: &str,
    at: i64,
    mut events: Vec<RunEvent>,
    edit: impl FnOnce(&mut RunState),
) -> u64 {
    let mut writer = RunFileWriter::open(&path(run_id), Default::default()).unwrap();
    let mut next = writer.state().clone();
    edit(&mut next);
    // A move journals its edge as the live lane does, beside the state that
    // shows it.
    if next.last_transition != writer.state().last_transition
        && let Some(taken) = next.last_transition.clone()
    {
        events.push(RunEvent::Transition(taken));
    }
    writer
        .record(next, at, events)
        .unwrap()
        .expect("the step changed something")
}

/// Write `bytes` as the run file of `run_id`.
pub(crate) fn garbage(run_id: &str, bytes: &[u8]) {
    let dir = runstate::run_dir(run_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(leviath_core::files::RUN_FILE), bytes).unwrap();
}

/// Rewrite `run_id`'s file as its spec alone, with no state to start from.
pub(crate) fn stateless(run_id: &str) {
    let spec = leviath_runtime::runfile::RunFileReader::open(&path(run_id))
        .unwrap()
        .spec()
        .clone();
    let mut bytes =
        leviath_runtime::runfile::codec::header(leviath_runtime::runfile::fingerprint());
    bytes.extend(
        leviath_runtime::runfile::codec::encode(
            leviath_runtime::runfile::codec::FrameKind::Spec,
            &spec,
        )
        .unwrap(),
    );
    garbage(run_id, &bytes);
}

/// Add to `run_id`'s file a step numbered `seq` that does not decode.
pub(crate) fn bad_step(run_id: &str, seq: u64) {
    let mut bytes = std::fs::read(path(run_id)).unwrap();
    bytes.extend(
        leviath_runtime::runfile::codec::encode(
            leviath_runtime::runfile::codec::FrameKind::Delta,
            &seq,
        )
        .unwrap(),
    );
    garbage(run_id, &bytes);
}

#[tokio::test]
async fn a_file_with_no_start_or_a_step_that_will_not_decode_cannot_be_walked() {
    runstate::with_isolated_runs_dir_async("run-file-bad-steps", |_d| async move {
        let run_id = recorded();
        stateless(&run_id);
        let reader = require(&run_id).unwrap();
        assert_eq!(initial(&run_id, &reader).unwrap_err().code(), "INTERNAL");
        let walked = walk(&run_id, &reader, &mut |_| ControlFlow::Continue(()));
        assert_eq!(walked.unwrap_err().code(), "INTERNAL");

        let run_id = recorded();
        bad_step(&run_id, 1);
        let reader = require(&run_id).unwrap();
        let walked = walk(&run_id, &reader, &mut |_| ControlFlow::Continue(()));
        assert_eq!(walked.unwrap_err().code(), "INTERNAL");
        let paired = walk_pairs(&run_id, &reader, &mut |_| ControlFlow::Continue(()));
        assert_eq!(paired.unwrap_err().code(), "INTERNAL");
    })
    .await;
}

#[tokio::test]
async fn a_run_with_no_file_is_a_miss_and_one_that_will_not_read_is_an_error() {
    runstate::with_isolated_runs_dir_async("run-file-open", |_d| async move {
        assert!(open("ghost").unwrap().is_none());
        let missing = require("ghost").unwrap_err();
        assert_eq!(missing.code(), "NOT_FOUND");
        assert!(missing.to_string().contains("ghost"), "{missing}");

        garbage("broken", b"not a run file");
        let broken = open("broken").unwrap_err();
        assert_eq!(broken.code(), "INTERNAL");
        assert!(broken.to_string().contains("converted"), "{broken}");

        let run_id = recorded();
        let reader = require(&run_id).unwrap();
        assert_eq!(reader.spec().run_id.as_str(), run_id);
        assert_eq!(initial(&run_id, &reader).unwrap().seq, 0);
    })
    .await;
}

/// Something at the run file's path that cannot be read is an error, not a
/// miss: here, a directory.
#[tokio::test]
async fn a_run_file_that_cannot_be_read_is_an_error() {
    runstate::with_isolated_runs_dir_async("run-file-unreadable", |_d| async move {
        std::fs::create_dir_all(path("dir-run")).unwrap();
        assert_eq!(open("dir-run").unwrap_err().code(), "INTERNAL");
    })
    .await;
}

#[tokio::test]
async fn a_walk_hands_over_every_step_in_order_and_stops_when_asked() {
    runstate::with_isolated_runs_dir_async("run-file-walk", |_d| async move {
        let run_id = recorded();
        for at in [10, 20, 30] {
            step(&run_id, at, vec![RunEvent::Log(format!("at {at}"))], |s| {
                s.cursor.iteration += 1;
            });
        }
        let reader = require(&run_id).unwrap();

        let mut seen = Vec::new();
        walk(&run_id, &reader, &mut |step| {
            seen.push((
                step.delta.seq,
                step.cursor.iteration,
                step.after.cursor.iteration,
            ));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(seen, vec![(1, 0, 1), (2, 1, 2), (3, 2, 3)]);

        let mut pairs = Vec::new();
        walk_pairs(&run_id, &reader, &mut |pair| {
            pairs.push((pair.before.seq, pair.after.seq));
            match pair.delta.seq {
                2 => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        })
        .unwrap();
        assert_eq!(pairs, vec![(0, 1), (1, 2)]);

        let mut first = Vec::new();
        walk(&run_id, &reader, &mut |step| {
            first.push(step.delta.seq);
            ControlFlow::Break(())
        })
        .unwrap();
        assert_eq!(first, vec![1]);
    })
    .await;
}
