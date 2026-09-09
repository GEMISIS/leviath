//! Stored parts in the OpenAI Chat Completions shape.

use super::*;
use leviath_core::media::{Blob, MediaRegistry, MediaType, Part};

fn png(name: &str, hydrated: bool) -> ContentBlock {
    let reg = MediaRegistry::builtin();
    let blob = Blob::new(MediaType::parse("image/png").unwrap(), vec![1, 2, 3]).named(name);
    let part = Part::stored(blob.describe(&reg)).named(name);
    let mut block = ContentBlock::media(&part).unwrap();
    if hydrated && let ContentBlock::Media { data, .. } = &mut block {
        *data = "AQID".to_string();
    }
    block
}

#[test]
fn a_user_turn_with_media_becomes_a_parts_array() {
    let content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: "look".into(),
        },
        png("a.png", true),
    ]);
    let out = message_to_openai("user", &content);
    assert_eq!(out.len(), 1);
    let parts = out[0]["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AQID");
}

#[test]
fn an_unhydrated_part_is_its_stand_in_and_text_only_stays_a_string() {
    let only_media = MessageContent::Blocks(vec![png("a.png", false)]);
    let out = message_to_openai("user", &only_media);
    let parts = out[0]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "[image/png, 3 B] a.png");
    let text_only = MessageContent::Blocks(vec![ContentBlock::Text { text: "hi".into() }]);
    let out = message_to_openai("user", &text_only);
    assert_eq!(out[0]["content"], "hi");
}

#[test]
fn media_beside_a_tool_result_follows_it_in_a_user_message() {
    let content = MessageContent::Blocks(vec![
        ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "rendered".into(),
            is_error: false,
        },
        png("out.png", true),
    ]);
    let out = message_to_openai("user", &content);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["role"], "tool");
    assert_eq!(out[1]["role"], "user");
    assert_eq!(out[1]["content"][0]["type"], "image_url");
}
