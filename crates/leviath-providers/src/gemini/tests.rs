//! The Gemini provider over local mocks of the native API.

use super::*;
use crate::provider::Message;
use crate::test_support::always_on_tracing_guard;
use leviath_testkit::{
    spawn_mock_recorder, spawn_mock_sequence, spawn_mock_server, spawn_mock_server_truncated_body,
};

fn client() -> reqwest::Client {
    crate::provider::build_http_client(None).expect("a test client builds")
}

fn provider_at(url: &str) -> GeminiProvider {
    GeminiProvider::new(client(), "test-key".to_string()).with_base_url(Some(url.to_string()))
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
        model: "gemini-3.5-flash".to_string(),
        max_tokens: 100,
        temperature: 0.2,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

fn sse(text: &str) -> Vec<u8> {
    [
        serde_json::json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "text", "text": text } }),
        serde_json::json!({ "event_type": "interaction.completed", "interaction": { "status": "completed",
            "usage": { "total_input_tokens": 3, "total_output_tokens": 2 } } }),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["event_type"].as_str().unwrap()))
    .collect::<String>()
    .into_bytes()
}

#[test]
fn pricing_prefers_config_then_the_published_table() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "gemini-3.5-flash".to_string(),
        crate::ModelCapabilityOverride {
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(2.0),
            ..Default::default()
        },
    );
    let provider = GeminiProvider::with_overrides(client(), "k".to_string(), overrides, None);
    assert!(provider.learned_models().is_some());
    assert_eq!(
        provider.pricing("gemini-3.5-flash").unwrap().input_per_mtok,
        1.0
    );
    assert_eq!(
        provider
            .pricing("gemini-3.1-pro-preview")
            .unwrap()
            .input_per_mtok,
        2.0
    );
    assert_eq!(provider.pricing("no-such-model-9"), None);
}

#[test]
fn the_base_is_the_native_root_whatever_spelling_names_it() {
    let provider = GeminiProvider::new(client(), "k".to_string());
    assert_eq!(provider.name(), "google");
    assert_eq!(provider.base_url, DEFAULT_BASE_URL);
    for configured in [
        "https://proxy.local/v1beta/openai",
        "https://proxy.local/v1beta/openai/",
        "https://proxy.local/v1beta/",
    ] {
        assert_eq!(
            provider_at(configured).base_url,
            "https://proxy.local/v1beta",
            "{configured}"
        );
    }
    assert_eq!(
        GeminiProvider::new(client(), "k".into())
            .with_base_url(None)
            .base_url,
        DEFAULT_BASE_URL
    );
}

#[test]
fn the_family_table_and_an_override_size_a_model() {
    assert_eq!(
        GeminiFamily::classify("gemini-3.1-flash-lite"),
        GeminiFamily::FlashLite
    );
    assert_eq!(
        GeminiFamily::classify("gemini-3.1-pro-preview"),
        GeminiFamily::Pro
    );
    assert_eq!(
        GeminiFamily::classify("gemini-3.5-flash"),
        GeminiFamily::Flash
    );
    assert_eq!(GeminiFamily::classify("gemini-future"), GeminiFamily::Other);
    let provider = GeminiProvider::new(client(), "k".to_string());
    assert_eq!(provider.max_context_tokens("gemini-3.5-flash"), 1_048_576);
    assert_eq!(
        provider
            .capabilities("gemini-3.1-pro-preview")
            .max_output_tokens,
        65_535
    );
    let mut overrides = HashMap::new();
    overrides.insert(
        "gemini-custom".to_string(),
        ModelCapabilities {
            supports_temperature: false,
            supports_streaming: false,
            supports_tools: false,
            supports_system_prompt: false,
            max_context_tokens: 42,
            max_output_tokens: 10,
            limits_source: LimitsSource::Builtin,
        }
        .into(),
    );
    let provider = GeminiProvider::with_overrides(client(), "k".to_string(), overrides, None);
    let caps = provider.capabilities("gemini-custom");
    assert_eq!(caps.max_context_tokens, 42);
    assert!(!caps.supports_temperature);
}

#[test]
fn with_overrides_wires_the_rate_limiter() {
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 5,
        tokens_per_minute: 1_000,
    };
    assert!(
        GeminiProvider::with_overrides(client(), "k".into(), HashMap::new(), Some(&cfg))
            .rate_limiter
            .is_some()
    );
    assert!(
        GeminiProvider::with_overrides(client(), "k".into(), HashMap::new(), None)
            .rate_limiter
            .is_none()
    );
}

#[test]
fn it_claims_its_own_models_and_no_one_elses() {
    let provider = GeminiProvider::new(client(), "k".to_string());
    assert_eq!(
        provider.serves_model("gemini-3.1-pro-preview"),
        Some("gemini-3.1-pro-preview".to_string())
    );
    for other in [
        "claude-opus-5",
        "gpt-5.5",
        "grok-4.6",
        "not-a-real-model-xyz",
    ] {
        assert!(provider.serves_model(other).is_none(), "{other}");
    }
}

// ── Inference ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_inference_streams_from_interactions_with_the_key_and_store_off() {
    let _guard = always_on_tracing_guard();
    let (url, seen) = spawn_mock_recorder(200, "OK", sse("hi there")).await;
    let provider =
        provider_at(&url).with_headers(vec![("X-Gateway-Token".to_string(), "t-1".to_string())]);
    let response = provider.infer(&simple_request()).await.unwrap();
    assert_eq!(response.content, "hi there");
    assert_eq!(response.tokens_used.prompt_tokens, 3);
    let raw = seen.lock().unwrap().join("");
    let lower = raw.to_ascii_lowercase();
    assert!(raw.contains("POST /interactions"), "{raw}");
    assert!(lower.contains("x-goog-api-key: test-key"), "{raw}");
    let own = lower.find("x-goog-api-key").unwrap();
    let extra = lower.find("x-gateway-token: t-1").unwrap();
    assert!(own < extra, "{raw}");
    assert!(raw.contains(r#""store":false"#), "{raw}");
    assert!(raw.contains(r#""temperature":0.2"#), "{raw}");
}

#[tokio::test]
async fn a_refusal_and_an_unreachable_host_are_errors() {
    let refused = provider_at(&spawn_mock_server(500, "Internal Server Error", b"boom").await);
    let err = refused.infer(&simple_request()).await.unwrap_err();
    assert!(err.to_string().contains("API error:"), "{err}");
    let gone = provider_at("http://127.0.0.1:19997");
    let err = gone.infer_stream(&simple_request()).await.err().unwrap();
    assert!(err.to_string().contains("Request failed:"), "{err}");
}

#[tokio::test]
async fn a_rate_limited_provider_waits_its_turn_for_an_inference() {
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", sse("ok"))]).await;
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 60,
        tokens_per_minute: 1_000_000,
    };
    let provider = GeminiProvider::with_overrides(client(), "k".into(), HashMap::new(), Some(&cfg))
        .with_base_url(Some(url));
    assert_eq!(
        provider.infer(&simple_request()).await.unwrap().content,
        "ok"
    );
}

// ── Counting ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tokens_are_counted_exactly_and_estimated_when_the_count_fails() {
    let exact = provider_at(&spawn_mock_server(200, "OK", br#"{"totalTokens": 99}"#).await);
    assert_eq!(exact.count_tokens("anything", "gemini-3.5-flash").await, 99);
    for failing in [
        spawn_mock_server(500, "Internal Server Error", b"boom").await,
        spawn_mock_server(200, "OK", b"not json").await,
        spawn_mock_server(200, "OK", br#"{"unexpected": true}"#).await,
        "http://127.0.0.1:19997".to_string(),
    ] {
        let provider = provider_at(&failing);
        assert_eq!(
            provider.count_tokens("12345678", "gemini-3.5-flash").await,
            2
        );
    }
    assert_eq!(
        provider_at("http://127.0.0.1:19997")
            .count_tokens("", "gemini-3.5-flash")
            .await,
        0
    );
}

#[tokio::test]
async fn the_count_call_goes_through_the_rate_limiter() {
    let (base, _bodies) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"totalTokens": 9}"#.to_vec()),
        (200, "OK", br#"{"totalTokens": 9}"#.to_vec()),
    ])
    .await;
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 1,
        tokens_per_minute: 1_000_000,
    };
    let provider = GeminiProvider::with_overrides(client(), "k".into(), HashMap::new(), Some(&cfg))
        .with_base_url(Some(base));
    assert_eq!(provider.count_tokens("first", "gemini-3.5-flash").await, 9);
    let held = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        provider.count_tokens("second", "gemini-3.5-flash"),
    )
    .await;
    assert!(held.is_err(), "the second count waits for the next minute");
}

// ── Files ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_media_part_goes_by_uri_and_the_trait_reaches_the_files_api() {
    let provider = GeminiProvider::new(client(), "k".to_string());
    let video = leviath_core::mime::MimeType::parse("video/mp4").unwrap();
    assert!(provider.media_limits("gemini-3.5-flash").by_file(&video, 1));
    assert!(provider.mime("gemini-3.5-flash").accepts(&video));
    let unreachable = provider_at("http://127.0.0.1:9");
    let upload = crate::files::FileUpload {
        bytes: std::sync::Arc::from(&b"x"[..]),
        mime_type: "video/mp4".into(),
        name: "a.mp4".into(),
        ttl_secs: 3_600,
    };
    assert!(unreachable.upload_file(&upload).await.is_err());
    let file = crate::files::RemoteFile {
        id: "files/a".into(),
        uri: None,
        expires_at: None,
    };
    assert!(unreachable.delete_file(&file).await.is_err());
}

// ── The listing ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn priming_teaches_capabilities_what_the_listing_knew_and_an_override_still_wins() {
    let body = br#"{"models":[
        {"name":"models/gemini-3.5-flash","displayName":"Flash","inputTokenLimit":2000000,"outputTokenLimit":8192},
        {"name":"models/gemini-bare","displayName":"Bare"},
        {"name":"gemini-unprefixed","inputTokenLimit":500000},
        {"no_name":true},
        {"name":123}
    ]}"#;
    let url = spawn_mock_server(200, "OK", body).await;
    let mut overrides = HashMap::new();
    overrides.insert(
        "gemini-3.5-flash".to_string(),
        ModelCapabilityOverride {
            max_context_tokens: Some(64_000),
            ..Default::default()
        },
    );
    let provider = GeminiProvider::with_overrides(client(), "k".to_string(), overrides, None)
        .with_base_url(Some(url));
    assert_eq!(
        provider.capabilities("gemini-bare").limits_source,
        LimitsSource::Builtin
    );
    let models = provider.list_models().await.unwrap();
    let mut ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    assert_eq!(
        ids,
        ["gemini-3.5-flash", "gemini-bare", "gemini-unprefixed"]
    );
    let caps = provider.capabilities("gemini-3.5-flash");
    assert_eq!(caps.max_context_tokens, 64_000, "the operator's number");
    assert_eq!(caps.max_output_tokens, 8_192, "and the API's for the rest");
    assert_eq!(caps.limits_source, LimitsSource::Override);
    assert_eq!(
        provider.capabilities("gemini-bare").limits_source,
        LimitsSource::Builtin,
        "nothing reported, nothing claimed"
    );
    assert_eq!(
        provider
            .capabilities("gemini-unprefixed")
            .max_context_tokens,
        500_000
    );
}

#[tokio::test]
async fn priming_follows_the_page_token_and_keeps_chat_models_only() {
    let page_one = br#"{"models":[
        {"name":"models/gemini-3.7-flash","displayName":"Gemini 3.7 Flash",
         "inputTokenLimit":1048576,"outputTokenLimit":65536,"maxTemperature":2,
         "supportedGenerationMethods":["generateContent","countTokens"]}
    ],"nextPageToken":"page-two"}"#;
    let page_two = br#"{"models":[
        {"name":"models/gemini-embedding-2","inputTokenLimit":8192,
         "supportedGenerationMethods":["embedContent"]},
        {"name":"models/gemini-fixed","inputTokenLimit":32768,"maxTemperature":0,
         "supportedGenerationMethods":["generateContent"]},
        {"name":"models/gemini-quiet","supportedGenerationMethods":["generateContent"]}
    ],"nextPageToken":""}"#;
    let (url, _bodies) = spawn_mock_sequence(vec![
        (200, "OK", page_one.to_vec()),
        (200, "OK", page_two.to_vec()),
    ])
    .await;
    let provider = provider_at(&url);
    assert_eq!(provider.served_catalog(), None, "unprimed: cannot say");
    provider.prime_capabilities().await.expect("primes");
    let mut catalog = provider.served_catalog().expect("primed");
    catalog.sort();
    assert_eq!(
        catalog,
        ["gemini-3.7-flash", "gemini-fixed", "gemini-quiet"]
    );
    let flash = provider.capabilities("gemini-3.7-flash");
    assert!(flash.supports_temperature);
    assert_eq!(flash.limits_source, LimitsSource::Api);
    assert!(!provider.capabilities("gemini-fixed").supports_temperature);
    let quiet = provider.capabilities("gemini-quiet");
    assert!(quiet.supports_temperature);
    assert_eq!(quiet.limits_source, LimitsSource::Builtin);
    let models = provider
        .list_models()
        .await
        .expect("answered from the store");
    assert_eq!(models.len(), 3);
}

#[tokio::test]
async fn a_listing_that_fails_says_how() {
    let cases: Vec<(String, &str)> = vec![
        (
            spawn_mock_server(401, "Unauthorized", b"bad key").await,
            "401",
        ),
        (
            spawn_mock_server(200, "OK", b"not json").await,
            "Invalid response:",
        ),
        (
            spawn_mock_server(200, "OK", b"{}").await,
            "Invalid response:",
        ),
        (
            spawn_mock_server_truncated_body(500, "Internal Server Error").await,
            "500",
        ),
        ("http://127.0.0.1:19997".to_string(), "Request failed:"),
    ];
    for (url, said) in cases {
        let err = provider_at(&url).list_models().await.unwrap_err();
        assert!(err.to_string().contains(said), "{url}: {err}");
    }
}
