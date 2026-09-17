//! OpenAI's media routes over local mocks, in the shapes measured live.

use super::*;
use crate::pricing::{PriceUnit, UnitPrice};
use crate::provider::{ContentBlock, Message, MessageContent};
use crate::responses::client::Auth;
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
use leviath_testkit::{spawn_mock_sequence, spawn_mock_server_with_headers};

fn endpoint(url: &str) -> Endpoint {
    Endpoint::new(reqwest::Client::new(), url, Auth::Key("sk-k".into()))
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

fn unit(usd: f64, unit: PriceUnit) -> Billing {
    Billing {
        unit: Some(UnitPrice { usd, unit }),
        tokens: None,
    }
}

const FAST: Duration = Duration::from_millis(1);

#[test]
fn every_named_model_has_its_kind_and_the_live_routes_have_none() {
    assert_eq!(kind("gpt-image-2"), Some(Kind::Image));
    assert_eq!(kind("chatgpt-image-latest"), Some(Kind::Image));
    assert_eq!(kind("sora-2-pro"), Some(Kind::Video));
    assert_eq!(kind("tts-1-hd"), Some(Kind::Speech));
    assert_eq!(kind("gpt-4o-mini-tts"), Some(Kind::Speech));
    assert_eq!(kind("whisper-1"), Some(Kind::Transcribe));
    assert_eq!(kind("gpt-4o-transcribe-diarize"), Some(Kind::Transcribe));
    assert_eq!(kind("gpt-realtime-whisper"), None, "a websocket model");
    assert_eq!(kind("gpt-live-transcribe"), None);
    assert_eq!(kind("gpt-5.5"), None);
    assert!(CATALOG.iter().all(|(id, _)| kind(id).is_some()));
}

#[tokio::test]
async fn an_image_is_made_with_no_response_format_and_priced_by_its_tokens() {
    let reply = serde_json::json!({
        "output_format": "png",
        "data": [{ "b64_json": "UE5H" }],
        "usage": { "input_tokens": 1000, "output_tokens": 1000 }
    });
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", reply.to_string().into_bytes())]).await;
    let billing = Billing {
        unit: None,
        tokens: Some(crate::ModelPricing::flat(5.0, 40.0)),
    };
    let response = run(
        &endpoint(&url),
        Kind::Image,
        &request(
            "gpt-image-1",
            "a lighthouse",
            vec![],
            serde_json::json!({ "quality": "low" }),
        ),
        &billing,
        FAST,
    )
    .await
    .expect("an image");
    assert_eq!(response.parts[0].name.as_deref(), Some("image-1.png"));
    assert!((response.tokens_used.reported_cost_usd.unwrap() - 0.045).abs() < 1e-9);
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert!(body.get("response_format").is_none());
    assert_eq!(body["quality"], "low");
}

#[tokio::test]
async fn a_video_is_created_polled_downloaded_deleted_and_priced_by_the_second() {
    let (url, bodies) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"id":"video_1","status":"queued"}"#.to_vec()),
        (
            200,
            "OK",
            br#"{"id":"video_1","status":"in_progress"}"#.to_vec(),
        ),
        (
            200,
            "OK",
            br#"{"id":"video_1","status":"completed","seconds":"4"}"#.to_vec(),
        ),
        (200, "OK", b"MP4".to_vec()),
        (200, "OK", br#"{"deleted":true}"#.to_vec()),
    ])
    .await;
    let response = run(
        &endpoint(&url),
        Kind::Video,
        &request(
            "sora-2",
            "a paper boat",
            vec![part("image/png", "start.png")],
            serde_json::json!({ "seconds": 4, "size": "1280x720" }),
        ),
        &unit(0.1, PriceUnit::VideoSecond),
        FAST,
    )
    .await
    .expect("a video");
    assert_eq!(response.parts[0].bytes, b"MP4");
    assert!((response.tokens_used.reported_cost_usd.unwrap() - 0.4).abs() < 1e-9);
    // The mock reads a request once, so a multipart body may arrive cut; the
    // count of requests is what it can say for certain.
    let seen = bodies.lock().unwrap();
    assert_eq!(seen.len(), 5, "created, polled twice, downloaded, deleted");
}

#[tokio::test]
async fn a_failed_video_says_why_and_a_job_with_no_id_is_an_error() {
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"id":"v"}"#.to_vec()),
        (
            200,
            "OK",
            br#"{"status":"failed","error":{"message":"moderation"}}"#.to_vec(),
        ),
    ])
    .await;
    let err = run(
        &endpoint(&url),
        Kind::Video,
        &request("sora-2", "x", vec![], Value::Null),
        &unit(0.1, PriceUnit::VideoSecond),
        FAST,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("moderation"), "{err}");

    let (url, _) = spawn_mock_sequence(vec![(200, "OK", b"{}".to_vec())]).await;
    let err = run(
        &endpoint(&url),
        Kind::Video,
        &request("sora-2", "x", vec![], Value::Null),
        &unit(0.1, PriceUnit::VideoSecond),
        FAST,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no id"), "{err}");
}

#[tokio::test]
async fn speech_is_the_reply_bytes_typed_by_the_header_and_priced_by_the_character() {
    let url =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: audio/wav\r\n", b"RIFF".to_vec())
            .await;
    let response = run(
        &endpoint(&url),
        Kind::Speech,
        &request(
            "tts-1",
            "hello",
            vec![],
            serde_json::json!({ "voice": "coral", "response_format": "wav", "speed": 1.2 }),
        ),
        &unit(15.0, PriceUnit::MillionChars),
        FAST,
    )
    .await
    .expect("speech");
    assert_eq!(response.parts[0].mime_type.as_str(), "audio/wav");
    assert_eq!(response.parts[0].name.as_deref(), Some("speech.wav"));
    assert!((response.tokens_used.reported_cost_usd.unwrap() - 15.0 * 5.0 / 1e6).abs() < 1e-12);

    let url = spawn_mock_server_with_headers(200, "OK", "", b"ID3".to_vec()).await;
    let untyped = run(
        &endpoint(&url),
        Kind::Speech,
        &request("gpt-4o-mini-tts", "hello", vec![], Value::Null),
        &Billing {
            unit: None,
            tokens: None,
        },
        FAST,
    )
    .await
    .expect("speech");
    assert_eq!(untyped.parts[0].mime_type.as_str(), "audio/mpeg");
    assert_eq!(untyped.tokens_used.reported_cost_usd, None);

    // gpt-4o-mini-tts is billed by the second of audio made, read from the
    // file: one second of 128 kbps MP3 at $0.90 an hour.
    let mut one_second = vec![0xFF, 0xF3, 0xC4, 0xC4];
    one_second.resize(16_000, 0);
    let url =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: audio/mpeg\r\n", one_second).await;
    let per_second = run(
        &endpoint(&url),
        Kind::Speech,
        &request("gpt-4o-mini-tts", "hello", vec![], Value::Null),
        &unit(0.9, PriceUnit::AudioHour),
        FAST,
    )
    .await
    .expect("speech");
    assert!((per_second.tokens_used.reported_cost_usd.unwrap() - 0.00025).abs() < 1e-12);
    let url =
        spawn_mock_server_with_headers(200, "OK", "Content-Type: audio/ogg\r\n", b"OggS".to_vec())
            .await;
    let unmeasured = run(
        &endpoint(&url),
        Kind::Speech,
        &request("gpt-4o-mini-tts", "hello", vec![], Value::Null),
        &unit(0.9, PriceUnit::AudioHour),
        FAST,
    )
    .await
    .expect("speech");
    assert_eq!(
        unmeasured.tokens_used.reported_cost_usd, None,
        "a length it cannot read"
    );

    let err = run(
        &endpoint("http://127.0.0.1:1"),
        Kind::Speech,
        &request("tts-1", "", vec![], Value::Null),
        &unit(15.0, PriceUnit::MillionChars),
        FAST,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("needs text"), "{err}");
}

#[tokio::test]
async fn a_transcript_is_priced_by_seconds_or_by_tokens_as_usage_says() {
    let whisper =
        serde_json::json!({ "text": " hello ", "usage": { "type": "duration", "seconds": 3600 } });
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", whisper.to_string().into_bytes())]).await;
    let response = run(
        &endpoint(&url),
        Kind::Transcribe,
        &request(
            "whisper-1",
            "",
            vec![part("audio/wav", "speech.wav")],
            serde_json::json!({ "language": "en", "prompt": "Leviath" }),
        ),
        &unit(0.36, PriceUnit::AudioHour),
        FAST,
    )
    .await
    .expect("a transcript");
    assert_eq!(response.content, "hello");
    assert_eq!(response.parts[0].name.as_deref(), Some("transcript.json"));
    assert!((response.tokens_used.reported_cost_usd.unwrap() - 0.36).abs() < 1e-9);
    let sent = bodies.lock().unwrap()[0].clone();
    assert!(
        sent.contains("verbose_json") && sent.contains("name=\"language\""),
        "{sent}"
    );

    let tokens = serde_json::json!({ "text": "hi", "usage": { "type": "tokens", "input_tokens": 1000000, "output_tokens": 0 } });
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", tokens.to_string().into_bytes())]).await;
    let diarized = run(
        &endpoint(&url),
        Kind::Transcribe,
        &request(
            "gpt-4o-transcribe-diarize",
            "",
            vec![part("audio/wav", "speech.wav")],
            Value::Null,
        ),
        &Billing {
            unit: None,
            tokens: Some(crate::ModelPricing::flat(2.5, 10.0)),
        },
        FAST,
    )
    .await
    .expect("a transcript");
    assert_eq!(diarized.tokens_used.reported_cost_usd, Some(2.5));
    let sent = bodies.lock().unwrap()[0].clone();
    assert!(
        sent.contains("diarized_json") && sent.contains("chunking_strategy"),
        "{sent}"
    );

    let plain = serde_json::json!({ "text": "hi" });
    let (url, bodies) =
        spawn_mock_sequence(vec![(200, "OK", plain.to_string().into_bytes())]).await;
    let unpriced = run(
        &endpoint(&url),
        Kind::Transcribe,
        &request(
            "gpt-4o-mini-transcribe",
            "",
            vec![part("audio/wav", "a.wav")],
            Value::Null,
        ),
        &Billing {
            unit: None,
            tokens: None,
        },
        FAST,
    )
    .await
    .expect("a transcript");
    assert_eq!(unpriced.tokens_used.reported_cost_usd, None);
    assert!(bodies.lock().unwrap()[0].contains("json"));

    let err = run(
        &endpoint("http://127.0.0.1:1"),
        Kind::Transcribe,
        &request("whisper-1", "words", vec![], Value::Null),
        &unit(0.36, PriceUnit::AudioHour),
        FAST,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("needs an audio part"), "{err}");
}

/// A stored part with no name, as an attachment handed on without one.
fn unnamed(mime: &str) -> ContentBlock {
    let blob = Blob::new(MimeType::parse(mime).unwrap(), vec![1, 2, 3]);
    let part = Part::stored(blob.describe(&MimeRegistry::builtin()));
    let mut block = ContentBlock::mime(&part).unwrap();
    if let ContentBlock::Mime { data, name, .. } = &mut block {
        *data = "AQID".into();
        *name = None;
    }
    block
}

/// Every step of a video job can fail, and each failure is the call's error.
#[tokio::test]
async fn a_video_job_that_fails_at_any_step_is_an_error() {
    let video = |url: &str| {
        let url = url.to_string();
        async move {
            run(
                &endpoint(&url),
                Kind::Video,
                &request("sora-2", "a boat", vec![unnamed("image/png")], Value::Null),
                &unit(0.1, PriceUnit::VideoSecond),
                FAST,
            )
            .await
        }
    };
    // The create is refused, or answers something that is not JSON.
    let (url, _) = spawn_mock_sequence(vec![(400, "Bad Request", b"{}".to_vec())]).await;
    assert!(video(&url).await.is_err());
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", b"not json".to_vec())]).await;
    assert!(video(&url).await.is_err());
    // The poll cannot be read.
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", br#"{"id":"v"}"#.to_vec())]).await;
    assert!(video(&url).await.is_err());
    // The file is refused, or cannot be fetched at all.
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"id":"v"}"#.to_vec()),
        (200, "OK", br#"{"status":"completed"}"#.to_vec()),
        (404, "Not Found", b"{}".to_vec()),
    ])
    .await;
    assert!(video(&url).await.is_err());
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"id":"v"}"#.to_vec()),
        (200, "OK", br#"{"status":"completed"}"#.to_vec()),
    ])
    .await;
    assert!(video(&url).await.is_err());
    // A finished job whose delete cannot be sent still hands its video back,
    // and a length given as a number is read as one.
    let (url, _) = spawn_mock_sequence(vec![
        (200, "OK", br#"{"id":"v"}"#.to_vec()),
        (200, "OK", br#"{"status":"completed","seconds":8}"#.to_vec()),
        (200, "OK", b"MP4".to_vec()),
    ])
    .await;
    let kept = video(&url).await.expect("the video is kept");
    assert!((kept.tokens_used.reported_cost_usd.unwrap() - 0.8).abs() < 1e-9);
}

#[tokio::test]
async fn speech_and_transcription_refusals_are_errors() {
    let (url, _) = spawn_mock_sequence(vec![(500, "Internal Server Error", b"{}".to_vec())]).await;
    assert!(
        run(
            &endpoint(&url),
            Kind::Speech,
            &request("tts-1", "hello", vec![], Value::Null),
            &unit(15.0, PriceUnit::MillionChars),
            FAST,
        )
        .await
        .is_err()
    );
    let cut = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
    assert!(
        run(
            &endpoint(&cut),
            Kind::Speech,
            &request("tts-1", "hello", vec![], Value::Null),
            &unit(15.0, PriceUnit::MillionChars),
            FAST,
        )
        .await
        .is_err(),
        "audio cut short is an error"
    );
    for answer in [
        (400, "Bad Request", b"{}".to_vec()),
        (200, "OK", b"not json".to_vec()),
    ] {
        let (url, bodies) = spawn_mock_sequence(vec![answer]).await;
        assert!(
            run(
                &endpoint(&url),
                Kind::Transcribe,
                &request("whisper-1", "", vec![unnamed("audio/wav")], Value::Null),
                &unit(0.36, PriceUnit::AudioHour),
                FAST,
            )
            .await
            .is_err()
        );
        assert!(bodies.lock().unwrap()[0].contains("filename=\"audio\""));
    }

    // Nothing listening: each route's request cannot be sent.
    for (kind, model, parts) in [
        (Kind::Video, "sora-2", vec![]),
        (Kind::Speech, "tts-1", vec![]),
        (Kind::Transcribe, "whisper-1", vec![unnamed("audio/wav")]),
    ] {
        assert!(
            run(
                &endpoint("http://127.0.0.1:1"),
                kind,
                &request(model, "words", parts, Value::Null),
                &unit(0.1, PriceUnit::VideoSecond),
                FAST,
            )
            .await
            .is_err(),
            "{model}"
        );
    }

    // A reply with no transcript is an empty one, not an error.
    let (url, _) = spawn_mock_sequence(vec![(200, "OK", b"{}".to_vec())]).await;
    let silent = run(
        &endpoint(&url),
        Kind::Transcribe,
        &request("whisper-1", "", vec![unnamed("audio/wav")], Value::Null),
        &unit(0.36, PriceUnit::AudioHour),
        FAST,
    )
    .await
    .expect("a transcript of nothing");
    assert_eq!(silent.content, "");
}
