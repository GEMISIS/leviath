//! Reasoning blocks kept for replay.

use super::*;

fn chunk(blocks: Value) -> Value {
    json!({ "reasoning_details": blocks })
}

#[test]
fn fragments_of_one_block_join_and_blocks_keep_their_order() {
    let mut kept = Details::for_model("google/gemini-4-argon");
    kept.absorb(&chunk(
        json!([{ "type": "reasoning.text", "text": "Let ", "index": 0 }]),
    ));
    kept.absorb(&chunk(json!([
        { "type": "reasoning.text", "text": "me think", "index": 0, "signature": "sig" },
        { "type": "reasoning.summary", "summary": "a ", "index": 1 },
    ])));
    kept.absorb(&chunk(json!([
        { "type": "reasoning.summary", "summary": "gist", "index": 1 },
        { "type": "reasoning.encrypted", "data": "AAA", "id": "call_1", "index": 2, "format": "google-gemini-v1" },
    ])));
    kept.absorb(&chunk(json!([
        { "type": "reasoning.encrypted", "data": "BBB", "id": "call_1", "index": 2 },
    ])));
    let blob = kept.seal().expect("blocks were kept");
    let replayed = replay("google/gemini-4-argon", &blob).expect("opens for its model");
    assert_eq!(
        replayed,
        vec![
            json!({ "type": "reasoning.text", "text": "Let me think", "index": 0, "signature": "sig" }),
            json!({ "type": "reasoning.summary", "summary": "a gist", "index": 1 }),
            json!({ "type": "reasoning.encrypted", "data": "AAABBB", "id": "call_1", "index": 2, "format": "google-gemini-v1" }),
        ]
    );
    assert!(kept.seal().is_none(), "sealed once, then empty");
}

#[test]
fn a_block_with_no_index_is_never_a_continuation_and_junk_is_skipped() {
    let mut kept = Details::for_model("m");
    kept.absorb(&chunk(json!([
        { "type": "reasoning.encrypted", "data": "x" },
        { "type": "reasoning.encrypted", "data": "y" },
        "not an object",
    ])));
    kept.absorb(&json!({ "reasoning_details": "not an array" }));
    kept.absorb(&json!({}));
    let blob = kept.seal().unwrap();
    assert_eq!(replay("m", &blob).unwrap().len(), 2);
}

#[test]
fn a_stream_that_keeps_nothing_keeps_nothing() {
    let mut inert = Details::default();
    inert.absorb(&chunk(
        json!([{ "type": "reasoning.text", "text": "x", "index": 0 }]),
    ));
    assert!(inert.seal().is_none());
    assert!(Details::for_model("m").seal().is_none());
}

#[test]
fn a_blob_opens_only_for_its_provider_and_its_model() {
    let blocks = [json!({ "type": "reasoning.encrypted", "data": "x" })];
    let blob = seal("anthropic/claude-sonnet-5.5", &blocks).unwrap();
    assert_eq!(
        replay("anthropic/claude-sonnet-5.5", &blob).unwrap().len(),
        1
    );
    assert!(
        replay("google/gemini-4-argon", &blob).is_none(),
        "another model"
    );
    assert!(seal("m", &[]).is_none());
    for other in [
        r#"{"responses":{"provider":"meta","items":["a"]}}"#,
        r#"{"openrouter":{"provider":"xai","model":"m","details":[{"a":1}]}}"#,
        r#"{"openrouter":{"provider":"openrouter","model":"m","details":[]}}"#,
        r#"{"openrouter":{"provider":"openrouter","model":"m"}}"#,
        r#"{"openrouter":{"provider":"openrouter","model":"m","details":"x"}}"#,
        "sealed-codex-item",
        "{not json",
    ] {
        assert!(replay("m", other).is_none(), "{other}");
    }
}

#[test]
fn a_buffered_message_seals_its_details() {
    let message =
        json!({ "content": "", "reasoning_details": [{ "type": "reasoning.text", "text": "t" }] });
    let blob = seal_message("m", &message).unwrap();
    assert_eq!(replay("m", &blob).unwrap().len(), 1);
    assert!(seal_message("m", &json!({ "content": "hi" })).is_none());
    assert!(seal_message("m", &json!({ "reasoning_details": "x" })).is_none());
}
