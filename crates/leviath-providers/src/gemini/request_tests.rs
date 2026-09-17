//! Interactions bodies.

use super::*;
use crate::files::RemoteFile;
use crate::provider::{ContentBlock, Message, MessageContent, SystemBlock, Tool};
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};

fn message(role: &str, content: MessageContent) -> Message {
    Message {
        role: role.into(),
        content,
        cache_breakpoint: false,
        reasoning: None,
    }
}

fn request(messages: Vec<Message>) -> InferenceRequest {
    InferenceRequest {
        system: vec![SystemBlock {
            text: "## task\ndo it".into(),
            cache_hint: leviath_core::CacheHint::Always,
            region: "task".into(),
            volatility: leviath_core::Volatility::Stable,
        }],
        messages,
        model: "gemini-3.5-flash".into(),
        max_tokens: 512,
        temperature: 0.3,
        tools: vec![Tool {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({ "type": "object" }),
        }],
        extra: Value::Null,
        request_timeout_secs: None,
    }
}

fn mime_block(mime: &str, data: &str, remote: Option<RemoteFile>) -> ContentBlock {
    let blob = Blob::new(MimeType::parse(mime).unwrap(), b"bytes".to_vec()).named("p");
    let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named("p");
    match ContentBlock::mime(&part).unwrap() {
        ContentBlock::Mime {
            part,
            name,
            deliver,
            ..
        } => ContentBlock::Mime {
            part,
            data: data.into(),
            name,
            deliver,
            remote,
        },
        other => other,
    }
}

#[test]
fn a_tool_loop_becomes_steps_with_the_signature_before_its_call() {
    let body = build(
        &request(vec![
            message("user", MessageContent::Text("start".into())),
            message(
                "assistant",
                MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "reading".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "call-1".into(),
                        name: "read_file".into(),
                        input: json!({ "path": "a.txt" }),
                        thought_signature: Some("sig-1".into()),
                    },
                ]),
            ),
            message(
                "user",
                MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "contents".into(),
                    is_error: false,
                }]),
            ),
        ]),
        true,
    );
    assert_eq!(body["model"], "gemini-3.5-flash");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["system_instruction"], "## task\ndo it");
    assert_eq!(body["generation_config"]["max_output_tokens"], 512);
    assert_eq!(body["generation_config"]["temperature"], 0.3);
    assert_eq!(
        body["tools"][0],
        json!({ "type": "function", "name": "read_file", "description": "Read a file", "parameters": { "type": "object" } })
    );
    let steps = body["input"].as_array().unwrap();
    let kinds: Vec<&str> = steps.iter().map(|s| s["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "user_input",
            "model_output",
            "thought",
            "function_call",
            "function_result"
        ]
    );
    assert_eq!(steps[0]["content"][0]["text"], "start");
    assert_eq!(steps[1]["content"][0]["text"], "reading");
    assert_eq!(steps[2]["signature"], "sig-1");
    assert_eq!(steps[3]["arguments"], json!({ "path": "a.txt" }));
    assert_eq!(steps[4]["name"], "read_file");
    assert_eq!(steps[4]["call_id"], "call-1");
    assert_eq!(steps[4]["result"], "contents");
}

#[test]
fn an_unsigned_call_is_told_as_text_and_a_model_that_does_not_sample_is_sent_no_temperature() {
    let mut req = request(vec![
        message(
            "assistant",
            MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "c".into(),
                name: "read_file".into(),
                input: json!({}),
                thought_signature: None,
            }]),
        ),
        message(
            "user",
            MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "c".into(),
                content: "done".into(),
                is_error: false,
            }]),
        ),
    ]);
    req.system.clear();
    req.tools.clear();
    let body = build(&req, false);
    assert!(body.get("system_instruction").is_none());
    assert!(body.get("tools").is_none());
    assert!(body["generation_config"].get("temperature").is_none());
    let raw = body["input"].to_string();
    assert!(!raw.contains("function_call"), "{raw}");
    assert!(
        raw.contains("Earlier in this run I called read_file"),
        "{raw}"
    );
    assert_eq!(
        body["input"][0]["type"], "user_input",
        "a user turn comes first"
    );
}

#[test]
fn parts_go_by_uri_when_uploaded_and_by_bytes_otherwise() {
    let uploaded = RemoteFile {
        id: "files/abc".into(),
        uri: Some("https://files/abc".into()),
        expires_at: None,
    };
    let body = build(
        &request(vec![message(
            "user",
            MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "look".into(),
                },
                mime_block("video/mp4", "", Some(uploaded)),
                mime_block("image/png", "AAAA", None),
                mime_block("audio/wav", "", None),
                mime_block("application/pdf", "UERG", None),
                mime_block("model/obj", "dg==", None),
            ]),
        )]),
        true,
    );
    let content = body["input"][0]["content"].as_array().unwrap();
    assert_eq!(content[0], json!({ "type": "text", "text": "look" }));
    assert_eq!(
        content[1],
        json!({ "type": "video", "uri": "https://files/abc", "mime_type": "video/mp4" })
    );
    assert_eq!(
        content[2],
        json!({ "type": "image", "data": "AAAA", "mime_type": "image/png" })
    );
    assert_eq!(content[3]["type"], "text", "no bytes: the stand-in");
    assert_eq!(content[4]["type"], "document");
    assert_eq!(content[5]["type"], "text", "no shape for a model file");
}

#[test]
fn a_stages_parameters_land_where_the_route_reads_them() {
    let mut req = request(vec![message("user", MessageContent::Text("hi".into()))]);
    req.extra = json!({
        "reasoning_effort": "none",
        "top_p": 0.9,
        "max_tokens": 5,
        "generation_config": { "seed": 7 },
        "response_format": { "type": "text" },
    });
    let body = build(&req, true);
    let generation = &body["generation_config"];
    assert_eq!(generation["thinking_level"], "minimal");
    assert_eq!(generation["top_p"], 0.9);
    assert_eq!(generation["seed"], 7);
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("reasoning_effort").is_none());
    assert_eq!(body["response_format"]["type"], "text");

    req.extra =
        json!({ "reasoning_effort": "high", "thinking_level": "low", "generation_config": 3 });
    let body = build(&req, true);
    assert_eq!(body["generation_config"]["thinking_level"], "low");

    req.extra = json!({ "thinking_level": "low", "reasoning_effort": "high" });
    let body = build(&req, true);
    assert_eq!(
        body["generation_config"]["thinking_level"], "low",
        "a level already named is kept"
    );
}

#[test]
fn text_of_reads_every_shape() {
    assert_eq!(text_of(&json!("plain")), "plain");
    assert_eq!(
        text_of(
            &json!([{ "type": "text", "text": "a" }, { "type": "image" }, { "type": "text", "text": "b" }])
        ),
        "ab"
    );
    assert_eq!(text_of(&Value::Null), "");
    assert_eq!(
        content_items(&json!("hi")),
        json!([{ "type": "text", "text": "hi" }])
    );
}

#[test]
fn a_call_with_no_words_and_no_signature_is_just_the_call_on_a_model_that_needs_none() {
    let mut req = request(vec![
        message("user", MessageContent::Text("go".into())),
        message(
            "assistant",
            MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "c".into(),
                name: "read_file".into(),
                input: json!({ "p": 1 }),
                thought_signature: None,
            }]),
        ),
        message(
            "user",
            MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "c".into(),
                content: "ok".into(),
                is_error: false,
            }]),
        ),
    ]);
    // A model outside the Gemini family is not held to signed calls.
    req.model = "learnlm-custom".into();
    let body = build(&req, true);
    let kinds: Vec<&str> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["user_input", "function_call", "function_result"]);
}
