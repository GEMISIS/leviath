//! Reasoning kept across a tool call, and a bare name found in the listing.

use super::*;
use crate::learned::LearnedModel;
use crate::provider::{ContentBlock, Message, MessageContent};
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
use serde_json::json;
use tokio_stream::StreamExt;

const GEMINI: &str = "google/gemini-4-argon";

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

fn call_block() -> ContentBlock {
    ContentBlock::ToolUse {
        id: "call_1".to_string(),
        name: "weather".to_string(),
        input: json!({}),
        thought_signature: None,
    }
}

/// An assistant turn that called a tool, carrying `reasoning`.
fn assistant_call(reasoning: Option<String>) -> Message {
    Message {
        role: "assistant".to_string(),
        content: MessageContent::Blocks(vec![call_block()]),
        cache_breakpoint: false,
        reasoning,
    }
}

/// The blob a reply from `model` would have sealed.
fn blob_from(model: &str) -> Option<String> {
    crate::openai_compat::details::seal(
        model,
        &[json!({ "type": "reasoning.encrypted", "data": "CiQ", "id": "call_1" })],
    )
}

/// The `reasoning_details` the request body sends, if any, and the role of the
/// message that carries them.
fn replayed(body: &serde_json::Value) -> Option<(String, serde_json::Value)> {
    body["messages"].as_array()?.iter().find_map(|m| {
        m.get("reasoning_details")
            .map(|d| (m["role"].as_str().unwrap().to_string(), d.clone()))
    })
}

/// What a Gemini model on OpenRouter streams for a tool call: its thinking,
/// then the call, with its signature as an encrypted block that lands AFTER
/// the chunk carrying the finish reason, a trailing chunk that repeats the
/// finish reason beside the usage, and no `extra_content` on the call at all.
const GEMINI_STREAM: &str = concat!(
    r#"data: {"model":"google/gemini-4-argon","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":"Need the ","reasoning_details":[{"type":"reasoning.text","text":"Need the ","index":0,"format":"google-gemini-v1"}]}}]}"#,
    "\n\n",
    r#"data: {"model":"google/gemini-4-argon","choices":[{"index":0,"delta":{"content":"","reasoning":"weather.","reasoning_details":[{"type":"reasoning.text","text":"weather.","index":0,"format":"google-gemini-v1"}]}}]}"#,
    "\n\n",
    r#"data: {"model":"google/gemini-4-argon","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"weather","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
    "\n\n",
    r#"data: {"model":"google/gemini-4-argon","choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.encrypted","id":"call_1","data":"CiQ","index":1,"format":"google-gemini-v1"}]}}]}"#,
    "\n\n",
    r#"data: {"model":"google/gemini-4-argon","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":7}}"#,
    "\n\n",
    "data: [DONE]\n\n",
);

/// Every chunk a mock body streams back for `model`.
async fn stream_chunks(body: &str, request: &InferenceRequest) -> Vec<StreamChunk> {
    let (url, _seen) =
        leviath_testkit::spawn_mock_recorder(200, "OK", body.as_bytes().to_vec()).await;
    let mut stream = provider_at(Some(url)).infer_stream(request).await.unwrap();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk.unwrap());
    }
    chunks
}

#[tokio::test]
async fn a_streamed_turn_replays_its_reasoning_to_the_model_that_made_it() {
    let provider = provider_at(None);
    let asked = request(GEMINI, vec![user("weather?")]);
    let chunks = stream_chunks(GEMINI_STREAM, &asked).await;

    // One blob, on the last chunk, with the block that arrived after the
    // finish reason in it.
    let sealed: Vec<_> = chunks.iter().filter_map(|c| c.reasoning.clone()).collect();
    assert_eq!(sealed.len(), 1, "sealed once");
    assert!(chunks.last().unwrap().reasoning.is_some());
    assert!(chunks.last().unwrap().finish_reason.is_none());
    let response = crate::provider::stream::collect_stream(Box::pin(tokio_stream::iter(
        chunks.into_iter().map(Ok),
    )))
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

    // The next request: the same model gets the details back, unmodified, on
    // the assistant message.
    let next = vec![user("weather?"), assistant_call(Some(blob.clone()))];
    let body = provider.build_request_body(&request(GEMINI, next.clone()));
    let (role, details) = replayed(&body).expect("replayed");
    assert_eq!(role, "assistant");
    assert_eq!(
        details,
        json!([
            { "type": "reasoning.text", "text": "Need the weather.", "index": 0, "format": "google-gemini-v1" },
            { "type": "reasoning.encrypted", "id": "call_1", "data": "CiQ", "index": 1, "format": "google-gemini-v1" },
        ])
    );
    let sent = &body["messages"][1];
    assert!(sent["tool_calls"][0].pointer("/extra_content").is_none());

    // Another model never sees them.
    let body = provider.build_request_body(&request("anthropic/claude-sonnet-5.5", next));
    assert!(replayed(&body).is_none());

    // Nor another provider's blob, nor a user turn that somehow carries one.
    let foreign = crate::responses::reasoning::seal("meta", &["x".to_string()]);
    let mut turns = vec![user("hi"), assistant_call(foreign)];
    turns[0].reasoning = Some(blob);
    let body = provider.build_request_body(&request(GEMINI, turns));
    assert!(replayed(&body).is_none());
}

/// Without a `[DONE]` the connection closing is the end, and the blob goes out
/// then.
#[tokio::test]
async fn a_stream_that_just_closes_seals_when_the_bytes_stop() {
    let body = GEMINI_STREAM.replace("data: [DONE]\n\n", "");
    let chunks = stream_chunks(&body, &request(GEMINI, vec![user("q")])).await;
    assert_eq!(chunks.iter().filter(|c| c.reasoning.is_some()).count(), 1);
    // A stream with no reasoning at all has nothing to add at the end.
    let plain = r#"data: {"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#
        .to_string() + "\n\n";
    let chunks = stream_chunks(&plain, &request(GEMINI, vec![user("q")])).await;
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].reasoning.is_none());
}

#[tokio::test]
async fn a_buffered_turn_seals_its_details_to_the_model_that_answered() {
    let body = json!({
        "model": GEMINI,
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
    let response = provider
        .infer(&request(GEMINI, vec![user("weather?")]))
        .await
        .unwrap();
    assert_eq!(response.reasoning_summary.as_deref(), Some("hm"));
    let blob = response.reasoning.expect("sealed");
    let body = provider.build_request_body(&request(
        GEMINI,
        vec![user("weather?"), assistant_call(Some(blob))],
    ));
    assert_eq!(replayed(&body).unwrap().1[0]["data"], "CiQ");
}

/// An audio answer can call a tool too; its reasoning is kept and shown the
/// same way, and goes out with the audio at the end.
#[tokio::test]
async fn an_audio_stream_keeps_and_shows_its_reasoning() {
    let body = concat!(
        r#"data: {"model":"google/gemini-4-argon","choices":[{"delta":{"reasoning":"hm","reasoning_details":[{"type":"reasoning.encrypted","id":"call_1","data":"CiQ","index":0}],"audio":{"data":"UklG","format":"wav"}}}]}"#,
        "\n\n",
        r#"data: {"model":"google/gemini-4-argon","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"weather","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let mut asked = request(GEMINI, vec![user("speak")]);
    asked.extra = json!({ "output_mimes": ["audio/wav"] });
    let chunks = stream_chunks(body, &asked).await;
    assert_eq!(chunks[0].reasoning_delta.as_deref(), Some("hm"));
    let last = chunks.last().unwrap();
    assert!(
        last.reasoning.is_some(),
        "the reasoning goes out at the end"
    );
    assert_eq!(chunks.iter().filter(|c| c.reasoning.is_some()).count(), 1);
    assert_eq!(chunks.iter().map(|c| c.parts.len()).sum::<usize>(), 1);

    // Audio held back until the end goes out with the reasoning in one chunk,
    // and a stream with neither ends quietly.
    let late = concat!(
        r#"data: {"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","data":"x","index":0}],"audio":{"data":"UklG"}}}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let chunks = stream_chunks(late, &asked).await;
    assert_eq!(chunks.len(), 2);
    assert!(chunks[1].reasoning.is_some());
    assert_eq!(chunks[1].parts.len(), 1);
    let neither = "data: [DONE]\n\n";
    assert!(stream_chunks(neither, &asked).await.is_empty());
    // A connection that closes without `[DONE]` seals then.
    let closed = late.replace("data: [DONE]\n\n", "");
    let chunks = stream_chunks(&closed, &asked).await;
    assert_eq!(chunks.iter().filter(|c| c.reasoning.is_some()).count(), 1);
}

// ─── where the details go ───────────────────────────────────────────────

fn png() -> ContentBlock {
    let reg = MimeRegistry::builtin();
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
    let part = Part::stored(blob.describe(&reg)).named("a.png");
    let mut block = ContentBlock::mime(&part).unwrap();
    if let ContentBlock::Mime { data, .. } = &mut block {
        *data = "AQID".to_string();
    }
    block
}

/// A turn that converts to several messages gets the details on the one that
/// is the assistant's, wherever it sits.
#[test]
fn the_details_ride_on_the_assistant_message_not_the_last_one() {
    let provider = provider_at(None);
    let blob = blob_from(GEMINI);
    let blocks = |extra: Vec<ContentBlock>| {
        let mut all = vec![
            call_block(),
            ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "sunny".to_string(),
                is_error: false,
            },
        ];
        all.extend(extra);
        Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(all),
            cache_breakpoint: false,
            reasoning: blob.clone(),
        }
    };
    for turn in [blocks(vec![]), blocks(vec![png()])] {
        let body = provider.build_request_body(&request(GEMINI, vec![user("q"), turn]));
        let roles: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(roles[..3], ["user", "assistant", "tool"], "{roles:?}");
        let (role, _) = replayed(&body).expect("replayed");
        assert_eq!(role, "assistant");
        assert!(body["messages"][1]["reasoning_details"].is_array());
        for later in &body["messages"].as_array().unwrap()[2..] {
            assert!(later.get("reasoning_details").is_none());
        }
    }
    // A turn that converts to no assistant message has nowhere to put them.
    let mut turn = user("hi");
    turn.reasoning = blob;
    let body = provider.build_request_body(&request(GEMINI, vec![turn]));
    assert!(replayed(&body).is_none());
}

// ─── whose reasoning it is ──────────────────────────────────────────────

#[test]
fn the_details_replay_only_to_the_model_that_answered() {
    let provider = provider_at(None);
    let next = |model: &str, extra: serde_json::Value, blob: Option<String>| {
        let mut asked = request(
            model,
            vec![user("q"), assistant_call(blob), user("and then?")],
        );
        asked.extra = extra;
        replayed(&provider.build_request_body(&asked)).is_some()
    };
    let none = serde_json::Value::Null;
    // The plain case, and a routing variant on either side.
    assert!(next(GEMINI, none.clone(), blob_from(GEMINI)));
    assert!(next(
        "google/gemini-4-argon:nitro",
        none.clone(),
        blob_from(GEMINI)
    ));
    assert!(next(
        GEMINI,
        none.clone(),
        blob_from("google/gemini-4-argon:floor")
    ));
    // A router, or a fallback list, may land somewhere else than where the
    // blocks were signed.
    assert!(!next("openrouter/auto", none.clone(), blob_from(GEMINI)));
    assert!(!next(
        GEMINI,
        json!({ "models": [GEMINI, "anthropic/claude-sonnet-5.5"] }),
        blob_from(GEMINI)
    ));
    // An empty list is no routing.
    assert!(next(GEMINI, json!({ "models": [] }), blob_from(GEMINI)));
    // Blocks that an auto-routed turn got from another model.
    assert!(!next(
        GEMINI,
        none.clone(),
        blob_from("anthropic/claude-sonnet-5.5")
    ));
    // A blob with no model, such as one from before it was kept.
    assert!(!next(
        GEMINI,
        none,
        Some(r#"{"openrouter":{"provider":"openrouter","details":[{"a":1}]}}"#.to_string())
    ));
}

/// A bare name is the model its listing names in full, so a reply sealed to
/// the full id replays to a request that spells it bare.
#[test]
fn a_bare_request_name_replays_through_the_listing() {
    let provider = provider_at(None);
    listed(&provider);
    let blob = blob_from("acme/vision-1");
    let body = provider.build_request_body(&request(
        "vision-1",
        vec![user("q"), assistant_call(blob.clone())],
    ));
    assert!(replayed(&body).is_some());
    // Unlisted, the bare name cannot be told to be that model.
    let bare = provider_at(None);
    let body = bare.build_request_body(&request("vision-1", vec![user("q"), assistant_call(blob)]));
    assert!(replayed(&body).is_none());
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

/// One name, one model: routing, rates, limits, modalities and the cache
/// markers all read the same listed id, and two ids that share a last segment
/// resolve to the smallest, whatever order they were learned in.
#[test]
fn a_name_two_ids_share_means_one_model_everywhere() {
    let provider = provider_at(None);
    let entry = |context, cache| LearnedModel {
        max_context_tokens: Some(context),
        explicit_cache_control: Some(cache),
        pricing: Some(crate::ModelPricing {
            input_per_mtok: context as f64,
            cached_input_per_mtok: 0.0,
            cache_write_per_mtok: 0.0,
            output_per_mtok: 0.0,
            long_context: None,
            unit: None,
            cache_write_1h_per_mtok: None,
            unpriced_above: None,
        }),
        ..Default::default()
    };
    provider.learned.replace(std::collections::HashMap::from([
        ("zed/twin".to_string(), entry(2_000, true)),
        ("acme/twin".to_string(), entry(1_000, false)),
    ]));
    assert_eq!(provider.learned.find_by_key("twin").unwrap(), "acme/twin");
    assert_eq!(provider.serves_model("twin").as_deref(), Some("acme/twin"));
    assert_eq!(provider.capabilities("twin").max_context_tokens, 1_000);
    assert_eq!(provider.pricing("twin").unwrap().input_per_mtok, 1_000.0);
    assert!(!provider.explicit_cache_control("twin"));
    // The exact id is that model, however many share its tail.
    assert_eq!(provider.capabilities("zed/twin").max_context_tokens, 2_000);
    assert!(provider.explicit_cache_control("zed/twin"));
    assert_eq!(
        provider.serves_model("zed/twin").as_deref(),
        Some("zed/twin")
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
        cache_write_1h_per_mtok: None,
        unpriced_above: None,
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
