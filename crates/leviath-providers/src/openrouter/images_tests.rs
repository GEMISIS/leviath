//! The gateway's Images API over local mocks, in the shapes its
//! documentation and listing give (2026-11-11).

use super::*;
use crate::learned::LearnedModel;
use crate::provider::{ContentBlock, Message, MessageContent, Provider};
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
use leviath_testkit::{spawn_mock_recorder, spawn_mock_sequence};
use serde_json::{Value, json};

const FLUX: &str = "black-forest-labs/flux-3-image";

/// OpenRouter's answer to an image model sent to chat, as live run 8 got it.
const REFUSAL: &str = r#"{"error":{"message":"black-forest-labs/flux-3-image is an image generation model and cannot be used with the chat/completions endpoint. Use the /api/v1/images endpoint instead.","code":404},"user_id":"u"}"#;

/// The documented reply: base64 bytes, their type, and the cost in dollars.
fn drawn() -> (u16, &'static str, Vec<u8>) {
    let reply = json!({
        "created": 1_748_372_400,
        "data": [ { "b64_json": "UE5H", "media_type": "image/png" } ],
        "usage": { "prompt_tokens": 0, "completion_tokens": 4175, "total_tokens": 4175, "cost": 0.04 }
    });
    (200, "OK", reply.to_string().into_bytes())
}

fn refused() -> (u16, &'static str, Vec<u8>) {
    (404, "Not Found", REFUSAL.as_bytes().to_vec())
}

/// A draw stage's request: its prompt, one reference image, and the
/// parameters it names.
fn request(model: &str) -> InferenceRequest {
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2]).named("ref.png");
    let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named("ref.png");
    InferenceRequest {
        system: vec![],
        messages: vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "a mossy temple tile".into(),
                },
                ContentBlock::Mime {
                    part: part.blob().unwrap().clone(),
                    data: "AQI=".into(),
                    name: part.name.clone(),
                    deliver: None,
                    remote: None,
                },
            ]),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.into(),
        max_tokens: 100,
        temperature: 0.5,
        tools: vec![],
        extra: json!({ "aspect_ratio": "1:1", "resolution": "1K", "n": 1, "ignored": "x" }),
        request_timeout_secs: Some(5),
    }
}

fn provider(url: &str) -> OpenRouterProvider {
    OpenRouterProvider::new(reqwest::Client::new(), "or-key".into())
        .with_base_url(Some(url.into()))
        .with_headers(vec![("X-Tenant".into(), "t1".into())])
}

/// The listing as it read live: flux on the Images API alone, a Gemini image
/// model on chat as well, a chat model with no image path.
fn listed(provider: &OpenRouterProvider) {
    let entry = |paths: &[&str]| LearnedModel {
        endpoints: Some(paths.iter().map(|p| p.to_string()).collect()),
        ..Default::default()
    };
    provider.learned.replace(std::collections::HashMap::from([
        (FLUX.to_string(), entry(&["/api/v1/images"])),
        (
            "google/gemini-3-pro-image".to_string(),
            entry(&["/api/v1/chat/completions", "/api/v1/images"]),
        ),
        (
            "anthropic/claude-sonnet-5".to_string(),
            entry(&["/api/v1/chat/completions", "/api/v1/messages"]),
        ),
    ]));
}

#[test]
fn the_listing_decides_which_models_take_the_images_api() {
    let gateway = provider("http://127.0.0.1:1");
    // Before the listing is read, nothing is sent there.
    assert!(!gateway.is_image_job(&request(FLUX)));
    listed(&gateway);
    assert!(gateway.is_image_job(&request(FLUX)));
    // The bare name a blueprint writes finds the same entry.
    assert!(gateway.is_image_job(&request("flux-3-image")));
    assert!(!gateway.is_image_job(&request("google/gemini-3-pro-image")));
    assert!(!gateway.is_image_job(&request("anthropic/claude-sonnet-5")));
    assert!(!gateway.is_image_job(&request("acme/unlisted")));
    assert!(images_only(&["/api/v1/images".to_string()]));
    assert!(!images_only(&[]));
}

/// The draw stage's request reaches `POST /images` in the documented shape,
/// and the picture comes back as the stage's output with the reported cost.
#[tokio::test]
async fn an_images_only_model_is_drawn_on_the_images_api() {
    let (url, bodies) = spawn_mock_sequence(vec![drawn()]).await;
    let gateway = provider(&url);
    listed(&gateway);
    let response = gateway
        .infer(&request("flux-3-image"))
        .await
        .expect("a picture");
    assert_eq!(response.parts.len(), 1);
    assert_eq!(response.parts[0].bytes, b"PNG");
    assert_eq!(response.parts[0].mime_type.as_str(), "image/png");
    assert_eq!(response.parts[0].name.as_deref(), Some("image-1.png"));
    assert_eq!(response.tokens_used.reported_cost_usd, Some(0.04));
    assert!(
        response.content.contains(&format!("openrouter/{FLUX}")),
        "{}",
        response.content
    );
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["model"], FLUX, "the listed id, not the bare name");
    assert_eq!(body["prompt"], "a mossy temple tile");
    assert_eq!(body["aspect_ratio"], "1:1");
    assert_eq!(body["resolution"], "1K");
    assert_eq!(body["n"], 1);
    assert!(body.get("ignored").is_none());
    assert!(body.get("response_format").is_none());
    assert!(body.get("messages").is_none(), "not a chat body");
    let reference = &body["input_references"][0];
    assert_eq!(reference["type"], "image_url");
    assert!(
        reference["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"),
        "{reference}"
    );
}

/// The call goes to `/images` with the gateway's own headers and the
/// operator's after them.
#[tokio::test]
async fn the_images_call_carries_the_gateway_headers() {
    let (url, requests) = spawn_mock_recorder(200, "OK", drawn().2).await;
    let gateway = provider(&url);
    listed(&gateway);
    gateway.infer(&request(FLUX)).await.expect("a picture");
    let sent = requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(sent.starts_with("post /images http/1.1"), "{sent}");
    assert!(sent.contains("authorization: bearer or-key"), "{sent}");
    assert!(sent.contains("x-title: leviath"), "{sent}");
    assert!(sent.contains("http-referer: https://leviath.dev"), "{sent}");
    assert!(sent.contains("x-tenant: t1"), "{sent}");
}

/// A streaming caller gets the same picture as one chunk.
#[tokio::test]
async fn a_streamed_draw_is_one_chunk_with_the_picture() {
    let (url, _) = spawn_mock_sequence(vec![drawn()]).await;
    let gateway = provider(&url);
    listed(&gateway);
    let stream = gateway
        .infer_stream(&request(FLUX))
        .await
        .expect("a stream");
    let response = crate::provider::stream::collect_stream(stream)
        .await
        .expect("a picture");
    assert_eq!(response.parts[0].bytes, b"PNG");
}

/// With no listing to say so, chat's refusal naming the Images API sends the
/// call there, and the model is remembered: the next call goes straight to
/// it.
#[tokio::test]
async fn a_refusal_naming_the_images_api_sends_the_model_there() {
    let (url, bodies) = spawn_mock_sequence(vec![refused(), drawn(), drawn()]).await;
    let gateway = provider(&url);
    let response = gateway.infer(&request(FLUX)).await.expect("a picture");
    assert_eq!(response.parts[0].bytes, b"PNG");
    assert_eq!(response.tokens_used.reported_cost_usd, Some(0.04));
    assert!(gateway.is_image_job(&request(FLUX)));
    gateway.infer(&request(FLUX)).await.expect("a picture");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[0].contains("\"messages\""), "the first went to chat");
    for body in &bodies[1..] {
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["prompt"], "a mossy temple tile");
    }
}

#[tokio::test]
async fn a_streamed_refusal_naming_the_images_api_is_answered_from_there() {
    let (url, _) = spawn_mock_sequence(vec![refused(), drawn()]).await;
    let gateway = provider(&url);
    let stream = gateway
        .infer_stream(&request(FLUX))
        .await
        .expect("a stream");
    let response = crate::provider::stream::collect_stream(stream)
        .await
        .expect("a picture");
    assert_eq!(response.parts[0].bytes, b"PNG");
}

/// A refusal that names another endpoint, or none, is not taken as the
/// Images API: it comes back as it was, and nothing is remembered.
#[tokio::test]
async fn other_refusals_are_not_rerouted() {
    let elsewhere = br#"{"error":{"message":"acme/embedder cannot be used with the chat/completions endpoint. Use the /api/v1/embeddings endpoint instead."}}"#;
    let (url, _) = spawn_mock_sequence(vec![
        (404, "Not Found", elsewhere.to_vec()),
        (404, "Not Found", b"No endpoints found for acme/x.".to_vec()),
    ])
    .await;
    let gateway = provider(&url);
    let err = gateway.infer(&request("acme/embedder")).await.unwrap_err();
    assert_eq!(
        err.failure_kind(),
        Some(crate::FailureKind::WrongEndpoint),
        "{err}"
    );
    assert!(!gateway.is_image_job(&request("acme/embedder")));
    let err = gateway.infer(&request("acme/x")).await.unwrap_err();
    assert_eq!(
        err.failure_kind(),
        Some(crate::FailureKind::NotFound),
        "{err}"
    );
    assert!(gateway.images_route.is_empty());
}

/// A prompt with no image to start from carries no `input_references`, and
/// a draw that fails after chat's refusal sent it there is that failure.
#[tokio::test]
async fn a_prompt_alone_is_drawn_and_a_rerouted_failure_is_the_error() {
    let (url, bodies) = spawn_mock_sequence(vec![
        drawn(),
        refused(),
        (
            502,
            "Bad Gateway",
            b"{\"error\":{\"message\":\"Generation failed\"}}".to_vec(),
        ),
    ])
    .await;
    let gateway = provider(&url);
    listed(&gateway);
    let mut alone = request(FLUX);
    alone.messages[0].content = MessageContent::Text("a mossy temple tile".into());
    gateway.infer(&alone).await.expect("a picture");
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["prompt"], "a mossy temple tile");
    assert!(body.get("input_references").is_none(), "{body}");
    let failed = gateway.infer(&request("acme/unlisted")).await.unwrap_err();
    assert_eq!(
        failed.failure_kind(),
        Some(crate::FailureKind::ServerError),
        "{failed}"
    );
}

/// A failed draw on the Images API is an error, not an empty picture.
#[tokio::test]
async fn a_failed_draw_is_an_error() {
    let (url, _) = spawn_mock_sequence(vec![
        (
            502,
            "Bad Gateway",
            b"{\"error\":{\"message\":\"Generation failed\"}}".to_vec(),
        ),
        (200, "OK", b"{\"data\":[]}".to_vec()),
    ])
    .await;
    let gateway = provider(&url);
    listed(&gateway);
    let failed = gateway.infer(&request(FLUX)).await.unwrap_err();
    assert_eq!(
        failed.failure_kind(),
        Some(crate::FailureKind::ServerError),
        "{failed}"
    );
    let empty = gateway.infer(&request(FLUX)).await.unwrap_err();
    assert!(empty.to_string().contains("no image"), "{empty}");
}
