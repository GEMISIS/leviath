//! Reasoning kept across a tool call, a bare name found in the listing, and
//! what a cache marker changes in two requests in a row.

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

// ─── two requests in a row ──────────────────────────────────────────────

/// `body` with every `cache_control` field removed.
fn unmarked(mut body: serde_json::Value) -> serde_json::Value {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(fields) => {
                fields.remove("cache_control");
                fields.values_mut().for_each(strip);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut body);
    body
}

/// Which of `body`'s messages carry a `cache_control` marker.
fn marked_messages(body: &serde_json::Value) -> Vec<usize> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, m)| m.to_string().contains("cache_control"))
        .map(|(i, _)| i)
        .collect()
}

/// A request on an Anthropic model repeats the one before it up to that
/// request's last marked message, byte for byte, however large the reasoning
/// turn between them: two reasoning details, one of several thousand
/// characters of text and of signature. A cache miss on such a request was
/// once seen through OpenRouter; this pins that what Leviath sends is not
/// why.
#[test]
fn a_large_reasoning_turn_between_two_requests_keeps_the_prefix() {
    let provider = provider_at(None);
    let model = "anthropic/claude-sonnet-5.5";
    let said = |role: &str, text: &str, marked: bool| Message {
        role: role.to_string(),
        content: text.into(),
        cache_breakpoint: marked,
        reasoning: None,
    };
    let called = |id: &str, text: Option<&str>, details: &[serde_json::Value]| {
        let mut blocks = Vec::new();
        if let Some(text) = text {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
        blocks.push(ContentBlock::ToolUse {
            id: id.to_string(),
            name: "read_file".to_string(),
            input: json!({ "path": "src/lib.rs" }),
            thought_signature: None,
        });
        Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: crate::openai_compat::details::seal(model, details),
        }
    };
    let answered = |id: &str| Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "fn a() {}".to_string(),
            is_error: false,
        }]),
        cache_breakpoint: false,
        reasoning: None,
    };
    let system = vec![
        crate::provider::SystemBlock {
            text: "You review code. ".repeat(600),
            cache_hint: leviath_core::CacheHint::Always,
            region: String::new(),
            volatility: leviath_core::Volatility::Stable,
        },
        crate::provider::SystemBlock {
            text: format!("## codebase\n{}", "fn a() {}\n".repeat(900)),
            cache_hint: leviath_core::CacheHint::UntilChanged,
            region: "codebase".to_string(),
            volatility: leviath_core::Volatility::Grows,
        },
    ];
    let small = [json!({ "type": "reasoning.text", "text": "Read it.", "signature": "s1" })];
    let earlier = vec![
        said("user", "Review the crate.", false),
        said("assistant", "I will read lib.rs first.", true),
        called("call_1", None, &small),
        answered("call_1"),
        said("user", "## codebase (continued)\nfn b() {}", true),
    ];
    let mut first = request(model, earlier.clone());
    first.system = system.clone();

    let large = [
        json!({
            "type": "reasoning.text",
            "text": "The parser accepts trailing commas. ".repeat(250),
            "signature": "Eq".repeat(3_000),
        }),
        json!({ "type": "reasoning.text", "text": "Now the tests.", "signature": "s2" }),
    ];
    let mut later = earlier;
    later[1].cache_breakpoint = false;
    later.extend([
        called("call_2", Some(""), &large),
        answered("call_2"),
        said("user", "## codebase (continued)\nfn c() {}", true),
    ]);
    let mut second = request(model, later);
    second.system = system;

    let (a, b) = (
        provider.build_request_body(&first),
        provider.build_request_body(&second),
    );
    let (a_messages, b_messages) = (
        a["messages"].as_array().unwrap(),
        b["messages"].as_array().unwrap(),
    );
    // The system messages, markers and all.
    let system_of = |messages: &[serde_json::Value]| {
        messages
            .iter()
            .filter(|m| m["role"] == "system")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(system_of(a_messages), system_of(b_messages));
    assert_eq!(system_of(a_messages).len(), 2);
    // Every message of the first, unmarked, leads the second.
    let (a_plain, b_plain) = (unmarked(a.clone()), unmarked(b.clone()));
    let (a_plain, b_plain) = (
        a_plain["messages"].as_array().unwrap(),
        b_plain["messages"].as_array().unwrap(),
    );
    assert!(b_plain.len() > a_plain.len());
    for (index, message) in a_plain.iter().enumerate() {
        assert_eq!(
            serde_json::to_string(&b_plain[index]).unwrap(),
            serde_json::to_string(message).unwrap(),
            "message {index}"
        );
    }
    // The second marks the message the first marked last, and its own last.
    let first_last = *marked_messages(&a).last().unwrap();
    assert_eq!(first_last, a_messages.len() - 1);
    let second_marks = marked_messages(&b);
    assert!(second_marks.contains(&first_last), "{second_marks:?}");
    assert!(
        second_marks.contains(&(b_messages.len() - 1)),
        "{second_marks:?}"
    );
    // The large details ride on the second's new assistant turn.
    let replayed_large = b_messages
        .iter()
        .filter_map(|m| m.get("reasoning_details"))
        .any(|d| d.as_array().is_some_and(|d| d.len() == 2));
    assert!(replayed_large);
}

/// One message of each kind a conversation carries, each marked when
/// `marked`: user text, assistant text, an assistant turn with text and a
/// tool call, a tool result, a turn with empty text, and a blocks turn of
/// text alone.
fn every_kind(marked: bool) -> Vec<Message> {
    let turn = |role: &str, content: MessageContent| Message {
        role: role.to_string(),
        content,
        cache_breakpoint: marked,
        reasoning: None,
    };
    vec![
        turn("user", "Review the crate.".into()),
        turn("assistant", "I will read lib.rs first.".into()),
        turn(
            "assistant",
            MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Reading.".to_string(),
                },
                call_block(),
            ]),
        ),
        turn(
            "user",
            MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "fn a() {}".to_string(),
                is_error: false,
            }]),
        ),
        turn("assistant", "".into()),
        turn(
            "user",
            MessageContent::Blocks(vec![ContentBlock::Text {
                text: "## codebase (continued)\nfn b() {}".to_string(),
            }]),
        ),
    ]
}

/// On a model that takes markers, a marker adds its `cache_control` and
/// changes nothing else: each kind of message is the same marked or not.
#[test]
fn a_marker_changes_no_byte_of_a_message_but_its_own() {
    let provider = provider_at(None);
    let model = "anthropic/claude-sonnet-5.5";
    for (index, message) in every_kind(true).into_iter().enumerate() {
        let mut plain = every_kind(false)[index].clone();
        plain.cache_breakpoint = false;
        let marked_body = provider.build_request_body(&request(model, vec![message]));
        let plain_body = provider.build_request_body(&request(model, vec![plain]));
        assert_eq!(unmarked(marked_body.clone()), plain_body, "kind {index}");
        // Each kind with text carries the marker; the empty turn has none to
        // carry, and sends no empty part.
        let carries = marked_body.to_string().contains("cache_control");
        assert_eq!(carries, index != 4, "kind {index}");
    }
    // All of them at once, every one marked or none.
    let marked_body = provider.build_request_body(&request(model, every_kind(true)));
    let plain_body = provider.build_request_body(&request(model, every_kind(false)));
    assert_eq!(unmarked(marked_body), plain_body);
    assert_eq!(plain_body["messages"][4]["content"], "");
    assert_eq!(
        plain_body["messages"][0]["content"],
        json!([{ "type": "text", "text": "Review the crate." }])
    );
}

/// A marker on a turn with no text is owed to the next turn that has some.
#[test]
fn a_marker_on_empty_text_is_carried_by_the_next_text() {
    let provider = provider_at(None);
    let mut messages = every_kind(false);
    messages[4].cache_breakpoint = true;
    let body = provider.build_request_body(&request("anthropic/claude-sonnet-5.5", messages));
    assert_eq!(marked_messages(&body), [5]);
    assert_eq!(body["messages"][4]["content"], "");
}

/// A model that takes no markers is sent what it was sent before: text as
/// strings, marked or not.
#[test]
fn a_model_without_markers_is_sent_text_as_strings() {
    let provider = provider_at(None);
    let expected = json!([
        { "role": "user", "content": "Review the crate." },
        { "role": "assistant", "content": "I will read lib.rs first." },
        {
            "role": "assistant",
            "content": "Reading.",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "weather", "arguments": "{}" }
            }]
        },
        { "role": "tool", "tool_call_id": "call_1", "content": "fn a() {}" },
        { "role": "assistant", "content": "" },
        { "role": "user", "content": "## codebase (continued)\nfn b() {}" },
    ]);
    for marked in [false, true] {
        let body = provider.build_request_body(&request("openai/gpt-5.5", every_kind(marked)));
        assert_eq!(body["messages"], expected, "marked: {marked}");
    }
}

/// A stable head and `chunks` chunks of a growing region, as assembly lays
/// them out.
fn grown_system(chunks: usize) -> Vec<crate::provider::SystemBlock> {
    let mut system = vec![crate::provider::SystemBlock {
        text: "You review code. ".repeat(600),
        cache_hint: leviath_core::CacheHint::Always,
        region: String::new(),
        volatility: leviath_core::Volatility::Stable,
    }];
    system.extend((0..chunks).map(|chunk| crate::provider::SystemBlock {
        text: format!(
            "## codebase (continued)\n{}",
            format!("fn f{chunk}() {{}}\n").repeat(900)
        ),
        cache_hint: leviath_core::CacheHint::UntilChanged,
        region: "codebase".to_string(),
        volatility: leviath_core::Volatility::Grows,
    }));
    system
}

/// The system messages of `body`.
fn system_of(body: &serde_json::Value) -> Vec<serde_json::Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .cloned()
        .collect()
}

/// A growing region that gains a chunk moves the system marker off a block
/// whose text did not change. That block reads the same either way: the
/// marker adds its `cache_control` and changes nothing else.
#[test]
fn a_system_marker_that_moves_changes_no_byte_of_the_block_it_left() {
    let provider = provider_at(None);
    let model = "anthropic/claude-sonnet-5.5";
    let body = |chunks: usize| {
        let mut asked = request(model, vec![user("go")]);
        asked.system = grown_system(chunks);
        provider.build_request_body(&asked)
    };
    let (before, after) = (body(2), body(3));
    let marked = |body: &serde_json::Value| {
        system_of(body)
            .iter()
            .enumerate()
            .filter(|(_, m)| m.to_string().contains("cache_control"))
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    // The fixture moves the marker: the last chunk before, the new one after.
    assert_eq!(marked(&before), [0, 2]);
    assert_eq!(marked(&after), [0, 3]);
    let (before, after) = (system_of(&unmarked(before)), system_of(&unmarked(after)));
    for (index, block) in before.iter().enumerate() {
        assert_eq!(
            serde_json::to_string(&after[index]).unwrap(),
            serde_json::to_string(block).unwrap(),
            "system block {index}"
        );
    }
    assert!(before[1]["content"].is_array());
}

/// An empty system block is sent as it was: a string when unmarked, and a
/// part carrying the marker when it ends the stable head.
#[test]
fn an_empty_system_block_is_sent_as_it_was() {
    let provider = provider_at(None);
    let mut system = grown_system(0);
    system.push(crate::provider::SystemBlock {
        text: String::new(),
        ..system[0].clone()
    });
    let mut asked = request("anthropic/claude-sonnet-5.5", vec![user("go")]);
    asked.system = system.clone();
    let sent = system_of(&provider.build_request_body(&asked));
    assert_eq!(
        sent[1]["content"],
        json!([{ "type": "text", "text": "", "cache_control": { "type": "ephemeral" } }])
    );
    // Two more stable blocks take both markers: unmarked, the string it was.
    system.push(system[0].clone());
    system.push(system[0].clone());
    asked.system = system;
    let sent = system_of(&provider.build_request_body(&asked));
    assert_eq!(sent[1]["content"], "");
}

/// A model that takes no markers is sent its system prompt as before: the
/// stable head and the rest, each one string.
#[test]
fn a_model_without_markers_is_sent_its_system_prompt_as_before() {
    let provider = provider_at(None);
    for chunks in [2, 3] {
        let mut asked = request("openai/gpt-5.5", vec![user("go")]);
        asked.system = grown_system(chunks);
        let sent = system_of(&provider.build_request_body(&asked));
        assert_eq!(sent, super::prefix::system_messages(&asked.system));
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|m| m["content"].is_string()));
    }
}
