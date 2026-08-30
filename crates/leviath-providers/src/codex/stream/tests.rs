//! The framer, driven as a pure function over a buffer.
//!
//! Every event shape here was captured from the live route rather than written
//! from the API reference, because the two disagree in one load-bearing place:
//! `response.completed` carries an empty `output` array.

use super::*;

/// Frame `value` as one server-sent event, the way the route sends it.
fn event(value: serde_json::Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        value["type"].as_str().unwrap_or(""),
        value
    )
}

/// Run one event through the parser with a fresh turn.
fn parse_one(value: serde_json::Value) -> Option<Option<crate::provider::Result<StreamChunk>>> {
    let mut turn = Turn::default();
    let mut buffer = event(value);
    parse_event(&mut buffer, &mut turn)
}

/// The chunk an event produces, when it produces one.
fn chunk(value: serde_json::Value) -> StreamChunk {
    parse_one(value)
        .expect("an event was consumed")
        .expect("the stream did not end")
        .expect("not an error")
}

#[test]
fn a_partial_event_is_left_in_the_buffer() {
    let mut turn = Turn::default();
    let mut buffer = "event: response.created\ndata: {\"type\":\"resp".to_string();
    assert!(parse_event(&mut buffer, &mut turn).is_none());
    assert!(!buffer.is_empty(), "the partial frame was eaten");
}

#[test]
fn text_deltas_become_content() {
    let c = chunk(serde_json::json!({
        "type": "response.output_text.delta",
        "delta": "Hello",
        "output_index": 0,
    }));
    assert_eq!(c.delta, "Hello");
    assert!(c.tool_calls.is_empty());
}

#[test]
fn an_opening_tool_item_carries_the_call_id_and_name() {
    // Captured shape: the item has both `id` (fc_...) and `call_id` (call_...)
    // and only the latter may be echoed on the matching output.
    let c = chunk(serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {
            "id": "fc_08b4274826",
            "type": "function_call",
            "status": "in_progress",
            "arguments": "",
            "call_id": "call_iR9KM9PfXAbxo4U4ULgDXLKe",
            "name": "read_file",
        },
    }));
    let delta = &c.tool_calls[0];
    assert_eq!(delta.index, 0);
    assert_eq!(delta.id.as_deref(), Some("call_iR9KM9PfXAbxo4U4ULgDXLKe"));
    assert_eq!(delta.name.as_deref(), Some("read_file"));
    assert_eq!(delta.arguments_delta, "");
}

#[test]
fn a_non_tool_item_opening_emits_nothing() {
    // A message or reasoning item opening carries nothing to accumulate.
    assert!(
        parse_one(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "id": "msg_1", "type": "message" },
        }))
        .is_none()
    );
}

#[test]
fn argument_deltas_are_indexed_by_output_index() {
    // Never a running counter: the Anthropic transport's scar is that a fresh
    // number splits one call into an id with no arguments and arguments with
    // an empty id.
    let c = chunk(serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "delta": "{\"path\":",
        "item_id": "fc_08b4274826",
        "output_index": 3,
    }));
    let delta = &c.tool_calls[0];
    assert_eq!(delta.index, 3);
    assert_eq!(delta.arguments_delta, "{\"path\":");
    assert!(
        delta.id.is_none(),
        "a later delta must not re-assign the id"
    );
    assert!(delta.name.is_none());
}

#[test]
fn an_event_with_no_output_index_falls_back_to_the_first_slot() {
    let c = chunk(serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "delta": "x",
    }));
    assert_eq!(c.tool_calls[0].index, 0);
}

#[test]
fn a_finished_reasoning_item_yields_its_sealed_blob() {
    let c = chunk(serde_json::json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "id": "rs_03942a3c50",
            "type": "reasoning",
            "encrypted_content": "sealed-bytes",
            "summary": [{ "type": "summary_text", "text": "thinking" }],
        },
    }));
    assert_eq!(c.reasoning.as_deref(), Some("sealed-bytes"));
    assert_eq!(c.delta, "", "a summary must not leak into the answer");
}

#[test]
fn a_reasoning_item_without_a_blob_yields_nothing() {
    // `include` was not requested, so there is nothing to replay.
    assert!(
        parse_one(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "id": "rs_1", "type": "reasoning", "summary": [] },
        }))
        .is_none()
    );
}

#[test]
fn a_finished_tool_item_does_not_re_emit_its_arguments() {
    // The arguments already arrived as deltas; emitting them again would
    // double every call's argument text and break the JSON.
    assert!(
        parse_one(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "fc_1",
                "type": "function_call",
                "call_id": "call_1",
                "name": "t",
                "arguments": "{\"path\":\"/etc/hostname\"}",
            },
        }))
        .is_none()
    );
}

#[test]
fn the_terminal_event_reports_usage_with_disjoint_input_counts() {
    // `input_tokens` includes `cached_tokens`, so the fresh figure is the
    // difference. Counting it whole bills the cached prefix twice.
    let c = chunk(serde_json::json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 9624,
                "input_tokens_details": { "cached_tokens": 8960, "cache_write_tokens": 4 },
                "output_tokens": 21,
                "output_tokens_details": { "reasoning_tokens": 8 },
                "total_tokens": 9645,
            },
        },
    }));
    let usage = c.tokens.expect("usage");
    assert_eq!(usage.prompt_tokens, 9624 - 8960 - 4);
    assert_eq!(usage.cached_tokens, 8960);
    assert_eq!(usage.cache_write_tokens, 4);
    // reasoning_tokens is a subset of output_tokens, never added to it.
    assert_eq!(usage.completion_tokens, 21);
}

#[test]
fn a_malformed_usage_block_clamps_instead_of_wrapping() {
    let c = chunk(serde_json::json!({
        "type": "response.completed",
        "response": {
            "usage": {
                "input_tokens": 10,
                "input_tokens_details": { "cached_tokens": 999, "cache_write_tokens": 999 },
                "output_tokens": 1,
            },
        },
    }));
    assert_eq!(c.tokens.expect("usage").prompt_tokens, 0);
}

#[test]
fn usage_is_zero_when_the_server_reports_none() {
    let c = chunk(serde_json::json!({
        "type": "response.completed",
        "response": { "status": "completed", "output": [] },
    }));
    let usage = c.tokens.expect("usage");
    assert_eq!(usage.prompt_tokens, 0);
    assert_eq!(usage.completion_tokens, 0);
}

#[test]
fn a_turn_that_called_a_tool_finishes_as_a_tool_call() {
    // The terminal event says `completed` either way, so the finish reason has
    // to come from what arrived earlier in the same response.
    let mut turn = Turn::default();
    let mut buffer = event(serde_json::json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": { "type": "function_call", "call_id": "call_1", "name": "t" },
    }));
    buffer.push_str(&event(serde_json::json!({
        "type": "response.completed",
        "response": { "status": "completed", "output": [] },
    })));

    parse_event(&mut buffer, &mut turn).expect("the opening event");
    let done = parse_event(&mut buffer, &mut turn)
        .expect("the terminal event")
        .expect("stream continues")
        .expect("not an error");
    assert!(matches!(done.finish_reason, Some(FinishReason::ToolCall)));
}

#[test]
fn a_turn_with_only_text_finishes_as_complete() {
    let c = chunk(serde_json::json!({
        "type": "response.completed",
        "response": { "status": "completed", "output": [] },
    }));
    assert!(matches!(c.finish_reason, Some(FinishReason::Complete)));
}

#[test]
fn an_incomplete_response_finishes_as_a_token_limit() {
    let c = chunk(serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "usage": { "input_tokens": 5, "output_tokens": 3 },
        },
    }));
    assert!(matches!(c.finish_reason, Some(FinishReason::TokenLimit)));
    assert_eq!(c.tokens.expect("usage").completion_tokens, 3);
}

#[test]
fn a_failure_after_the_headers_becomes_an_error_rather_than_a_clean_end() {
    // Without its own arm this falls through to the ignore case and the stream
    // ends tidily, leaving a truncated answer and nothing saying why.
    let err = parse_one(serde_json::json!({
        "type": "response.failed",
        "response": {
            "status": "failed",
            "error": { "code": "server_error", "message": "upstream exploded" },
        },
    }))
    .expect("consumed")
    .expect("stream continues")
    .unwrap_err();
    assert!(err.to_string().contains("upstream exploded"), "got: {err}");
}

#[test]
fn a_top_level_error_event_is_also_an_error() {
    let err = parse_one(serde_json::json!({
        "type": "error",
        "error": { "message": "rate limited" },
    }))
    .expect("consumed")
    .expect("stream continues")
    .unwrap_err();
    assert!(err.to_string().contains("rate limited"), "got: {err}");
}

#[test]
fn a_failure_with_no_message_still_says_something() {
    let err = parse_one(serde_json::json!({ "type": "response.failed" }))
        .expect("consumed")
        .expect("stream continues")
        .unwrap_err();
    assert!(err.to_string().contains("without a reason"), "got: {err}");
}

#[test]
fn a_failure_carrying_only_a_bare_message_uses_it() {
    let err = parse_one(serde_json::json!({ "type": "error", "message": "bare" }))
        .expect("consumed")
        .expect("stream continues")
        .unwrap_err();
    assert!(err.to_string().contains("bare"), "got: {err}");
}

#[test]
fn the_bookkeeping_events_are_ignored() {
    // Each of these arrives on every response and carries nothing the
    // collector needs.
    for kind in [
        "response.created",
        "response.in_progress",
        "response.content_part.added",
        "response.content_part.done",
        "response.output_text.done",
        "response.reasoning_summary_part.added",
        "response.reasoning_summary_part.done",
        "response.reasoning_summary_text.delta",
        "response.reasoning_summary_text.done",
        "response.function_call_arguments.done",
        "something.we.have.never.seen",
    ] {
        assert!(
            parse_one(serde_json::json!({ "type": kind, "response": {} })).is_none(),
            "{kind} produced a chunk"
        );
    }
}

#[test]
fn an_event_with_no_data_line_is_skipped() {
    let mut turn = Turn::default();
    let mut buffer = "event: ping\n\n".to_string();
    assert!(parse_event(&mut buffer, &mut turn).is_none());
    assert!(buffer.is_empty(), "the frame was not consumed");
}

#[test]
fn an_unparseable_data_line_is_skipped() {
    let mut turn = Turn::default();
    let mut buffer = "event: x\ndata: {not json\n\n".to_string();
    assert!(parse_event(&mut buffer, &mut turn).is_none());
}

#[test]
fn an_event_with_no_type_is_skipped() {
    let mut turn = Turn::default();
    let mut buffer = "data: {\"delta\":\"x\"}\n\n".to_string();
    assert!(parse_event(&mut buffer, &mut turn).is_none());
}

#[test]
fn a_completed_event_with_no_response_object_is_skipped() {
    let mut turn = Turn::default();
    let mut buffer = "data: {\"type\":\"response.completed\"}\n\n".to_string();
    assert!(parse_event(&mut buffer, &mut turn).is_none());
}

#[test]
fn an_incomplete_event_with_no_response_object_is_skipped() {
    let mut turn = Turn::default();
    let mut buffer = "data: {\"type\":\"response.incomplete\"}\n\n".to_string();
    assert!(parse_event(&mut buffer, &mut turn).is_none());
}

#[test]
fn an_item_event_with_no_item_is_skipped() {
    for kind in ["response.output_item.added", "response.output_item.done"] {
        let mut turn = Turn::default();
        let mut buffer = format!("data: {{\"type\":\"{kind}\"}}\n\n");
        assert!(parse_event(&mut buffer, &mut turn).is_none(), "{kind}");
    }
}

#[tokio::test]
async fn a_whole_tool_calling_turn_collects_into_one_response() {
    // End to end over the shared framer, in the exact event order the live
    // route sent, including the empty `output` array on the terminal event
    // that makes accumulating from the stream mandatory.
    let frames: Vec<String> = vec![
        event(serde_json::json!({ "type": "response.created", "response": {} })),
        event(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": { "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "" },
        })),
        event(serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0, "item_id": "fc_1", "delta": "{\"path\":",
        })),
        event(serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0, "item_id": "fc_1", "delta": "\"/etc/hostname\"}",
        })),
        event(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"/etc/hostname\"}" },
        })),
        event(serde_json::json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [],
                "usage": { "input_tokens": 66, "output_tokens": 21 },
            },
        })),
    ];
    let bytes = tokio_stream::iter(
        frames
            .into_iter()
            .map(|f| Ok(bytes::Bytes::from(f)))
            .collect::<Vec<_>>(),
    );

    let response = crate::provider::collect_stream(Box::pin(codex_sse_stream(bytes)))
        .await
        .expect("collected");

    assert_eq!(response.tool_calls.len(), 1);
    let call = &response.tool_calls[0];
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "read_file");
    assert_eq!(
        call.arguments,
        serde_json::json!({ "path": "/etc/hostname" })
    );
    assert!(matches!(response.finish_reason, FinishReason::ToolCall));
    assert_eq!(response.tokens_used.completion_tokens, 21);
}

#[tokio::test]
async fn a_reasoning_blob_survives_collection() {
    let frames: Vec<String> = vec![
        event(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "type": "reasoning", "encrypted_content": "sealed", "summary": [] },
        })),
        event(serde_json::json!({ "type": "response.output_text.delta", "delta": "42" })),
        event(serde_json::json!({
            "type": "response.completed",
            "response": { "status": "completed", "output": [] },
        })),
    ];
    let bytes = tokio_stream::iter(
        frames
            .into_iter()
            .map(|f| Ok(bytes::Bytes::from(f)))
            .collect::<Vec<_>>(),
    );
    let response = crate::provider::collect_stream(Box::pin(codex_sse_stream(bytes)))
        .await
        .expect("collected");
    assert_eq!(response.content, "42");
    assert_eq!(response.reasoning.as_deref(), Some("sealed"));
}
