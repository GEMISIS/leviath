//! Bedrock's image models over local mocks, in the shapes measured live.

use super::*;
use crate::Provider;
use crate::provider::{ContentBlock, Message, MessageContent};
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
use leviath_testkit::spawn_mock_sequence;

fn image_part() -> ContentBlock {
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
    let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named("a.png");
    let mut block = ContentBlock::mime(&part).unwrap();
    if let ContentBlock::Mime { data, .. } = &mut block {
        *data = "AQID".into();
    }
    block
}

fn request(model: &str, text: &str, parts: Vec<ContentBlock>, extra: Value) -> InferenceRequest {
    let mut blocks = vec![];
    if !text.is_empty() {
        blocks.push(ContentBlock::Text { text: text.into() });
    }
    blocks.extend(parts);
    InferenceRequest {
        system: vec![],
        messages: vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.into(),
        max_tokens: 100,
        temperature: 0.5,
        tools: vec![],
        extra,
        request_timeout_secs: Some(5),
    }
}

#[test]
fn the_image_models_are_known_through_a_profile_too() {
    assert!(is_image_model("stability.stable-image-core-v1:1"));
    assert!(is_image_model(
        "us.stability.stable-image-remove-background-v1:0"
    ));
    assert!(is_image_model("amazon.nova-canvas-v1:0"));
    assert!(is_image_model("amazon.titan-image-generator-v2:0"));
    assert!(!is_image_model("amazon.nova-pro-v1:0"));
    assert!(!is_image_model("us.anthropic.claude-sonnet-5"));
}

#[test]
fn a_stability_body_carries_the_prompt_the_image_and_the_parameters() {
    let generate = body(
        "stability.stable-image-core-v1:1",
        &request(
            "stability.stable-image-core-v1:1",
            "a lighthouse",
            vec![],
            serde_json::json!({ "aspect_ratio": "16:9", "output_format": "jpeg" }),
        ),
    );
    assert_eq!(generate["prompt"], "a lighthouse");
    assert_eq!(generate["aspect_ratio"], "16:9");
    assert_eq!(
        generate["output_format"], "jpeg",
        "a stage's own format wins"
    );
    assert!(generate.get("image").is_none());

    let edit = body(
        "stability.sd3-5-large-v1:0",
        &request(
            "stability.sd3-5-large-v1:0",
            "green",
            vec![image_part()],
            Value::Null,
        ),
    );
    assert_eq!(edit["image"], "AQID");
    assert_eq!(edit["mode"], "image-to-image");
    assert_eq!(edit["strength"], 0.7);

    let tool = body(
        "us.stability.stable-image-remove-background-v1:0",
        &request(
            "x",
            "Remove the background.",
            vec![image_part()],
            Value::Null,
        ),
    );
    assert!(
        tool.get("prompt").is_none(),
        "a tool that refuses a prompt is sent none"
    );
    assert!(tool.get("mode").is_none());
}

#[test]
fn a_nova_canvas_body_is_a_task_with_its_generation_config() {
    let generate = body(
        "amazon.nova-canvas-v1:0",
        &request(
            "amazon.nova-canvas-v1:0",
            "a lighthouse",
            vec![],
            serde_json::json!({ "width": 512, "height": 512 }),
        ),
    );
    assert_eq!(generate["taskType"], "TEXT_IMAGE");
    assert_eq!(generate["textToImageParams"]["text"], "a lighthouse");
    assert_eq!(generate["imageGenerationConfig"]["numberOfImages"], 1);
    assert_eq!(generate["imageGenerationConfig"]["width"], 512);

    let variation = body(
        "amazon.nova-canvas-v1:0",
        &request(
            "amazon.nova-canvas-v1:0",
            "brighter",
            vec![image_part()],
            Value::Null,
        ),
    );
    assert_eq!(variation["taskType"], "IMAGE_VARIATION");
    assert_eq!(variation["imageVariationParams"]["images"][0], "AQID");
}

#[test]
fn a_reply_is_its_images_or_the_refusal_it_carries() {
    let parts = images(
        "m",
        &serde_json::json!({ "images": ["UE5H", 5], "finish_reasons": [null] }),
    )
    .unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name.as_deref(), Some("image-1.png"));
    let jpeg = images(
        "m",
        &serde_json::json!({ "images": ["UE5H"], "output_format": "jpeg" }),
    )
    .unwrap();
    assert_eq!(jpeg[0].mime_type.as_str(), "image/jpeg");

    assert!(
        images(
            "m",
            &serde_json::json!({ "images": ["UE5H"], "output_format": "not a format" })
        )
        .is_err(),
        "a format that makes no type is an error"
    );
    let refused = images(
        "m",
        &serde_json::json!({ "error": "blocked by the content filter" }),
    );
    assert!(refused.unwrap_err().to_string().contains("content filter"));
    let filtered = images(
        "m",
        &serde_json::json!({ "images": [], "finish_reasons": ["Filter reason: prompt"] }),
    );
    assert!(filtered.unwrap_err().to_string().contains("Filter reason"));
    assert!(
        images("m", &serde_json::json!({ "images": [] }))
            .unwrap_err()
            .to_string()
            .contains("no image")
    );
    assert!(
        images("m", &serde_json::json!({ "images": ["not base64!"] }))
            .unwrap_err()
            .to_string()
            .contains("not base64")
    );
}

#[tokio::test]
async fn an_image_is_made_through_invoke_model_and_priced_per_image() {
    let reply = serde_json::json!({ "images": ["UE5H"], "finish_reasons": [null], "seeds": [1] });
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", reply.to_string().into_bytes()),
        (200, "OK", reply.to_string().into_bytes()),
    ])
    .await;
    let provider = super::super::BedrockProvider::new(reqwest::Client::new(), "ABSK".into())
        .with_base_url(Some(url));
    let model = "stability.stable-image-core-v1:1";
    let response = provider
        .infer(&request(model, "a lighthouse", vec![], Value::Null))
        .await
        .expect("an image");
    assert_eq!(response.parts.len(), 1);
    let per_image = crate::pricing::published_unit_rate("bedrock", model).map(|row| row.usd);
    assert_eq!(response.tokens_used.reported_cost_usd, per_image);
    let sent: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(sent["prompt"], "a lighthouse");

    let streamed = provider
        .infer_stream(&request(model, "again", vec![], Value::Null))
        .await
        .expect("a stream");
    let collected = crate::provider::collect_stream(streamed).await.unwrap();
    assert_eq!(collected.parts.len(), 1);

    assert!(!provider.capabilities(model).supports_tools);
    assert_eq!(provider.mime(model).output, ["image/*"]);
    assert!(provider.pricing(model).and_then(|p| p.unit).is_some());
}

#[tokio::test]
async fn an_image_call_that_is_refused_or_unreadable_is_an_error() {
    let model = "stability.stable-image-core-v1:1";
    for (status, reason, body) in [
        (400, "Bad Request", b"{\"message\":\"no\"}".to_vec()),
        (200, "OK", b"not json".to_vec()),
        (200, "OK", b"{\"images\":[]}".to_vec()),
    ] {
        let (url, _) = spawn_mock_sequence(vec![(status, reason, body)]).await;
        let provider = super::super::BedrockProvider::new(reqwest::Client::new(), "ABSK".into())
            .with_base_url(Some(url));
        assert!(
            provider
                .infer(&request(model, "a lighthouse", vec![], Value::Null))
                .await
                .is_err()
        );
    }
}
