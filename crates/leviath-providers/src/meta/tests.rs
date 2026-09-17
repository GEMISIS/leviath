//! The provider over a local mock of Meta's Model API.

use super::*;
use crate::provider::{Message, MessageContent};
use leviath_testkit::{spawn_mock_recorder, spawn_mock_sequence, spawn_mock_server};

fn provider(url: &str) -> MetaProvider {
    MetaProvider::new(reqwest::Client::new(), "meta-key".into()).with_base_url(Some(url.into()))
}

fn request(model: &str) -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Text("earlier".to_string()),
                cache_breakpoint: false,
                reasoning: crate::responses::reasoning::seal("meta", &["meta-thought".into()]),
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Text("go".to_string()),
                cache_breakpoint: false,
                reasoning: None,
            },
        ],
        model: model.to_string(),
        max_tokens: 4096,
        temperature: 0.2,
        tools: vec![],
        extra: serde_json::json!({ "stop": ["END"] }),
        request_timeout_secs: None,
    }
}

fn sse(text: &str) -> Vec<u8> {
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({ "type": "response.output_text.delta", "delta": text }),
        serde_json::json!({ "type": "response.completed", "response": {
            "usage": { "input_tokens": 50, "output_tokens": 5,
                "input_tokens_details": { "cached_tokens": 40 } } } })
    )
    .into_bytes()
}

#[tokio::test]
async fn an_inference_replays_meta_reasoning_and_drops_what_meta_refuses() {
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("done"))]).await;
    let response = provider(&url)
        .with_reasoning_effort(Some("high".into()))
        .infer(&request("muse-spark-1.3"))
        .await
        .expect("inference");
    assert_eq!(response.content, "done");
    assert_eq!(response.tokens_used.cached_tokens, 40);
    assert_eq!(response.tokens_used.reported_cost_usd, None);
    let body: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 4096);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert!(body.get("stop").is_none(), "{body}");
    assert_eq!(body["input"][0]["type"], "reasoning");
    assert_eq!(body["input"][0]["encrypted_content"], "meta-thought");
    assert_eq!(body["input"][0]["summary"], serde_json::json!([]));
}

#[tokio::test]
async fn an_effort_of_none_is_never_sent() {
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("ok"))]).await;
    provider(&url)
        .with_reasoning_effort(Some("none".into()))
        .infer(&request("muse-spark-1.3"))
        .await
        .unwrap();
    assert!(!bodies.lock().unwrap()[0].contains("\"reasoning\":{"));
}

#[tokio::test]
async fn a_refused_request_is_an_error_and_the_key_and_headers_are_sent() {
    let url = spawn_mock_server(
        400,
        "Bad Request",
        br#"{"error":{"message":"bad"}}"#.to_vec(),
    )
    .await;
    assert!(
        provider(&url)
            .infer(&request("muse-spark-1.3"))
            .await
            .is_err()
    );

    let (url, seen) = spawn_mock_recorder(200, "OK", sse("hi")).await;
    let _stream = provider(&url)
        .with_headers(vec![("X-Tenant".into(), "t1".into())])
        .with_request_timeout(Some(10))
        .with_rate_limit(Some(&RateLimitConfig {
            requests_per_minute: 3000,
            tokens_per_minute: 4_000_000,
        }))
        .infer_stream(&request("muse-spark-1.3"))
        .await
        .expect("a stream");
    let raw = seen.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(raw.contains("authorization: bearer meta-key"), "{raw}");
    assert!(raw.contains("x-tenant: t1"), "{raw}");
}

#[tokio::test]
async fn the_listing_names_what_the_key_reaches_and_the_tables_size_it() {
    let listing = serde_json::json!({ "data": [
        { "id": "muse-spark-1.3", "created": 1786147200 },
        { "id": "muse-spark-1.3-contributor", "created": 1786147200 },
        { "id": "muse-image-1.0", "created": 0 },
        { "created": 1 }
    ]});
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", listing.to_string().into_bytes())]).await;
    let provider = provider(&url);
    assert_eq!(
        provider.serves_model("muse-spark-1.1"),
        Some("muse-spark-1.1".into())
    );
    let models = provider.check_credential().await.expect("listed");
    assert_eq!(models.len(), 3);
    assert_eq!(provider.serves_model("muse-spark-1.1"), None);
    assert_eq!(
        provider.serves_model("muse-spark-1.3"),
        Some("muse-spark-1.3".into())
    );
    assert_eq!(provider.max_context_tokens("muse-spark-1.3"), 1_048_576);
    assert_eq!(provider.served_catalog().map(|c| c.len()), Some(3));
    assert!(provider.learned_models().is_some());
    // Listed again from what was learned, with no second request.
    assert_eq!(provider.list_models().await.unwrap().len(), 3);
    let released = models
        .iter()
        .find(|m| m.id == "muse-spark-1.3")
        .unwrap()
        .released;
    assert_eq!(released, Some(1_786_147_200));
    let image = models.iter().find(|m| m.id == "muse-image-1.0").unwrap();
    assert_eq!(
        image.released, None,
        "the live listing's created 0 is no date"
    );
}

#[tokio::test]
async fn a_listing_that_fails_fails_the_check() {
    let url = spawn_mock_server(401, "Unauthorized", b"no".to_vec()).await;
    assert!(provider(&url).list_models().await.is_err());
}

#[test]
fn media_models_take_what_their_endpoints_take_and_chat_carries_audio_and_video() {
    let provider = provider("http://127.0.0.1:1");
    assert_eq!(provider.name(), "meta");
    let spark = provider.mime("muse-spark-1.3");
    assert!(spark.input.iter().any(|p| p == "video/*"), "{spark:?}");
    assert!(spark.input.iter().any(|p| p == "audio/*"), "{spark:?}");
    assert_eq!(provider.mime("muse-image-1.0").output, ["image/*"]);
    assert_eq!(
        provider.mime("muse-voice-transcribe-1.0").input,
        ["audio/wav"]
    );
    assert!(!provider.capabilities("muse-image-1.0").supports_tools);
    assert_eq!(table_capabilities("llama-9"), FALLBACK_CAPABILITIES);
}

#[tokio::test]
async fn an_override_can_price_turn_off_temperature_and_retype_a_model() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "muse-spark-1.3".to_string(),
        ModelCapabilityOverride {
            supports_temperature: Some(false),
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(2.0),
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    );
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("ok"))]).await;
    let provider = provider(&url).with_overrides(overrides);
    assert_eq!(
        provider.pricing("muse-spark-1.3").unwrap().output_per_mtok,
        2.0
    );
    assert_eq!(provider.mime("muse-spark-1.3").input, ["text/*"]);
    provider.infer(&request("muse-spark-1.3")).await.unwrap();
    assert!(!bodies.lock().unwrap()[0].contains("temperature"));
    // With no override, the shipped table prices it (or nothing does).
    let _ = provider.pricing("muse-spark-1.2");
}

#[tokio::test]
async fn muse_spark_takes_media_by_file_and_the_media_models_take_none() {
    let (url, seen) = spawn_mock_recorder(200, "OK", br#"{"id":"file-m"}"#.to_vec()).await;
    let meta = provider(&url);
    let mp4 = leviath_core::mime::MimeType::parse("video/mp4").unwrap();
    assert!(meta.media_limits("muse-spark-1.3").by_file(&mp4, 1));
    assert!(!meta.media_limits("muse-image-1.0").by_file(&mp4, 1));
    let upload = crate::files::FileUpload {
        bytes: std::sync::Arc::from(&b"....ftyp"[..]),
        mime_type: "video/mp4".into(),
        name: "a.mp4".into(),
        ttl_secs: 3_600,
    };
    assert_eq!(meta.upload_file(&upload).await.unwrap().id, "file-m");
    let (gone, deleted) = spawn_mock_recorder(404, "Not Found", b"{}".to_vec()).await;
    provider(&gone)
        .delete_file(&crate::files::RemoteFile {
            id: "file-m".into(),
            uri: None,
            expires_at: None,
        })
        .await
        .unwrap();
    let raw = seen.lock().unwrap().join("");
    assert!(raw.contains("name=\"purpose\"\r\n\r\nuser_data"), "{raw}");
    let deleted = deleted.lock().unwrap().join("");
    assert!(deleted.contains("DELETE /files/file-m"), "{deleted}");
}

#[tokio::test]
async fn a_media_model_runs_through_both_entry_points_and_the_listing_skips_a_nameless_entry() {
    let reply = serde_json::json!({ "data": [ { "b64_json": "SlBFRw==" } ] })
        .to_string()
        .into_bytes();
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", reply.clone()), (200, "OK", reply)]).await;
    let meta = provider(&url);
    let buffered = meta.infer(&request("muse-image-1.0")).await.unwrap();
    assert_eq!(buffered.parts.len(), 1);
    assert_eq!(
        buffered.tokens_used.reported_cost_usd,
        Some(0.01),
        "priced per image"
    );
    let mut stream = meta.infer_stream(&request("muse-image-1.0")).await.unwrap();
    use tokio_stream::StreamExt;
    assert_eq!(stream.next().await.unwrap().unwrap().parts.len(), 1);
    assert_eq!(meta.count_tokens("12345678", "muse-spark-1.3").await, 2);

    let listing = br#"{"data":[{"object":"model"},{"id":5},{"id":"muse-spark-1.3","created":1}]}"#;
    let listed = provider(&spawn_mock_server(200, "OK", listing.to_vec()).await);
    let models = listed.list_models().await.unwrap();
    assert_eq!(models.len(), 1);
}
