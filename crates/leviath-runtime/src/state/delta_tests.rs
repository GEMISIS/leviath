//! Property tests for state deltas: the delta between any two states takes
//! the first to the second, and a run file's deltas replayed from its first
//! state reach every state it recorded.
//!
//! A state is generated from the two fixtures [`base`] and [`busy`], which
//! differ in every field: each change from one to the other is taken or
//! left, so a field added to [`RunState`] is generated as soon as the
//! fixtures set it. The context and the stage ledger, which a delta records
//! piece by piece rather than whole, are generated in full on top.

use proptest::prelude::*;

use super::context::{ContextState, EntryKind, EntryMeta, EntryState, RegionState, TaintState};
use super::tests::{base, busy};
use super::*;
use crate::spec::names::RegionName;
use leviath_core::taint::TaintLevel;

/// The changes that take [`base`] to [`busy`], one per field.
fn field_changes() -> Vec<Change> {
    StateDelta::between(&base(), &busy(), 0, Vec::new()).changes
}

/// One entry: a few texts and kinds, so two entries are often equal and a
/// region often only grows.
fn entry() -> impl Strategy<Value = EntryState> {
    (0..3usize, 0..3usize, 0..3u32).prop_map(|(text, kind, tokens)| EntryState {
        text: ["a", "b", "c"][text].to_string(),
        parts: Vec::new(),
        tokens,
        timestamp: 1,
        kind: match kind {
            0 => EntryKind::Text,
            1 => EntryKind::UserMessage,
            _ => EntryKind::ToolResult {
                call_id: "c1".into(),
                tool: "read_file".into(),
                is_error: false,
            },
        },
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    })
}

/// One region named `name`.
fn region(name: &'static str) -> impl Strategy<Value = RegionState> {
    (
        prop::collection::vec(entry(), 0..4),
        0..3u32,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(move |(entries, size, compact, tainted)| RegionState {
            name: RegionName::new(name).unwrap(),
            max_tokens: size * 100,
            current_tokens: entries.iter().map(|e| e.tokens).sum(),
            needs_message_compaction: compact,
            taint: tainted.then(|| TaintState {
                level: TaintLevel::Private,
                entries: vec![TaintLevel::Private; entries.len()],
            }),
            entries,
        })
}

/// A context: some of four regions, in any order, some of them hidden.
fn context() -> impl Strategy<Value = ContextState> {
    let names = ["system", "task", "notes", "conversation"];
    (
        prop::sample::subsequence(names.to_vec(), 0..=4).prop_shuffle(),
        prop::sample::subsequence(names.to_vec(), 0..=2),
        0..3u32,
    )
        .prop_flat_map(|(order, hidden, size)| {
            let regions: Vec<_> = order.into_iter().map(region).collect();
            (regions, Just(hidden), Just(size))
        })
        .prop_map(|(regions, hidden, size)| ContextState {
            regions,
            hidden: hidden
                .into_iter()
                .map(|n| RegionName::new(n).unwrap())
                .collect(),
            max_tokens: size * 1000,
        })
}

/// A stage ledger: up to three records, each [`busy`]'s one or that with
/// another status.
fn ledger() -> impl Strategy<Value = Vec<StageRecord>> {
    let record = busy().ledger[0].clone();
    prop::collection::vec(any::<bool>(), 0..4).prop_map(move |done| {
        done.into_iter()
            .map(|done| StageRecord {
                status: match done {
                    true => StageStatus::Complete,
                    false => StageStatus::Pending,
                },
                ..record.clone()
            })
            .collect()
    })
}

/// A state: [`base`] with each field changed to [`busy`]'s when its bit in
/// the mask is set, and a context and a ledger of its own.
fn state() -> impl Strategy<Value = RunState> {
    (any::<u64>(), context(), ledger()).prop_map(|(mask, context, ledger)| {
        let mut s = base();
        for (i, change) in field_changes().into_iter().enumerate() {
            if mask & (1 << i) != 0 {
                let one = StateDelta {
                    seq: 0,
                    at: 0,
                    changes: vec![change],
                    events: Vec::new(),
                };
                one.apply(&mut s);
            }
        }
        s.context = context;
        s.ledger = ledger;
        s
    })
}

/// The fixtures differ in every field a delta records, so the generator
/// reaches every kind of change.
#[test]
fn the_fixtures_differ_in_every_field() {
    let kinds = |changes: Vec<Change>| -> std::collections::BTreeSet<String> {
        changes
            .iter()
            .map(|c| format!("{c:?}").split('(').next().unwrap().to_string())
            .collect()
    };
    let mut all = kinds(field_changes());
    all.extend(kinds(
        StateDelta::between(&busy(), &base(), 0, Vec::new()).changes,
    ));
    assert_eq!(all.len(), 31, "{all:?}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The delta between two states, applied to the first, is the second.
    #[test]
    fn a_delta_takes_a_state_to_the_next(a in state(), b in state()) {
        let delta = StateDelta::between(&a, &b, 1, Vec::new());
        let mut replayed = a.clone();
        delta.apply(&mut replayed);
        let mut expected = b;
        expected.seq = a.seq + 1;
        prop_assert_eq!(replayed, expected);
    }

    /// A run file of generated steps, checkpointed every other step: every
    /// step its deltas reach from its first state is the state recorded
    /// there, and the same as the file reads at that step from its
    /// checkpoints.
    #[test]
    fn a_run_files_deltas_reach_every_state_it_recorded(
        steps in prop::collection::vec(state(), 1..6),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lvr");
        let spec = crate::spec::run_spec::tests::spec();
        let first = base();
        let policy = crate::runfile::CheckpointPolicy { every: 2, size_ratio: 1e9 };
        let mut writer =
            crate::runfile::RunFileWriter::create(&path, &spec, &Default::default(), &first, policy)
                .unwrap();
        let mut recorded = Vec::new();
        for step in steps {
            if let Some(seq) = writer.record(step.clone(), 1, Vec::new()).unwrap() {
                let mut at = step;
                at.seq = seq;
                recorded.push(at);
            }
        }
        let file = crate::runfile::RunFileReader::open(&path).unwrap();
        let mut replayed = first;
        for (delta, expected) in file.deltas(1, file.last_seq()).unwrap().iter().zip(&recorded) {
            delta.apply(&mut replayed);
            prop_assert_eq!(&replayed, expected);
            prop_assert_eq!(&file.state_at(expected.seq).unwrap(), expected);
        }
        prop_assert_eq!(replayed.seq as usize, recorded.len());
    }
}
