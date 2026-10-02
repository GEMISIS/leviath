//! Journal records of every kind, and how each one reads.

use leviath_legacy_runs::journal::JournalRecord;
use leviath_runtime::state::{RunEvent, RunStatus};
use serde_json::json;

use crate::common::Run;

fn events(run: &Run) -> Vec<RunEvent> {
    let (_, file) = run.converted();
    assert_eq!(file.fold(), file.last);
    file.deltas.into_iter().flat_map(|d| d.events).collect()
}

fn logged(events: &[RunEvent], needle: &str) -> bool {
    events
        .iter()
        .any(|e| matches!(e, RunEvent::Log(l) if l.contains(needle)))
}

#[test]
fn every_kind_of_record_reads_as_an_event_or_a_change() {
    let run = Run::fixture("finished");
    let header = run.records()[0].clone();
    let JournalRecord::Header { meta, .. } = &header else {
        panic!("the journal starts with its header");
    };
    let checkpoint = json!({"Checkpoint": {
        "meta": serde_json::to_value(meta).unwrap(),
        "context": {"stage_name": "main", "total_tokens": 0, "max_tokens": 100, "regions": []},
        "at": 5
    }});
    let failed = json!({"failed": {"kind": "auth", "transient": false, "capacity": false, "next": "reported"}});
    let recorded = run
        .records()
        .into_iter()
        .find(|r| matches!(r, JournalRecord::InferenceAttempt(_)))
        .unwrap();
    let attempt = |outcome: serde_json::Value| {
        let mut a = serde_json::to_value(&recorded).unwrap();
        a["InferenceAttempt"]["id"] = json!("a1");
        a["InferenceAttempt"]["outcome"] = outcome;
        a
    };
    let usage = |provider: &str, model: &str, cost: serde_json::Value| {
        json!({"InferenceUsage": {
            "kind": "stage", "stage": "main", "iteration": 3, "provider": provider, "model": model,
            "prompt_tokens": 1, "completion_tokens": 1, "cached_tokens": 0, "cache_write_tokens": 0,
            "cost_usd": cost, "cost_reported_by_provider": true, "at": 6
        }})
    };
    let failover = |to: &str| {
        json!({"InferenceFailover": {
            "stage": "main", "iteration": 3, "from_provider": "openai", "from_model": "gpt-mock",
            "to_provider": to, "to_model": "m2", "reason": "down", "kind": "unavailable", "at": 7
        }})
    };
    run.journal(|r| r.push(header.clone()));
    run.append(json!([
        checkpoint,
        {"ContextDiff": {"delta": {"stage_name": "main", "total_tokens": 1, "max_tokens": 100,
            "regions": [{"Set": {"name": "task", "kind": "pinned", "current_tokens": 1, "max_tokens": 100,
                "entries": [{"content": "again", "tokens": 1}]}}]}, "at": 8}},
        attempt(failed),
        usage("openai", "gpt-mock", json!(0.5)),
        usage("openai", "has space", json!(null)),
        failover("other"),
        failover(""),
        {"ToolBatch": {"calls": [
            {"id": "c2", "execution_id": "x2", "name": "shell", "arguments": "not json", "result": "inline"}
        ], "at": 9, "stage_index": 0, "iteration": 3, "response": ""}},
        {"ToolCallDone": {"iteration": 3, "call_id": "c2", "execution_id": "x2", "result": "no",
            "outcome": "failed", "at": 10}},
        {"Interaction": {"request_id": "q1", "kind": "confirm", "prompt": "ok?", "stage": "main",
            "settlement": "timed_out", "asked_at": 10, "at": 11}},
        {"Message": {"message": {"role": "user", "content": "hello"}, "at": 12}},
        {"OwnershipChanged": {"machine_id": "m2", "world_id": "w2", "at": 13}},
        {"ArtifactsProduced": {"execution_id": "x2", "artifacts": [], "at": 14}},
        {"Inference": {"stage": "main", "iteration": 3,
            "request": {"model": "gpt-mock", "system": [], "messages": [], "tool_names": [],
                "temperature": 0.0, "max_tokens": 1},
            "response": {"content": "", "tool_calls": [], "prompt_tokens": 1, "completion_tokens": 1,
                "cached_tokens": 0, "cache_write_tokens": 0},
            "at": 15}}
    ]));
    let events = events(&run);
    assert!(logged(
        &events,
        "attempt a1 on openai/gpt-mock did not answer"
    ));
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Inference { attempt, spend, .. } if attempt == "a1" && spend.reported_calls == 1
    )));
    assert!(logged(&events, "which are not valid names"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::Failover { reason, .. } if reason == "down"))
    );
    assert!(logged(&events, "failed over from openai/gpt-mock to /m2"));
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::ToolStarted(c) if c.args.value() == &json!("not json")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::ToolFinished { result, .. } if result.text == "inline" && !result.is_error
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::ToolFinished { result, .. } if result.text == "no" && result.is_error
    )));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::Answered { id, .. } if id == "q1"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::Message(m) if m.text == "hello"))
    );
    assert!(logged(&events, "machine m2"));
    assert!(logged(&events, "produced 0 files"));
    assert!(logged(&events, "a model call in stage main"));
}

#[test]
fn a_journal_that_starts_with_a_full_checkpoint_seeds_from_it() {
    let run = Run::fixture("finished");
    run.journal(|records| {
        let JournalRecord::Header { meta, .. } = &records[0] else {
            panic!("the journal starts with its header");
        };
        let meta = meta.clone();
        let JournalRecord::ContextCheckpoint { snapshot, at } = records[1].clone() else {
            panic!("the second record is the first checkpoint");
        };
        records[1] = JournalRecord::Checkpoint {
            meta,
            context: snapshot,
            at,
        };
    });
    let (_, file) = run.converted();
    assert!(file.spec.seeded.contains_key("task"));
    assert_eq!(file.fold(), file.last);
}

#[test]
fn a_batch_with_some_results_back_keeps_them() {
    let run = Run::fixture("mid-tool-batch");
    run.journal(|records| {
        for r in records.iter_mut() {
            if let JournalRecord::ToolBatch { calls, .. } = r {
                let mut second = calls[0].clone();
                second.id = "call_2".into();
                second.arguments = "{".into();
                calls.push(second);
            }
        }
    });
    run.append(json!([{"ToolCallDone": {"iteration": 1, "call_id": "call_1", "result": "slept", "at": 1}}]));
    let (report, file) = run.converted();
    let batch = file.last.pending.as_ref().unwrap();
    assert_eq!(batch.calls.len(), 2);
    assert_eq!(batch.calls[1].args.value(), &json!("{"));
    assert_eq!(batch.done["call_1"].text, "slept");
    assert!(report.defaulted("pending.done.*.is_error").is_some());
    assert_eq!(file.last.status, RunStatus::Active);
}

/// Every step is stamped no earlier than the one before it, the last one
/// too, even when the run's metadata says it last moved before its journal
/// did.
#[test]
fn every_step_is_stamped_no_earlier_than_the_one_before() {
    let run = Run::fixture("finished");
    let first = run.records()[0].clone();
    let JournalRecord::Header { meta, .. } = &first else {
        panic!("the journal starts with its header");
    };
    let early = meta.started_at - 100;
    run.meta(|m| m.updated_at = early);
    let (_, file) = run.converted();
    let stamps: Vec<i64> = file.deltas.iter().map(|d| d.at).collect();
    assert!(stamps.len() > 1);
    assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "{stamps:?}");
}
