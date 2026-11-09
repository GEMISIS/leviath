//! Reasoning blocks kept for replay.

use super::*;

/// Feed `blocks` to a keeping stream as one chunk's delta.
fn feed(kept: &mut Details, blocks: Value) {
    kept.absorb(&json!({}), &json!({ "reasoning_details": blocks }));
}

fn sealed(kept: &mut Details) -> Vec<Value> {
    let blob = kept.finish("m").expect("blocks were kept");
    replay("m", false, &blob).expect("opens for its model")
}

#[test]
fn fragments_of_one_block_join_and_blocks_keep_their_order() {
    let mut kept = Details::keeping();
    feed(
        &mut kept,
        json!([{ "type": "reasoning.text", "text": "Let ", "index": 0 }]),
    );
    feed(
        &mut kept,
        json!([
            { "type": "reasoning.text", "text": "me think", "index": 0, "signature": "sig" },
            { "type": "reasoning.summary", "summary": "a ", "index": 1 },
        ]),
    );
    feed(
        &mut kept,
        json!([
            { "type": "reasoning.summary", "summary": "gist", "index": 1 },
            { "type": "reasoning.encrypted", "data": "AAA", "id": "call_1", "index": 2, "format": "google-gemini-v1" },
        ]),
    );
    feed(
        &mut kept,
        json!([{ "type": "reasoning.encrypted", "data": "BBB", "id": "call_1", "index": 2 }]),
    );
    assert_eq!(
        sealed(&mut kept),
        vec![
            json!({ "type": "reasoning.text", "text": "Let me think", "index": 0, "signature": "sig" }),
            json!({ "type": "reasoning.summary", "summary": "a gist", "index": 1 }),
            json!({ "type": "reasoning.encrypted", "data": "AAABBB", "id": "call_1", "index": 2, "format": "google-gemini-v1" }),
        ]
    );
    assert!(kept.finish("m").is_none(), "sealed once");
    assert!(kept.is_sealed());
}

/// Claude ends a thinking block with `"text": null` and its signature; a null
/// or a non-string never wipes what was gathered, and an early signature
/// survives a later null or empty one.
#[test]
fn a_null_fragment_never_overwrites_what_was_gathered() {
    let mut kept = Details::keeping();
    feed(
        &mut kept,
        json!([{ "type": "reasoning.text", "text": "think", "index": 0,
                 "signature": "early", "format": "anthropic-claude-v1" }]),
    );
    feed(
        &mut kept,
        json!([{ "type": "reasoning.text", "text": " more", "index": 0, "signature": null }]),
    );
    feed(
        &mut kept,
        json!([{ "type": "reasoning.text", "text": 5, "index": 0, "signature": "", "format": null }]),
    );
    feed(
        &mut kept,
        json!([{ "type": "reasoning.text", "text": null, "index": 0, "signature": "final" }]),
    );
    assert_eq!(
        sealed(&mut kept),
        vec![
            json!({ "type": "reasoning.text", "text": "think more", "index": 0,
                     "signature": "final", "format": "anthropic-claude-v1" })
        ]
    );
}

/// A field the block first lacked, or held as null, is filled by a later
/// fragment, whatever its type.
#[test]
fn a_later_fragment_fills_what_the_block_lacked() {
    let mut kept = Details::keeping();
    feed(
        &mut kept,
        json!([{ "type": "reasoning.text", "text": null, "index": 0, "signature": "" }]),
    );
    feed(
        &mut kept,
        json!([{ "type": "reasoning.text", "text": "now", "index": 0, "signature": "s", "extra": 3 }]),
    );
    assert_eq!(
        sealed(&mut kept),
        vec![
            json!({ "type": "reasoning.text", "text": "now", "index": 0, "signature": "s", "extra": 3 })
        ]
    );
}

/// Two encrypted blocks for different calls are two blocks, even at the same
/// `index`; a fragment that names no id continues the one open.
#[test]
fn a_different_id_starts_a_new_block() {
    let mut kept = Details::keeping();
    feed(
        &mut kept,
        json!([
            { "type": "reasoning.encrypted", "id": "rs_1", "data": "AAA", "index": 0 },
            { "type": "reasoning.encrypted", "id": "rs_2", "data": "BBB", "index": 0 },
        ]),
    );
    feed(
        &mut kept,
        json!([{ "type": "reasoning.encrypted", "data": "ccc", "index": 0 }]),
    );
    let blocks = sealed(&mut kept);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["data"], "AAAccc");
    assert_eq!(blocks[1]["data"], "BBB");
}

#[test]
fn a_block_with_no_index_is_never_a_continuation_and_junk_is_skipped() {
    let mut kept = Details::keeping();
    feed(
        &mut kept,
        json!([
            { "type": "reasoning.encrypted", "data": "x" },
            { "type": "reasoning.encrypted", "data": "y" },
            "not an object",
        ]),
    );
    kept.absorb(&json!({}), &json!({ "reasoning_details": "not an array" }));
    kept.absorb(&json!({}), &json!({}));
    assert_eq!(sealed(&mut kept).len(), 2);
}

#[test]
fn a_stream_that_keeps_nothing_keeps_nothing() {
    let mut inert = Details::default();
    feed(
        &mut inert,
        json!([{ "type": "reasoning.text", "text": "x", "index": 0 }]),
    );
    assert!(inert.finish("m").is_none());
    assert!(!inert.is_sealed());
    assert!(Details::keeping().finish("m").is_none());
}

/// The blob names the model the replies said answered, the asked one when
/// they said none.
#[test]
fn the_blob_names_the_model_that_answered() {
    let mut kept = Details::keeping();
    kept.absorb(
        &json!({ "model": "" }),
        &json!({ "reasoning_details": [{ "type": "reasoning.text", "text": "a", "index": 0 }] }),
    );
    kept.absorb(
        &json!({ "model": "anthropic/claude-sonnet-5.5" }),
        &json!({}),
    );
    kept.absorb(&json!({ "model": "other/ignored" }), &json!({}));
    let blob = kept.finish("openrouter/auto").unwrap();
    assert!(replay("anthropic/claude-sonnet-5.5", false, &blob).is_some());
    assert!(replay("openrouter/auto", false, &blob).is_none());

    let mut unnamed = Details::keeping();
    feed(
        &mut unnamed,
        json!([{ "type": "reasoning.text", "text": "a", "index": 0 }]),
    );
    let blob = unnamed.finish("google/gemini-4-argon").unwrap();
    assert!(replay("google/gemini-4-argon", false, &blob).is_some());
}

#[test]
fn a_blob_opens_only_for_the_model_that_answered_and_a_concrete_request() {
    let blocks = [json!({ "type": "reasoning.encrypted", "data": "x" })];
    let blob = seal("anthropic/claude-sonnet-5.5", &blocks).unwrap();
    assert_eq!(
        replay("anthropic/claude-sonnet-5.5", false, &blob)
            .unwrap()
            .len(),
        1
    );
    // A routing variant is the same model.
    assert!(replay("anthropic/claude-sonnet-5.5:nitro", false, &blob).is_some());
    let variant = seal("anthropic/claude-sonnet-5.5:floor", &blocks).unwrap();
    assert!(replay("anthropic/claude-sonnet-5.5", false, &variant).is_some());
    // Another model, or a request that lets the gateway choose, gets none.
    assert!(replay("google/gemini-4-argon", false, &blob).is_none());
    assert!(replay("anthropic/claude-sonnet-5.5", true, &blob).is_none());
    assert!(seal("m", &[]).is_none());
    for other in [
        r#"{"responses":{"provider":"meta","items":["a"]}}"#,
        r#"{"openrouter":{"provider":"xai","model":"m","details":[{"a":1}]}}"#,
        r#"{"openrouter":{"provider":"openrouter","model":"m","details":[]}}"#,
        r#"{"openrouter":{"provider":"openrouter","model":"m"}}"#,
        r#"{"openrouter":{"provider":"openrouter","model":"m","details":"x"}}"#,
        r#"{"openrouter":{"provider":"openrouter","details":[{"a":1}]}}"#,
        "sealed-codex-item",
        "{not json",
    ] {
        assert!(replay("m", false, other).is_none(), "{other}");
    }
}

#[test]
fn a_buffered_message_seals_to_the_model_the_body_names() {
    let message =
        json!({ "content": "", "reasoning_details": [{ "type": "reasoning.text", "text": "t" }] });
    let named = json!({ "model": "anthropic/claude-sonnet-5.5" });
    let blob = seal_message("openrouter/auto", &named, &message).unwrap();
    assert!(replay("anthropic/claude-sonnet-5.5", false, &blob).is_some());
    let blob = seal_message("m", &json!({}), &message).unwrap();
    assert!(replay("m", false, &blob).is_some());
    assert!(seal_message("m", &json!({}), &json!({ "content": "hi" })).is_none());
    assert!(seal_message("m", &json!({}), &json!({ "reasoning_details": "x" })).is_none());
}
