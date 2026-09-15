//! Tests for the provider itself, over local mock servers.

use super::*;
use crate::provider::{FinishReason, Message};
use leviath_testkit::{
    spawn_mock_recorder, spawn_mock_sequence, spawn_mock_server, spawn_mock_server_with_headers,
};
use serde_json::json;

fn client() -> reqwest::Client {
    crate::provider::build_http_client(None).expect("a test client builds")
}

/// A provider whose every host is `url`.
fn provider_at(url: &str) -> BedrockProvider {
    BedrockProvider::new(client(), "ABSK-test".to_string())
        .with_base_url(Some(url.to_string()))
        .with_control_url(Some(url.to_string()))
        .with_mantle_url(Some(url.to_string()))
        .with_pricing_url(Some(format!("{url}/prices")))
}

fn request(model: &str) -> InferenceRequest {
    InferenceRequest {
        system: Vec::new(),
        messages: vec![Message {
            role: "user".to_string(),
            content: "hi".to_string().into(),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.to_string(),
        max_tokens: 16,
        temperature: 0.0,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

fn converse_reply() -> Vec<u8> {
    json!({
        "output": { "message": { "role": "assistant", "content": [{ "text": "hello" }] } },
        "stopReason": "end_turn",
        "usage": { "inputTokens": 3, "outputTokens": 2, "totalTokens": 5 }
    })
    .to_string()
    .into_bytes()
}

/// The account's mode is read off the control plane and remembered, so the
/// live answer for a model reflects it: `none` is zero for an ordinary
/// model and still 30 days for a covered Claude; `default` is unknown;
/// `aws_review` keeps nothing for a model that allows `none`. A write goes
/// out as `PUT` with the mode and is remembered too; a word Bedrock does
/// not take is refused before any request.
#[tokio::test]
async fn the_account_retention_mode_is_read_written_and_remembered() {
    let p = BedrockProvider::new(client(), "k".to_string());
    assert!(
        p.live_retention("amazon.nova-2").is_none(),
        "nothing read yet"
    );
    assert!(p.retention_mode().is_none());

    let url = spawn_mock_server(
        200,
        "OK",
        br#"{"mode":"none","updated_at":"2026-06-07T20:19:44.723Z"}"#.to_vec(),
    )
    .await;
    let p = provider_at(&url);
    let read = p.account_retention().await.unwrap().unwrap();
    assert_eq!(read.mode, "none");
    assert_eq!(p.retention_mode().as_deref(), Some("none"));
    let plain = p.live_retention("amazon.nova-2").unwrap();
    assert!(plain.is_zero(), "{plain:?}");
    assert_eq!(plain.source, crate::retention::Source::Live);
    let covered = p.live_retention("us.anthropic.claude-fable-5-1").unwrap();
    assert_eq!(covered.retention, crate::retention::Retention::Days(30));
    assert!(covered.note.contains("aws_review"), "{}", covered.note);

    let url = spawn_mock_server(
        200,
        "OK",
        br#"{"mode":"default","updated_at":1733529600}"#.to_vec(),
    )
    .await;
    let p = provider_at(&url);
    p.account_retention().await.unwrap();
    assert_eq!(
        p.live_retention("amazon.nova-2").unwrap().retention,
        crate::retention::Retention::Unknown
    );

    let url = spawn_mock_server(200, "OK", br#"{"mode":"aws_review"}"#.to_vec()).await;
    let p = provider_at(&url);
    let written = p.set_account_retention("aws_review").await.unwrap();
    assert_eq!(written.mode, "aws_review");
    assert!(written.updated_at.is_none());
    assert!(p.live_retention("amazon.nova-2").unwrap().is_zero());
    assert_eq!(
        p.live_retention("anthropic.claude-mythos-5")
            .unwrap()
            .retention,
        crate::retention::Retention::Days(30)
    );

    let err = p
        .set_account_retention("sometimes")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a Bedrock data retention mode"), "{err}");

    // A gateway fronting the control plane: nothing to read, and a write is refused.
    let gated = BedrockProvider::new(client(), "k".to_string())
        .with_base_url(Some("http://gw.local".to_string()));
    assert!(gated.account_retention().await.unwrap().is_none());
    let err = gated
        .set_account_retention("none")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("gateway"), "{err}");

    // An answer that is not a retention setting is an API error, not a panic.
    let url = spawn_mock_server(200, "OK", br#"{"updated_at":1}"#.to_vec()).await;
    let p = provider_at(&url);
    let err = p.account_retention().await.unwrap_err().to_string();
    assert!(err.contains("not one"), "{err}");
    // The mock answers one request, so each call gets its own.
    let url = spawn_mock_server(200, "OK", br#"{"updated_at":1}"#.to_vec()).await;
    let err = provider_at(&url)
        .set_account_retention("none")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not one"), "{err}");
    // A body that is not JSON at all, and a plane nobody answers on.
    let url = spawn_mock_server(200, "OK", b"<html>".to_vec()).await;
    assert!(
        provider_at(&url)
            .set_account_retention("none")
            .await
            .is_err()
    );
    let dead = BedrockProvider::new(client(), "k".to_string())
        .with_control_url(Some("http://127.0.0.1:1".to_string()));
    let err = dead.set_account_retention("none").await.unwrap_err();
    assert!(err.is_transient(), "{err}");
    // A body that is not JSON at all, and a plane nobody answers on.
    let url = spawn_mock_server(200, "OK", b"<html>".to_vec()).await;
    assert!(
        provider_at(&url)
            .set_account_retention("none")
            .await
            .is_err()
    );
    let dead = BedrockProvider::new(client(), "k".to_string())
        .with_control_url(Some("http://127.0.0.1:1".to_string()));
    let err = dead.set_account_retention("none").await.unwrap_err();
    assert!(err.is_transient(), "{err}");
    // A body that is not JSON at all, and a plane nobody answers on.
    let url = spawn_mock_server(200, "OK", b"<html>".to_vec()).await;
    assert!(
        provider_at(&url)
            .set_account_retention("none")
            .await
            .is_err()
    );
    let dead = BedrockProvider::new(client(), "k".to_string())
        .with_control_url(Some("http://127.0.0.1:1".to_string()));
    let err = dead.set_account_retention("none").await.unwrap_err();
    assert!(err.is_transient(), "{err}");
    // And a refusal from the plane is surfaced as such.
    let url = spawn_mock_server(403, "Forbidden", b"{}".to_vec()).await;
    assert!(provider_at(&url).account_retention().await.is_err());
    let url = spawn_mock_server(403, "Forbidden", b"{}".to_vec()).await;
    assert!(
        provider_at(&url)
            .set_account_retention("none")
            .await
            .is_err()
    );
}

#[test]
fn the_default_hosts_follow_the_region() {
    let p = BedrockProvider::new(client(), "k".to_string());
    assert_eq!(p.region(), "us-east-1");
    assert_eq!(
        p.runtime_base(),
        "https://bedrock-runtime.us-east-1.amazonaws.com"
    );
    assert_eq!(
        p.control_base().as_deref(),
        Some("https://bedrock.us-east-1.amazonaws.com")
    );
    assert_eq!(
        p.mantle_hosts(),
        Some(vec!["https://bedrock-mantle.us-east-1.api.aws".to_string()])
    );
    assert!(p.price_url().unwrap().contains("/us-east-1/"));
    // CountTokens takes the bare id, not the profile the inference uses.
    assert_eq!(
        p.count_url("us.anthropic.claude-sonnet-4-6"),
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4-6/count-tokens"
    );
    assert_eq!(
        p.count_url("global.amazon.nova-pro-v1:0"),
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/amazon.nova-pro-v1%3A0/count-tokens"
    );
    let p = p.with_region(Some(" eu-west-1 ".to_string()));
    assert_eq!(p.region(), "eu-west-1");
    assert_eq!(
        p.runtime_base(),
        "https://bedrock-runtime.eu-west-1.amazonaws.com"
    );
    assert_eq!(
        p.control_base().as_deref(),
        Some("https://bedrock.eu-west-1.amazonaws.com")
    );
    // The region's own host first, then the one that carries every model.
    assert_eq!(
        p.mantle_hosts(),
        Some(vec![
            "https://bedrock-mantle.eu-west-1.api.aws".to_string(),
            "https://bedrock-mantle.us-east-1.api.aws".to_string(),
        ])
    );
    let p = p.with_region(Some("  ".to_string())).with_region(None);
    assert_eq!(p.region(), "eu-west-1");
}

#[test]
fn a_gateway_replaces_the_runtime_and_silences_the_side_calls() {
    let p = BedrockProvider::new(client(), "k".to_string())
        .with_base_url(Some("http://gateway.local/bedrock/".to_string()));
    assert_eq!(p.runtime_base(), "http://gateway.local/bedrock");
    assert_eq!(p.control_base(), None);
    assert_eq!(p.mantle_hosts(), None);
    assert_eq!(p.price_url(), None);
    let p = p
        .with_control_url(Some("http://control.local/".to_string()))
        .with_mantle_url(Some("http://mantle.local/".to_string()))
        .with_pricing_url(Some("http://prices.local/index.json".to_string()));
    assert_eq!(p.control_base().as_deref(), Some("http://control.local"));
    assert_eq!(
        p.mantle_hosts(),
        Some(vec!["http://mantle.local".to_string()])
    );
    assert_eq!(
        p.price_url().as_deref(),
        Some("http://prices.local/index.json")
    );
    let p = p
        .with_base_url(None)
        .with_control_url(None)
        .with_mantle_url(None)
        .with_pricing_url(None);
    assert_eq!(p.runtime_base(), "http://gateway.local/bedrock");
    assert_eq!(p.control_base().as_deref(), Some("http://control.local"));
}

#[test]
fn with_overrides_wires_the_limiter_and_the_corrections() {
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 5,
        tokens_per_minute: 1_000,
    };
    let mut overrides = HashMap::new();
    overrides.insert(
        "mine.model".to_string(),
        ModelCapabilityOverride {
            max_context_tokens: Some(77),
            input_per_mtok: Some(1.0),
            output_per_mtok: Some(2.0),
            input_types: Some(vec!["text/*".to_string(), "image/*".to_string()]),
            ..Default::default()
        },
    );
    let p = BedrockProvider::with_overrides(client(), "k".to_string(), overrides, Some(&cfg));
    assert!(p.rate_limiter.is_some());
    assert!(p.learned_models().is_some());
    assert_eq!(p.max_context_tokens("mine.model"), 77);
    assert_eq!(p.pricing("mine.model").unwrap().output_per_mtok, 2.0);
    assert!(p.mime("mine.model").takes_mime());
    assert_eq!(p.serves_model("mine.model").as_deref(), Some("mine.model"));
    assert_eq!(p.name(), "bedrock");
    let unlimited =
        BedrockProvider::with_overrides(client(), "k".to_string(), HashMap::new(), None);
    assert!(unlimited.rate_limiter.is_none());
}

#[test]
fn model_ids_are_percent_encoded_in_the_path() {
    assert_eq!(
        encode_model_id("anthropic.claude-3-5-haiku-20241022-v1:0"),
        "anthropic.claude-3-5-haiku-20241022-v1%3A0"
    );
    assert_eq!(
        encode_model_id("arn:aws:bedrock:us-east-1:123:inference-profile/x"),
        "arn%3Aaws%3Abedrock%3Aus-east-1%3A123%3Ainference-profile%2Fx"
    );
    assert_eq!(
        encode_model_id("us.amazon.nova-pro_v1~"),
        "us.amazon.nova-pro_v1~"
    );
    let p = BedrockProvider::new(client(), "k".to_string());
    assert_eq!(
        p.converse_url("us.amazon.nova-pro-v1:0", true),
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.amazon.nova-pro-v1%3A0/converse-stream"
    );
    assert!(p.converse_url("m", false).ends_with("/model/m/converse"));
}

#[test]
fn the_headers_carry_the_bearer_token() {
    let p = BedrockProvider::new(client(), "ABSK-secret".to_string());
    let headers = p.header_pairs();
    assert!(headers.contains(&("authorization", "Bearer ABSK-secret".to_string())));
    assert!(headers.iter().any(|(n, _)| *n == "content-type"));
}

#[test]
fn routing_claims_bedrock_shaped_ids_and_never_a_bare_vendor_name() {
    let p = BedrockProvider::new(client(), "k".to_string());
    assert!(p.serves_model("us.anthropic.claude-sonnet-5").is_some());
    assert!(p.serves_model("openai.gpt-oss-120b-1:0").is_some());
    assert_eq!(p.serves_model("claude-sonnet-5"), None);
    assert_eq!(p.serves_model("gpt-5.5"), None);
    assert_eq!(p.served_catalog(), None);
    let mut learned = HashMap::new();
    learned.insert(
        "odd.listed-model".to_string(),
        crate::learned::LearnedModel::default(),
    );
    p.learned.replace(learned);
    assert!(p.serves_model("odd.listed-model").is_some());
    assert_eq!(
        p.served_catalog().unwrap(),
        vec!["odd.listed-model".to_string()]
    );
}

#[test]
fn claude_prices_from_the_anthropic_rows_and_the_rest_from_the_listing() {
    let p = BedrockProvider::new(client(), "k".to_string());
    let claude = p
        .pricing("us.anthropic.claude-sonnet-5")
        .expect("Anthropic rows price it");
    assert_eq!(
        Some(claude),
        crate::pricing::published_rates("anthropic", "claude-sonnet-5")
    );
    assert_eq!(p.pricing("us.amazon.nova-pro-v1:0"), None);
    let mut learned = HashMap::new();
    learned.insert(
        "us.amazon.nova-pro-v1:0".to_string(),
        crate::learned::LearnedModel {
            pricing: Some(crate::ModelPricing::flat(0.8, 3.2)),
            ..Default::default()
        },
    );
    p.learned.replace(learned);
    assert_eq!(
        p.pricing("us.amazon.nova-pro-v1:0").unwrap().input_per_mtok,
        0.8
    );
    assert_eq!(p.pricing("nobody.model"), None);
}

#[test]
fn capabilities_are_corrected_by_the_listing_and_then_the_operator() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "us.anthropic.claude-sonnet-5".to_string(),
        ModelCapabilityOverride {
            max_output_tokens: Some(9),
            ..Default::default()
        },
    );
    let p = BedrockProvider::with_overrides(client(), "k".to_string(), overrides, None);
    let caps = p.capabilities("us.anthropic.claude-sonnet-5");
    assert_eq!(caps.max_context_tokens, 1_000_000);
    assert_eq!(caps.max_output_tokens, 9);
    assert_eq!(caps.limits_source, crate::LimitsSource::Override);
    let mut learned = HashMap::new();
    learned.insert(
        "us.amazon.nova-pro-v1:0".to_string(),
        crate::learned::LearnedModel {
            input_types: Some(vec!["text/*".to_string()]),
            supports_tools: Some(false),
            ..Default::default()
        },
    );
    p.learned.replace(learned);
    assert!(!p.capabilities("us.amazon.nova-pro-v1:0").supports_tools);
    assert!(!p.mime("us.amazon.nova-pro-v1:0").takes_mime());
}

#[tokio::test]
async fn infer_reads_a_converse_reply() {
    let _guard = crate::test_support::always_on_tracing_guard();
    let url = spawn_mock_server(200, "OK", converse_reply()).await;
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 100,
        tokens_per_minute: 100_000,
    };
    let p = BedrockProvider::with_overrides(client(), "k".to_string(), HashMap::new(), Some(&cfg))
        .with_base_url(Some(url));
    let response = p.infer(&request("us.amazon.nova-pro-v1:0")).await.unwrap();
    assert_eq!(response.content, "hello");
    assert_eq!(response.finish_reason, FinishReason::Complete);
    assert_eq!(response.tokens_used.total_tokens, 5);
}

#[tokio::test]
async fn a_throttled_reply_carries_the_retry_after() {
    let url = spawn_mock_server_with_headers(
        429,
        "Too Many Requests",
        "Content-Type: application/json\r\nRetry-After: 7\r\n",
        br#"{"message":"slow"}"#.to_vec(),
    )
    .await;
    let p = provider_at(&url);
    let err = p.infer(&request("m.x")).await.unwrap_err();
    assert!(matches!(
        err,
        ProviderError::RateLimitExceeded {
            retry_after_secs: Some(7)
        }
    ));
    let url = spawn_mock_server(429, "Too Many Requests", b"{}".to_vec()).await;
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 100,
        tokens_per_minute: 100_000,
    };
    let limited =
        BedrockProvider::with_overrides(client(), "k".to_string(), HashMap::new(), Some(&cfg))
            .with_base_url(Some(url));
    let err = limited.infer(&request("m.x")).await.unwrap_err();
    assert!(matches!(
        err,
        ProviderError::RateLimitExceeded {
            retry_after_secs: None
        }
    ));
}

#[tokio::test]
async fn an_error_body_that_never_finishes_is_still_reported() {
    let url = leviath_testkit::spawn_mock_server_truncated_body(400, "Bad Request").await;
    let err = provider_at(&url).infer(&request("m.x")).await.unwrap_err();
    assert_eq!(err.failure_kind(), Some(FailureKind::BadRequest));
}

#[tokio::test]
async fn a_rejected_key_is_told_apart_from_a_denied_model() {
    let bad_key = spawn_mock_server_with_headers(
        403,
        "Forbidden",
        "Content-Type: application/json\r\nx-amzn-ErrorType: UnrecognizedClientException:http://internal.amazon.com/coral/\r\n",
        br#"{"message":"The security token included in the request is invalid."}"#.to_vec(),
    )
    .await;
    let err = provider_at(&bad_key)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::AuthFailed)
    );
    assert!(
        err.to_string()
            .contains("UnrecognizedClientException: The security token"),
        "{err}"
    );

    let denied = spawn_mock_server_with_headers(
        403,
        "Forbidden",
        "Content-Type: application/json\r\nx-amzn-ErrorType: AccessDeniedException\r\n",
        br#"{"message":"no model access"}"#.to_vec(),
    )
    .await;
    let err = provider_at(&denied)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(err.unavailable_reason(), Some(UnavailableReason::Forbidden));

    let untyped = spawn_mock_server(403, "Forbidden", b"nope".to_vec()).await;
    let err = provider_at(&untyped)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(err.unavailable_reason(), Some(UnavailableReason::Forbidden));
    assert!(err.to_string().contains("nope"), "{err}");

    let unauthorised = spawn_mock_server(401, "Unauthorized", b"{}".to_vec()).await;
    let err = provider_at(&unauthorised)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::AuthFailed)
    );
}

#[tokio::test]
async fn other_failures_are_labelled_with_their_kind_and_message() {
    let invalid = spawn_mock_server_with_headers(
        400,
        "Bad Request",
        "Content-Type: application/json\r\nx-amzn-ErrorType: ValidationException\r\n",
        br#"{"message":"messages.0: blank"}"#.to_vec(),
    )
    .await;
    let err = provider_at(&invalid)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(err.failure_kind(), Some(FailureKind::BadRequest));
    assert!(
        err.to_string()
            .contains("ValidationException: messages.0: blank"),
        "{err}"
    );
    assert!(!err.is_transient());

    let missing =
        spawn_mock_server(404, "Not Found", br#"{"Message":"no such model"}"#.to_vec()).await;
    let err = provider_at(&missing)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(err.failure_kind(), Some(FailureKind::NotFound));
    assert!(err.to_string().contains("no such model"), "{err}");

    let broken =
        spawn_mock_server(500, "Internal Server Error", b"<html>oops</html>".to_vec()).await;
    let err = provider_at(&broken)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(err.failure_kind(), Some(FailureKind::ServerError));
    assert!(err.to_string().contains("<html>oops</html>"), "{err}");
    assert!(err.is_transient());

    let anthropic_shaped = spawn_mock_server(
        400,
        "Bad Request",
        br#"{"type":"error","error":{"type":"invalid_request_error","message":"anthropic says no"}}"#.to_vec(),
    )
    .await;
    let err = provider_at(&anthropic_shaped)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("anthropic says no"), "{err}");

    let drained = spawn_mock_server(402, "Payment Required", b"{}".to_vec()).await;
    let err = provider_at(&drained)
        .infer(&request("m.x"))
        .await
        .unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::CreditsExhausted)
    );
}

#[tokio::test]
async fn a_body_that_is_not_a_converse_reply_is_invalid() {
    let url = spawn_mock_server(200, "OK", b"{not json".to_vec()).await;
    let err = provider_at(&url).infer(&request("m.x")).await.unwrap_err();
    assert!(matches!(err, ProviderError::InvalidResponse(_)), "{err}");
    let url = spawn_mock_server(200, "OK", br#"{"output":{}}"#.to_vec()).await;
    let err = provider_at(&url).infer(&request("m.x")).await.unwrap_err();
    assert!(matches!(err, ProviderError::InvalidResponse(_)), "{err}");
}

#[tokio::test]
async fn a_host_that_refuses_the_connection_is_unreachable() {
    let p = provider_at("http://127.0.0.1:1");
    let err = p.infer(&request("m.x")).await.unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::Unreachable)
    );
    let err = match p.infer_stream(&request("m.x")).await {
        Ok(_) => panic!("nothing listens on port 1"),
        Err(e) => e,
    };
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::Unreachable)
    );
    assert_eq!(
        p.count_tokens("four words of text", "us.meta.llama-future-v1:0")
            .await,
        5
    );
}

#[tokio::test]
async fn infer_stream_reads_the_event_stream() {
    let mut body = super::eventstream::fixtures::event(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"streamed"}}"#,
    );
    body.extend(super::eventstream::fixtures::event(
        "messageStop",
        r#"{"stopReason":"end_turn"}"#,
    ));
    body.extend(super::eventstream::fixtures::event(
        "metadata",
        r#"{"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2}}"#,
    ));
    let url = spawn_mock_server_with_headers(
        200,
        "OK",
        "Content-Type: application/vnd.amazon.eventstream\r\n",
        body,
    )
    .await;
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 100,
        tokens_per_minute: 100_000,
    };
    let p = BedrockProvider::with_overrides(client(), "k".to_string(), HashMap::new(), Some(&cfg))
        .with_base_url(Some(url));
    let stream = p
        .infer_stream(&request("us.amazon.nova-pro-v1:0"))
        .await
        .unwrap();
    let response = crate::provider::collect_stream(stream).await.unwrap();
    assert_eq!(response.content, "streamed");
    assert_eq!(response.tokens_used.total_tokens, 2);
    let url = spawn_mock_server(400, "Bad Request", br#"{"message":"x"}"#.to_vec()).await;
    let err = match provider_at(&url).infer_stream(&request("m.x")).await {
        Ok(_) => panic!("a 400 is not a stream"),
        Err(e) => e,
    };
    assert_eq!(err.failure_kind(), Some(FailureKind::BadRequest));
}

fn listing_page() -> Vec<u8> {
    json!({ "modelSummaries": [
        {
            "modelId": "amazon.nova-pro-v1:0", "modelName": "Nova Pro",
            "inputModalities": ["TEXT", "IMAGE"], "outputModalities": ["TEXT"],
            "inferenceTypesSupported": ["ON_DEMAND", "INFERENCE_PROFILE"],
            "modelLifecycle": { "status": "ACTIVE" }
        },
        {
            "modelId": "anthropic.claude-sonnet-5", "modelName": "Claude Sonnet 5",
            "inputModalities": ["TEXT", "IMAGE"], "outputModalities": ["TEXT"],
            "inferenceTypesSupported": ["INFERENCE_PROFILE"]
        }
    ] })
    .to_string()
    .into_bytes()
}

fn profiles_page(next: Option<&str>) -> Vec<u8> {
    let mut page = json!({ "inferenceProfileSummaries": [{
        "inferenceProfileId": match next { Some(_) => "us.anthropic.claude-sonnet-5", None => "eu.amazon.nova-pro-v1:0" },
        "inferenceProfileName": "profile",
        "status": "ACTIVE",
        "models": [{ "modelArn": match next {
            Some(_) => "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-sonnet-5",
            None => "arn:aws:bedrock:eu-west-1::foundation-model/amazon.nova-pro-v1:0",
        } }]
    }] });
    if let Some(token) = next {
        page["nextToken"] = json!(token);
    }
    page.to_string().into_bytes()
}

fn price_page() -> Vec<u8> {
    json!({
        "products": {
            "A": { "attributes": { "model": "Nova Pro", "inferenceType": "Input tokens", "usagetype": "USE1-NovaPro-input-tokens", "feature": "On-demand Inference" } },
            "B": { "attributes": { "model": "Nova Pro", "inferenceType": "Output tokens", "usagetype": "USE1-NovaPro-output-tokens", "feature": "On-demand Inference" } }
        },
        "terms": { "OnDemand": {
            "A": { "t": { "priceDimensions": { "d": { "unit": "1K tokens", "pricePerUnit": { "USD": "0.0008" } } } } },
            "B": { "t": { "priceDimensions": { "d": { "unit": "1K tokens", "pricePerUnit": { "USD": "0.0032" } } } } }
        } }
    })
    .to_string()
    .into_bytes()
}

/// The mantle listing, as measured on 2026-09-15: Sonnet 5 allows `none`,
/// OpenAI's models never do, Fable 5 needs `aws_review`, and one made-up
/// row allows `none` and `aws_review` but not `default`.
fn mantle_listing() -> Vec<u8> {
    json!({
        "object": "list",
        "data": [
            { "id": "anthropic.claude-sonnet-5", "status": "available",
              "data_retention": { "allowed_modes": ["none", "default", "aws_review"], "mode": "default", "source": "model_default" } },
            { "id": "openai.gpt-5.4", "status": "available",
              "data_retention": { "allowed_modes": ["default", "aws_review"], "mode": "default", "source": "model_default" } },
            { "id": "anthropic.claude-fable-5", "status": "unavailable",
              "status_reason": "This model is not available under data retention mode 'default'.",
              "data_retention": { "allowed_modes": ["aws_review"], "mode": "default", "source": "model_default" } },
            { "id": "vendor.odd", "status": "available",
              "data_retention": { "allowed_modes": ["none", "aws_review"], "mode": "none", "source": "model_default" } },
            { "id": "vendor.silent" },
            { "data_retention": { "allowed_modes": ["none"] } },
            { "id": 7, "data_retention": { "allowed_modes": ["none"] } },
            { "id": "vendor.garbled", "data_retention": "none" }
        ]
    })
    .to_string()
    .into_bytes()
}

/// A refresh re-reads the account's mode every time and the listing only
/// while none has been read; a host that cannot be reached is logged and
/// leaves what was known.
#[tokio::test]
async fn a_refresh_re_reads_the_mode_and_reads_the_listing_once() {
    let _guard = crate::test_support::always_on_tracing_guard();
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"mode":"none"}"#.to_vec()),
        (200, "OK", mantle_listing()),
        (200, "OK", br#"{"mode":"default"}"#.to_vec()),
    ])
    .await;
    let p = provider_at(&url);
    p.refresh_retention().await;
    assert_eq!(p.retention_mode().as_deref(), Some("none"));
    assert!(p.model_retention("openai.gpt-5.4").is_some());
    p.refresh_retention().await;
    assert_eq!(p.retention_mode().as_deref(), Some("default"));
    assert_eq!(
        bodies.lock().unwrap().len(),
        3,
        "the listing was not read again"
    );

    let dead = provider_at("http://127.0.0.1:1");
    dead.refresh_retention().await;
    assert!(dead.retention_mode().is_none());
    assert!(dead.model_retention("openai.gpt-5.4").is_none());
}

/// A provider that has read `mode` off the control plane and the listing
/// off the mantle host.
async fn provider_knowing(mode: &str) -> BedrockProvider {
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", format!(r#"{{"mode":"{mode}"}}"#).into_bytes()),
        (200, "OK", mantle_listing()),
    ])
    .await;
    let p = provider_at(&url);
    p.account_retention().await.unwrap();
    assert_eq!(
        p.read_model_retention().await.unwrap(),
        4,
        "the silent row is skipped"
    );
    p
}

/// What the listing says narrows the account's mode: a model never offered
/// under `none` cannot run with zero retention whatever the account says
/// (and is unavailable under `none`); a listed model is looked up by its
/// bare id, so a profile finds it; an account that inherits serves each
/// model under its own default; a model the account's mode does not serve
/// is unavailable; an unlisted model is left to the account's mode alone.
#[tokio::test]
async fn the_listing_says_which_models_can_run_under_mode_none() {
    use crate::retention::Retention;
    let p = provider_knowing("none").await;
    assert_eq!(
        p.model_retention("us.anthropic.claude-sonnet-5")
            .unwrap()
            .allowed_modes,
        vec!["none", "default", "aws_review"]
    );
    assert!(p.model_retention("amazon.nova-2").is_none());
    // The whole listing, sorted, with each row's availability in Bedrock's
    // words: what `lev providers retention` prints.
    let rows = p.model_retentions();
    let ids: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "anthropic.claude-fable-5",
            "anthropic.claude-sonnet-5",
            "openai.gpt-5.4",
            "vendor.odd"
        ]
    );
    assert!(rows[0].1.unavailable());
    assert!(
        rows[0]
            .1
            .status_reason
            .as_deref()
            .is_some_and(|r| r.contains("not available")),
        "{:?}",
        rows[0].1.status_reason
    );
    assert!(!rows[1].1.unavailable());
    assert!(
        p.live_retention("us.anthropic.claude-sonnet-5")
            .unwrap()
            .is_zero()
    );
    assert!(p.live_retention("amazon.nova-2").unwrap().is_zero());
    let gpt = p.live_retention("openai.gpt-5.4").unwrap();
    assert_eq!(gpt.retention, Retention::Unknown);
    assert!(gpt.note.contains("never none"), "{}", gpt.note);
    assert!(gpt.note.contains("unavailable"), "{}", gpt.note);
    let fable = p.live_retention("anthropic.claude-fable-5").unwrap();
    assert_eq!(fable.retention, Retention::Days(30));
    assert!(fable.note.contains("never none"), "{}", fable.note);

    let p = provider_knowing("inherit").await;
    let sonnet = p.live_retention("us.anthropic.claude-sonnet-5").unwrap();
    assert_eq!(sonnet.retention, Retention::Unknown);
    assert!(sonnet.note.contains("inherits"), "{}", sonnet.note);
    let gpt = p.live_retention("openai.gpt-5.4").unwrap();
    assert!(gpt.note.contains("never none"), "{}", gpt.note);
    assert!(!gpt.note.contains("unavailable"), "{}", gpt.note);
    assert!(
        p.live_retention("vendor.odd").unwrap().is_zero(),
        "its own default is none"
    );
    let nova = p.live_retention("amazon.nova-2").unwrap();
    assert_eq!(nova.retention, Retention::Unknown);
    assert!(nova.note.contains("inherits"), "{}", nova.note);

    let p = provider_knowing("default").await;
    let odd = p.live_retention("vendor.odd").unwrap();
    assert_eq!(odd.retention, Retention::Unknown);
    assert!(
        odd.note.contains("does not serve this model under"),
        "{}",
        odd.note
    );
    let fable = p.live_retention("anthropic.claude-fable-5").unwrap();
    assert_eq!(fable.retention, Retention::Days(30));

    let p = provider_knowing("aws_review").await;
    assert!(
        p.live_retention("us.anthropic.claude-sonnet-5")
            .unwrap()
            .is_zero()
    );
    let fable = p.live_retention("anthropic.claude-fable-5").unwrap();
    assert_eq!(fable.retention, Retention::Days(30));
    assert!(fable.note.contains("never none"), "{}", fable.note);
    assert!(!fable.note.contains("unavailable"), "{}", fable.note);
    // A covered Claude the listing does not carry keeps the documented answer.
    let mythos = p.live_retention("us.anthropic.claude-mythos-5").unwrap();
    assert_eq!(mythos.retention, Retention::Days(30));
    assert!(mythos.note.contains("needs aws_review"), "{}", mythos.note);

    // Behind a gateway the mantle host is not reached and nothing is read.
    let gated = BedrockProvider::new(client(), "k".to_string())
        .with_base_url(Some("http://gw.local".to_string()));
    assert_eq!(gated.read_model_retention().await.unwrap(), 0);
    // A page without the array is an invalid response, not a panic.
    let url = spawn_mock_server(200, "OK", br#"{"models":[]}"#.to_vec()).await;
    let err = provider_at(&url)
        .read_model_retention()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no data"), "{err}");
}

/// Priming reads the account's mode and the listing after the prices, both
/// best effort.
/// The operator's extra headers reach the wire on an inference, after the
/// provider's own.
#[tokio::test]
async fn extra_headers_ride_every_inference() {
    let (url, seen) = spawn_mock_recorder(200, "OK", converse_reply()).await;
    let p =
        provider_at(&url).with_headers(vec![("X-Gateway-Token".to_string(), "t-1".to_string())]);
    p.infer(&request("amazon.nova-2")).await.unwrap();
    let sent = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(sent.contains("x-gateway-token: t-1"), "{sent}");
    let own = sent.find("authorization").expect("the key is sent");
    let extra = sent.find("x-gateway-token").expect("the extra is sent");
    assert!(own < extra, "the provider's own header comes first: {sent}");
}

#[tokio::test]
async fn priming_reads_the_retention_mode_and_the_listing() {
    let _guard = crate::test_support::always_on_tracing_guard();
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", listing_page()),
        (200, "OK", profiles_page(None)),
        (200, "OK", price_page()),
        (200, "OK", br#"{"mode":"none"}"#.to_vec()),
        (200, "OK", mantle_listing()),
    ])
    .await;
    let p = provider_at(&url);
    p.prime_capabilities().await.unwrap();
    assert_eq!(p.retention_mode().as_deref(), Some("none"));
    assert!(p.model_retention("anthropic.claude-sonnet-5").is_some());
}

#[tokio::test]
async fn priming_reads_the_listing_every_profile_page_and_the_prices() {
    let _guard = crate::test_support::always_on_tracing_guard();
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", listing_page()),
        (200, "OK", profiles_page(Some("page2"))),
        (200, "OK", profiles_page(None)),
        (200, "OK", price_page()),
    ])
    .await;
    let p = provider_at(&url);
    let models = p.list_models().await.unwrap();
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "amazon.nova-pro-v1:0",
            "eu.amazon.nova-pro-v1:0",
            "us.anthropic.claude-sonnet-5"
        ]
    );
    assert!(models.iter().all(|m| m.learned && m.provider == "bedrock"));
    let nova = &models[1];
    assert_eq!(nova.display_name.as_deref(), Some("Nova Pro"));
    assert_eq!(nova.pricing.unwrap().input_per_mtok, 0.8);
    assert!(nova.mime.takes_mime());
    assert_eq!(nova.capabilities.max_context_tokens, 300_000);
    assert_eq!(
        nova.capabilities.limits_source,
        crate::LimitsSource::Builtin
    );
    let sonnet = &models[2];
    assert_eq!(sonnet.capabilities.max_context_tokens, 1_000_000);
    assert_eq!(
        sonnet.pricing,
        crate::pricing::published_rates("anthropic", "claude-sonnet-5"),
        "a Claude is priced from Anthropic's rows on the listing too"
    );
    // Four requests went out, GETs with the key on the three AWS calls.
    assert_eq!(bodies.lock().unwrap().len(), 4);
    // Listed once, answered from memory after.
    assert_eq!(p.list_models().await.unwrap().len(), 3);
    assert_eq!(p.served_catalog().unwrap().len(), 3);
}

#[tokio::test]
async fn a_price_file_that_cannot_be_read_leaves_the_rates_unset() {
    let _guard = crate::test_support::always_on_tracing_guard();
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", listing_page()),
        (200, "OK", profiles_page(None)),
        (500, "Internal Server Error", b"{}".to_vec()),
    ])
    .await;
    let p = provider_at(&url);
    p.prime_capabilities().await.unwrap();
    assert_eq!(p.pricing("amazon.nova-pro-v1:0"), None);
    // No price URL at all reads nothing either.
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", listing_page()),
        (200, "OK", profiles_page(None)),
    ])
    .await;
    let p = BedrockProvider::new(client(), "k".to_string())
        .with_base_url(Some(url.clone()))
        .with_control_url(Some(url));
    p.prime_capabilities().await.unwrap();
    assert_eq!(p.learned.ids().len(), 2);
}

#[tokio::test]
async fn a_profile_page_that_fails_fails_the_prime() {
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", listing_page()),
        (200, "OK", profiles_page(Some("page2"))),
        (500, "Internal Server Error", b"{}".to_vec()),
    ])
    .await;
    let err = provider_at(&url).prime_capabilities().await.unwrap_err();
    assert_eq!(err.failure_kind(), Some(FailureKind::ServerError));
}

#[tokio::test]
async fn a_listing_that_fails_or_misparses_is_reported() {
    let url = spawn_mock_server(401, "Unauthorized", b"{}".to_vec()).await;
    let err = provider_at(&url).list_models().await.unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::AuthFailed)
    );
    let url = spawn_mock_server(200, "OK", br#"{"models":[]}"#.to_vec()).await;
    let err = provider_at(&url).list_models().await.unwrap_err();
    assert!(matches!(err, ProviderError::InvalidResponse(_)), "{err}");
    let err = provider_at("http://127.0.0.1:1")
        .check_credential()
        .await
        .unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::Unreachable)
    );
}

#[tokio::test]
async fn check_credential_reads_the_listing_again() {
    // Priming reads the listing, the profiles, the prices, then the
    // account's data retention mode and what each model allows.
    let retention = || br#"{"mode":"none"}"#.to_vec();
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", listing_page()),
        (200, "OK", profiles_page(None)),
        (200, "OK", price_page()),
        (200, "OK", retention()),
        (200, "OK", mantle_listing()),
        (200, "OK", listing_page()),
        (200, "OK", profiles_page(None)),
        (200, "OK", price_page()),
        (200, "OK", retention()),
        (200, "OK", mantle_listing()),
    ])
    .await;
    let p = provider_at(&url);
    p.list_models().await.unwrap();
    assert_eq!(bodies.lock().unwrap().len(), 5);
    assert_eq!(p.retention_mode().as_deref(), Some("none"));
    assert!(p.model_retention("openai.gpt-5.4").is_some());
    let models = p.check_credential().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(bodies.lock().unwrap().len(), 10);
}

#[tokio::test]
async fn behind_a_gateway_the_listing_is_the_compiled_table() {
    let _guard = crate::test_support::always_on_tracing_guard();
    let p = BedrockProvider::new(client(), "k".to_string())
        .with_base_url(Some("http://127.0.0.1:1".to_string()));
    let models = p.list_models().await.unwrap();
    assert!(!models.is_empty());
    assert!(models.iter().all(|m| !m.learned));
    let sonnet = models
        .iter()
        .find(|m| m.id == "us.anthropic.claude-sonnet-5")
        .expect("the card table names Sonnet 5");
    assert_eq!(sonnet.display_name.as_deref(), Some("Claude Sonnet 5"));
    assert_eq!(sonnet.capabilities.max_output_tokens, 128_000);
    assert!(sonnet.mime.takes_mime());
    assert!(p.check_credential().await.is_ok());
}

#[tokio::test]
async fn counting_uses_bedrock_first_and_remembers_a_refusal() {
    let _guard = crate::test_support::always_on_tracing_guard();
    // A model the table has not seen is tried on the runtime route.
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", br#"{"inputTokens": 42}"#.to_vec())]).await;
    let p = provider_at(&url);
    assert_eq!(
        p.count_tokens("some text", "us.meta.llama-future-v1:0")
            .await,
        42
    );
    let sent = bodies.lock().unwrap()[0].clone();
    assert!(sent.contains("\"converse\""), "{sent}");
    assert!(sent.contains("some text"), "{sent}");

    // One the card lists as unsupported is not tried there at all: a Nova
    // has no other route, and a Claude goes straight to Anthropic's.
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", br#"{"input_tokens": 9}"#.to_vec())]).await;
    let p = provider_at(&url);
    assert_eq!(
        p.count_tokens("some text", "us.amazon.nova-pro-v1:0").await,
        3
    );
    assert_eq!(
        p.count_tokens("some text", "us.anthropic.claude-sonnet-5")
            .await,
        9
    );
    {
        let sent = bodies.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].contains("\"anthropic.claude-sonnet-5\""),
            "{}",
            sent[0]
        );
    }

    // A Claude the runtime route refuses: the Anthropic route answers, and
    // the refusal is remembered so the next count skips the runtime.
    let (url, bodies) = spawn_mock_sequence(vec![
        (
            400,
            "Bad Request",
            br#"{"message":"model not supported"}"#.to_vec(),
        ),
        (200, "OK", br#"{"input_tokens": 17}"#.to_vec()),
        (200, "OK", br#"{"input_tokens": 18}"#.to_vec()),
    ])
    .await;
    let cfg = crate::provider::RateLimitConfig {
        requests_per_minute: 100,
        tokens_per_minute: 100_000,
    };
    let p = BedrockProvider::with_overrides(client(), "k".to_string(), HashMap::new(), Some(&cfg))
        .with_base_url(Some(url.clone()))
        .with_mantle_url(Some(url));
    assert_eq!(
        p.count_tokens("t", "us.anthropic.claude-sonnet-4-6").await,
        17
    );
    assert!(p.count_route.contains("us.anthropic.claude-sonnet-4-6"));
    assert_eq!(
        p.count_tokens("t", "us.anthropic.claude-sonnet-4-6").await,
        18
    );
    let sent = bodies.lock().unwrap();
    assert_eq!(sent.len(), 3);
    assert!(
        sent[1].contains("\"anthropic.claude-sonnet-4-6\""),
        "{}",
        sent[1]
    );
    assert!(
        sent[2].contains("\"anthropic.claude-sonnet-4-6\""),
        "{}",
        sent[2]
    );
}

#[tokio::test]
async fn a_count_the_regions_mantle_host_refuses_goes_to_the_next() {
    let _guard = crate::test_support::always_on_tracing_guard();
    let missing =
        br#"{"type":"error","error":{"type":"not_found_error","message":"no such model"}}"#;
    let (first, first_bodies) = spawn_mock_sequence(vec![
        (404, "Not Found", missing.to_vec()),
        (404, "Not Found", missing.to_vec()),
        (500, "Internal Server Error", b"{}".to_vec()),
    ])
    .await;
    let (second, second_bodies) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"input_tokens": 21}"#.to_vec()),
        (404, "Not Found", missing.to_vec()),
    ])
    .await;
    let hosts = vec![first.clone(), second.clone()];
    let p = provider_at(&first);
    // The first host has no such model; the second counts it.
    assert_eq!(
        p.count_on_anthropic_route(&hosts, "t", "us.anthropic.claude-sonnet-5")
            .await
            .unwrap(),
        21
    );
    // Neither has it: the last answer is the one reported.
    let err = p
        .count_on_anthropic_route(&hosts, "t", "us.anthropic.claude-sonnet-5")
        .await
        .unwrap_err();
    assert_eq!(err.failure_kind(), Some(FailureKind::NotFound));
    // Any other answer from the first host is final.
    let err = p
        .count_on_anthropic_route(&hosts, "t", "us.anthropic.claude-sonnet-5")
        .await
        .unwrap_err();
    assert_eq!(err.failure_kind(), Some(FailureKind::ServerError));
    assert_eq!(first_bodies.lock().unwrap().len(), 3);
    assert_eq!(second_bodies.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn counting_falls_back_to_the_heuristic_when_no_route_counts() {
    let _guard = crate::test_support::always_on_tracing_guard();
    // A non-Anthropic model the runtime refuses has no second route.
    // (`llama-future` is a model the table has not seen, so the runtime
    // route is tried and its answer remembered.)
    let url = spawn_mock_server(404, "Not Found", br#"{"message":"unknown"}"#.to_vec()).await;
    let p = provider_at(&url);
    let text = "twenty characters!!!";
    assert_eq!(p.count_tokens(text, "us.meta.llama-future-v1:0").await, 5);
    assert!(p.count_route.contains("us.meta.llama-future-v1:0"));
    // A Claude behind a gateway with no mantle origin: nothing counts it.
    let url = spawn_mock_server(400, "Bad Request", b"{}".to_vec()).await;
    let p = BedrockProvider::new(client(), "k".to_string()).with_base_url(Some(url));
    assert_eq!(
        p.count_tokens("claude text here", "us.anthropic.claude-sonnet-4-6")
            .await,
        crate::tokenizer::count_tokens("claude text here", "claude-sonnet-4-6")
    );
    // A server error on the runtime route is not a refusal of the model.
    let url = spawn_mock_server(500, "Internal Server Error", b"{}".to_vec()).await;
    let p = provider_at(&url);
    assert_eq!(p.count_tokens(text, "us.meta.llama-future-v1:0").await, 5);
    assert!(!p.count_route.contains("us.meta.llama-future-v1:0"));
    // Answers without the count field.
    let url = spawn_mock_server(200, "OK", b"{}".to_vec()).await;
    assert_eq!(
        provider_at(&url)
            .count_tokens(text, "us.meta.llama-future-v1:0")
            .await,
        5
    );
    let (url, _) = spawn_mock_sequence(vec![
        (400, "Bad Request", b"{}".to_vec()),
        (200, "OK", b"{}".to_vec()),
    ])
    .await;
    let p = provider_at(&url);
    assert_eq!(
        p.count_tokens(text, "us.anthropic.claude-sonnet-4-6").await,
        6
    );
    // An answer that is not JSON, on either route, and a mantle host that
    // fails outright.
    let url = spawn_mock_server(200, "OK", b"{not json".to_vec()).await;
    let p = provider_at(&url);
    assert_eq!(p.count_tokens(text, "us.meta.llama-future-v1:0").await, 5);
    assert!(!p.count_route.contains("us.meta.llama-future-v1:0"));
    let (url, _) = spawn_mock_sequence(vec![
        (400, "Bad Request", b"{}".to_vec()),
        (200, "OK", b"{not json".to_vec()),
    ])
    .await;
    assert_eq!(
        provider_at(&url)
            .count_tokens(text, "us.anthropic.claude-sonnet-4-6")
            .await,
        6
    );
    let (url, _) = spawn_mock_sequence(vec![
        (400, "Bad Request", b"{}".to_vec()),
        (500, "Internal Server Error", b"{}".to_vec()),
    ])
    .await;
    assert_eq!(
        provider_at(&url)
            .count_tokens(text, "us.anthropic.claude-sonnet-4-6")
            .await,
        6
    );
}

#[test]
fn aws_error_types_and_messages_are_read_from_what_aws_sends() {
    let mut headers = reqwest::header::HeaderMap::new();
    assert_eq!(aws_error_type(&headers), None);
    headers.insert(
        "x-amzn-errortype",
        "ThrottlingException:http://x".parse().unwrap(),
    );
    assert_eq!(
        aws_error_type(&headers).as_deref(),
        Some("ThrottlingException")
    );
    headers.insert("x-amzn-errortype", "  ".parse().unwrap());
    assert_eq!(aws_error_type(&headers), None);
    assert_eq!(error_message(r#"{"message":"a"}"#).as_deref(), Some("a"));
    assert_eq!(error_message(r#"{"Message":"b"}"#).as_deref(), Some("b"));
    assert_eq!(
        error_message(r#"{"error":{"message":"c"}}"#).as_deref(),
        Some("c")
    );
    assert_eq!(error_message(r#"{"other":1}"#), None);
    assert_eq!(error_message("plain"), None);
    assert!(is_bad_key(Some("ExpiredTokenException"), ""));
    assert!(is_bad_key(Some("InvalidSignatureException"), ""));
    assert!(!is_bad_key(
        Some("AccessDeniedException"),
        "no model access"
    ));
    // Measured live: a key AWS does not know yet, or any more.
    assert!(is_bad_key(
        Some("AccessDeniedException"),
        r#"{"Message":"Authentication failed: Please make sure your API Key is valid."}"#
    ));
    assert!(is_bad_key(
        None,
        "The security token included in the request is invalid."
    ));
}
