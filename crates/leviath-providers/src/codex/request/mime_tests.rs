//! Stored parts as Responses content parts.

use super::*;
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};

fn png(name: &str) -> ContentBlock {
    let reg = MimeRegistry::builtin();
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named(name);
    let part = Part::stored(blob.describe(&reg)).named(name);
    let mut block = ContentBlock::mime(&part).unwrap();
    if let ContentBlock::Mime { data, .. } = &mut block {
        *data = "AQID".to_string();
    }
    block
}

fn message(blocks: Vec<ContentBlock>) -> Message {
    Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(blocks),
        cache_breakpoint: false,
        reasoning: None,
    }
}

#[test]
fn text_and_mime_share_one_message_item() {
    let mut input = Vec::new();
    push_message(
        &mut input,
        &message(vec![
            ContentBlock::Text { text: "see".into() },
            png("a.png"),
        ]),
        false,
    );
    assert_eq!(input.len(), 1);
    let parts = input[0]["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "input_text");
    assert_eq!(parts[1]["type"], "input_image");
    assert_eq!(parts[1]["image_url"], "data:image/png;base64,AQID");
}

#[test]
fn mime_alone_makes_an_item_with_no_empty_text_part() {
    let mut input = Vec::new();
    push_message(&mut input, &message(vec![png("a.png")]), false);
    let parts = input[0]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["type"], "input_image");
}

#[test]
fn mime_before_a_tool_call_is_flushed_ahead_of_it() {
    let mut input = Vec::new();
    push_message(
        &mut input,
        &message(vec![
            png("a.png"),
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "render".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
            },
            png("b.png"),
        ]),
        false,
    );
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["content"][0]["type"], "input_image");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[3]["content"][0]["type"], "input_image");
}
