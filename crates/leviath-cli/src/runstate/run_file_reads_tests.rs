//! A run's context, ledger and history read off its run file.

use std::sync::Arc;

use leviath_runtime::runfile::RunFileWriter;
use leviath_runtime::spec::names::{EdgeName, StageName};
use leviath_runtime::state::{
    EntryKind, EntryMeta, EntryState, RunEvent, TransitionReason, TransitionRecord,
};

use super::*;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, run_on_disk};

/// A run of the coder blueprint recorded under `runs`, the way the daemon
/// records a new one. Returns its directory.
pub(crate) fn recorded(runs: &Path) -> PathBuf {
    let agent = tempfile::tempdir().unwrap().keep();
    let manifest = manifest_in(&agent, &crate::test_support::inline_coder_manifest());
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(crate::test_support::FakeProvider::new().context_window(100_000)),
    );
    runs.join(run_on_disk(Config::default(), registry, runs, &manifest))
}

/// Record one more step of the run in `dir`: its last state with `edit` made
/// to it, stamped `at`.
pub(crate) fn step(dir: &Path, at: i64, edit: impl FnOnce(&mut RunState)) {
    step_with(dir, at, vec![RunEvent::Log("step".into())], edit);
}

/// [`step`], carrying `events`.
pub(crate) fn step_with(
    dir: &Path,
    at: i64,
    events: Vec<RunEvent>,
    edit: impl FnOnce(&mut RunState),
) {
    let mut writer = RunFileWriter::open(&path_in(dir), Default::default()).unwrap();
    let mut next = writer.state().clone();
    edit(&mut next);
    writer.record(next, at, events).unwrap();
}

/// Add `text` to the first region of a state's window.
pub(crate) fn say(state: &mut RunState, text: &str) {
    state.context.regions[0].entries.push(EntryState {
        text: text.to_string(),
        parts: Vec::new(),
        tokens: 2,
        timestamp: 0,
        kind: EntryKind::Text,
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    });
}

/// Move a state along the edge `edge` from `from` to `to`.
pub(crate) fn take(state: &mut RunState, from: &str, to: &str, edge: &str) {
    state.cursor.stage = StageName::new(to).unwrap();
    state.last_transition = Some(TransitionRecord {
        from: StageName::new(from).unwrap(),
        to: StageName::new(to).unwrap(),
        edge: Some(EdgeName::new(edge).unwrap()),
        reason: TransitionReason::Condition,
        visit: format!("{to}-1"),
    });
}

#[tokio::test]
async fn the_window_and_the_ledger_are_the_last_steps() {
    let runs = tempfile::tempdir().unwrap();
    let dir = recorded(runs.path());
    step(&dir, 10, |s| say(s, "hello there"));
    let window = context_in(&dir).expect("a window");
    assert!(
        window
            .regions
            .iter()
            .flat_map(|r| &r.entries)
            .any(|e| e.content == "hello there"),
        "{window:?}"
    );
    step(&dir, 11, |s| take(s, "analyze", "implement", "next"));
    let (_, state) = latest_in(&dir).unwrap();
    let names = |records: Vec<StageRecord>| -> Vec<String> {
        records.into_iter().map(|r| r.name).collect()
    };
    assert_eq!(
        names(stages_in(&dir).unwrap()),
        names(leviath_runtime::runfile::stage_records(&state))
    );
}

#[tokio::test]
async fn a_history_holds_every_window_change_and_every_edge_taken() {
    let runs = tempfile::tempdir().unwrap();
    let dir = recorded(runs.path());
    step(&dir, 10, |s| say(s, "one"));
    // A step that leaves the window alone is not a point, and an edge back
    // into the same stage is still an edge.
    step(&dir, 15, |s| take(s, "analyze", "analyze", "again"));
    step(&dir, 20, |s| {
        say(s, "two");
        take(s, "analyze", "implement", "next");
    });
    let history = history_in(&dir).expect("a history");
    let at: Vec<i64> = history.points.iter().map(|p| p.at).collect();
    assert_eq!(at[1..], [10, 20]);
    assert_eq!(history.points.len(), 3);
    assert_eq!(
        history.transitions,
        Some(vec![
            ("analyze".to_string(), "analyze".to_string()),
            ("analyze".to_string(), "implement".to_string()),
        ])
    );
    assert!(
        history
            .points
            .iter()
            .all(|p| p.meta.callback_secret.is_none())
    );
}

#[test]
fn a_directory_with_no_run_file_reads_as_nothing() {
    let runs = tempfile::tempdir().unwrap();
    let dir = runs.path().join("ghost");
    assert!(open_in(&dir).is_err());
    assert!(latest_in(&dir).is_none());
    assert!(context_in(&dir).is_none());
    assert!(stages_in(&dir).is_none());
    assert!(history_in(&dir).is_none());
}

/// A file whose spec is there and whose start is not, or with a step that
/// does not decode, has no history to show.
#[tokio::test]
async fn a_run_file_with_a_broken_start_or_step_has_no_history() {
    use leviath_runtime::runfile::codec::{FrameKind, encode, header};
    let runs = tempfile::tempdir().unwrap();
    let dir = recorded(runs.path());
    let spec = leviath_runtime::runfile::RunFileReader::open(&path_in(&dir))
        .unwrap()
        .spec()
        .clone();
    let good = std::fs::read(path_in(&dir)).unwrap();

    let mut bytes = header(leviath_runtime::runfile::fingerprint());
    bytes.extend(encode(FrameKind::Spec, &spec).unwrap());
    std::fs::write(path_in(&dir), &bytes).unwrap();
    assert!(history_in(&dir).is_none());
    assert!(latest_in(&dir).is_none());

    let mut bytes = good;
    bytes.extend(encode(FrameKind::Delta, &1u64).unwrap());
    std::fs::write(path_in(&dir), &bytes).unwrap();
    assert!(history_in(&dir).is_none());
}
