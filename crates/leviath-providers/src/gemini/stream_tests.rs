//! Interactions streams.

use super::*;
use serde_json::json;

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["event_type"].as_str().unwrap_or("x")
    )
}

/// Every chunk the events yield, in order.
fn run(events: &[Value]) -> Vec<crate::provider::Result<StreamChunk>> {
    let mut buffer: String = events.iter().map(|e| event(e.clone())).collect();
    buffer.push_str("data: not json\n\n");
    buffer.push_str(": a comment\n\n");
    let mut turn = Turn::default();
    let mut out = Vec::new();
    while !buffer.is_empty() {
        let before = buffer.len();
        if let Some(Some(chunk)) = parse_event(&mut buffer, &mut turn) {
            out.push(chunk);
        }
        if buffer.len() == before {
            break;
        }
    }
    out
}

#[test]
fn text_arrives_as_it_comes_and_the_turn_ends_with_usage() {
    let chunks = run(&[
        json!({ "event_type": "interaction.created", "interaction": { "id": "v1" } }),
        json!({ "event_type": "step.start", "index": 0, "step": { "type": "model_output" } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "text", "text": "Hel" },
            "metadata": { "total_usage": { "total_input_tokens": 7 } } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "text", "text": "lo" } }),
        json!({ "event_type": "step.stop", "index": 0,
            "usage": { "total_input_tokens": 10, "total_cached_tokens": 4, "total_output_tokens": 5, "total_thought_tokens": 3 } }),
        json!({ "event_type": "interaction.completed", "interaction": { "status": "completed" } }),
    ]);
    let chunks: Vec<StreamChunk> = chunks.into_iter().map(Result::unwrap).collect();
    assert_eq!(chunks[0].delta, "Hel");
    assert_eq!(chunks[1].delta, "lo");
    let last = chunks.last().unwrap();
    assert_eq!(last.finish_reason, Some(FinishReason::Complete));
    let usage = last.tokens.clone().unwrap();
    assert_eq!(usage.prompt_tokens, 6);
    assert_eq!(usage.cached_tokens, 4);
    assert_eq!(usage.completion_tokens, 8, "thinking is billed as output");
}

#[test]
fn a_call_is_assembled_from_its_pieces_and_carries_the_thoughts_signature() {
    let chunks = run(&[
        json!({ "event_type": "step.start", "index": 0, "step": { "type": "thought", "signature": "" } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "thought", "signature": "sig-9", "content": [] } }),
        json!({ "event_type": "step.start", "index": 1, "step": { "type": "function_call", "id": "c1", "name": "set", "arguments": { "a": 1 } } }),
        json!({ "event_type": "step.delta", "index": 1, "delta": { "type": "function_call", "arguments": { "b": 2 } } }),
        json!({ "event_type": "step.delta", "index": 1, "delta": { "type": "function_call", "id": "c1b", "name": "set2", "arguments": "{\"c\":3}" } }),
        json!({ "event_type": "step.stop", "index": 1 }),
        json!({ "event_type": "step.start", "index": 2, "step": { "type": "thought", "signature": "sig-10" } }),
        json!({ "event_type": "step.delta", "index": 3, "delta": { "type": "function_call", "id": "c2", "name": "get" } }),
        json!({ "event_type": "step.stop", "index": 3 }),
        json!({ "event_type": "step.stop", "index": 9 }),
        json!({ "event_type": "interaction.completed", "interaction": { "status": "completed", "usage": { "total_input_tokens": 1 } } }),
    ]);
    let chunks: Vec<StreamChunk> = chunks.into_iter().map(Result::unwrap).collect();
    let calls: Vec<&ToolCallDelta> = chunks.iter().flat_map(|c| c.tool_calls.iter()).collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].index, 0);
    assert_eq!(calls[0].id.as_deref(), Some("c1b"));
    assert_eq!(calls[0].name.as_deref(), Some("set2"));
    let args: Value = serde_json::from_str(&calls[0].arguments_delta).unwrap();
    assert_eq!(args, json!({ "a": 1, "b": 2, "c": 3 }));
    assert_eq!(calls[0].thought_signature.as_deref(), Some("sig-9"));
    assert_eq!(calls[1].index, 1);
    assert_eq!(calls[1].thought_signature.as_deref(), Some("sig-10"));
    assert_eq!(calls[1].arguments_delta, "{}");
    let last = chunks.last().unwrap();
    assert_eq!(last.finish_reason, Some(FinishReason::ToolCall));
    assert_eq!(last.tokens.clone().unwrap().prompt_tokens, 1);
}

/// The events gemini-3.5-flash sent for one call, as recorded: the signature
/// comes as its own `thought_signature` delta.
#[test]
fn a_signature_sent_as_its_own_delta_rides_on_the_call() {
    let chunks = run(&[
        json!({ "event_type": "interaction.created", "interaction": { "id": "", "status": "in_progress" } }),
        json!({ "event_type": "interaction.status_update", "interaction_id": "", "status": "in_progress" }),
        json!({ "event_type": "step.start", "index": 0, "step": { "type": "thought" } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "signature": "EoEDCv4C", "type": "thought_signature" } }),
        json!({ "event_type": "step.stop", "index": 0 }),
        json!({ "event_type": "step.start", "index": 1, "step": { "id": "call_308410", "type": "function_call", "name": "current_time", "arguments": {} } }),
        json!({ "event_type": "step.stop", "index": 1 }),
        json!({ "event_type": "interaction.completed", "interaction": { "id": "", "status": "requires_action",
            "usage": { "total_input_tokens": 33, "total_cached_tokens": 0, "total_output_tokens": 10, "total_thought_tokens": 49 } } }),
    ]);
    let chunks: Vec<StreamChunk> = chunks.into_iter().map(Result::unwrap).collect();
    let calls: Vec<&ToolCallDelta> = chunks.iter().flat_map(|c| c.tool_calls.iter()).collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].thought_signature.as_deref(), Some("EoEDCv4C"));
    assert_eq!(
        chunks.last().unwrap().finish_reason,
        Some(FinishReason::ToolCall)
    );
}

#[test]
fn media_the_model_makes_is_a_part() {
    let chunks = run(&[
        json!({ "event_type": "step.start", "index": 0, "step": { "type": "model_output",
            "content": [ { "type": "text", "text": "here" }, { "type": "image", "data": "iVBORw==", "mime_type": "image/png" } ] } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "image", "data": "AAAA", "mime_type": "image/jpeg" } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "image", "data": "!!", "mime_type": "image/png" } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "image", "data": "AAAA", "mime_type": "not a type" } }),
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "image" } }),
        json!({ "event_type": "step.start", "index": 1, "step": { "type": "model_output", "content": [ {} ] } }),
        json!({ "event_type": "interaction.completed", "interaction": { "status": "requires_action" } }),
    ]);
    let chunks: Vec<StreamChunk> = chunks.into_iter().map(Result::unwrap).collect();
    assert_eq!(chunks[0].delta, "here");
    assert_eq!(chunks[0].parts[0].mime_type.as_str(), "image/png");
    assert_eq!(chunks[1].parts[0].mime_type.as_str(), "image/jpeg");
    assert_eq!(chunks.len(), 3, "an unreadable image yields nothing");
    let last = chunks.last().unwrap();
    assert_eq!(last.finish_reason, Some(FinishReason::Complete));
    assert_eq!(last.tokens.clone().unwrap().total_tokens, 0);
}

#[test]
fn every_ending_reads_as_what_it_was() {
    let ended = |status: &str| {
        run(&[
            json!({ "event_type": "interaction.completed", "interaction": { "status": status } }),
        ])
        .pop()
        .unwrap()
    };
    assert_eq!(
        ended("incomplete").unwrap().finish_reason,
        Some(FinishReason::TokenLimit)
    );
    assert_eq!(
        ended("budget_exceeded").unwrap().finish_reason,
        Some(FinishReason::TokenLimit)
    );
    assert_eq!(
        ended("something_new").unwrap().finish_reason,
        Some(FinishReason::Unknown)
    );
    assert!(ended("failed").unwrap_err().to_string().contains("failed"));
    assert!(ended("cancelled").is_err());
    let no_status = run(&[json!({ "event_type": "interaction.completed" })])
        .pop()
        .unwrap();
    assert_eq!(
        no_status.unwrap().finish_reason,
        Some(FinishReason::Complete)
    );

    let errors = run(&[
        json!({ "event_type": "error", "error": { "code": "not_found", "message": "gone" } }),
        json!({ "event_type": "error" }),
        json!({ "event_type": "interaction.status_update", "status": "in_progress" }),
        json!({ "event_type": "step.start", "index": 0 }),
        json!({ "event_type": "step.start", "index": 0, "step": {} }),
        json!({ "event_type": "step.delta", "index": 0 }),
        json!({ "event_type": "step.delta", "index": 0, "delta": {} }),
        json!({ "no": "type" }),
    ]);
    assert_eq!(errors.len(), 2);
    assert!(errors[0].as_ref().unwrap_err().to_string().contains("gone"));
    assert!(
        errors[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("reported an error")
    );
}

#[tokio::test]
async fn the_framer_reads_a_whole_stream() {
    let body: String = [
        json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "text", "text": "ok" } }),
        json!({ "event_type": "interaction.completed", "interaction": { "status": "completed" } }),
    ]
    .into_iter()
    .map(event)
    .collect();
    let bytes = tokio_stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(body))]);
    let response = crate::provider::collect_stream(Box::pin(sse_stream(bytes)))
        .await
        .unwrap();
    assert_eq!(response.content, "ok");
}
