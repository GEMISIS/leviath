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
    assert_eq!(kind("gemini-omni-1.2-flash"), Some(Kind::Omni));
    assert_eq!(kind("gemini-3.5-flash"), None);
    assert!(CATALOG.iter().all(|(id, _)| kind(id).is_some()));
    assert!(!adjusted("gemini-omni-1.2-flash", crate::ModelCapabilities::default()).supports_tools);

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

/// A length written `"4s"` is four seconds, not "unset": Veo answered it
/// with its eight-second default, twice the clip and twice the bill.
/// Resolutions and aspect ratios are sent in Google's spelling.
#[tokio::test]
async fn a_veo_length_written_with_its_unit_is_sent_as_seconds() {
    for (extra, seconds) in [
        (
            serde_json::json!({ "duration": "4s", "resolution": "1080P" }),
            4,
        ),
        (serde_json::json!({ "duration_seconds": " 6 " }), 6),
        (
            serde_json::json!({ "duration": 8.0, "aspect_ratio": "9:16" }),
            8,
        ),
    ] {
        let video = spawn_mock_server_with_headers(
            200,
            "OK",
            "Content-Type: video/mp4\r\n",
            b"MP4".to_vec(),
        )
        .await;
        let done = serde_json::json!({
            "done": true,
            "response": { "generateVideoResponse": { "generatedSamples": [{ "video": { "uri": video } }] } }
        });
        let (url, bodies) = spawn_mock_sequence(vec![
            (200, "OK", br#"{"name":"op"}"#.to_vec()),
            (200, "OK", done.to_string().into_bytes()),
        ])
        .await;
        provider(&url)
            .infer(&request(
                "veo-3.1-generate-preview",
                "a paper boat",
                vec![],
                extra.clone(),
            ))
            .await
            .expect("a video");
        let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(body["parameters"]["durationSeconds"], seconds, "{extra}");
        if extra.get("resolution").is_some() {
            assert_eq!(body["parameters"]["resolution"], "1080p");
        }
        if extra.get("aspect_ratio").is_some() {
            assert_eq!(body["parameters"]["aspectRatio"], "9:16");
        }
    }
}

/// A paid parameter that does not read is refused before anything is sent,
/// never dropped for Veo to answer at its default.
#[tokio::test]
async fn a_veo_parameter_that_does_not_read_is_refused_before_sending() {
    let nothing_listens = provider("http://127.0.0.1:9");
    for (extra, named) in [
        (serde_json::json!({ "duration": "4.5s" }), "`duration`"),
        (serde_json::json!({ "duration": 4.5 }), "`duration`"),
        (serde_json::json!({ "duration": "four" }), "`duration`"),
        (serde_json::json!({ "duration": 0 }), "`duration`"),
        (
            serde_json::json!({ "duration_seconds": true }),
            "`duration_seconds`",
        ),
        (
            serde_json::json!({ "resolution": "lowest" }),
            "`resolution`",
        ),
        (serde_json::json!({ "resolution": 720 }), "`resolution`"),
        (
            serde_json::json!({ "aspect_ratio": "1:1" }),
            "`aspect_ratio`",
        ),
    ] {
        let err = nothing_listens
            .infer(&request(
                "veo-3.1-generate-preview",
                "a paper boat",
                vec![],
                extra.clone(),
            ))
            .await
            .unwrap_err();
        let said = err.to_string();
        assert!(said.contains(named), "{extra}: {said}");
        assert!(said.contains("must be"), "{extra}: {said}");
    }
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

/// A call made again after a restart polls the operation it started before
/// and starts nothing.
#[tokio::test]
async fn a_veo_call_made_again_polls_the_operation_it_started() {
    let video =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: video/mp4\r\n", b"MP4".to_vec())
            .await;
    let done = serde_json::json!({
        "done": true,
        "response": { "generateVideoResponse": { "generatedSamples": [{ "video": { "uri": video } }] } }
    });
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", done.to_string().into_bytes())]).await;
    let log = crate::jobs::JobLog::new(
        [(
            "google/veo-3.1-lite-generate-preview/video".to_string(),
            "models/veo/operations/op1".to_string(),
        )]
        .into(),
    );
    let response = log
        .scope(provider(&url).infer(&request(
            "veo-3.1-lite-generate-preview",
            "a paper boat",
            vec![],
            Value::Null,
        )))
        .await
        .expect("the video it paid for");
    assert_eq!(response.parts[0].bytes, b"MP4");
    assert_eq!(bodies.lock().unwrap().len(), 1, "one poll, no start");
}

const OMNI: &str = "gemini-omni-1.2-flash";

/// The answer an omni model gives, as measured: a signed thought, then the
/// clip inline in a `model_output` step, and usage by the token.
fn omni_reply(content: Value) -> Vec<u8> {
    serde_json::json!({
        "status": "completed",
        "usage": {
            "total_input_tokens": 12,
            "total_cached_tokens": 2,
            "total_output_tokens": 5793,
            "total_thought_tokens": 589
        },
        "steps": [
            { "type": "thought", "signature": "sig" },
            { "type": "model_output", "content": content },
            { "type": "model_output" }
        ]
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn an_omni_model_is_asked_for_a_video_with_no_tools_and_answers_inline() {
    let (url, bodies) = spawn_mock_sequence(vec![(
        200,
        "OK",
        omni_reply(serde_json::json!([
            { "type": "video", "mime_type": "video/mp4", "data": "TVA0" }
        ])),
    )])
    .await;
    let mut asked = request(
        OMNI,
        "a red ball",
        vec![image_part()],
        serde_json::json!({
            "duration": "3s", "aspect_ratio": "9:16", "resolution": "4K", "seed": 7
        }),
    );
    // What the stage carries is not what the model is sent: it refuses any
    // function declaration, and reads no system instruction.
    asked.tools = vec![leviath_core::Tool {
        name: "submit_output".into(),
        description: String::new(),
        parameters: serde_json::json!({}),
    }];
    let response = provider(&url).infer(&asked).await.expect("a video");
    assert_eq!(response.parts[0].bytes, b"MP4");
    assert_eq!(response.parts[0].name.as_deref(), Some("video-1.mp4"));
    assert!(
        response.content.contains("video-1.mp4"),
        "{}",
        response.content
    );
    assert_eq!(response.tokens_used.prompt_tokens, 10);
    assert_eq!(response.tokens_used.cached_tokens, 2);
    assert_eq!(response.tokens_used.completion_tokens, 5793 + 589);
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("system_instruction").is_none(), "{body}");
    assert!(body.get("stream").is_none(), "{body}");
    assert_eq!(body["store"], false);
    assert_eq!(
        body["response_format"],
        serde_json::json!({
            "type": "video", "duration": "3s", "aspect_ratio": "9:16", "resolution": "4k"
        })
    );
    assert_eq!(body["generation_config"]["seed"], 7);
    let content = &body["input"][0]["content"];
    assert_eq!(content[0]["text"], "a red ball");
    assert_eq!(content[1]["type"], "image");

    let bare = omni_body(&request(OMNI, "a red ball", vec![], Value::Null)).unwrap();
    assert_eq!(
        bare["response_format"],
        serde_json::json!({ "type": "video" })
    );
    assert!(bare.get("generation_config").is_none());
}

/// A large clip arrives as a `uri` fetched with the key, an item may name no
/// type, and the streaming path hands both on in its one chunk.
#[tokio::test]
async fn an_omni_clip_behind_a_uri_is_downloaded() {
    use tokio_stream::StreamExt as _;
    let video =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: video/mp4\r\n", b"BIG".to_vec())
            .await;
    let (url, _) = spawn_mock_sequence(vec![(
        200,
        "OK",
        omni_reply(serde_json::json!([
            { "type": "text", "text": "" },
            { "type": "video", "uri": video },
            { "type": "video", "mime_type": "video/webm", "data": "V0VC" },
            { "type": "video" }
        ])),
    )])
    .await;
    let mut stream = provider(&url)
        .infer_stream(&request(OMNI, "a red ball", vec![], Value::Null))
        .await
        .expect("a stream");
    let chunk = stream.next().await.expect("one chunk").expect("the clips");
    assert_eq!(chunk.parts.len(), 2);
    assert_eq!(chunk.parts[0].bytes, b"BIG");
    assert_eq!(chunk.parts[0].name.as_deref(), Some("video-1.mp4"));
    assert_eq!(chunk.parts[1].mime_type.as_str(), "video/webm");
    assert_eq!(chunk.parts[1].name.as_deref(), Some("video-2.bin"));
}

/// A reply with no media is the model's words, for the stage to judge.
#[tokio::test]
async fn an_omni_reply_without_media_is_its_text() {
    let (url, _) = spawn_mock_sequence(vec![(
        200,
        "OK",
        omni_reply(serde_json::json!([{ "type": "text", "text": "I cannot make that." }])),
    )])
    .await;
    let response = provider(&url)
        .infer(&request(OMNI, "a red ball", vec![], Value::Null))
        .await
        .expect("a reply");
    assert!(response.parts.is_empty());
    assert_eq!(response.content, "I cannot make that.");
}

#[tokio::test]
async fn omni_failures_and_bad_answers_are_errors() {
    let run = |status: u16, body: Vec<u8>| async move {
        let (url, _) = spawn_mock_sequence(vec![(status, "X", body)]).await;
        provider(&url)
            .infer(&request(OMNI, "a red ball", vec![], Value::Null))
            .await
            .unwrap_err()
            .to_string()
    };
    let said = run(
        200,
        br#"{"status":"failed","error":{"message":"quota"}}"#.to_vec(),
    )
    .await;
    assert!(
        said.contains("ended failed") && said.contains("quota"),
        "{said}"
    );
    let said = run(200, br#"{"status":"cancelled"}"#.to_vec()).await;
    assert!(said.contains("ended cancelled"), "{said}");
    let said = run(200, b"not json".to_vec()).await;
    assert!(said.contains("unreadable omni reply"), "{said}");
    let said = run(
        200,
        omni_reply(serde_json::json!([{ "type": "video", "data": "!!" }])),
    )
    .await;
    assert!(said.contains("undecodable omni media"), "{said}");
    run(400, b"{}".to_vec()).await;
    // The clip's uri cannot be fetched.
    let said = run(
        200,
        omni_reply(serde_json::json!([{ "type": "video", "uri": "http://127.0.0.1:1/v" }])),
    )
    .await;
    assert!(said.contains("downloading"), "{said}");

    // Refused before anything is sent: no prompt, and a length that does
    // not read.
    let nothing_listens = provider("http://127.0.0.1:9");
    let err = nothing_listens
        .infer(&request(OMNI, "", vec![], Value::Null))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("needs a prompt"), "{err}");
    let err = nothing_listens
        .infer(&request(
            OMNI,
            "a red ball",
            vec![],
            serde_json::json!({ "duration": "long" }),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`duration`"), "{err}");
    let err = nothing_listens
        .infer(&request(
            OMNI,
            "a red ball",
            vec![],
            serde_json::json!({ "resolution": "8k" }),
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`resolution`"), "{err}");
}

/// A media model is one by what it makes, whatever it is called: the table's
/// row for `gemini-nano-banana-2.1` says images, and a transcription model
/// answers in text from audio. The live models speak a socket.
#[test]
fn a_media_model_is_known_by_what_it_makes() {
    assert_eq!(kind("gemini-nano-banana-2.1"), Some(Kind::Prompted));
    assert_eq!(kind("gemini-3.1-flash-lite-image"), Some(Kind::Prompted));
    assert_eq!(kind("gemini-3.8-flash-lite-tts"), Some(Kind::Prompted));
    assert_eq!(kind("gemini-3.5-transcribe"), Some(Kind::Transcribe));
    assert_eq!(kind("gemini-omni-flash-preview"), Some(Kind::Omni));
    for socket in [
        "gemini-3.5-transcribe-live",
        "gemini-3.8-live",
        "lyria-realtime-exp",
    ] {
        assert_eq!(kind(socket), None, "{socket}");
    }
    for chat in ["gemini-3.8-flash", "gemini-4-carbon", "gemma-4-31b-it"] {
        assert_eq!(kind(chat), None, "{chat}");
    }

    // A transcript is tokens, so its reply budget is the model's own.
    let listed = crate::ModelCapabilities {
        max_context_tokens: 131_072,
        max_output_tokens: 65_536,
        ..crate::ModelCapabilities::default()
    };
    let transcribe = adjusted("gemini-3.5-transcribe", listed.clone());
    assert!(!transcribe.supports_tools);
    assert_eq!(transcribe.max_output_tokens, 65_536);
    assert_eq!(
        adjusted("gemini-nano-banana-2.1", listed).max_output_tokens,
        4_096
    );
    let google = provider("http://127.0.0.1:1");
    let mime = google.mime("gemini-3.5-transcribe");
    assert!(mime.input.iter().any(|t| t == "audio/*"), "{mime:?}");
    assert_eq!(mime.output, ["text/*"]);
}

/// A transcription model is sent the audio and the prompt alone: it refuses
/// a system instruction and a function declaration alike. With no words in
/// the stage, the audio goes by itself.
#[tokio::test]
async fn a_transcription_model_is_sent_its_audio_and_no_tools() {
    let audio = || {
        let blob = Blob::new(MimeType::parse("audio/wav").unwrap(), vec![1, 2, 3]).named("a.wav");
        let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named("a.wav");
        let mut block = ContentBlock::mime(&part).unwrap();
        if let ContentBlock::Mime { data, .. } = &mut block {
            *data = "AQID".into();
        }
        block
    };
    let events = [
        serde_json::json!({ "event_type": "step.start", "index": 0, "step": { "type": "model_output" } }),
        serde_json::json!({ "event_type": "step.delta", "index": 0, "delta": { "type": "text", "text": "The quick brown fox." } }),
        serde_json::json!({ "event_type": "interaction.completed", "interaction": { "status": "completed" } }),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    let (url, bodies) = spawn_mock_sequence(vec![(200, "OK", events.into_bytes())]).await;
    let mut asked = request("gemini-3.5-transcribe", "", vec![audio()], Value::Null);
    asked.system = vec![crate::provider::SystemBlock {
        text: "Transcribe.".into(),
        cache_hint: leviath_core::CacheHint::UntilChanged,
        volatility: leviath_core::Volatility::Stable,
        region: String::new(),
    }];
    asked.tools = vec![leviath_core::Tool {
        name: "submit_output".into(),
        description: String::new(),
        parameters: serde_json::json!({}),
    }];
    let response = provider(&url).infer(&asked).await.expect("a transcript");
    assert_eq!(response.content, "The quick brown fox.");
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("system_instruction").is_none(), "{body}");
    let content = body["input"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1, "the audio alone: {body}");
    assert_eq!(content[0]["type"], "audio");
}
