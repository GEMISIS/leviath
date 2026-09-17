//! Google's media models over local mocks, in the shapes measured live.

use super::*;
use crate::provider::Message;
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
use leviath_testkit::{spawn_mock_sequence, spawn_mock_server_with_headers};

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
    let mut blocks = vec![ContentBlock::Text { text: text.into() }];
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

fn provider(url: &str) -> crate::gemini::GeminiProvider {
    let mut provider = crate::gemini::GeminiProvider::new(reqwest::Client::new(), "g-key".into())
        .with_base_url(Some(url.into()));
    provider.video_poll = Duration::from_millis(1);
    provider
}

#[test]
fn the_media_models_are_known_by_name_and_sized_as_media() {
    assert_eq!(kind("veo-3.1-lite-generate-preview"), Some(Kind::Video));
    for prompted in [
        "gemini-3.1-flash-image",
        "nano-banana-pro-preview",
        "gemini-2.5-flash-preview-tts",
        "lyria-3.5",
    ] {
        assert_eq!(kind(prompted), Some(Kind::Prompted), "{prompted}");
    }
    assert_eq!(kind("gemini-3.5-flash"), None);
    assert!(CATALOG.iter().all(|(id, _)| kind(id).is_some()));

    let listed = crate::ModelCapabilities {
        max_context_tokens: 480,
        max_output_tokens: 65_535,
        ..crate::ModelCapabilities::default()
    };
    let veo = adjusted("veo-3.1-generate-preview", listed.clone());
    assert!(!veo.supports_tools);
    assert_eq!(
        (veo.max_context_tokens, veo.max_output_tokens),
        (32_000, 4_096)
    );
    assert_eq!(adjusted("gemini-3.5-flash", listed.clone()), listed);
}

#[test]
fn a_prompted_model_is_sent_the_prompt_and_its_parts_alone() {
    let body = prompted_body(&request(
        "gemini-2.5-flash-preview-tts",
        "Say hello",
        vec![image_part()],
        serde_json::json!({
            "voice": "Puck", "language": "en-US", "aspect_ratio": "16:9",
            "image_size": "2K", "temperature": 0.4, "seed": 7
        }),
    ));
    assert!(body.get("system_instruction").is_none());
    assert!(body.get("tools").is_none());
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    let content = &body["input"][0]["content"];
    assert_eq!(content[0]["text"], "Say hello");
    assert_eq!(content[1]["type"], "image");
    let config = &body["generation_config"];
    assert_eq!(
        config["speech_config"],
        serde_json::json!([{ "voice": "Puck", "language": "en-US" }])
    );
    assert_eq!(config["image_config"]["aspect_ratio"], "16:9");
    assert_eq!(config["image_config"]["image_size"], "2K");
    assert_eq!(config["seed"], 7);

    let bare = prompted_body(&request("lyria-3.5", "a jingle", vec![], Value::Null));
    assert!(bare.get("generation_config").is_none());
    let voice_only = prompted_body(&request(
        "gemini-2.5-flash-preview-tts",
        "hi",
        vec![],
        serde_json::json!({ "language": "fr-FR" }),
    ));
    assert_eq!(
        voice_only["generation_config"]["speech_config"],
        serde_json::json!([{ "language": "fr-FR" }])
    );
}

#[tokio::test]
async fn a_veo_video_is_started_polled_downloaded_and_priced_by_the_second() {
    let video =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: video/mp4\r\n", b"MP4".to_vec())
            .await;
    let done = serde_json::json!({
        "name": "models/veo/operations/op1",
        "done": true,
        "response": { "generateVideoResponse": { "generatedSamples": [{ "video": { "uri": video } }] } }
    });
    let (url, bodies) = spawn_mock_sequence(vec![
        (
            200,
            "OK",
            br#"{"name":"models/veo/operations/op1"}"#.to_vec(),
        ),
        (
            200,
            "OK",
            br#"{"name":"models/veo/operations/op1"}"#.to_vec(),
        ),
        (200, "OK", done.to_string().into_bytes()),
    ])
    .await;
    let response = provider(&url)
        .infer(&request(
            "veo-3.1-lite-generate-preview",
            "a paper boat",
            vec![image_part()],
            serde_json::json!({ "duration": 4, "aspect_ratio": "16:9", "negative_prompt": "rain" }),
        ))
        .await
        .expect("a video");
    assert_eq!(response.parts[0].bytes, b"MP4");
    assert_eq!(response.parts[0].name.as_deref(), Some("video.mp4"));
    // The shipped table prices Veo 3.1 Lite by the second.
    let per_second = crate::pricing::published_unit_rate("google", "veo-3.1-lite-generate-preview")
        .map(|row| row.usd);
    assert_eq!(
        response.tokens_used.reported_cost_usd,
        per_second.map(|usd| usd * 4.0)
    );
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["instances"][0]["prompt"], "a paper boat");
    assert_eq!(body["instances"][0]["image"]["mimeType"], "image/png");
    assert_eq!(body["parameters"]["durationSeconds"], 4);
    assert_eq!(body["parameters"]["aspectRatio"], "16:9");
    assert_eq!(body["parameters"]["negativePrompt"], "rain");
}

#[tokio::test]
async fn veo_refusals_failures_and_bad_answers_are_errors() {
    let refused = serde_json::json!({
        "done": true,
        "response": { "generateVideoResponse": { "raiMediaFilteredReasons": ["it named a public figure"] } }
    });
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"name":"op"}"#.to_vec()),
        (200, "OK", refused.to_string().into_bytes()),
    ])
    .await;
    let err = provider(&url)
        .infer_stream(&request(
            "veo-3.1-generate-preview",
            "x",
            vec![],
            Value::Null,
        ))
        .await
        .err()
        .expect("refused");
    assert!(err.to_string().contains("public figure"), "{err}");

    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"name":"op"}"#.to_vec()),
        (200, "OK", br#"{"error":{"message":"quota"}}"#.to_vec()),
    ])
    .await;
    let err = provider(&url)
        .infer(&request(
            "veo-3.1-generate-preview",
            "x",
            vec![],
            Value::Null,
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("quota"), "{err}");

    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"name":"op"}"#.to_vec()),
        (200, "OK", br#"{"error":"plain"}"#.to_vec()),
    ])
    .await;
    let err = provider(&url)
        .infer(&request(
            "veo-3.1-generate-preview",
            "x",
            vec![],
            Value::Null,
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("plain"), "{err}");

    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"name":"op"}"#.to_vec()),
        (200, "OK", br#"{"done":true}"#.to_vec()),
    ])
    .await;
    let err = provider(&url)
        .infer(&request(
            "veo-3.1-generate-preview",
            "x",
            vec![],
            Value::Null,
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("carried no video"), "{err}");

    let (url, _) = spawn_mock_sequence(vec![(200, "OK", b"{}".to_vec())]).await;
    let err = provider(&url)
        .infer(&request(
            "veo-3.1-generate-preview",
            "x",
            vec![],
            Value::Null,
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no operation name"), "{err}");

    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"name":"op"}"#.to_vec()),
        (500, "Internal Server Error", b"down".to_vec()),
    ])
    .await;
    assert!(
        provider(&url)
            .infer(&request(
                "veo-3.1-generate-preview",
                "x",
                vec![],
                Value::Null
            ))
            .await
            .is_err()
    );

    let err = provider("http://127.0.0.1:1")
        .infer(&request(
            "veo-3.1-generate-preview",
            "",
            vec![],
            Value::Null,
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("needs a prompt"), "{err}");
}

#[tokio::test]
async fn a_prompted_model_streams_through_interactions_with_its_own_body() {
    let events = [
        serde_json::json!({ "event_type": "step.start", "index": 0, "step": { "type": "model_output" } }),
        serde_json::json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "image", "mime_type": "image/jpeg", "data": "SlBFRw==" } }),
        serde_json::json!({ "event_type": "interaction.completed", "interaction": { "status": "completed" } }),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", events.into_bytes())]).await;
    let response = provider(&url)
        .infer(&request(
            "gemini-3.1-flash-image",
            "a lighthouse",
            vec![],
            Value::Null,
        ))
        .await
        .expect("an image");
    assert_eq!(response.parts[0].name.as_deref(), Some("image-1.jpg"));
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert!(body.get("system_instruction").is_none(), "{body}");
}

#[test]
fn a_media_model_is_not_narrowed_and_takes_no_file_by_uri() {
    let google = provider("http://127.0.0.1:1");
    assert_eq!(google.mime("veo-3.1-generate-preview").output, ["video/*"]);
    assert!(
        google
            .mime("gemini-3.5-flash")
            .input
            .iter()
            .any(|t| t == "video/*")
    );
    assert_eq!(
        google.media_limits("veo-3.1-generate-preview").file_bytes,
        None
    );
    assert!(google.media_limits("gemini-3.5-flash").file_bytes.is_some());
    assert!(google.serves_model("lyria-3.5").is_some());

    // A plain-text turn beside the prompt, and a voice with no language.
    let mut request = request(
        "gemini-2.5-flash-preview-tts",
        "Say hi",
        vec![],
        Value::Null,
    );
    request.messages.push(Message {
        role: "user".into(),
        content: MessageContent::Text("and more".into()),
        cache_breakpoint: false,
        reasoning: None,
    });
    request.extra = serde_json::json!({ "voice": "Kore" });
    let body = prompted_body(&request);
    assert_eq!(
        body["generation_config"]["speech_config"],
        serde_json::json!([{ "voice": "Kore" }])
    );
    assert_eq!(body["input"][0]["content"].as_array().unwrap().len(), 1);
}

/// Every step of a Veo operation can fail, and each failure is the call's
/// error.
#[tokio::test]
async fn a_veo_operation_that_fails_at_any_step_is_an_error() {
    let veo = |url: String| async move {
        provider(&url)
            .infer(&request(
                "veo-3.1-generate-preview",
                "a boat",
                vec![],
                Value::Null,
            ))
            .await
    };
    let done = |uri: &str| {
        serde_json::json!({ "done": true, "response": { "generateVideoResponse": {
            "generatedSamples": [{ "video": { "uri": uri } }] } } })
        .to_string()
        .into_bytes()
    };
    // The start is refused.
    let (url, _) = spawn_mock_sequence(vec![(400, "Bad Request", b"{}".to_vec())]).await;
    assert!(veo(url).await.is_err());
    // The operation cannot be reached.
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", br#"{"name":"op"}"#.to_vec())]).await;
    assert!(veo(url).await.is_err());
    // The video is refused, unreachable, or cut short.
    let refused = leviath_testkit::spawn_mock_server(404, "Not Found", b"gone".to_vec()).await;
    let cut = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
    for uri in [refused.as_str(), "http://127.0.0.1:1/video", cut.as_str()] {
        let (url, _) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"name":"op"}"#.to_vec()),
            (200, "OK", done(uri)),
        ])
        .await;
        assert!(veo(url).await.is_err(), "{uri}");
    }
}

/// Lyria reports no token cost; the clip it makes is priced from the table.
#[tokio::test]
async fn a_clip_is_priced_by_the_clip() {
    let events = [
        serde_json::json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "text", "text": "[0.0:] la" } }),
        serde_json::json!({ "event_type": "step.delta", "index": 1, "delta": { "type": "audio", "mime_type": "audio/mpeg", "data": "SUQz" } }),
        serde_json::json!({ "event_type": "interaction.completed", "interaction": { "status": "completed", "usage": { "total_output_tokens": 500 } } }),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", events.into_bytes())]).await;
    let response = provider(&url)
        .infer(&request(
            "lyria-3-clip-preview",
            "a jingle",
            vec![],
            Value::Null,
        ))
        .await
        .expect("a clip");
    assert_eq!(response.parts[0].name.as_deref(), Some("audio-1.mp3"));
    let per_clip =
        crate::pricing::published_unit_rate("google", "lyria-3-clip-preview").map(|row| row.usd);
    assert!(per_clip.is_some(), "the shipped table prices Lyria");
    assert_eq!(response.tokens_used.reported_cost_usd, per_clip);
}
