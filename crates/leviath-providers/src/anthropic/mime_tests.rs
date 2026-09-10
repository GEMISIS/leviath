//! What Claude models take: the table, then the operator's override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use leviath_core::mime::MimeType;

#[test]
fn every_claude_reads_images_and_pdfs_until_an_override_says_otherwise() {
    let mut provider = AnthropicProvider::new(reqwest::Client::new(), "k".to_string());
    let mime = provider.mime("claude-sonnet-5");
    assert!(mime.accepts(&MimeType::parse("image/png").unwrap()));
    assert!(mime.accepts(&MimeType::parse("application/pdf").unwrap()));
    assert!(!mime.accepts(&MimeType::parse("audio/wav").unwrap()));
    provider.capability_overrides.insert(
        "claude-sonnet-5".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    );
    assert!(!provider.mime("claude-sonnet-5").takes_mime());
    assert!(provider.mime("claude-opus-5").takes_mime());
}

#[test]
fn a_hydrated_part_is_an_image_block_and_an_unhydrated_one_its_stand_in() {
    use leviath_core::mime::{Blob, MimeRegistry, Part};
    let provider = AnthropicProvider::new(reqwest::Client::new(), "k".to_string());
    let reg = MimeRegistry::builtin();
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
    let part = Part::stored(blob.describe(&reg)).named("a.png");
    let mut hydrated = crate::ContentBlock::mime(&part).unwrap();
    if let crate::ContentBlock::Mime { data, .. } = &mut hydrated {
        *data = "AQID".to_string();
    }
    let request = InferenceRequest {
        system: Vec::new(),
        messages: vec![crate::Message {
            role: "user".to_string(),
            content: crate::MessageContent::Blocks(vec![
                crate::ContentBlock::Text { text: "see".into() },
                hydrated,
                crate::ContentBlock::mime(&part).unwrap(),
            ]),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: "claude-sonnet-5".to_string(),
        max_tokens: 10,
        temperature: 0.0,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };
    let body = provider.build_request_body(&request);
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["data"], "AQID");
    assert_eq!(content[2]["type"], "text");
    assert_eq!(content[2]["text"], "[image/png, 3 B] a.png");
}
