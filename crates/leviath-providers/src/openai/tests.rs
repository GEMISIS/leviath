//! The OpenAI provider over local mocks of the Responses API.

use super::*;
use crate::provider::{LimitsSource, Message, MessageContent};
use crate::test_support::always_on_tracing_guard;
use leviath_testkit::{
    spawn_mock_recorder, spawn_mock_sequence, spawn_mock_server, spawn_mock_server_truncated_body,
};

fn client() -> reqwest::Client {
    crate::provider::build_http_client(None).expect("a test client builds")
}

fn provider_with_url(url: String) -> OpenAIProvider {
    OpenAIProvider::new(client(), "test-key".to_string()).with_base_url(Some(url))
}

fn simple_request() -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![Message {
            role: "user".to_string(),
            content: "hi".into(),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: "gpt-5.4".to_string(),
        max_tokens: 100,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

/// A finished response stream: a sealed reasoning item, some text, usage.
fn sse(text: &str) -> Vec<u8> {
    let frame =
        |v: serde_json::Value| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    let mut out = String::new();
    out.push_str(&frame(serde_json::json!({
        "type": "response.output_item.done",
        "item": { "type": "reasoning", "encrypted_content": "sealed-by-openai" }
    })));
    out.push_str(&frame(
        serde_json::json!({ "type": "response.output_text.delta", "delta": text }),
    ));
    out.push_str(&frame(serde_json::json!({
        "type": "response.completed",
        "response": { "status": "completed", "output": [],
            "usage": { "input_tokens": 100, "output_tokens": 20,
                "input_tokens_details": { "cached_tokens": 80 } } }
    })));
    out.into_bytes()
}

fn sent_json(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).expect("a JSON body")
}

/// Verbatim shape of OpenAI's refusal of a temperature.
const TEMPERATURE_REFUSED: &[u8] = br#"{"error":{"message":"Unsupported value: 'temperature' does not support 0.7 with this model. Only the default (1) value is supported.","type":"invalid_request_error","param":"temperature","code":"unsupported_value"}}"#;

#[test]
fn pricing_prefers_config_then_the_published_table() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "gpt-5.5".to_string(),
        crate::ModelCapabilityOverride {
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(2.0),
            ..Default::default()
        },
    );
    let provider = OpenAIProvider::with_overrides(client(), "k".to_string(), overrides, None);
    assert!(provider.learned_models().is_some());
    let configured = provider.pricing("gpt-5.5").expect("configured");
    assert_eq!(configured.input_per_mtok, 1.0);
    assert_eq!(configured.output_per_mtok, 2.0);
    assert_eq!(
        provider.pricing("gpt-5.4").expect("listed").input_per_mtok,
        2.5
    );
    assert_eq!(provider.pricing("no-such-model-9"), None);
}

#[test]
fn the_table_sizes_each_family() {
    let provider = OpenAIProvider::new(client(), "k".to_string());
    assert_eq!(provider.name(), "openai");
    let gpt55 = provider.builtin_capabilities("gpt-5.5");
    assert!(
        !gpt55.supports_temperature,
        "gpt-5.5 takes only its default"
    );
    assert!(gpt55.supports_streaming);
    assert_eq!(gpt55.max_context_tokens, 922_000);
    assert_eq!(gpt55.max_output_tokens, 128_000);
    for family in ["gpt-5.4-mini", "gpt-5.4-nano", "gpt-5-mini"] {
        assert_eq!(provider.max_context_tokens(family), 272_000, "{family}");
    }
    for large in ["gpt-5.4", "gpt-5.4-pro"] {
        assert_eq!(provider.max_context_tokens(large), 922_000, "{large}");
    }
    assert_eq!(provider.max_context_tokens("gpt-5.6-terra"), 922_000);
    let gpt41 = provider.builtin_capabilities("gpt-4.1");
    assert!(gpt41.supports_temperature);
    assert_eq!(gpt41.max_output_tokens, 32_768);
    for o in ["o3-mini", "o4-mini"] {
        let caps = provider.builtin_capabilities(o);
        assert!(!caps.supports_temperature);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 100_000);
    }
    assert_eq!(
        provider
            .builtin_capabilities("totally-unknown")
            .max_context_tokens,
        ModelCapabilities::default().max_context_tokens
    );
}

#[test]
fn an_override_replaces_the_table() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "gpt-5.4-mini".to_string(),
        ModelCapabilities {
            supports_temperature: false,
            supports_streaming: false,
            supports_tools: false,
            supports_system_prompt: false,
            max_context_tokens: 1,
            max_output_tokens: 1,
            limits_source: LimitsSource::Builtin,
        }
        .into(),
    );
    let provider = OpenAIProvider::with_overrides(client(), "k".to_string(), overrides, None);
    let caps = provider.capabilities("gpt-5.4-mini");
    assert!(!caps.supports_temperature);
    assert_eq!(caps.max_context_tokens, 1);
}

#[tokio::test]
async fn tokens_are_counted_with_tiktoken_and_a_large_prompt_off_the_async_threads() {
    let provider = OpenAIProvider::new(client(), "k".to_string());
    let small = provider.count_tokens("Hello, world!", "gpt-5.4-mini").await;
    assert!(small > 0 && small < 20);
    assert_eq!(provider.count_tokens("", "gpt-5.4-mini").await, 0);
    let text = "word ".repeat(TIKTOKEN_INLINE_BYTES / 5 + 1_000);
    assert!(text.len() > TIKTOKEN_INLINE_BYTES);
    assert_eq!(
        provider.count_tokens(&text, "gpt-5.4-mini").await,
        crate::tokenizer::count_tokens(&text, "gpt-5.4-mini")
    );
}

#[test]
fn the_base_url_is_replaced_only_when_one_is_given() {
    let kept = OpenAIProvider::new(client(), "k".to_string()).with_base_url(None);
    assert_eq!(kept.endpoint.base_url, DEFAULT_BASE_URL);
    let moved = OpenAIProvider::new(client(), "k".to_string())
        .with_base_url(Some("https://custom.example.com/".to_string()));
    assert_eq!(moved.endpoint.base_url, "https://custom.example.com");
}

#[test]
fn with_overrides_wires_the_rate_limiter() {
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 5,
        tokens_per_minute: 1_000,
    };
    let limited = OpenAIProvider::with_overrides(client(), "k".into(), HashMap::new(), Some(&cfg));
    assert!(limited.endpoint.rate_limiter.is_some());
    let unlimited = OpenAIProvider::with_overrides(client(), "k".into(), HashMap::new(), None);
    assert!(unlimited.endpoint.rate_limiter.is_none());
}

#[tokio::test]
async fn an_inference_streams_to_responses_with_store_off_and_seals_reasoning() {
    let _guard = always_on_tracing_guard();
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("hi there"))]).await;
    let provider = provider_with_url(url);
    let response = provider.infer(&simple_request()).await.unwrap();
    assert_eq!(response.content, "hi there");
    assert_eq!(response.tokens_used.cached_tokens, 80);
    assert!(
        response
            .reasoning
            .as_deref()
            .is_some_and(|r| r.contains("sealed-by-openai") && r.contains("\"openai\"")),
        "{:?}",
        response.reasoning
    );
    let body = sent_json(&bodies.lock().unwrap()[0]);
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_output_tokens"], 100);
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    assert!(body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn only_a_reasoning_model_is_asked_for_its_reasoning() {
    for (model, asked) in [
        ("gpt-4.1", false),
        ("gpt-5-chat-latest", false),
        ("o3", true),
    ] {
        let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("ok"))]).await;
        let provider = provider_with_url(url);
        let request = InferenceRequest {
            model: model.to_string(),
            ..simple_request()
        };
        provider.infer(&request).await.unwrap();
        let body = sent_json(&bodies.lock().unwrap()[0]);
        assert_eq!(body.get("include").is_some(), asked, "{model}: {body}");
    }
}

#[tokio::test]
async fn a_replayed_turn_hands_back_only_openais_reasoning() {
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("ok"))]).await;
    let provider = provider_with_url(url);
    let mut request = simple_request();
    request.messages.push(Message {
        role: "assistant".into(),
        content: MessageContent::Text("earlier".into()),
        cache_breakpoint: false,
        reasoning: crate::responses::reasoning::seal("openai", &["mine".to_string()]),
    });
    request.messages.push(Message {
        role: "assistant".into(),
        content: MessageContent::Text("elsewhere".into()),
        cache_breakpoint: false,
        reasoning: crate::responses::reasoning::seal("xai", &["theirs".to_string()]),
    });
    provider.infer(&request).await.unwrap();
    let raw = bodies.lock().unwrap()[0].clone();
    assert!(raw.contains("mine"), "{raw}");
    assert!(!raw.contains("theirs"), "{raw}");
}

#[tokio::test]
async fn chat_completions_parameters_are_translated() {
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", sse("ok")), (200, "OK", sse("ok"))]).await;
    let provider = provider_with_url(url);
    let request = InferenceRequest {
        extra: serde_json::json!({
            "reasoning_effort": "low",
            "max_completion_tokens": 9,
            "response_format": { "type": "json_schema", "json_schema": {
                "name": "answer", "schema": { "type": "object" }, "strict": true } }
        }),
        ..simple_request()
    };
    provider.infer(&request).await.unwrap();
    let body = sent_json(&bodies.lock().unwrap()[0]);
    assert_eq!(body["reasoning"]["effort"], "low");
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!(
        body["text"]["format"],
        serde_json::json!({ "type": "json_schema", "name": "answer", "schema": { "type": "object" }, "strict": true })
    );

    // A caller who already wrote the Responses names keeps them.
    let native = InferenceRequest {
        extra: serde_json::json!({
            "reasoning": { "effort": "high" },
            "reasoning_effort": "low",
            "text": { "format": { "type": "text" } },
            "response_format": { "type": "json_object" }
        }),
        ..simple_request()
    };
    provider.infer(&native).await.unwrap();
    let body = sent_json(&bodies.lock().unwrap()[1]);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["text"]["format"]["type"], "text");
}

#[test]
fn a_response_format_that_is_not_a_schema_is_carried_as_it_is() {
    assert_eq!(
        text_format(serde_json::json!({ "type": "json_object" })),
        serde_json::json!({ "type": "json_object" })
    );
    assert_eq!(
        text_format(serde_json::json!({ "type": "json_schema", "json_schema": "odd" })),
        serde_json::json!({ "type": "json_schema", "json_schema": "odd" })
    );
}

#[tokio::test]
async fn a_model_taking_no_temperature_is_sent_none_and_one_that_does_is_sent_its_own() {
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", sse("ok")), (200, "OK", sse("ok"))]).await;
    let provider = provider_with_url(url);
    provider
        .infer(&InferenceRequest {
            model: "o3".to_string(),
            ..simple_request()
        })
        .await
        .unwrap();
    provider
        .infer(&InferenceRequest {
            model: "gpt-4o".to_string(),
            temperature: 0.5,
            ..simple_request()
        })
        .await
        .unwrap();
    let sent = bodies.lock().unwrap().clone();
    assert!(!sent[0].contains("temperature"), "{}", sent[0]);
    assert!(sent[1].contains(r#""temperature":0.5"#), "{}", sent[1]);
}

#[tokio::test]
async fn a_refused_temperature_is_retried_without_one_and_remembered() {
    let (url, bodies) = spawn_mock_sequence(vec![
        (400, "Bad Request", TEMPERATURE_REFUSED.to_vec()),
        (200, "OK", sse("ok")),
        (200, "OK", sse("ok")),
    ])
    .await;
    let provider = provider_with_url(url);
    let request = InferenceRequest {
        temperature: 0.7,
        ..simple_request()
    };
    assert_eq!(provider.infer(&request).await.unwrap().content, "ok");
    assert!(provider.temperature_is_unsupported("gpt-5.4"));
    assert!(!provider.capabilities("gpt-5.4").supports_temperature);
    provider.infer(&request).await.unwrap();
    let carried: Vec<bool> = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| b.contains("temperature"))
        .collect();
    assert_eq!(carried, vec![true, false, false]);
}

#[tokio::test]
async fn a_refusal_of_anything_else_is_the_error() {
    let other = br#"{"error":{"message":"Unsupported value: 'reasoning.effort' does not support 'none' with this model.","type":"invalid_request_error"}}"#;
    let (url, bodies) = spawn_mock_sequence(vec![(400, "Bad Request", other.to_vec())]).await;
    let provider = provider_with_url(url);
    let err = provider.infer(&simple_request()).await.unwrap_err();
    assert!(err.to_string().contains("API error:"), "{err}");
    assert_eq!(bodies.lock().unwrap().len(), 1);
    let err = provider_with_url(spawn_mock_server(503, "Down", b"down").await)
        .infer_stream(&simple_request())
        .await
        .err()
        .unwrap();
    assert!(
        err.to_string().contains("API error:") || err.to_string().contains("503"),
        "{err}"
    );
}

#[tokio::test]
async fn extra_headers_ride_every_request_after_the_providers_own() {
    let (url, seen) = spawn_mock_recorder(200, "OK", sse("hi")).await;
    let provider = provider_with_url(url)
        .with_headers(vec![("X-Gateway-Token".to_string(), "t-1".to_string())]);
    provider.infer(&simple_request()).await.unwrap();
    let request = leviath_core::sync::lock(&seen)[0].to_ascii_lowercase();
    assert!(request.contains("post /responses"), "{request}");
    let own = request.find("authorization").expect("the key is sent");
    let extra = request
        .find("x-gateway-token: t-1")
        .expect("the extra is sent");
    assert!(own < extra, "{request}");
}

#[tokio::test]
async fn a_request_deadline_and_an_unreachable_host_are_errors() {
    let provider = provider_with_url("http://127.0.0.1:19997".to_string());
    let request = InferenceRequest {
        request_timeout_secs: Some(5),
        ..simple_request()
    };
    let err = provider.infer(&request).await.unwrap_err();
    assert!(err.to_string().contains("Request failed:"), "{err}");
    let err = provider.list_models().await.unwrap_err();
    assert!(err.to_string().contains("Request failed:"), "{err}");
}

#[tokio::test]
async fn images_and_pdfs_go_by_file_and_the_file_is_deleted_after() {
    let (url, seen) = spawn_mock_recorder(200, "OK", br#"{"id":"file-o"}"#.to_vec()).await;
    let provider = provider_with_url(url);
    let png = leviath_core::mime::MimeType::parse("image/png").unwrap();
    assert!(provider.media_limits("gpt-5.5").by_file(&png, 1));
    let upload = crate::files::FileUpload {
        bytes: std::sync::Arc::from(&b"\x89PNG"[..]),
        mime_type: "image/png".into(),
        name: "a.png".into(),
        ttl_secs: 3_600,
    };
    assert_eq!(provider.upload_file(&upload).await.unwrap().id, "file-o");
    assert!(
        seen.lock()
            .unwrap()
            .join("")
            .contains("name=\"purpose\"\r\n\r\nuser_data")
    );
    let (gone, deleted) = spawn_mock_recorder(404, "Not Found", b"{}".to_vec()).await;
    provider_with_url(gone)
        .delete_file(&crate::files::RemoteFile {
            id: "file-o".into(),
            uri: None,
            expires_at: None,
        })
        .await
        .unwrap();
    assert!(
        deleted
            .lock()
            .unwrap()
            .join("")
            .contains("DELETE /files/file-o")
    );
}

#[test]
fn it_claims_its_own_models_and_no_one_elses() {
    let provider = OpenAIProvider::new(client(), "k".to_string());
    assert_eq!(
        provider.serves_model("gpt-5.5"),
        Some("gpt-5.5".to_string())
    );
    assert!(provider.serves_model("claude-opus-5").is_none());
    assert!(provider.serves_model("gemini-3.1-pro-preview").is_none());
    assert!(provider.serves_model("grok-4.6").is_none());
    assert_eq!(provider.serves_model("o3"), Some("o3".to_string()));
    assert_eq!(
        provider.serves_model("o1-preview"),
        Some("o1-preview".to_string())
    );
    assert!(provider.serves_model("opus-5").is_none());
    assert!(provider.serves_model("not-a-real-model-xyz").is_none());
}

// ── The listing ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn priming_learns_ids_and_dates_but_not_shape() {
    let body = br#"{"object":"list","data":[
        {"id":"gpt-5.5","object":"model","created":1776824847,"owned_by":"system","shutdown_date":null},
        {"id":"text-embedding-3-large","object":"model","created":1,"owned_by":"system"},
        {"id":"whisper-1","object":"model","created":2,"owned_by":"openai-internal"},
        {"id":"gpt-realtime-2.1","object":"model","created":4,"owned_by":"system"},
        {"id":"gpt-transcribe","object":"model","created":5,"owned_by":"system"},
        {"id":"gpt-4o-mini-tts","object":"model","created":6,"owned_by":"system"},
        {"id":"o3","object":"model","created":3,"owned_by":"system","shutdown_date":"2027-01-01"},
        {"no_id": true},
        {"id": 42}
    ]}"#;
    let (url, _bodies) = spawn_mock_sequence(vec![(200, "OK", body.to_vec())]).await;
    let provider = provider_with_url(url);
    assert_eq!(provider.served_catalog(), None, "unprimed: cannot say");
    let before = provider.capabilities("gpt-5.5");
    provider.prime_capabilities().await.expect("primes");
    let mut catalog = provider.served_catalog().expect("primed");
    catalog.sort();
    // Chat models and the media models that run on their own routes; the
    // embeddings and the realtime (websocket) models are neither.
    assert_eq!(
        catalog,
        [
            "gpt-4o-mini-tts",
            "gpt-5.5",
            "gpt-transcribe",
            "o3",
            "whisper-1"
        ]
    );
    assert_eq!(provider.capabilities("gpt-5.5"), before);
    let listed = provider.list_models().await.expect("from the store");
    let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "gpt-4o-mini-tts",
            "gpt-5.5",
            "gpt-transcribe",
            "o3",
            "whisper-1"
        ]
    );
    let gpt = &listed[1];
    assert_eq!(gpt.released, Some(1_776_824_847));
    assert_eq!(gpt.provider, "openai");
    let o3 = &listed[3];
    assert_eq!(o3.retires.as_deref(), Some("2027-01-01"));
    assert!(o3.learned);
    assert_eq!(o3.display_name, None);
}

#[tokio::test]
async fn a_listing_that_fails_says_how() {
    let unauthorized = provider_with_url(spawn_mock_server(401, "Unauthorized", b"bad key").await);
    assert!(
        unauthorized
            .list_models()
            .await
            .unwrap_err()
            .to_string()
            .contains("401")
    );
    let garbled = provider_with_url(spawn_mock_server(200, "OK", b"not json").await);
    assert!(
        garbled
            .list_models()
            .await
            .unwrap_err()
            .to_string()
            .contains("Invalid response:")
    );
    let empty = provider_with_url(spawn_mock_server(200, "OK", br#"{"object":"list"}"#).await);
    let err = empty.prime_capabilities().await.unwrap_err();
    assert!(err.to_string().contains("data"), "{err}");
    let truncated =
        provider_with_url(spawn_mock_server_truncated_body(500, "Internal Server Error").await);
    assert!(
        truncated
            .list_models()
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown error")
    );
}

#[test]
fn a_refused_temperature_outranks_the_table_and_the_override() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "gpt-5.5".to_string(),
        ModelCapabilityOverride {
            supports_temperature: Some(true),
            ..Default::default()
        },
    );
    let provider = OpenAIProvider::with_overrides(client(), "k".to_string(), overrides, None);
    assert!(provider.capabilities("gpt-5.5").supports_temperature);
    provider.temperature_unsupported.insert("gpt-5.5");
    assert!(!provider.capabilities("gpt-5.5").supports_temperature);
}

#[tokio::test]
async fn a_callers_own_non_object_reasoning_and_text_are_left_as_written() {
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("ok"))]).await;
    let provider = provider_with_url(url);
    let request = InferenceRequest {
        extra: serde_json::json!({
            "reasoning": "high", "reasoning_effort": "low",
            "text": "plain", "response_format": { "type": "json_object" }
        }),
        ..simple_request()
    };
    provider.infer(&request).await.unwrap();
    let body = sent_json(&bodies.lock().unwrap()[0]);
    assert_eq!(body["reasoning"], "high");
    assert_eq!(body["text"], "plain");
}

/// A second host of OpenAI's API: registered under its own name, listing
/// under that name, sending its key in the header it was told to, and
/// routing the deployment names it serves.
#[tokio::test]
async fn a_named_host_uses_its_own_name_header_and_deployments() {
    let listing = br#"{"data":[{"id":"gpt-5.5","created":1}]}"#;
    let (url, recorded) = spawn_mock_recorder(200, "OK", listing.to_vec()).await;
    let provider = OpenAIProvider::new(client(), "azure-key".to_string())
        .with_base_url(Some(url))
        .named("azure-east")
        .with_auth_header(Some("api-key".to_string()))
        .with_serves(vec!["prod-gpt55".to_string()]);
    assert_eq!(provider.name(), "azure-east");
    assert_eq!(
        provider.serves_model("prod-gpt55").as_deref(),
        Some("prod-gpt55")
    );
    assert_eq!(provider.serves_model("gpt-5.5").as_deref(), Some("gpt-5.5"));
    assert_eq!(provider.serves_model("llama"), None);

    let listed = provider.list_models().await.expect("lists");
    assert_eq!(listed[0].provider, "azure-east");
    let catalog = provider.served_catalog().expect("primed");
    assert!(catalog.contains(&"prod-gpt55".to_string()));
    let request = recorded.lock().unwrap()[0].to_ascii_lowercase();
    assert!(request.contains("api-key: azure-key"));
    assert!(!request.contains("authorization"));
}

/// A media model is run on its own route, buffered or streamed, priced from
/// the shipped table, and takes what its route takes by value.
#[tokio::test]
async fn a_media_model_runs_on_its_own_route_through_the_provider() {
    let reply = serde_json::json!({ "output_format": "png", "data": [{ "b64_json": "UE5H" }],
        "usage": { "input_tokens": 10, "output_tokens": 10 } });
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", reply.to_string().into_bytes()),
        (200, "OK", reply.to_string().into_bytes()),
    ])
    .await;
    let provider = provider_with_url(url).with_poll_interval(std::time::Duration::from_millis(1));
    let request = InferenceRequest {
        model: "gpt-image-1".into(),
        ..simple_request()
    };
    let made = provider.infer(&request).await.expect("an image");
    assert_eq!(made.parts.len(), 1);
    assert!(
        made.tokens_used.reported_cost_usd.is_some(),
        "the shipped table prices gpt-image-1 by its tokens"
    );
    let streamed = provider.infer_stream(&request).await.expect("a stream");
    let collected = crate::provider::collect_stream(streamed).await.unwrap();
    assert_eq!(collected.parts.len(), 1);
    assert!(bodies.lock().unwrap()[0].contains("\"prompt\""));

    assert_eq!(provider.mime("sora-2").output, ["video/*"]);
    assert!(
        provider
            .mime("gpt-5.5")
            .output
            .iter()
            .all(|t| t != "video/*"),
        "a chat model is narrowed to what a Responses body carries"
    );
    assert_eq!(provider.media_limits("sora-2").file_bytes, None);
    assert!(provider.media_limits("gpt-5.5").file_bytes.is_some());
    assert_eq!(provider.serves_model("whisper-1"), Some("whisper-1".into()));
    assert!(!provider.capabilities("tts-1").supports_tools);
}
