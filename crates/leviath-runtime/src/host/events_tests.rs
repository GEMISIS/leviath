//! What a tool-call frame carries of a call's arguments, and the new frames'
//! wire shape.

use super::*;
use serde_json::{Value, json};

/// A small argument object goes out whole and is not marked as cut.
#[test]
fn small_arguments_go_out_whole() {
    let args = json!({ "path": "src/lib.rs", "start_line": 3 });
    let (sent, cut) = capped_arguments(&args);
    assert_eq!(sent, args);
    assert!(!cut);
}

/// A `write_file` with a large file in it keeps its path whole and has its
/// content shortened, and the result fits.
#[test]
fn a_large_string_is_cut_and_the_short_one_beside_it_kept() {
    let args = json!({ "path": "src/big.rs", "content": "x".repeat(50_000) });
    let (sent, cut) = capped_arguments(&args);
    assert!(cut);
    assert_eq!(sent["path"], "src/big.rs");
    assert_eq!(sent["content"].as_str().map(str::len), Some(1024));
    assert!(sent.to_string().len() <= MAX_EVENT_ARGUMENT_BYTES);
}

/// Many medium strings inside an array are cut further, step by step, until
/// the whole object fits; numbers and booleans are left alone.
#[test]
fn nested_strings_are_cut_until_the_object_fits() {
    let items: Vec<Value> = (0..40)
        .map(|n| json!({ "text": "y".repeat(500), "n": n, "ok": true }))
        .collect();
    let args = json!({ "items": items });
    let (sent, cut) = capped_arguments(&args);
    assert!(cut);
    assert!(sent.to_string().len() <= MAX_EVENT_ARGUMENT_BYTES);
    let first = &sent["items"][0];
    assert_eq!(first["n"], 0);
    assert_eq!(first["ok"], true);
    let kept = first["text"].as_str().map_or(0, str::len);
    assert!(kept < 500);
}

/// An object too big even with every string emptied goes out as an empty
/// object, still marked as cut.
#[test]
fn an_object_too_big_to_shrink_goes_out_empty() {
    let map: serde_json::Map<String, Value> = (0..2_000)
        .map(|n| (format!("key_{n}"), Value::from(n)))
        .collect();
    let (sent, cut) = capped_arguments(&Value::Object(map));
    assert!(cut);
    assert_eq!(sent, json!({}));
}

/// A frame from a daemon that sent no arguments still reads, with an empty
/// object and not cut.
#[test]
fn a_tool_call_frame_without_arguments_still_reads() {
    let frame = json!({
        "event": "tool_call_started",
        "run_id": "r",
        "agent_id": "a",
        "call_id": "c",
        "execution_id": "x",
        "tool": "read_file",
    });
    let event: WorldEvent = serde_json::from_value(frame).expect("it reads");
    assert_eq!(
        event,
        WorldEvent::ToolCallStarted {
            run_id: "r".into(),
            agent_id: "a".into(),
            call_id: "c".into(),
            execution_id: "x".into(),
            tool: "read_file".into(),
            arguments: json!({}),
            arguments_truncated: false,
        }
    );
}

/// An answer that fits goes on the frame whole; a longer one is cut at a
/// character boundary and marked, and one already marked stays marked.
#[test]
fn a_submitted_answer_is_cut_to_fit_the_frame() {
    use leviath_core::output::FinalOutput;
    let small = capped_output(FinalOutput::new("done", None, "s".into(), 1));
    assert_eq!(small.content, "done");
    assert!(!small.truncated);

    let long = "\u{e9}".repeat(MAX_EVENT_OUTPUT_BYTES);
    let cut = capped_output(FinalOutput::new(&long, None, "s".into(), 1));
    assert!(cut.truncated);
    assert!(cut.content.len() <= MAX_EVENT_OUTPUT_BYTES);
    assert!(cut.content.chars().all(|c| c == '\u{e9}'));

    let mut marked = FinalOutput::new("short", None, "s".into(), 1);
    marked.truncated = true;
    assert!(capped_output(marked).truncated);
}

/// The settled frame carries the journal's record at the top level, so its
/// wire names are the record's own, and it reads back as it was sent.
#[test]
fn a_settled_frame_round_trips_with_the_record_flat() {
    let event = WorldEvent::InteractionSettled {
        run_id: "r".into(),
        agent_id: "r".into(),
        interaction: Box::new(crate::runfile::record::InteractionRecord {
            request_id: "r-ask-1".into(),
            kind: leviath_core::interaction::InteractionKind::FreeText,
            tool: None,
            prompt: "Which?".into(),
            stage: "plan".into(),
            settlement: leviath_core::interaction::Settlement::TimedOut,
            asked_at: 10,
            at: 20,
        }),
    };
    let wire = serde_json::to_value(&event).expect("it serializes");
    assert_eq!(wire["event"], "interaction_settled");
    assert_eq!(wire["request_id"], "r-ask-1");
    assert_eq!(wire["settlement"], "timed_out");
    let back: WorldEvent = serde_json::from_value(wire).expect("it reads back");
    assert_eq!(back, event);
    assert_eq!(back.run_id(), "r");
}

/// The submitted frame names its run and reads back as it was sent.
#[test]
fn a_submitted_frame_round_trips() {
    let event = WorldEvent::OutputSubmitted {
        run_id: "r".into(),
        agent_id: "a".into(),
        stage: "answer".into(),
        output: leviath_core::output::FinalOutput::new("done", None, "answer".into(), 5),
    };
    let wire = serde_json::to_value(&event).expect("it serializes");
    assert_eq!(wire["event"], "output_submitted");
    let back: WorldEvent = serde_json::from_value(wire).expect("it reads back");
    assert_eq!(back, event);
    assert_eq!(back.run_id(), "r");
}
