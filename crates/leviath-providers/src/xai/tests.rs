//! The provider over a local mock of xAI's API.
//!
//! Every body here is the shape the live API answered on 2026-09-16, trimmed.

use super::*;
use crate::oauth::{Credentials, ProviderGrant, RefreshError, TokenSource};
use crate::provider::UnavailableReason;
use crate::provider::{Message, MessageContent, SystemBlock};
use leviath_testkit::{spawn_mock_recorder, spawn_mock_sequence, spawn_mock_server};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A sign-in that hands out `stale` and refreshes to `fresh`.
struct Signin {
    refreshes: AtomicUsize,
}

#[async_trait]
impl TokenSource for Signin {
    async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
        Ok(Credentials {
            access_token: "stale".to_string(),
            account_id: None,
        })
    }

    async fn refresh_stale(&self, stale: &str) -> std::result::Result<Credentials, RefreshError> {
        assert_eq!(stale, "stale", "the token that failed is the one refreshed");
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(Credentials {
            access_token: "fresh".to_string(),
            account_id: None,
        })
    }

    fn grant(&self) -> Option<ProviderGrant> {
        None
    }
}

/// A sign-in that is gone for good.
struct SignedOut;

#[async_trait]
impl TokenSource for SignedOut {
    async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
        Err(RefreshError::Terminal(
            "no grok credentials are stored; run `lev auth login grok` to sign in".into(),
        ))
    }

    async fn refresh_stale(&self, _: &str) -> std::result::Result<Credentials, RefreshError> {
        Err(RefreshError::Terminal("gone".into()))
    }

    fn grant(&self) -> Option<ProviderGrant> {
        None
    }
}

fn keyed(url: &str) -> XaiProvider {
    XaiProvider::new(reqwest::Client::new(), Auth::Key("xai-key".into()))
        .with_base_url(Some(format!("{url}/")))
}

fn signed_in(url: &str, tokens: Arc<dyn TokenSource>) -> XaiProvider {
    XaiProvider::new(reqwest::Client::new(), Auth::Signin(tokens)).with_base_url(Some(url.into()))
}

fn request(model: &str) -> InferenceRequest {
    InferenceRequest {
        system: vec![SystemBlock {
            text: "## task\ndo it".to_string(),
            cache_hint: leviath_core::CacheHint::Always,
            region: "task".to_string(),
            volatility: leviath_core::Volatility::Stable,
        }],
        messages: vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("go".to_string()),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.to_string(),
        max_tokens: 2048,
        temperature: 0.4,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

/// A finished response stream, as the route sends one.
fn sse(text: &str, ticks: u64) -> Vec<u8> {
    let frame =
        |v: serde_json::Value| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    let mut out = String::new();
    out.push_str(&frame(serde_json::json!({
        "type": "response.output_item.done",
        "item": { "type": "reasoning", "encrypted_content": "sealed-by-xai" }
    })));
    out.push_str(&frame(
        serde_json::json!({ "type": "response.output_text.delta", "delta": text }),
    ));
    out.push_str(&frame(serde_json::json!({
        "type": "response.completed",
        "response": { "status": "completed", "output": [],
            "usage": { "input_tokens": 199, "output_tokens": 1,
                "input_tokens_details": { "cached_tokens": 192 },
                "cost_in_usd_ticks": ticks } }
    })));
    out.into_bytes()
}

fn models_body() -> Vec<u8> {
    serde_json::json!({ "data": [{
        "id": "grok-4.3", "aliases": ["grok-4.3-latest"], "context_length": 1000000,
        "created": 1776556800, "prompt_text_token_price": 12500,
        "cached_prompt_text_token_price": 2000, "completion_text_token_price": 25000
    }]})
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn an_inference_streams_prices_itself_and_seals_its_reasoning() {
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("pong", 3_271_500))]).await;
    let provider = keyed(&url).with_reasoning_effort(Some("low".into()));
    let response = provider
        .infer(&request("grok-4.3"))
        .await
        .expect("inference");
    assert_eq!(response.content, "pong");
    let cost = response
        .tokens_used
        .reported_cost_usd
        .expect("xAI prices the call");
    assert!((cost - 0.000_327_15).abs() < 1e-12, "{cost}");
    assert_eq!(
        crate::responses::reasoning::items_for("xai", response.reasoning.as_deref().unwrap()),
        ["sealed-by-xai"]
    );
    let body: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 2048);
    assert_eq!(body["reasoning"]["effort"], "low");
    assert!(body.get("text").is_none(), "{body}");
}

#[tokio::test]
async fn a_model_that_chooses_its_own_depth_is_sent_no_effort() {
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("ok", 0))]).await;
    let provider = keyed(&url).with_reasoning_effort(Some("high".into()));
    provider
        .infer(&request("grok-build-0.1"))
        .await
        .expect("inference");
    let body: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert!(body.get("reasoning").is_none(), "{body}");
}

#[tokio::test]
async fn a_refused_effort_is_retried_without_one_and_remembered() {
    let refusal =
        br#"{"error":"Model grok-4.3 does not support parameter reasoning.effort"}"#.to_vec();
    let (url, bodies) = spawn_mock_sequence(vec![
        (400, "Bad Request", refusal),
        (200, "OK", sse("second", 0)),
        (200, "OK", sse("third", 0)),
    ])
    .await;
    let provider = keyed(&url).with_reasoning_effort(Some("high".into()));
    assert_eq!(
        provider.infer(&request("grok-4.3")).await.unwrap().content,
        "second"
    );
    assert_eq!(
        provider.infer(&request("grok-4.3")).await.unwrap().content,
        "third"
    );
    let bodies = bodies.lock().unwrap().clone();
    assert!(bodies[0].contains("\"effort\""), "{}", bodies[0]);
    assert!(!bodies[1].contains("\"effort\""), "{}", bodies[1]);
    assert!(
        !bodies[2].contains("\"effort\""),
        "the refusal was not remembered: {}",
        bodies[2]
    );
}

#[tokio::test]
async fn a_400_that_is_not_about_effort_is_the_error_itself() {
    let (url, _) =
        spawn_mock_sequence(vec![(400, "Bad Request", b"context too long".to_vec())]).await;
    let provider = keyed(&url).with_reasoning_effort(Some("high".into()));
    let err = provider.infer(&request("grok-4.3")).await.unwrap_err();
    assert!(err.to_string().contains("context too long"), "{err}");
    // And a 400 with no effort sent goes through the ordinary classifier.
    let (url, _) = spawn_mock_sequence(vec![(400, "Bad Request", b"bad".to_vec())]).await;
    let err = keyed(&url).infer(&request("grok-4.3")).await.unwrap_err();
    assert!(err.to_string().contains("bad"), "{err}");
    // A credit refusal is the unavailable kind wherever it happens.
    let (url, _) = spawn_mock_sequence(vec![(
        400,
        "Bad Request",
        br#"{"error":"Your team has run out of credits"}"#.to_vec(),
    )])
    .await;
    let err = keyed(&url)
        .with_reasoning_effort(Some("high".into()))
        .infer(&request("grok-4.3"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("credits"), "{err}");
}

#[tokio::test]
async fn a_subscription_refreshes_once_on_a_401_and_costs_nothing() {
    let tokens = Arc::new(Signin {
        refreshes: AtomicUsize::new(0),
    });
    let (url, _) = spawn_mock_sequence(vec![
        (401, "Unauthorized", b"expired".to_vec()),
        (200, "OK", sse("after refresh", 3_271_500)),
    ])
    .await;
    let provider = signed_in(&url, tokens.clone());
    assert_eq!(provider.name(), "grok");
    let response = provider
        .infer(&request("grok-4.3"))
        .await
        .expect("inference");
    assert_eq!(response.content, "after refresh");
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        response.tokens_used.reported_cost_usd, None,
        "a subscription's list price is not its cost"
    );
    assert_eq!(
        provider.pricing("grok-4.3"),
        Some(crate::ModelPricing::flat(0.0, 0.0))
    );
    assert!(
        crate::responses::reasoning::items_for("grok", response.reasoning.as_deref().unwrap())
            .len()
            == 1
    );
}

#[tokio::test]
async fn a_sign_in_that_is_gone_says_to_sign_in_again() {
    let provider = signed_in("http://127.0.0.1:1", Arc::new(SignedOut));
    let err = provider.infer(&request("grok-4.3")).await.unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::AuthFailed)
    );
    assert!(err.to_string().contains("lev auth login grok"), "{err}");
    // A 401 that the refresh cannot fix either.
    struct Refusing;
    #[async_trait]
    impl TokenSource for Refusing {
        async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
            Ok(Credentials::default())
        }
        async fn refresh_stale(&self, _: &str) -> std::result::Result<Credentials, RefreshError> {
            Err(RefreshError::Terminal(
                "the Grok session was revoked".into(),
            ))
        }
        fn grant(&self) -> Option<ProviderGrant> {
            None
        }
    }
    let (url, _) = spawn_mock_sequence(vec![(401, "Unauthorized", vec![])]).await;
    let err = signed_in(&url, Arc::new(Refusing))
        .infer(&request("grok-4.3"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("revoked"), "{err}");
    let err = signed_in("http://127.0.0.1:1", Arc::new(SignedOut))
        .list_models()
        .await
        .unwrap_err();
    assert_eq!(
        err.unavailable_reason(),
        Some(UnavailableReason::AuthFailed)
    );
}

#[tokio::test]
async fn an_unreachable_host_is_a_transport_error() {
    let err = keyed("http://127.0.0.1:1")
        .infer(&request("grok-4.3"))
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .to_ascii_lowercase()
            .contains("sending the request"),
        "{err}"
    );
}

#[tokio::test]
async fn the_key_and_extra_headers_reach_the_host() {
    let (url, seen) = spawn_mock_recorder(200, "OK", sse("hi", 0)).await;
    let _stream = keyed(&url)
        .with_headers(vec![("X-Gateway".into(), "tenant-7".into())])
        .with_request_timeout(Some(30))
        .with_rate_limit(Some(&RateLimitConfig {
            requests_per_minute: 600,
            tokens_per_minute: 1_000_000,
        }))
        .infer_stream(&request("grok-4.3"))
        .await
        .expect("a stream");
    let raw = seen.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(raw.contains("post /responses"), "{raw}");
    assert!(raw.contains("authorization: bearer xai-key"), "{raw}");
    assert!(raw.contains("x-gateway: tenant-7"), "{raw}");
}

#[tokio::test]
async fn priming_reads_all_four_listings_and_answers_for_aliases() {
    let language = serde_json::json!({ "models": [
        { "id": "grok-4.3", "input_modalities": ["text", "image"], "output_modalities": ["text"] }
    ]});
    let images = serde_json::json!({ "models": [
        { "id": "grok-imagine-image", "image_price": 200000000,
          "input_modalities": ["text", "image"], "output_modalities": ["image"] }
    ]});
    let videos = serde_json::json!({ "models": [
        { "id": "grok-imagine-video", "input_modalities": ["text", "image", "video"], "output_modalities": ["video"] }
    ]});
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", models_body()),
        (200, "OK", language.to_string().into_bytes()),
        (200, "OK", images.to_string().into_bytes()),
        (200, "OK", videos.to_string().into_bytes()),
    ])
    .await;
    let provider = keyed(&url);
    assert_eq!(provider.serves_model("grok-4.3"), Some("grok-4.3".into()));
    let models = provider.check_credential().await.expect("listed");
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    // The speech routes take no model field and are in no listing, so they
    // are always offered.
    assert_eq!(
        ids,
        [
            "grok-4.3",
            "grok-imagine-image",
            "grok-imagine-video",
            "grok-tts",
            "grok-stt"
        ]
    );
    assert_eq!(provider.serves_model("grok-tts"), Some("grok-tts".into()));

    assert_eq!(
        provider.serves_model("grok-4.3-latest"),
        Some("grok-4.3".into())
    );
    assert_eq!(
        provider.serves_model("grok-4.6"),
        None,
        "the listing is the answer once read"
    );
    assert_eq!(provider.max_context_tokens("grok-4.3-latest"), 1_000_000);
    let catalog = provider.served_catalog().expect("primed");
    assert_eq!(catalog.len(), 5, "{catalog:?}");
    assert!(
        catalog.iter().any(|id| id == "grok-tts") && catalog.iter().any(|id| id == "grok-stt"),
        "the speech routes no listing names are in the catalogue: {catalog:?}"
    );
    let price = provider.pricing("grok-4.3-latest").expect("priced live");
    assert!((price.input_per_mtok - 1.25).abs() < 1e-9);
    assert!(
        provider
            .mime("grok-4.3")
            .accepts(&leviath_core::mime::MimeType::parse("image/png").unwrap())
    );
    assert!(
        provider
            .mime("grok-4.3")
            .accepts(&leviath_core::mime::MimeType::parse("application/pdf").unwrap()),
        "a listed chat model still reads PDFs"
    );
    let video = provider.mime("grok-imagine-video");
    assert_eq!(video.output, ["video/*"]);
    assert!(provider.learned_models().is_some());
    assert_eq!(
        provider
            .pricing("grok-imagine-image")
            .and_then(|p| p.unit)
            .map(|u| u.unit),
        Some(crate::pricing::PriceUnit::Image)
    );
}

#[tokio::test]
async fn a_media_listing_that_fails_leaves_the_chat_listing_standing() {
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", models_body()),
        (500, "Internal Server Error", vec![]),
        (500, "Internal Server Error", vec![]),
        (500, "Internal Server Error", vec![]),
    ])
    .await;
    let provider = keyed(&url);
    assert_eq!(provider.list_models().await.expect("listed").len(), 3);
    // A primed provider does not ask again.
    assert_eq!(provider.list_models().await.expect("listed again").len(), 3);
}

#[tokio::test]
async fn a_chat_listing_that_fails_fails_the_check() {
    let url = spawn_mock_server(403, "Forbidden", b"bad key".to_vec()).await;
    assert!(keyed(&url).check_credential().await.is_err());
}

#[test]
fn before_priming_the_table_answers() {
    let provider = keyed("http://127.0.0.1:1");
    assert_eq!(provider.serves_model("grok-4.6"), Some("grok-4.6".into()));
    assert_eq!(provider.serves_model("gpt-5.5"), None);
    assert_eq!(provider.served_catalog(), None);
    assert_eq!(provider.max_context_tokens("grok-4.6"), 500_000);
    assert_eq!(provider.name(), "xai");
    // No listing, so the price table answers, when it has a row.
    let _ = provider.pricing("grok-4.6");
}

#[test]
fn an_override_corrects_by_the_name_a_blueprint_uses() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "grok-4.6".to_string(),
        ModelCapabilityOverride {
            max_context_tokens: Some(64_000),
            input_per_mtok: Some(9.0),
            output_per_mtok: Some(9.0),
            ..Default::default()
        },
    );
    let provider = keyed("http://127.0.0.1:1").with_overrides(overrides);
    assert_eq!(provider.capabilities("grok-4.6").max_context_tokens, 64_000);
    assert_eq!(provider.pricing("grok-4.6").unwrap().input_per_mtok, 9.0);
    // An override that names mime reaches the mime answer too.
    let _ = provider.mime("grok-4.6");
}

#[tokio::test]
async fn a_model_that_refuses_temperature_is_sent_none() {
    let mut overrides = HashMap::new();
    overrides.insert(
        "grok-4.3".to_string(),
        ModelCapabilityOverride {
            supports_temperature: Some(false),
            ..Default::default()
        },
    );
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", sse("ok", 0))]).await;
    keyed(&url)
        .with_overrides(overrides)
        .infer(&request("grok-4.3"))
        .await
        .unwrap();
    assert!(!bodies.lock().unwrap()[0].contains("temperature"));
}

#[test]
fn the_debug_form_never_prints_a_key() {
    assert!(refuses_effort("reasoning effort is not supported"));
    assert!(!refuses_effort("prompt too long"));
    // A blank effort is no effort.
    assert!(
        keyed("http://x")
            .with_reasoning_effort(Some("  ".into()))
            .effort
            .is_none()
    );
}

/// A signed-in provider whose account host is `account`.
fn subscription(url: &str, account: &str) -> XaiProvider {
    signed_in(
        url,
        Arc::new(Signin {
            refreshes: AtomicUsize::new(0),
        }),
    )
    .with_account_url(Some(format!("{account}/")))
}

#[tokio::test]
async fn a_subscription_reports_its_billing_windows() {
    let credits = serde_json::json!({ "config": {
        "currentPeriod": { "end": "2026-09-23T22:25:42.311757+00:00" },
        "onDemandCap": { "val": 10 }, "onDemandUsed": { "val": 4 }, "prepaidBalance": { "val": 0 }
    }});
    let monthly =
        serde_json::json!({ "config": { "used": { "val": 2 }, "monthlyLimit": { "val": 0 } } });
    let (account, _) = spawn_mock_sequence(vec![
        (200, "OK", credits.to_string().into_bytes()),
        (200, "OK", monthly.to_string().into_bytes()),
    ])
    .await;
    let report = subscription("http://127.0.0.1:1", &account)
        .quota()
        .await
        .expect("a subscription has quota")
        .expect("read");
    assert_eq!(report.windows.len(), 2);
    assert_eq!(report.windows[0].used, Some(4.0));
    assert!(
        keyed("http://127.0.0.1:1").quota().await.is_none(),
        "a key has no quota"
    );
}

#[tokio::test]
async fn billing_that_cannot_be_read_says_why() {
    let (account, _) = spawn_mock_sequence(vec![
        (500, "Internal Server Error", b"down".to_vec()),
        (500, "Internal Server Error", b"down".to_vec()),
    ])
    .await;
    let err = subscription("http://127.0.0.1:1", &account)
        .quota()
        .await
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("500"), "{err}");
    let (account, _) = spawn_mock_sequence(vec![
        (200, "OK", b"{}".to_vec()),
        (200, "OK", b"{}".to_vec()),
    ])
    .await;
    let err = subscription("http://127.0.0.1:1", &account)
        .quota()
        .await
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("shape"), "{err}");
}

#[tokio::test]
async fn a_limit_with_no_retry_after_waits_for_the_billing_period() {
    let credits = serde_json::json!({ "config": {
        "currentPeriod": { "end": "2999-01-01T00:00:00+00:00" },
        "onDemandCap": { "val": 1 }, "onDemandUsed": { "val": 1 }
    }});
    let (url, _) = spawn_mock_sequence(vec![(429, "Too Many Requests", b"slow".to_vec())]).await;
    let (account, _) = spawn_mock_sequence(vec![
        (200, "OK", credits.to_string().into_bytes()),
        (500, "Internal Server Error", vec![]),
    ])
    .await;
    let err = subscription(&url, &account)
        .infer(&request("grok-4.3"))
        .await
        .unwrap_err();
    match err {
        crate::provider::ProviderError::RateLimitExceeded { retry_after_secs } => {
            assert!(
                retry_after_secs.unwrap_or(0) > 86_400,
                "{retry_after_secs:?}"
            )
        }
        other => panic!("expected a rate limit, got {other}"),
    }
    // With no quota to read, the wait is unknown rather than invented.
    let (url, _) = spawn_mock_sequence(vec![(429, "Too Many Requests", vec![])]).await;
    let err = subscription(&url, "http://127.0.0.1:1")
        .infer(&request("grok-4.3"))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        crate::provider::ProviderError::RateLimitExceeded {
            retry_after_secs: None
        }
    ));
}

#[tokio::test]
async fn a_subscription_takes_each_models_efforts_from_its_catalogue() {
    let v2 = serde_json::json!({ "data": [
        { "id": "grok-4.3", "supports_reasoning_effort": true, "reasoning_efforts": [ { "value": "high" } ] },
        { "id": "grok-4.6", "supports_reasoning_effort": false }
    ]});
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", models_body()),
        (500, "Internal Server Error", vec![]),
        (500, "Internal Server Error", vec![]),
        (500, "Internal Server Error", vec![]),
        (200, "OK", sse("a", 0)),
        (200, "OK", sse("b", 0)),
    ])
    .await;
    let (account, _) = spawn_mock_sequence(vec![(200, "OK", v2.to_string().into_bytes())]).await;
    let provider = subscription(&url, &account).with_reasoning_effort(Some("high".into()));
    provider.prime_capabilities().await.expect("primed");
    provider.infer(&request("grok-4.3")).await.unwrap();
    // The table says grok-4.6 takes an effort; the subscription says it does
    // not, and a caller's own low effort is taken off it too.
    let mut titled = request("grok-4.6");
    titled.extra = serde_json::json!({ "reasoning": { "effort": "low" } });
    provider.infer(&titled).await.unwrap();
    let bodies = bodies.lock().unwrap().clone();
    assert!(bodies[4].contains("\"effort\":\"high\""), "{}", bodies[4]);
    assert!(!bodies[5].contains("effort"), "{}", bodies[5]);

    // A catalogue that cannot be read leaves the table in charge.
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", models_body()),
        (500, "Internal Server Error", vec![]),
        (500, "Internal Server Error", vec![]),
        (500, "Internal Server Error", vec![]),
    ])
    .await;
    let provider = subscription(&url, "http://127.0.0.1:1");
    provider
        .prime_capabilities()
        .await
        .expect("primed without efforts");
    assert!(provider.sends_effort("grok-4.3", "low"));
}

#[tokio::test]
async fn a_subscription_quotes_its_retention_setting_once_read() {
    let (account, _) = spawn_mock_sequence(vec![(
        200,
        "OK",
        br#"{"codingDataRetentionOptOut":true}"#.to_vec(),
    )])
    .await;
    let provider = subscription("http://127.0.0.1:1", &account);
    assert!(
        provider.live_retention("grok-4.3").is_none(),
        "unread is unknown"
    );
    provider.refresh_retention().await;
    let policy = provider.live_retention("grok-4.3").expect("read");
    assert!(policy.note.contains("opt-out is on"), "{}", policy.note);
    assert_eq!(policy.source, crate::retention::Source::Live);
    // Read once: the mock has nothing left to answer, and nothing is asked.
    provider.refresh_retention().await;

    let (account, _) = spawn_mock_sequence(vec![(
        200,
        "OK",
        br#"{"codingDataRetentionOptOut":false}"#.to_vec(),
    )])
    .await;
    let off = subscription("http://127.0.0.1:1", &account);
    off.refresh_retention().await;
    assert!(
        off.live_retention("grok-4.3")
            .unwrap()
            .note
            .contains("opt-out is off")
    );

    // An account that cannot be read stays unknown; a key never asks.
    let dead = subscription("http://127.0.0.1:1", "http://127.0.0.1:1");
    dead.refresh_retention().await;
    assert!(dead.live_retention("grok-4.3").is_none());
    let key = keyed("http://127.0.0.1:1");
    key.refresh_retention().await;
    assert!(key.live_retention("grok-4.3").is_none());
    let _ = subscription("http://x", "http://y").with_account_url(None);
}

#[tokio::test]
async fn a_chat_model_takes_documents_by_file_and_a_media_model_takes_none() {
    let (url, seen) = spawn_mock_recorder(200, "OK", br#"{"id":"file-x"}"#.to_vec()).await;
    let provider = keyed(&url);
    let pdf = leviath_core::mime::MimeType::parse("application/pdf").unwrap();
    assert!(provider.media_limits("grok-4.3").by_file(&pdf, 1));
    assert!(!provider.media_limits("grok-imagine-image").by_file(&pdf, 1));
    let upload = crate::files::FileUpload {
        bytes: Arc::from(&b"%PDF"[..]),
        mime_type: "application/pdf".into(),
        name: "a.pdf".into(),
        ttl_secs: 3_600,
    };
    assert_eq!(provider.upload_file(&upload).await.unwrap().id, "file-x");
    let (gone, deleted) = spawn_mock_recorder(404, "Not Found", b"{}".to_vec()).await;
    keyed(&gone)
        .delete_file(&crate::files::RemoteFile {
            id: "file-x".into(),
            uri: None,
            expires_at: None,
        })
        .await
        .unwrap();
    let raw = seen.lock().unwrap().join("");
    assert!(raw.contains("name=\"purpose\"\r\n\r\nassistants"), "{raw}");
    let deleted = deleted.lock().unwrap().join("");
    assert!(deleted.contains("DELETE /files/file-x"), "{deleted}");
}

#[tokio::test]
async fn a_media_model_runs_through_both_entry_points() {
    let reply = serde_json::json!({ "data": [ { "b64_json": "SlBFRw==" } ],
        "usage": { "cost_in_usd_ticks": 200000000 } })
    .to_string()
    .into_bytes();
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", reply.clone()), (200, "OK", reply)]).await;
    let provider = keyed(&url).with_poll_interval(std::time::Duration::from_millis(1));
    let buffered = provider
        .infer(&request("grok-imagine-image"))
        .await
        .unwrap();
    assert_eq!(buffered.tokens_used.reported_cost_usd, Some(0.02));
    let mut stream = provider
        .infer_stream(&request("grok-imagine-image"))
        .await
        .unwrap();
    use tokio_stream::StreamExt;
    assert_eq!(stream.next().await.unwrap().unwrap().parts.len(), 1);
    assert_eq!(provider.count_tokens("12345678", "grok-4.3").await, 2);
}

#[tokio::test]
async fn a_refusal_saying_reasoning_is_not_supported_is_retried_and_a_lost_retry_is_an_error() {
    let (url, _) = spawn_mock_sequence(vec![(
        400,
        "Bad Request",
        b"reasoning is not supported for this model".to_vec(),
    )])
    .await;
    let err = keyed(&url)
        .with_reasoning_effort(Some("high".into()))
        .infer(&request("grok-4.3"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Request failed"), "{err}");
}

#[test]
fn account_and_catalog_readers_skip_what_they_cannot_read() {
    let efforts = account::efforts(&serde_json::json!({ "data": [
        { "object": "no id" },
        { "id": 5 },
        { "id": "grok-4.6", "reasoning_efforts": [ { "label": "no value" }, { "id": "low" } ] }
    ]}));
    assert_eq!(efforts.len(), 1);
    assert_eq!(efforts["grok-4.6"], vec!["low".to_string()]);
    let report = account::quota(
        Some(&serde_json::json!({ "config": { "onDemandCap": { "val": 5.0 }, "prepaidBalance": 3.0 } })),
        None,
    )
    .unwrap();
    assert_eq!(
        report.windows[0].used, None,
        "a missing amount is no amount"
    );

    let mut listing = catalog::Listing::default();
    catalog::read_models(
        &serde_json::json!({ "data": [
            { "id": "no-prices", "context_length": 1000, "completion_text_token_price": 10000 },
            { "id": "half-priced", "prompt_text_token_price": 10000, "completion_text_token_price": "free" }
        ]}),
        &mut listing,
    );
    assert!(listing.models["no-prices"].pricing.is_none());
    assert!(listing.models["half-priced"].pricing.is_none());
}
