//! Reading an old journal's bytes, and folding its records.

use super::*;
use leviath_core::region::{EntryKind, SerializedToolCall};

fn meta() -> RunMeta {
    let mut meta = RunMeta::new(
        "run-1".to_string(),
        "coder".to_string(),
        "/agents/coder".to_string(),
        "do it".to_string(),
        None,
        "/work".to_string(),
        2,
    );
    meta.started_at = 1_700_000_000;
    meta.updated_at = 1_700_000_000;
    meta
}

fn header() -> JournalRecord {
    JournalRecord::Header {
        identity: RunIdentity {
            run_id: "run-1".to_string(),
            machine_id: "m".to_string(),
            world_id: "w".to_string(),
            created_at: 1,
        },
        meta: Box::new(meta()),
    }
}

fn entry(content: &str, tokens: usize) -> RegionEntrySnapshot {
    RegionEntrySnapshot {
        content: content.into(),
        tokens,
        kind: EntryKind::Text,
        metadata: None,
        key: None,
        taint: Default::default(),
        reasoning: None,
    }
}

fn turn(call_ids: &[&str]) -> RegionEntrySnapshot {
    RegionEntrySnapshot {
        kind: EntryKind::AssistantTurn {
            tool_calls: call_ids
                .iter()
                .map(|id| SerializedToolCall {
                    id: id.to_string(),
                    name: "shell".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                })
                .collect(),
        },
        ..entry("", 1)
    }
}

fn region(name: &str, entries: Vec<RegionEntrySnapshot>) -> RegionSnapshot {
    RegionSnapshot {
        name: name.to_string(),
        kind: "clearable".to_string(),
        current_tokens: entries.iter().map(|e| e.tokens).sum(),
        max_tokens: 1000,
        entries,
        description: None,
    }
}

fn window(regions: Vec<RegionSnapshot>) -> ContextSnapshot {
    ContextSnapshot {
        stage_name: "s1".to_string(),
        total_tokens: 0,
        max_tokens: 10_000,
        regions,
    }
}

fn call(id: &str, result: Option<&str>) -> ToolCallRecord {
    ToolCallRecord {
        id: id.to_string(),
        execution_id: String::new(),
        name: "shell".to_string(),
        arguments: "{}".to_string(),
        result: result.map(|r| r.to_string().into()),
        thought_signature: None,
    }
}

fn batch(iteration: usize, calls: Vec<ToolCallRecord>) -> JournalRecord {
    JournalRecord::ToolBatch {
        calls,
        at: 1,
        stage_index: 0,
        iteration,
        visit_id: String::new(),
        requested_by: String::new(),
        response: String::new(),
    }
}

fn done(iteration: usize, call_id: &str, result: &str) -> JournalRecord {
    JournalRecord::ToolCallDone {
        iteration,
        call_id: call_id.to_string(),
        execution_id: String::new(),
        result: result.to_string().into(),
        outcome: None,
        at: 2,
    }
}

/// The bytes of a journal holding `records`.
fn framed(records: &[JournalRecord]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend(1u16.to_be_bytes());
    for r in records {
        let payload = serde_json::to_vec(r).unwrap();
        out.extend((payload.len() as u64).to_be_bytes());
        out.extend(payload);
    }
    out
}

#[test]
fn every_record_reads_back_as_it_was_written() {
    let records = vec![
        header(),
        JournalRecord::Message {
            message: MessageRecord {
                role: "user".to_string(),
                content: "hi".to_string(),
            },
            at: 3,
        },
        batch(1, vec![call("c1", None)]),
    ];
    assert_eq!(read(&framed(&records)).unwrap(), records);
}

#[test]
fn a_file_that_is_not_a_journal_is_refused() {
    assert!(read(b"LV").unwrap_err().contains("too short"));
    assert!(read(b"LVR2xx").unwrap_err().contains("not an LVR1"));
    assert!(read(b"LVR1x").unwrap_err().contains("before its version"));
    let mut newer = MAGIC.to_vec();
    newer.extend(2u16.to_be_bytes());
    assert!(read(&newer).unwrap_err().contains("version 2"));
}

/// A record kind this build does not know is stepped over, and a torn tail
/// ends the read with what came before it.
#[test]
fn an_unknown_record_is_skipped_and_a_torn_tail_ends_the_read() {
    let mut bytes = framed(&[header()]);
    let unknown = br#"{"FutureKind":{}}"#;
    bytes.extend((unknown.len() as u64).to_be_bytes());
    bytes.extend(unknown);
    bytes.extend(framed(&[done(1, "c1", "ok")])[6..].iter());
    let whole = bytes.clone();
    assert_eq!(read(&whole).unwrap().len(), 2);

    let mut torn_payload = whole.clone();
    torn_payload.extend(100u64.to_be_bytes());
    torn_payload.extend(b"{\"Head");
    assert_eq!(read(&torn_payload).unwrap().len(), 2);

    let mut torn_length = whole.clone();
    torn_length.extend([0, 0, 1]);
    assert_eq!(read(&torn_length).unwrap().len(), 2);

    let mut absurd = whole;
    absurd.extend(u64::MAX.to_be_bytes());
    absurd.extend(b"{}");
    assert_eq!(read(&absurd).unwrap().len(), 2);
}

#[test]
fn a_journal_without_its_header_does_not_fold() {
    assert_eq!(fold(&[]), None);
    assert_eq!(fold(&[done(1, "c1", "ok")]), None);
}

/// The window is rebuilt from checkpoints and the diffs over them, and the
/// metadata is the last one written.
#[test]
fn the_window_and_metadata_are_where_the_journal_left_them() {
    let mut later = meta();
    later.iteration = 4;
    let mut last = meta();
    last.iteration = 5;
    let records = vec![
        header(),
        JournalRecord::ContextCheckpoint {
            snapshot: window(vec![
                region("conv", vec![entry("a", 1)]),
                region("plan", vec![entry("p", 2)]),
                region("gone", vec![entry("g", 1)]),
                region("same", vec![entry("s", 1)]),
            ]),
            at: 1,
        },
        JournalRecord::ContextDiff {
            delta: ContextDelta {
                stage_name: "s2".to_string(),
                total_tokens: 9,
                max_tokens: 10_000,
                regions: vec![
                    RegionDelta::Append {
                        name: "conv".to_string(),
                        entries: vec![entry("b", 1)],
                        current_tokens: 2,
                    },
                    RegionDelta::Append {
                        name: "nowhere".to_string(),
                        entries: vec![entry("x", 1)],
                        current_tokens: 1,
                    },
                    RegionDelta::Clear {
                        name: "plan".to_string(),
                    },
                    RegionDelta::Clear {
                        name: "nowhere".to_string(),
                    },
                    RegionDelta::Remove {
                        name: "gone".to_string(),
                    },
                    RegionDelta::Set(region("same", vec![entry("t", 3)])),
                    RegionDelta::Set(region("new", vec![entry("n", 1)])),
                ],
            },
            at: 2,
        },
        JournalRecord::Header {
            identity: RunIdentity {
                run_id: "run-1".to_string(),
                machine_id: "m2".to_string(),
                world_id: "w2".to_string(),
                created_at: 1,
            },
            meta: Box::new(later),
        },
        JournalRecord::StatusChanged {
            status: RunStatus::Paused,
            at: 3,
        },
        JournalRecord::Message {
            message: MessageRecord {
                role: "user".to_string(),
                content: "ignored by the fold".to_string(),
            },
            at: 3,
        },
    ];
    let folded = fold(&records).unwrap();
    assert_eq!(folded.meta.iteration, 4);
    assert_eq!(folded.meta.status, RunStatus::Paused);
    assert_eq!(folded.context.stage_name, "s2");
    let names: Vec<&str> = folded
        .context
        .regions
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, ["conv", "plan", "same", "new"]);
    assert_eq!(folded.context.regions[0].entries.len(), 2);
    assert_eq!(folded.context.regions[1].current_tokens, 0);
    assert_eq!(folded.context.regions[2].entries[0].content, "t");

    let records = vec![
        header(),
        JournalRecord::Checkpoint {
            meta: Box::new(meta()),
            context: window(vec![region("conv", vec![entry("a", 1)])]),
            at: 4,
        },
        JournalRecord::Progress {
            meta: Box::new(last),
            delta: ContextDelta {
                stage_name: "s3".to_string(),
                total_tokens: 1,
                max_tokens: 10_000,
                regions: Vec::new(),
            },
            at: 5,
        },
    ];
    let folded = fold(&records).unwrap();
    assert_eq!(folded.meta.iteration, 5);
    assert_eq!(folded.context.stage_name, "s3");
    assert_eq!(folded.context.regions.len(), 1);
}

/// A batch still in flight comes back with every result that was recorded,
/// and a completion for another batch or another call changes nothing.
#[test]
fn a_batch_in_flight_keeps_the_results_that_came_back() {
    let records = vec![
        header(),
        batch(0, vec![call("c1", None), call("c2", Some("inline"))]),
        done(0, "c1", "ran"),
        done(7, "c1", "stale"),
        done(0, "c9", "unknown"),
    ];
    let pending = fold(&records).unwrap().pending_batch.unwrap();
    let results: Vec<Option<&str>> = pending
        .calls
        .iter()
        .map(|c| c.result.as_ref().map(|r| r.as_str()))
        .collect();
    assert_eq!(results, [Some("ran"), Some("inline")]);
}

/// A batch the dispatcher settled by itself is never pending, and neither is
/// one a later call moved past or whose turn is already in the window.
#[test]
fn a_batch_that_landed_is_not_pending() {
    let settled = vec![header(), batch(0, vec![call("c1", Some("ok"))])];
    assert_eq!(fold(&settled).unwrap().pending_batch, None);

    let moved_on = vec![header(), batch(3, vec![call("c1", None)])];
    assert_eq!(fold(&moved_on).unwrap().pending_batch, None);

    let landed = vec![
        header(),
        JournalRecord::ContextCheckpoint {
            snapshot: window(vec![region(
                "conv",
                vec![entry("text", 1), turn(&["other"]), turn(&["c1"])],
            )]),
            at: 1,
        },
        batch(0, vec![call("c1", None)]),
    ];
    assert_eq!(fold(&landed).unwrap().pending_batch, None);

    let elsewhere = vec![
        header(),
        JournalRecord::ContextCheckpoint {
            snapshot: window(vec![region("conv", vec![turn(&["other"])])]),
            at: 1,
        },
        batch(0, vec![call("c1", None)]),
    ];
    assert!(fold(&elsewhere).unwrap().pending_batch.is_some());
}

/// A batch with no calls matches no turn in the window.
#[test]
fn an_empty_batch_matches_no_turn() {
    let batch = PendingToolBatch {
        iteration: 0,
        calls: Vec::new(),
    };
    let snapshot = window(vec![region("conv", vec![turn(&["c1"])])]);
    assert!(!context_contains_batch(&snapshot, &batch));
}

/// The first point of an old run's history is the first record that held its
/// window, whichever kind it is, and a journal with none has no first point.
#[test]
fn the_first_point_is_the_first_record_that_held_the_window() {
    let delta = || ContextDelta {
        stage_name: "s".to_string(),
        total_tokens: 0,
        max_tokens: 10_000,
        regions: Vec::new(),
    };
    let status = JournalRecord::StatusChanged {
        status: RunStatus::Running,
        at: 1,
    };
    let held = [
        JournalRecord::ContextCheckpoint {
            snapshot: window(Vec::new()),
            at: 2,
        },
        JournalRecord::ContextDiff {
            delta: delta(),
            at: 3,
        },
        JournalRecord::Progress {
            meta: Box::new(meta()),
            delta: delta(),
            at: 4,
        },
        JournalRecord::Checkpoint {
            meta: Box::new(meta()),
            context: window(Vec::new()),
            at: 5,
        },
    ];
    for (record, at) in held.into_iter().zip(2..) {
        assert_eq!(
            first_point_at(&[header(), status.clone(), record]),
            Some(at)
        );
    }
    assert_eq!(first_point_at(&[header(), status]), None);
}
