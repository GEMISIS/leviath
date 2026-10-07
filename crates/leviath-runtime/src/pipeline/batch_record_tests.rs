use std::collections::HashMap;

use super::*;
use leviath_core::execution::ToolOutcome;
use leviath_core::region::EntryContent;

fn call(id: &str) -> crate::components::ToolCall {
    crate::components::ToolCall {
        tool_id: id.to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    }
}

fn lane_call(id: &str) -> leviath_providers::ToolCall {
    leviath_providers::ToolCall {
        id: id.to_string(),
        name: "ask_user_text".to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    }
}

/// The records of a batch over calls `a`, `b`, `c` and `q`, executions `xa`
/// to `xq`: `a` finished before a restart, `b` was interrupted by it, `c` is
/// refused now, and `q` goes to the lane.
fn records(resumed: bool) -> Vec<RunRecord> {
    let calls = ["a", "b", "c", "q"].map(call);
    let executions: HashMap<String, String> = ["a", "b", "c", "q"]
        .map(|id| (id.to_string(), format!("x{id}")))
        .into();
    let inline = [("c".to_string(), "[error] refused".to_string())];
    let recovered = [
        ("a".to_string(), EntryContent::from("ran a".to_string())),
        (
            "b".to_string(),
            EntryContent::from(crate::restore::INTERRUPTED_TOOL_RESULT.to_string()),
        ),
    ];
    BatchDispatch {
        calls: &calls,
        executions: &executions,
        inline: &inline,
        recovered: &recovered,
        resumed,
        stage_index: 0,
        iteration: 2,
        visit_id: "v",
        requested_by: "r",
        response: "",
    }
    .records(&[lane_call("q")])
}

/// A batch the run's file already records is not recorded again: the results
/// settled now end the executions they belong to, the interrupted one
/// unobserved, a result the file holds is left alone, and the call going to
/// the lane is sent again as the execution it was.
#[test]
fn a_resumed_batch_records_only_what_is_new() {
    let said: Vec<String> = records(true)
        .iter()
        .map(|r| match r {
            RunRecord::ToolCallDone {
                call_id,
                execution_id,
                outcome,
                iteration,
                ..
            } => format!("done {call_id} {execution_id} {outcome:?} {iteration}"),
            RunRecord::ToolCallsResent {
                calls,
                requested_by,
                ..
            } => format!("resent {calls:?} {requested_by}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        said,
        [
            format!("done c xc {:?} 2", Some(ToolOutcome::Failed)),
            format!("done b xb {:?} 2", Some(ToolOutcome::Indeterminate)),
            r#"resent [("q", "xq")] r"#.to_string(),
        ]
    );
}

/// A batch dispatched the first time is one batch record, every call in it.
#[test]
fn a_new_batch_is_one_batch_record() {
    let records = records(false);
    let [RunRecord::ToolBatch { calls, .. }] = records.as_slice() else {
        panic!("one batch record: {records:?}");
    };
    let ids: Vec<(&str, &str)> = calls
        .iter()
        .map(|c| (c.id.as_str(), c.execution_id.as_str()))
        .collect();
    assert_eq!(ids, [("a", "xa"), ("b", "xb"), ("c", "xc"), ("q", "xq")]);
}

/// A resumed batch with nothing settled and nothing to send records nothing.
#[test]
fn a_resumed_batch_with_nothing_new_records_nothing() {
    let calls = [call("a")];
    let executions = HashMap::new();
    let recovered = [("a".to_string(), EntryContent::from("ran a".to_string()))];
    let records = BatchDispatch {
        calls: &calls,
        executions: &executions,
        inline: &[],
        recovered: &recovered,
        resumed: true,
        stage_index: 0,
        iteration: 0,
        visit_id: "",
        requested_by: "",
        response: "",
    }
    .records(&[]);
    assert!(records.is_empty());
}
