//! Meta's media models: Muse Image and Muse Voice Transcribe.
//!
//! - `muse-image-1.0`: the shared images routes ([`crate::media::images`]),
//!   with Meta's `images: [{image_url}]` for an edit.
//! - `muse-voice-transcribe-1.0`: `POST /asr/transcribe` as a multipart body
//!   with two parts, `request` (JSON: `mode`, `model`, `audioEncoding`, and the
//!   optional `languageBias` and `keywords`) and `audio`. **WAV only**: mono,
//!   16-bit PCM, at 16 or 24 kHz, at most 32 MB and ten minutes. Leviath does
//!   not convert audio, so anything else is refused here with the format the
//!   route needs, rather than sent and refused by Meta with less to go on.

use serde_json::{Value, json};

use crate::media::{self, images};
use crate::pricing::UnitPrice;
use crate::provider::{InferenceRequest, InferenceResponse, ProviderError, Result};
use crate::responses::client::Endpoint;

/// What a media model does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Make or edit images.
    Image,
    /// Transcribe audio.
    Transcribe,
}

/// The media kind `model` is, or `None` for a chat model.
pub(crate) fn kind(model: &str) -> Option<Kind> {
    match model {
        m if m.starts_with("muse-image") => Some(Kind::Image),
        m if m.starts_with("muse-voice-transcribe") => Some(Kind::Transcribe),
        _ => None,
    }
}

/// The largest audio body the route takes.
const MAX_AUDIO_BYTES: usize = 32 * 1024 * 1024;

/// Run the media model `request` names, priced at `unit` when known.
pub(crate) async fn run(
    endpoint: &Endpoint,
    kind: Kind,
    request: &InferenceRequest,
    unit: Option<UnitPrice>,
) -> Result<InferenceResponse> {
    match kind {
        Kind::Image => {
            images::run(
                endpoint,
                &images::Route {
                    provider: super::PROVIDER_NAME,
                    shape: images::EditShape::Meta,
                    default_mime: "image/png",
                    hints: &["size", "aspect_ratio", "quality"],
                    reported_cost: false,
                    unit,
                    tokens: None,
                    response_format: true,
                },
                request,
            )
            .await
        }
        Kind::Transcribe => transcribe(endpoint, request, unit).await,
    }
}

/// What a WAV file's header says, when it is one the route takes.
#[derive(Debug, PartialEq)]
pub(crate) struct Wav {
    /// The sample rate, 16 000 or 24 000.
    pub(crate) sample_rate: u32,
    /// Seconds of audio.
    pub(crate) seconds: f64,
}

/// Read a WAV header, or say what about the file the route will refuse.
pub(crate) fn wav(bytes: &[u8]) -> std::result::Result<Wav, String> {
    const NEED: &str = "Muse Voice Transcribe takes WAV audio only: mono, 16-bit PCM, at \
                        16 or 24 kHz. Convert it first (for example `ffmpeg -i in.mp3 -ac 1 \
                        -ar 16000 -sample_fmt s16 out.wav`)";
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("the audio is not a WAV file. {NEED}"));
    }
    let le16 = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let le32 =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let mut at = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut data_len: Option<u32> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = le32(at + 4) as usize;
        let body = at + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            format = Some((le16(body), le16(body + 2), le32(body + 4), le16(body + 14)));
        } else if id == b"data" {
            data_len = Some(size as u32);
        }
        at = body + size + (size % 2);
    }
    let Some((encoding, channels, sample_rate, bits)) = format else {
        return Err(format!("the WAV file has no format chunk. {NEED}"));
    };
    if encoding != 1 || channels != 1 || bits != 16 || !matches!(sample_rate, 16_000 | 24_000) {
        return Err(format!(
            "the WAV file is {channels} channel(s), {bits}-bit, {sample_rate} Hz{}. {NEED}",
            if encoding == 1 { "" } else { ", not PCM" }
        ));
    }
    let seconds = f64::from(data_len.unwrap_or(0)) / f64::from(sample_rate * 2);
    Ok(Wav {
        sample_rate,
        seconds,
    })
}

/// Transcribe the request's audio.
async fn transcribe(
    endpoint: &Endpoint,
    request: &InferenceRequest,
    unit: Option<UnitPrice>,
) -> Result<InferenceResponse> {
    let audio = media::first_part(request, |m| m.starts_with("audio/")).ok_or_else(|| {
        ProviderError::InvalidResponse(
            "muse-voice-transcribe needs an audio part: the stage handed it none".into(),
        )
    })?;
    if audio.bytes.len() > MAX_AUDIO_BYTES {
        return Err(ProviderError::InvalidResponse(format!(
            "the audio is {} bytes; Muse Voice Transcribe takes at most {MAX_AUDIO_BYTES}",
            audio.bytes.len()
        )));
    }
    let header = wav(&audio.bytes).map_err(ProviderError::InvalidResponse)?;

    let mut settings = serde_json::Map::new();
    settings.insert("model".into(), json!(request.model));
    settings.insert(
        "mode".into(),
        json!(media::extra_str(request, "mode").unwrap_or_else(|| "PUSH_TO_TALK".to_string())),
    );
    settings.insert("audioEncoding".into(), json!("WAV"));
    for (key, field) in [("language_bias", "languageBias"), ("keywords", "keywords")] {
        if let Some(list) = request.extra.get(key).and_then(Value::as_array) {
            settings.insert(field.into(), json!(list));
        }
    }
    let settings = Value::Object(settings).to_string();
    let url = endpoint.url("/asr/transcribe");
    let name = audio
        .name
        .clone()
        .unwrap_or_else(|| "audio.wav".to_string());
    let response = endpoint
        .send(|client| {
            let request_part = reqwest::multipart::Part::text(settings.clone())
                .mime_str("application/json")
                .expect("application/json is a valid mime type");
            let audio_part = reqwest::multipart::Part::bytes(audio.bytes.clone())
                .file_name(name.clone())
                .mime_str("audio/wav")
                .expect("audio/wav is a valid mime type");
            let form = reqwest::multipart::Form::new()
                .part("request", request_part)
                .part("audio", audio_part);
            client.post(&url).multipart(form)
        })
        .await?;
    let response =
        crate::provider::check_http_response(response, endpoint.rate_limiter.as_ref()).await?;
    let reply: Value = crate::provider::decode_json(response).await?;
    let text = reply
        .get("transcript")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let seconds = reply
        .get("audioDurationMs")
        .and_then(Value::as_f64)
        .map_or(header.seconds, |ms| ms / 1000.0);
    let transcript = serde_json::to_vec_pretty(&reply).expect("a JSON value serialises");
    let parts = vec![media::json_blob(transcript, "transcript.json")];
    let cost = unit.map(|u| u.cost(seconds / 3600.0));
    Ok(media::response(text, parts, cost))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::PriceUnit;
    use crate::provider::{ContentBlock, Message, MessageContent};
    use crate::responses::client::Auth;
    use base64::Engine as _;
    use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
    use leviath_testkit::{spawn_mock_recorder, spawn_mock_sequence};

    /// A WAV header and `samples` of silence.
    fn wav_bytes(channels: u16, rate: u32, bits: u16, encoding: u16, samples: u32) -> Vec<u8> {
        let data_len = samples * u32::from(bits / 8) * u32::from(channels);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(b"abc\0");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&encoding.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(bits / 8) * u32::from(channels)).to_le_bytes());
        out.extend_from_slice(&((bits / 8) * channels).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend(std::iter::repeat_n(0u8, data_len as usize));
        out
    }

    fn request(model: &str, audio: Option<Vec<u8>>) -> InferenceRequest {
        let mut blocks = vec![ContentBlock::Text {
            text: "a cat".into(),
        }];
        if let Some(bytes) = audio {
            let blob =
                Blob::new(MimeType::parse("audio/wav").unwrap(), bytes.clone()).named("clip.wav");
            let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named("clip.wav");
            blocks.push(ContentBlock::Mime {
                part: part.blob().unwrap().clone(),
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                name: part.name.clone(),
                deliver: None,
                remote: None,
            });
        }
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
            extra: json!({ "keywords": ["Leviath"], "language_bias": ["English"] }),
            request_timeout_secs: None,
        }
    }

    fn endpoint(url: &str) -> Endpoint {
        Endpoint::new(reqwest::Client::new(), url, Auth::Key("k".into()))
    }

    fn hourly() -> Option<UnitPrice> {
        Some(UnitPrice {
            usd: 0.18,
            unit: PriceUnit::AudioHour,
        })
    }

    #[test]
    fn kinds_follow_the_names() {
        assert_eq!(kind("muse-image-1.0"), Some(Kind::Image));
        assert_eq!(kind("muse-voice-transcribe-1.0"), Some(Kind::Transcribe));
        assert_eq!(kind("muse-spark-1.3"), None);
    }

    #[test]
    fn only_mono_sixteen_bit_pcm_at_sixteen_or_twenty_four_khz_is_taken() {
        let ok = wav(&wav_bytes(1, 16_000, 16, 1, 16_000)).unwrap();
        assert_eq!(ok.sample_rate, 16_000);
        assert!((ok.seconds - 1.0).abs() < 1e-9);
        assert!(wav(&wav_bytes(1, 24_000, 16, 1, 10)).is_ok());
        assert!(
            wav(&wav_bytes(2, 16_000, 16, 1, 10))
                .unwrap_err()
                .contains("2 channel")
        );
        assert!(
            wav(&wav_bytes(1, 44_100, 16, 1, 10))
                .unwrap_err()
                .contains("44100 Hz")
        );
        assert!(
            wav(&wav_bytes(1, 16_000, 24, 1, 10))
                .unwrap_err()
                .contains("24-bit")
        );
        assert!(
            wav(&wav_bytes(1, 16_000, 16, 3, 10))
                .unwrap_err()
                .contains("not PCM")
        );
        assert!(wav(b"ID3 not a wav").unwrap_err().contains("not a WAV"));
        let mut no_fmt = b"RIFF\0\0\0\0WAVE".to_vec();
        no_fmt.extend_from_slice(b"data\0\0\0\0");
        assert!(wav(&no_fmt).unwrap_err().contains("no format chunk"));
    }

    #[tokio::test]
    async fn a_transcription_sends_both_parts_and_prices_the_hour() {
        let reply = json!({ "transcript": "hello", "audioDurationMs": 1_800_000, "turns": [] });
        let (url, seen) = spawn_mock_recorder(200, "OK", reply.to_string().into_bytes()).await;
        let response = run(
            &endpoint(&url),
            Kind::Transcribe,
            &request(
                "muse-voice-transcribe-1.0",
                Some(wav_bytes(1, 16_000, 16, 1, 1_600)),
            ),
            hourly(),
        )
        .await
        .unwrap();
        assert_eq!(response.content, "hello");
        assert!((response.tokens_used.reported_cost_usd.unwrap() - 0.09).abs() < 1e-9);
        let raw = seen.lock().unwrap().join("");
        assert!(raw.contains("POST /asr/transcribe"), "{raw}");
        assert!(raw.contains("name=\"request\""), "{raw}");
        assert!(raw.contains("\"audioEncoding\":\"WAV\""), "{raw}");
        assert!(raw.contains("\"keywords\":[\"Leviath\"]"), "{raw}");
        assert!(raw.contains("\"languageBias\":[\"English\"]"), "{raw}");
        // With no duration in the reply, the header's answers.
        let (url, _) =
            spawn_mock_sequence(vec![(200, "OK", br#"{"transcript":"x"}"#.to_vec())]).await;
        let priced = run(
            &endpoint(&url),
            Kind::Transcribe,
            &request(
                "muse-voice-transcribe-1.0",
                Some(wav_bytes(1, 16_000, 16, 1, 57_600)),
            ),
            hourly(),
        )
        .await
        .unwrap();
        assert!(priced.tokens_used.reported_cost_usd.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn audio_the_route_refuses_is_refused_before_sending() {
        let ep = endpoint("http://127.0.0.1:1");
        let none = run(
            &ep,
            Kind::Transcribe,
            &request("muse-voice-transcribe-1.0", None),
            hourly(),
        )
        .await
        .unwrap_err();
        assert!(none.to_string().contains("needs an audio part"));
        let mp3 = run(
            &ep,
            Kind::Transcribe,
            &request("muse-voice-transcribe-1.0", Some(b"ID3 mp3".to_vec())),
            hourly(),
        )
        .await
        .unwrap_err();
        assert!(mp3.to_string().contains("WAV audio only"));
        let huge = vec![0u8; MAX_AUDIO_BYTES + 1];
        let big = run(
            &ep,
            Kind::Transcribe,
            &request("muse-voice-transcribe-1.0", Some(huge)),
            hourly(),
        )
        .await
        .unwrap_err();
        assert!(big.to_string().contains("at most"));
    }

    #[tokio::test]
    async fn an_image_uses_metas_edit_shape_and_unit_price() {
        let reply = json!({ "data": [ { "b64_json": "UE5H" } ] });
        let (url, _) = spawn_mock_sequence(vec![(200, "OK", reply.to_string().into_bytes())]).await;
        let response = run(
            &endpoint(&url),
            Kind::Image,
            &request("muse-image-1.0", None),
            Some(UnitPrice {
                usd: 0.01,
                unit: PriceUnit::Image,
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.parts[0].mime_type.as_str(), "image/png");
        assert_eq!(response.tokens_used.reported_cost_usd, Some(0.01));
    }

    #[tokio::test]
    async fn a_transcription_that_cannot_be_read_is_an_error_and_an_unnamed_clip_is_named() {
        let clip = wav_bytes(1, 16_000, 16, 1, 16);
        let mut req = request("muse-voice-transcribe-1.0", None);
        req.extra = json!({ "language_bias": "English" });
        let blob = Blob::new(MimeType::parse("audio/wav").unwrap(), clip.clone());
        let part = Part::stored(blob.describe(&MimeRegistry::builtin()));
        req.messages[0].content = MessageContent::Blocks(vec![ContentBlock::Mime {
            part: part.blob().unwrap().clone(),
            data: base64::engine::general_purpose::STANDARD.encode(&clip),
            name: None,
            deliver: None,
            remote: None,
        }]);
        let (url, seen) = spawn_mock_recorder(200, "OK", br#"{"transcript":"ok"}"#.to_vec()).await;
        run(&endpoint(&url), Kind::Transcribe, &req, None)
            .await
            .unwrap();
        let raw = seen.lock().unwrap().join("");
        assert!(raw.contains("filename=\"audio.wav\""), "{raw}");
        assert!(
            !raw.contains("languageBias"),
            "a bias that is not a list is left out: {raw}"
        );

        assert!(
            run(
                &endpoint("http://127.0.0.1:9"),
                Kind::Transcribe,
                &req,
                None
            )
            .await
            .is_err()
        );
        let (refused, _) = spawn_mock_sequence(vec![(500, "Boom", b"{}".to_vec())]).await;
        assert!(
            run(&endpoint(&refused), Kind::Transcribe, &req, None)
                .await
                .is_err()
        );
        let (garbled, _) = spawn_mock_sequence(vec![(200, "OK", b"not json".to_vec())]).await;
        assert!(
            run(&endpoint(&garbled), Kind::Transcribe, &req, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn audio_that_does_not_decode_is_no_audio() {
        let mut req = request("muse-voice-transcribe-1.0", None);
        let blob = Blob::new(MimeType::parse("audio/wav").unwrap(), vec![1]);
        let part = Part::stored(blob.describe(&MimeRegistry::builtin()));
        req.messages[0].content = MessageContent::Blocks(vec![ContentBlock::Mime {
            part: part.blob().unwrap().clone(),
            data: "not base64!".into(),
            name: None,
            deliver: None,
            remote: None,
        }]);
        assert!(
            run(
                &endpoint("http://127.0.0.1:9"),
                Kind::Transcribe,
                &req,
                None
            )
            .await
            .is_err()
        );
    }
}
