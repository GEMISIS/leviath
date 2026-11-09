//! Reasoning kept across a tool call, and a bare name found in the listing.

use super::*;
use crate::learned::LearnedModel;
use crate::provider::{ContentBlock, Message, MessageContent};
use leviath_core::mime::MimeType;
use serde_json::json;

fn provider_at(url: Option<String>) -> OpenRouterProvider {
    OpenRouterProvider::new(reqwest::Client::new(), "k".to_string()).with_base_url(url)
}

fn request(model: &str, messages: Vec<Message>) -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages,
        model: model.to_string(),
        max_tokens: 100,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

fn user(text: &str) -> Message {
    Message {
        role: "user".to_string(),
        content: text.into(),
        cache_breakpoint: false,
        reasoning: None,
    }
}

/// An assistant turn that called a tool, carrying `reasoning`.
fn assistant_call(reasoning: Option<String>) -> Message {
    Message {
        role: "assistant".to_string(),
        content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "weather".to_string(),
            input: json!({}),
            thought_signature: None,
        }]),
        cache_breakpoint: false,
        reasoning,
    }
}

/// What a Gemini model on OpenRouter streams for a tool call: its thinking,
/// then its signature as an encrypted block, split over chunks, with no
/// `extra_content` on the call at all.
const GEMINI_STREAM: &str = concat!(
    r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":"Need the ","reasoning_details":[{"type":"reasoning.text","text":"Need the ","index":0,"format":"google-gemini-v1"}]}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"content":"","reasoning":"weather.","reasoning_details":[{"type":"reasoning.text","text":"weather.","index":0,"format":"google-gemini-v1"}]}}]}"#,
    "\n\n",
    r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"weather","arguments":"{}"}}],"reasoning_details":[{"type":"reasoning.encrypted","id":"call_1","data":"CiQ","index":1,"format":"google-gemini-v1"}]},"finish_reason":"tool_calls"}]}"#,
    "\n\n",
    r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn a_streamed_turn_replays_its_reasoning_to_the_model_that_made_it() {
    let (url, _seen) =
        leviath_testkit::spawn_mock_recorder(200, "OK", GEMINI_STREAM.as_bytes().to_vec()).await;
    let provider = provider_at(Some(url));
    let model = "google/gemini-4-argon";
    let stream = provider
        .infer_stream(&request(model, vec![user("weather?")]))
        .await
        .unwrap();
    let response = crate::provider::stream::collect_stream(stream)
        .await
        .unwrap();

    assert_eq!(
        response.reasoning_summary.as_deref(),
        Some("Need the weather.")
    );
    assert!(
        response.tool_calls[0].thought_signature.is_none(),
        "the signature rides in the details, and is not sent a second time as the call's own"
    );
    let blob = response.reasoning.expect("the details were sealed");

    // The next request: the same model gets the details back, unmodified.
    let next = vec![user("weather?"), assistant_call(Some(blob.clone()))];
    let body = provider.build_request_body(&request(model, next.clone()));
    let sent = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(
        sent["reasoning_details"],
        json!([
            { "type": "reasoning.text", "text": "Need the weather.", "index": 0, "format": "google-gemini-v1" },
            { "type": "reasoning.encrypted", "id": "call_1", "data": "CiQ", "index": 1, "format": "google-gemini-v1" },
        ])
    );
    assert!(sent["tool_calls"][0].pointer("/extra_content").is_none());

    // Another model never sees them.
    let body = provider.build_request_body(&request("anthropic/claude-sonnet-5.5", next));
    assert!(!body.to_string().contains("reasoning_details"));

    // Nor another provider's blob, nor a user turn that somehow carries one.
    let foreign = crate::responses::reasoning::seal("meta", &["x".to_string()]);
    let mut turns = vec![user("hi"), assistant_call(foreign)];
    turns[0].reasoning = Some(blob);
    let body = provider.build_request_body(&request(model, turns));
    assert!(!body.to_string().contains("reasoning_details"));
}

#[tokio::test]
async fn a_buffered_turn_seals_its_details_too() {
    let body = json!({
        "choices": [{
            "message": {
                "content": "",
                "tool_calls": [{ "id": "call_1", "type": "function",
                    "function": { "name": "weather", "arguments": "{}" } }],
                "reasoning": "hm",
                "reasoning_details": [
                    { "type": "reasoning.encrypted", "id": "call_1", "data": "CiQ", "format": "google-gemini-v1" }
                ]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string();
    let url = leviath_testkit::spawn_mock_server(200, "OK", body.as_bytes()).await;
    let provider = provider_at(Some(url));
    let model = "google/gemini-4-argon";
    let response = provider
        .infer(&request(model, vec![user("weather?")]))
        .await
        .unwrap();
    let blob = response.reasoning.expect("sealed");
    let body = provider.build_request_body(&request(
        model,
        vec![user("weather?"), assistant_call(Some(blob))],
    ));
    assert_eq!(body["messages"][1]["reasoning_details"][0]["data"], "CiQ");
}

#[test]
fn a_cache_marked_assistant_text_turn_keeps_its_reasoning() {
    let provider = provider_at(None);
    let model = "anthropic/claude-sonnet-5.5";
    let blob = crate::openai_compat::details::seal(
        model,
        &[json!({ "type": "reasoning.text", "text": "t", "signature": "s" })],
    );
    let mut turn = user("earlier answer");
    turn.role = "assistant".to_string();
    turn.cache_breakpoint = true;
    turn.reasoning = blob;
    let body = provider.build_request_body(&request(model, vec![user("q"), turn, user("more")]));
    let sent = &body["messages"][1];
    assert_eq!(sent["reasoning_details"][0]["signature"], "s");
    assert_eq!(sent["content"][0]["cache_control"]["type"], "ephemeral");
}

// ─── a bare name finds its listing ──────────────────────────────────────

fn listed(provider: &OpenRouterProvider) {
    provider.learned.replace(std::collections::HashMap::from([(
        "acme/vision-1".to_string(),
        LearnedModel {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            max_context_tokens: Some(777_000),
            ..Default::default()
        },
    )]));
}

#[test]
fn a_bare_name_reads_the_listing_as_the_full_id_does() {
    let provider = provider_at(None);
    listed(&provider);
    let image = MimeType::parse("image/png").unwrap();
    for name in ["acme/vision-1", "vision-1"] {
        assert!(provider.mime(name).accepts(&image), "{name}");
        assert_eq!(
            provider.capabilities(name).max_context_tokens,
            777_000,
            "{name}"
        );
    }
    // A name the listing does not carry falls to the guess and the table.
    assert!(!provider.mime("unlisted-model").accepts(&image));
    assert_ne!(
        provider.capabilities("unlisted-model").max_context_tokens,
        777_000
    );
}

#[test]
fn a_bare_name_finds_its_listed_price() {
    let provider = provider_at(None);
    let rates = crate::ModelPricing {
        input_per_mtok: 1.0,
        cached_input_per_mtok: 0.1,
        cache_write_per_mtok: 1.25,
        output_per_mtok: 2.0,
        long_context: None,
        unit: None,
    };
    provider.learned.replace(std::collections::HashMap::from([(
        "acme/vision-1".to_string(),
        LearnedModel {
            pricing: Some(rates),
            ..Default::default()
        },
    )]));
    assert!(provider.pricing("vision-1").is_some());
    assert!(provider.pricing("acme/vision-1").is_some());
    assert!(provider.pricing("other").is_none());
}
