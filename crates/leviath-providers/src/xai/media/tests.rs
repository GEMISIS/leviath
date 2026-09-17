//! xAI's media routes over local mocks.

use super::*;
use crate::pricing::PriceUnit;
use crate::provider::{ContentBlock, Message, MessageContent};
use crate::responses::client::Auth;
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
use leviath_testkit::{spawn_mock_recorder, spawn_mock_sequence, spawn_mock_server_with_headers};

fn endpoint(url: &str) -> Endpoint {
    Endpoint::new(reqwest::Client::new(), url, Auth::Key("xai-k".into()))
}

fn part(mime: &str, name: &str) -> ContentBlock {
    let blob = Blob::new(MimeType::parse(mime).unwrap(), vec![1, 2, 3]).named(name);
    let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named(name);
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
        max_tokens: 0,
        temperature: 0.0,
        tools: vec![],
        extra,
        request_timeout_secs: Some(5),
    }
}

fn billing(unit: PriceUnit, usd: f64) -> Billing {
    Billing {
        reported: true,
        unit: Some(UnitPrice { usd, unit }),
    }
}

const FAST: Duration = Duration::from_millis(1);

#[test]
fn every_named_model_has_its_kind() {
    assert_eq!(kind("grok-imagine-image-2.0"), Some(Kind::Image));
    assert_eq!(kind("grok-imagine-video-1.5"), Some(Kind::Video));
    assert_eq!(kind("grok-tts"), Some(Kind::Speech));
    assert_eq!(kind("grok-stt"), Some(Kind::Transcribe));
    assert_eq!(kind("grok-4.3"), None);
    assert!(CATALOG.iter().all(|(id, _)| kind(id).is_some()));
}

#[tokio::test]
async fn a_video_is_submitted_polled_downloaded_and_priced_by_the_second() {
    let video =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: video/mp4\r\n", b"MP4".to_vec())
            .await;
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"req-1"}"#.to_vec()),
        (200, "OK", br#"{"status":"pending"}"#.to_vec()),
        (
            200,
            "OK",
            serde_json::json!({ "status": "done", "video": { "url": video, "duration": 8 } })
                .to_string()
                .into_bytes(),
        ),
    ])
    .await;
    let response = run(
        &endpoint(&url),
        "xai",
        Kind::Video,
        &request(
            "grok-imagine-video",
            "a cat surfing",
            vec![part("image/png", "start.png")],
            serde_json::json!({ "duration": 8, "resolution": "720p" }),
        ),
        &billing(PriceUnit::VideoSecond, 0.05),
        FAST,
    )
    .await
    .expect("a video");
    assert_eq!(response.parts[0].bytes, b"MP4");
    assert_eq!(response.parts[0].name.as_deref(), Some("video.mp4"));
    assert!((response.tokens_used.reported_cost_usd.unwrap() - 0.4).abs() < 1e-9);
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["duration"], 8);
    assert_eq!(body["resolution"], "720p");
    assert!(
        body["image"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png")
    );
}

#[tokio::test]
async fn a_video_edit_or_extension_goes_to_its_own_route_and_a_url_may_sit_at_the_top() {
    let video = spawn_mock_server_with_headers(200, "OK", "", b"MP4".to_vec()).await;
    let done = serde_json::json!({ "status": "done", "url": video, "usage": { "cost_in_usd_ticks": 500000000 } });
    let (url, seen) = spawn_mock_recorder(200, "OK", br#"{"request_id":"r"}"#.to_vec()).await;
    let _ = run(
        &endpoint(&url),
        "xai",
        Kind::Video,
        &request(
            "grok-imagine-video",
            "longer",
            vec![part("video/mp4", "in.mp4")],
            serde_json::json!({ "operation": "extend" }),
        ),
        &billing(PriceUnit::VideoSecond, 0.05),
        FAST,
    )
    .await;
    assert!(
        seen.lock()
            .unwrap()
            .join("")
            .contains("POST /videos/extensions")
    );

    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (200, "OK", done.to_string().into_bytes()),
    ])
    .await;
    let response = run(
        &endpoint(&url),
        "grok",
        Kind::Video,
        &request(
            "grok-imagine-video",
            "restyle",
            vec![part("video/mp4", "in.mp4")],
            Value::Null,
        ),
        &billing(PriceUnit::VideoSecond, 0.05),
        FAST,
    )
    .await
    .unwrap();
    assert_eq!(
        response.parts[0].mime_type.as_str(),
        "video/mp4",
        "no type sent means mp4"
    );
    assert_eq!(
        response.tokens_used.reported_cost_usd,
        Some(0.05),
        "the ticks win"
    );
}

#[tokio::test]
async fn a_video_task_that_fails_or_answers_badly_says_so() {
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (
            200,
            "OK",
            br#"{"status":"failed","error":{"message":"moderated"}}"#.to_vec(),
        ),
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (
            200,
            "OK",
            br#"{"status":"failed","error":"quota"}"#.to_vec(),
        ),
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (200, "OK", br#"{"status":"failed"}"#.to_vec()),
        (200, "OK", br#"{"no":"id"}"#.to_vec()),
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (200, "OK", br#"{"status":"done"}"#.to_vec()),
    ])
    .await;
    let ep = endpoint(&url);
    let req = request("grok-imagine-video", "x", vec![], Value::Null);
    let b = billing(PriceUnit::VideoSecond, 0.05);
    let err = |r: Result<InferenceResponse>| r.unwrap_err().to_string();
    assert!(err(run(&ep, "xai", Kind::Video, &req, &b, FAST).await).contains("moderated"));
    assert!(err(run(&ep, "xai", Kind::Video, &req, &b, FAST).await).contains("quota"));
    assert!(err(run(&ep, "xai", Kind::Video, &req, &b, FAST).await).contains("no reason given"));
    assert!(err(run(&ep, "xai", Kind::Video, &req, &b, FAST).await).contains("request_id"));
    assert!(err(run(&ep, "xai", Kind::Video, &req, &b, FAST).await).contains("no URL"));
}

#[tokio::test]
async fn speech_is_the_audio_bytes_typed_by_the_reply_and_priced_by_the_character() {
    let (url, seen) = spawn_mock_recorder(200, "OK", b"ID3AUDIO".to_vec()).await;
    let response = run(
        &endpoint(&url),
        "xai",
        Kind::Speech,
        &request(
            "grok-tts",
            "hello there",
            vec![],
            serde_json::json!({ "voice_id": "eve", "speed": 1.2, "codec": "mp3", "sample_rate": 24000 }),
        ),
        &billing(PriceUnit::MillionChars, 15.0),
        FAST,
    )
    .await
    .unwrap();
    // The mock sends a JSON content type, which is not audio: the default holds.
    assert_eq!(response.parts[0].mime_type.as_str(), "audio/mpeg");
    assert_eq!(response.parts[0].name.as_deref(), Some("speech.mp3"));
    assert!((response.tokens_used.reported_cost_usd.unwrap() - 11.0 * 15.0 / 1e6).abs() < 1e-12);
    let raw = seen.lock().unwrap().join("");
    assert!(raw.contains("\"language\":\"auto\""), "{raw}");
    assert!(
        raw.contains("\"output_format\":{\"codec\":\"mp3\",\"sample_rate\":24000}"),
        "{raw}"
    );

    let wav =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: audio/wav\r\n", b"RIFF".to_vec())
            .await;
    let spoken = run(
        &endpoint(&wav),
        "grok",
        Kind::Speech,
        &request(
            "grok-tts",
            "hi",
            vec![],
            serde_json::json!({ "language": "en", "sample_rate": 8000 }),
        ),
        &Billing {
            reported: false,
            unit: None,
        },
        FAST,
    )
    .await
    .unwrap();
    assert_eq!(spoken.parts[0].name.as_deref(), Some("speech.wav"));
    assert_eq!(
        spoken.tokens_used.reported_cost_usd, None,
        "a subscription reports no cost"
    );

    let silent = run(
        &endpoint(&url),
        "xai",
        Kind::Speech,
        &request("grok-tts", "", vec![], Value::Null),
        &billing(PriceUnit::MillionChars, 15.0),
        FAST,
    )
    .await
    .unwrap_err();
    assert!(silent.to_string().contains("needs text"));
}

#[tokio::test]
async fn a_transcription_uploads_the_audio_and_keeps_the_word_timings() {
    let reply = serde_json::json!({ "text": "hello world", "duration": 7200.0, "words": [ { "text": "hello", "start": 0.0 } ] });
    let (url, seen) = spawn_mock_recorder(200, "OK", reply.to_string().into_bytes()).await;
    let response = run(
        &endpoint(&url),
        "xai",
        Kind::Transcribe,
        &request(
            "grok-stt",
            "",
            vec![part("audio/wav", "clip.wav")],
            serde_json::json!({ "language": "en", "diarization": true }),
        ),
        &billing(PriceUnit::AudioHour, 0.1),
        FAST,
    )
    .await
    .unwrap();
    assert_eq!(response.content, "hello world");
    assert_eq!(response.parts[0].name.as_deref(), Some("transcript.json"));
    assert!((response.tokens_used.reported_cost_usd.unwrap() - 0.2).abs() < 1e-9);
    let raw = seen.lock().unwrap().join("");
    assert!(raw.contains("multipart/form-data"), "{raw}");
    assert!(raw.contains("name=\"diarization\""), "{raw}");

    let nothing = run(
        &endpoint(&url),
        "xai",
        Kind::Transcribe,
        &request("grok-stt", "hi", vec![], Value::Null),
        &billing(PriceUnit::AudioHour, 0.1),
        FAST,
    )
    .await
    .unwrap_err();
    assert!(nothing.to_string().contains("needs an audio part"));
}

#[tokio::test]
async fn an_image_model_goes_through_the_shared_images_route() {
    let reply = serde_json::json!({ "data": [ { "b64_json": "SlBFRw==" } ] });
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", reply.to_string().into_bytes())]).await;
    let response = run(
        &endpoint(&url),
        "xai",
        Kind::Image,
        &request("grok-imagine-image", "a boat", vec![], Value::Null),
        &billing(PriceUnit::Image, 0.02),
        FAST,
    )
    .await
    .unwrap();
    assert_eq!(response.tokens_used.reported_cost_usd, Some(0.02));
}

#[tokio::test]
async fn every_way_a_video_speech_or_transcription_call_can_fail_is_an_error() {
    let b = Billing {
        reported: false,
        unit: None,
    };
    let video = request("grok-imagine-video", "x", vec![], Value::Null);
    let speech = request("grok-tts", "say it", vec![], Value::Null);
    let transcription = request(
        "grok-stt",
        "",
        vec![part("audio/wav", "a.wav")],
        Value::Null,
    );
    let nobody = endpoint("http://127.0.0.1:9");
    for (kind, req) in [
        (Kind::Video, &video),
        (Kind::Speech, &speech),
        (Kind::Transcribe, &transcription),
    ] {
        assert!(
            run(&nobody, "xai", kind, req, &b, FAST).await.is_err(),
            "{kind:?}"
        );
        let (refused, _) = spawn_mock_sequence(vec![(500, "Boom", b"{}".to_vec())]).await;
        assert!(
            run(&endpoint(&refused), "xai", kind, req, &b, FAST)
                .await
                .is_err(),
            "{kind:?}"
        );
    }
    for (kind, req) in [(Kind::Video, &video), (Kind::Transcribe, &transcription)] {
        let (garbled, _) = spawn_mock_sequence(vec![(200, "OK", b"not json".to_vec())]).await;
        assert!(
            run(&endpoint(&garbled), "xai", kind, req, &b, FAST)
                .await
                .is_err(),
            "{kind:?}"
        );
    }

    // A status check that cannot be read, a download that fails, and a video
    // whose type is no type.
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (500, "Boom", b"{}".to_vec()),
    ])
    .await;
    assert!(
        run(&endpoint(&url), "xai", Kind::Video, &video, &b, FAST)
            .await
            .is_err()
    );
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (
            200,
            "OK",
            br#"{"status":"done","url":"http://127.0.0.1:9/v.mp4"}"#.to_vec(),
        ),
    ])
    .await;
    assert!(
        run(&endpoint(&url), "xai", Kind::Video, &video, &b, FAST)
            .await
            .is_err()
    );
    let odd =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: video/\r\n", b"MP4".to_vec())
            .await;
    let done = serde_json::json!({ "status": "done", "url": odd });
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (200, "OK", done.to_string().into_bytes()),
    ])
    .await;
    assert!(
        run(&endpoint(&url), "xai", Kind::Video, &video, &b, FAST)
            .await
            .is_err()
    );

    // Speech whose body is cut short, and speech whose type is no type.
    let torn = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
    assert!(
        run(&endpoint(&torn), "xai", Kind::Speech, &speech, &b, FAST)
            .await
            .is_err()
    );
    let odd =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: audio/\r\n", b"ID3".to_vec())
            .await;
    assert!(
        run(&endpoint(&odd), "xai", Kind::Speech, &speech, &b, FAST)
            .await
            .is_err()
    );

    // A transcription of an unnamed clip is named for the route.
    let blob = Blob::new(MimeType::parse("audio/wav").unwrap(), vec![1, 2, 3]);
    let stored = Part::stored(blob.describe(&MimeRegistry::builtin()));
    let unnamed = request(
        "grok-stt",
        "",
        vec![ContentBlock::Mime {
            part: stored.blob().unwrap().clone(),
            data: "AQID".into(),
            name: None,
            deliver: None,
            remote: None,
        }],
        Value::Null,
    );
    let (url, seen) = spawn_mock_recorder(200, "OK", br#"{"text":"hi"}"#.to_vec()).await;
    run(&endpoint(&url), "xai", Kind::Transcribe, &unnamed, &b, FAST)
        .await
        .unwrap();
    assert!(seen.lock().unwrap().join("").contains("filename=\"audio\""));
}

#[tokio::test]
async fn a_failure_reason_that_is_an_object_is_quoted_and_speech_takes_a_codec_alone() {
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (
            200,
            "OK",
            br#"{"status":"failed","error":{"code":7}}"#.to_vec(),
        ),
    ])
    .await;
    let video = request("grok-imagine-video", "x", vec![], Value::Null);
    let b = Billing {
        reported: false,
        unit: None,
    };
    let err = run(&endpoint(&url), "xai", Kind::Video, &video, &b, FAST)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("\"code\":7"), "{err}");

    let (url, seen) = spawn_mock_recorder(200, "OK", b"ID3".to_vec()).await;
    let speech = request(
        "grok-tts",
        "hi",
        vec![],
        serde_json::json!({ "codec": "wav" }),
    );
    run(&endpoint(&url), "xai", Kind::Speech, &speech, &b, FAST)
        .await
        .unwrap();
    let raw = seen.lock().unwrap().join("");
    assert!(
        raw.contains("\"output_format\":{\"codec\":\"wav\"}"),
        "{raw}"
    );
}

#[tokio::test]
async fn a_video_past_its_deadline_and_audio_that_does_not_decode_are_errors() {
    let b = Billing {
        reported: false,
        unit: None,
    };
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"request_id":"r"}"#.to_vec()),
        (200, "OK", br#"{"status":"pending"}"#.to_vec()),
    ])
    .await;
    let mut video = request("grok-imagine-video", "x", vec![], Value::Null);
    video.request_timeout_secs = Some(0);
    let err = run(&endpoint(&url), "xai", Kind::Video, &video, &b, FAST)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("deadline"), "{err}");

    let blob = Blob::new(MimeType::parse("audio/wav").unwrap(), vec![1]);
    let stored = Part::stored(blob.describe(&MimeRegistry::builtin()));
    let torn = request(
        "grok-stt",
        "",
        vec![ContentBlock::Mime {
            part: stored.blob().unwrap().clone(),
            data: "not base64!".into(),
            name: None,
            deliver: None,
            remote: None,
        }],
        Value::Null,
    );
    let err = run(
        &endpoint("http://127.0.0.1:9"),
        "xai",
        Kind::Transcribe,
        &torn,
        &b,
        FAST,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("needs an audio part"), "{err}");
}
