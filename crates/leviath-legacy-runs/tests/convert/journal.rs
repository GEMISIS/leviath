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
    let failover = |to: &str, model: &str| {
        json!({"InferenceFailover": {
            "stage": "main", "iteration": 3, "from_provider": "openai", "from_model": "gpt-mock",
            "to_provider": to, "to_model": model, "reason": "down", "kind": "unavailable", "at": 7
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
        failover("other", "m2"),
        failover("", "m2"),
        failover("other", "has space"),
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
        "model call a1 on openai/gpt-mock failed: auth"
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::Attempt(a) if a.id == "a1"))
    );
    // A call that failed answered nothing, so the bill after it is for no
    // attempt of its.
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Inference { attempt, spend, .. } if attempt.is_empty() && spend.reported_calls == 1
    )));
    assert!(logged(&events, "which are not valid names"));
    let moves: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Failover { to, reason, .. } if reason == "down" => Some(to.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(moves, ["other/m2", "m2"]);
    assert!(logged(
        &events,
        "failed over from openai/gpt-mock to other/has space"
    ));
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
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Artifacts { execution_id, .. } if execution_id == "x2"
    )));
    assert!(logged(&events, "a model call in stage main"));
}

/// Why the window changed reads back from a converted run the way it did
/// from the old one: a committed change as a commit, and a change recorded
/// one region at a time as a note on that region.
#[test]
fn the_causes_of_context_changes_carry_over() {
    let run = Run::fixture("finished");
    run.append(json!([
        {"ContextChange": {"region": "notes", "cause": "context_tool", "entries_added": 2,
            "entries_removed": 1, "token_delta": 40, "at": 20}},
        {"ContextTransaction": {"revision_before": "cw1-a", "revision_after": "cw1-b",
            "cause": "tool_result", "regions": [{"region": "plan", "digest_before": "d1",
            "digest_after": "d2", "tokens_before": 1, "tokens_after": 5, "entries_before": 0,
            "entries_after": 1, "entries_added": 1}], "execution_id": "x1", "at": 21}}
    ]));
    let events = events(&run);
    let noted: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::ContextNoted(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(noted.len(), 1, "{events:?}");
    assert_eq!(noted[0].region, "notes");
    assert_eq!(
        noted[0].cause,
        leviath_runtime::state::journal::CauseState::ContextTool
    );
    assert_eq!(
        (
            noted[0].entries_added,
            noted[0].entries_removed,
            noted[0].token_delta
        ),
        (2, 1, 40)
    );
    let committed: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::ContextCommitted(c) => Some(c),
            _ => None,
        })
        .filter(|c| c.execution_id.as_deref() == Some("x1"))
        .collect();
    assert_eq!(committed.len(), 1, "{events:?}");
    assert_eq!(committed[0].regions[0].region, "plan");
    // The fixture's own commits came across too: the run was a model reply
    // and a tool result before this.
    let causes = events
        .iter()
        .filter(|e| matches!(e, RunEvent::ContextCommitted(_)))
        .count();
    assert_eq!(causes, 4);
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

/// Every record that held the window is a point in the converted run's
/// history, as it was in the release that wrote it, even one that left the
/// window as it was: the first is the state the run started in, and each
/// later one is a delta that carries the window.
#[test]
fn every_record_that_held_the_window_is_a_point_in_the_history() {
    for name in ["real-finished", "finished"] {
        let run = Run::fixture(name);
        // A step that moved nothing but the totals, as most of an old run's
        // steps are.
        let progress = run
            .records()
            .into_iter()
            .rfind(|r| matches!(r, JournalRecord::Progress { .. }))
            .expect("a progress step");
        run.journal(|records| {
            let at = records.len() - 1;
            records.insert(at, progress);
        });
        let held = run
            .records()
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    JournalRecord::ContextCheckpoint { .. }
                        | JournalRecord::ContextDiff { .. }
                        | JournalRecord::Progress { .. }
                        | JournalRecord::Checkpoint { .. }
                )
            })
            .count();
        let (_, file) = run.converted();
        assert_eq!(file.fold(), file.last, "{name}");
        let points = 1 + file
            .deltas
            .iter()
            .filter(|d| {
                d.changes
                    .iter()
                    .any(|c| matches!(c, leviath_runtime::state::Change::Context(_)))
            })
            .count();
        assert_eq!(points, held, "{name}");
    }
}

/// A converted run's model calls and tool executions read back as the
/// release that wrote it served them: each attempt whole (how it ended, how
/// long it took, the digest of what it sent), the title call billed apart
/// from the stage's calls, and each execution under the id it was dispatched
/// with, in the iteration whose answer asked for it, ended as its record
/// says. A step is read against the cursor before it, as every reader of a
/// run file reads one.
#[test]
fn model_calls_and_executions_read_back_as_recorded() {
    use leviath_runtime::state::journal::CallKind;
    let run = Run::fixture("finished");
    let recorded: Vec<_> = run
        .records()
        .into_iter()
        .filter_map(|r| match r {
            JournalRecord::InferenceAttempt(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(recorded.len(), 2);
    let (_, file) = run.converted();
    let mut state = file.states[0].clone();
    let mut attempts = Vec::new();
    let mut titles = Vec::new();
    let mut started = Vec::new();
    let mut dispatched = Vec::new();
    let mut completed = Vec::new();
    for d in &file.deltas {
        for e in &d.events {
            match e {
                RunEvent::Attempt(a) => attempts.push((**a).clone()),
                RunEvent::Inference {
                    kind: CallKind::Title,
                    stage,
                    ..
                } => titles.push(stage.clone()),
                RunEvent::ToolStarted(c) => started.push((c.id.clone(), state.cursor.iteration)),
                RunEvent::Dispatched {
                    call_id,
                    execution_id,
                    requested_by,
                } => dispatched.push((call_id.clone(), execution_id.clone(), requested_by.clone())),
                RunEvent::Completed {
                    call_id, outcome, ..
                } => completed.push((call_id.clone(), *outcome)),
                _ => {}
            }
        }
        d.apply(&mut state);
    }
    assert_eq!(attempts.len(), recorded.len(), "{attempts:?}");
    for (kept, a) in attempts.iter().zip(&recorded) {
        assert_eq!(kept.id, a.id);
        assert_eq!(kept.duration_ms, a.duration_ms);
        assert_eq!(
            kept.finish_reason.as_deref(),
            Some(a.finish_reason.as_str())
        );
        assert_eq!(kept.digest.system_hash, a.digest.system_hash);
        assert_eq!(kept.digest.messages as usize, a.digest.messages);
        assert_eq!(kept.digest.tools as usize, a.digest.tools);
        assert_eq!(kept.digest.max_tokens as usize, a.digest.max_tokens);
        assert_eq!(kept.digest.temperature, a.digest.temperature);
    }
    assert_eq!(titles, vec![None], "the title call belongs to no stage");
    assert_eq!(started, vec![("call_1".to_string(), 1)]);
    assert_eq!(
        dispatched,
        vec![(
            "call_1".to_string(),
            "x18da3de08fc75b28-00000002".to_string(),
            recorded[0].id.clone()
        )]
    );
    assert_eq!(
        completed,
        vec![("call_1".to_string(), None)],
        "the record did not say how the call ended"
    );
    assert_eq!(file.fold(), file.last);

    // A journal that billed no call before its batch still reads the batch
    // in the iteration it was dispatched in.
    let run = Run::fixture("finished");
    run.journal(|records| records.retain(|r| !matches!(r, JournalRecord::InferenceUsage { .. })));
    let (_, file) = run.converted();
    let mut state = file.states[0].clone();
    let mut started = Vec::new();
    for d in &file.deltas {
        for e in &d.events {
            if let RunEvent::ToolStarted(c) = e {
                started.push((c.id.clone(), state.cursor.iteration));
            }
        }
        d.apply(&mut state);
    }
    assert_eq!(started, vec![("call_1".to_string(), 1)]);
    assert_eq!(file.fold(), file.last);
}

/// A model call is read in the stage its records name. An old journal wrote
/// a call's attempt and its bill as the call was made, and the step that
/// moved the run into the stage only at its next save, after them; the call
/// still reads as made in the stage it was made in, as the release that
/// wrote it served it, and the stage is visited once.
#[test]
fn a_model_call_reads_in_the_stage_its_records_name() {
    let run = Run::fixture("finished");
    let text = std::fs::read_to_string(run.path("blueprint.leviath")).unwrap();
    run.write(
        "blueprint.leviath",
        &format!(
            "{text}\n[stages.merge]\nmode = \"autonomous\"\nmodel = {{ models = [{{ provider = \"openai\", model = \"gpt-mock\" }}] }}\n"
        ),
    );
    let second = "a18da3de090c500c0-00000003";
    run.journal(|records| {
        let at = records
            .iter()
            .position(|r| matches!(r, JournalRecord::InferenceAttempt(a) if a.id == second))
            .expect("the second call's attempt");
        for r in records[at..].iter_mut() {
            match r {
                JournalRecord::InferenceAttempt(a) => a.stage = "merge".into(),
                JournalRecord::InferenceUsage { stage, .. } => *stage = "merge".into(),
                JournalRecord::Progress { meta, .. } => meta.current_stage = "merge".into(),
                _ => {}
            }
        }
    });
    run.json("meta.json", |v| v["current_stage"] = json!("merge"));
    let (_, file) = run.converted();
    let mut state = file.states[0].clone();
    let mut read = Vec::new();
    for d in &file.deltas {
        for e in &d.events {
            if let RunEvent::Attempt(a) = e {
                read.push((a.id.clone(), state.cursor.stage.to_string()));
            }
        }
        d.apply(&mut state);
    }
    assert_eq!(
        read,
        vec![
            ("a18da3de08fb580d8-00000001".to_string(), "main".to_string()),
            (second.to_string(), "merge".to_string()),
        ]
    );
    assert_eq!(file.last.cursor.stage.as_str(), "merge");
    assert_eq!(file.last.visits.get("merge"), Some(&1));
    assert_eq!(file.fold(), file.last);
}
