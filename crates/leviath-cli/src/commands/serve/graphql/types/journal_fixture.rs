//! A run file recorded from journal records, the way the persistence lane
//! records one, for the tests that read what a run did.
//!
//! The world hands the lane one journal record per thing that happened, and
//! the lane turns each into the events of a step. These tests describe a run
//! the same way, as the records it would have written, so what they read back
//! is what the lane would have kept: each record becomes a step of its own,
//! after a step that moves the run's cursor to where the record says it
//! happened.

use leviath_core::run_archive::RunRecord;
use leviath_runtime::runfile::{CheckpointPolicy, RunFileReader, RunFileWriter, journal_events};
use leviath_runtime::spec::env::CodeFiles;
use leviath_runtime::spec::names::{RunId, StageName};
use leviath_runtime::state::RunState;

use crate::commands::serve::core::run_file::{self, tests::recorded};

/// Start a run file for `run_id` with no step taken: the coder-shaped test
/// blueprint's spec and starting state, under this run's id.
pub(crate) fn started(run_id: &str) -> RunFileWriter {
    started_from(run_id, |_| {})
}

/// [`started`], with `edit` made to the state the run starts in.
pub(crate) fn started_from(run_id: &str, edit: impl FnOnce(&mut RunState)) -> RunFileWriter {
    started_at(run_id, None, edit)
}

/// [`started_from`], resolved at `created_at` when that is given.
pub(crate) fn started_at(
    run_id: &str,
    created_at: Option<i64>,
    edit: impl FnOnce(&mut RunState),
) -> RunFileWriter {
    let template = recorded();
    let reader = RunFileReader::open(&run_file::path(&template)).expect("the template reads");
    let mut spec = reader.spec().clone();
    spec.run_id = RunId::new(run_id).expect("a run id");
    spec.created_at = created_at.unwrap_or(spec.created_at);
    let mut initial = reader.state_at(0).expect("a starting state");
    edit(&mut initial);
    std::fs::remove_dir_all(crate::runstate::run_dir(&template)).expect("the template goes");
    let path = run_file::path(run_id);
    std::fs::create_dir_all(path.parent().expect("a run directory")).expect("the run directory");
    RunFileWriter::create(
        &path,
        &spec,
        &CodeFiles::new(),
        &initial,
        CheckpointPolicy::default(),
    )
    .expect("the run file is written")
}

/// When `record` happened, when it says.
fn at_of(record: &RunRecord) -> Option<i64> {
    match record {
        RunRecord::InferenceAttempt(a) => Some(a.at),
        RunRecord::InferenceFailover(f) => Some(f.at),
        RunRecord::ToolBatch { at, .. }
        | RunRecord::ToolCallDone { at, .. }
        | RunRecord::ArtifactsProduced { at, .. }
        | RunRecord::Interaction { at, .. }
        | RunRecord::ContextTransaction { at, .. }
        | RunRecord::ContextChange { at, .. } => Some(*at),
        _ => None,
    }
}

/// Move `state`'s cursor to where `record` says it happened.
fn place(state: &mut RunState, stages: &[StageName], record: &RunRecord) {
    let named = |name: &str| StageName::new(name).ok();
    let (stage, iteration, visit) = match record {
        RunRecord::ToolBatch {
            stage_index,
            iteration,
            visit_id,
            ..
        } => (
            stages.get(*stage_index).cloned(),
            Some(*iteration),
            Some(visit_id.clone()),
        ),
        RunRecord::ToolCallDone { iteration, .. } => (None, Some(*iteration), None),
        RunRecord::InferenceAttempt(a) => (named(&a.stage), None, None),
        RunRecord::InferenceFailover(f) => (named(&f.stage), Some(f.iteration), None),
        RunRecord::Interaction { stage, .. } => (named(stage), None, None),
        _ => (None, None, None),
    };
    if let Some(stage) = stage {
        state.cursor.stage = stage;
    }
    if let Some(iteration) = iteration {
        state.cursor.iteration = u32::try_from(iteration).unwrap_or(u32::MAX);
    }
    if let Some(visit) = visit {
        state.cursor.visit = visit;
    }
}

/// Record `records` as the run file of `run_id`, one step per record. A
/// record that moves the run's cursor is preceded by a step that only moves it.
pub(crate) fn journal(run_id: &str, records: &[RunRecord]) {
    let mut writer = started(run_id);
    append(&mut writer, records);
}

/// Record `records` as the run file of `run_id`, after a first step that
/// makes `edit` to the state the run started in.
pub(crate) fn journal_with(run_id: &str, edit: impl FnOnce(&mut RunState), records: &[RunRecord]) {
    let mut writer = started(run_id);
    let mut state = writer.state().clone();
    edit(&mut state);
    writer.record(state, 1, Vec::new()).expect("a step");
    append(&mut writer, records);
}

/// A window holding `tokens` in one entry of its first region and nothing
/// else, as a run in `stage` holds it.
fn window(state: &mut RunState, stage: &str, tokens: usize) {
    use leviath_runtime::state::context::{EntryKind, EntryMeta, EntryState};
    state.cursor.stage = StageName::new(stage).expect("a stage");
    let tokens = u32::try_from(tokens).expect("a small window");
    for (at, region) in state.context.regions.iter_mut().enumerate() {
        region.entries.clear();
        region.current_tokens = 0;
        if at == 0 {
            region.current_tokens = tokens;
            region.entries.push(EntryState {
                text: format!("a window of {tokens}"),
                parts: Vec::new(),
                tokens,
                timestamp: 0,
                kind: EntryKind::Text,
                meta: EntryMeta::None,
                key: None,
                reasoning: None,
            });
        }
    }
}

/// A run file whose history is one window per total in `totals`, the run in
/// `stage` throughout: the first is the window it starts with, and each later
/// one a step at `1 + its place in the list`.
pub(crate) fn windows(run_id: &str, stage: &str, totals: &[usize]) {
    let (first, rest) = totals.split_first().expect("at least one window");
    let mut writer = started_from(run_id, |state| window(state, stage, *first));
    for (at, total) in rest.iter().enumerate() {
        let mut state = writer.state().clone();
        window(&mut state, stage, *total);
        writer
            .record(state, 2 + at as i64, Vec::new())
            .expect("a step");
    }
}

/// A ledger line per stage named, in order, none of them entered.
pub(crate) fn ledger(names: &[&str]) -> Vec<leviath_runtime::state::StageRecord> {
    names
        .iter()
        .map(|name| leviath_runtime::state::StageRecord {
            stage: StageName::new(*name).expect("a stage"),
            status: leviath_runtime::state::StageStatus::Pending,
            entered: false,
            visits: Vec::new(),
            started_at: None,
            ..stay("", 0)
        })
        .collect()
}

/// A ledger line for a stage the run is still in, holding one stay `id` that
/// began at `entered_at`.
pub(crate) fn stay(id: &str, entered_at: i64) -> leviath_runtime::state::StageRecord {
    use leviath_runtime::state::{Clock, Spend, StageRecord, StageStatus, VisitRecord};
    StageRecord {
        stage: StageName::new("plan").expect("a stage"),
        status: StageStatus::Active,
        entered: true,
        spend: Spend::default(),
        models: Vec::new(),
        visits: vec![VisitRecord {
            id: id.to_string(),
            entered_at,
            left_at: None,
            spend: Spend::default(),
            clock: Clock::default(),
        }],
        region_tokens: Default::default(),
        first_call_prompt_tokens: None,
        runaway_warned: false,
        output_cap_raised: false,
        started_at: Some(entered_at),
        ended_at: None,
        clock: Clock::default(),
    }
}

/// Record `records` as further steps of the run file `writer` is writing.
pub(crate) fn append(writer: &mut RunFileWriter, records: &[RunRecord]) {
    let mut at = 0;
    let stages = writer_stages(writer);
    for record in records {
        at = at_of(record).unwrap_or(at);
        let mut state = writer.state().clone();
        place(&mut state, &stages, record);
        writer
            .record(state.clone(), at, Vec::new())
            .expect("a step");
        let events = journal_events(record);
        if !events.is_empty() {
            writer.record(state, at, events).expect("a step");
        }
    }
}

/// The stages of the run `writer` is writing, in the graph's order.
fn writer_stages(writer: &RunFileWriter) -> Vec<StageName> {
    RunFileReader::open(writer.path())
        .expect("the run file reads")
        .spec()
        .graph
        .stages
        .iter()
        .map(|stage| stage.name.clone())
        .collect()
}
