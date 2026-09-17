//! What every blob-in, blob-out provider shares: reading a request's prompt
//! and parts, polling a long task, downloading a result, and handing parts
//! back through both the buffered and the streaming path.
//!
//! Meshy (3D), xAI's Imagine and speech routes, and Meta's Muse Image and
//! Voice Transcribe all run the same shape: take the text and the media parts a
//! stage assembled, call an endpoint that is not chat, and answer with files.
//! The runtime stores what comes back in the run's blob store and routes it by
//! the stage's `output_routing`, exactly as for Meshy.

pub(crate) mod images;

use std::time::{Duration, Instant};

use base64::Engine as _;
use leviath_core::mime::{Blob, MimeType};
use serde_json::Value;

use crate::provider::{
    ContentBlock, FinishReason, InferenceRequest, InferenceResponse, MessageContent, ProviderError,
    Result, StreamChunk, TokenUsage,
};

/// The largest body a media download reads: a 15-second 1080p video or a long
/// speech track can pass the 64 MiB a JSON reply is capped at. What is kept is
/// still bounded by `[mime] max_part_bytes` when the runtime stores it.
pub(crate) const DOWNLOAD_CAP: usize = 512 * 1024 * 1024;

/// How long a media task may run when the stage names no timeout.
pub(crate) const DEFAULT_TASK_SECS: u64 = 900;

/// Every hydrated mime block whose type passes `want`, as a `data:` URI.
pub(crate) fn data_uris(request: &InferenceRequest, want: impl Fn(&str) -> bool) -> Vec<String> {
    parts(request, want)
        .map(|(part, data, _)| crate::mime::data_uri(&part.mime_type, data))
        .collect()
}

/// Every hydrated mime block whose type passes `want`: the part, its base64
/// bytes, and its name.
fn parts<'a>(
    request: &'a InferenceRequest,
    want: impl Fn(&str) -> bool + 'a,
) -> impl Iterator<Item = (&'a leviath_core::mime::BlobRef, &'a str, Option<&'a str>)> + 'a {
    request
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(move |block| match block {
            ContentBlock::Mime {
                part, data, name, ..
            } if !data.is_empty() && want(part.mime_type.as_str()) => {
                Some((part, data.as_str(), name.as_deref()))
            }
            _ => None,
        })
}

/// A decoded media part: its type, bytes and name.
pub(crate) struct Part {
    /// The part's mime type.
    pub(crate) mime_type: MimeType,
    /// Its bytes.
    pub(crate) bytes: Vec<u8>,
    /// Its name, when it has one.
    pub(crate) name: Option<String>,
}

/// The first hydrated part whose type passes `want`, decoded. An upload
/// endpoint takes bytes, not a data URI.
pub(crate) fn first_part(request: &InferenceRequest, want: impl Fn(&str) -> bool) -> Option<Part> {
    let (part, data, name) = parts(request, want).next()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    Some(Part {
        mime_type: part.mime_type.clone(),
        bytes,
        name: name.map(str::to_string),
    })
}

/// The plain text of a request, across its text blocks and plain messages.
///
/// The texture prompt an upstream stage wrote lands here, as the text of the
/// region it wrote it to; it is the dynamic hint the operation textures with.
pub(crate) fn request_text(request: &InferenceRequest) -> String {
    // Assembly emits a stored part as a pointer text block ("[region] [mime,
    // size] name") beside its bytes block. That pointer is not the user's
    // prompt, so exclude any text carrying a stored part's stand-in, or the
    // texture prompt and the animation action would be the mesh's file line
    // rather than "weathered bronze" or "walk".
    let stand_ins: Vec<&str> = request
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::Mime { part, .. } => Some(part.stand_in.as_str()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect();
    // The runtime's placeholder turn is not a prompt either: a stage whose
    // prompt lives in a pinned region has no user turn but this one, and
    // reading it sent the model "Begin." while the task sat unread below.
    let is_pointer = |text: &str| {
        text.trim() == crate::provider::OPENING_TURN || stand_ins.iter().any(|s| text.contains(s))
    };

    let mut chunks = Vec::new();
    for message in &request.messages {
        match &message.content {
            MessageContent::Text(text) => {
                if !is_pointer(text) {
                    chunks.push(text.clone());
                }
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if let ContentBlock::Text { text } = block
                        && !is_pointer(text)
                    {
                        chunks.push(text.clone());
                    }
                }
            }
        }
    }
    // A pinned region renders into the system prompt, not a message, so a
    // task or prompt that lives in one never reached here and every such
    // stage ran on the default action or refused for want of a prompt. With
    // no message text, read the system blocks instead: only the ones a region
    // produced (a hint carries no region), never the runtime's own
    // instruction blocks, and never a region whose text is a stored part's
    // stand-in. Message text still wins when there is any, so a prompt an
    // upstream stage wrote into a conversation is not diluted by the task.
    if chunks.is_empty() {
        for block in &request.system {
            if block.region.is_empty() || RUNTIME_REGIONS.contains(&block.region.as_str()) {
                continue;
            }
            let body = unlabelled(&block.text, &block.region);
            if !is_pointer(body) && !body.trim().is_empty() {
                chunks.push(body.to_string());
            }
        }
    }
    chunks.join("\n").trim().to_string()
}

/// Regions the runtime writes for its own purposes, whose text is never a
/// prompt: the stage's standing instructions, and the mirrored final answer.
const RUNTIME_REGIONS: &[&str] = &["stage_instructions", "final_output"];

/// A region's system block without the `## <region>` heading assembly puts
/// on it, so the prompt is the region's text and not its label. A block
/// carrying no such heading is returned whole.
fn unlabelled<'a>(text: &'a str, region: &str) -> &'a str {
    text.strip_prefix("## ")
        .and_then(|rest| rest.strip_prefix(region))
        .and_then(|rest| rest.strip_prefix('\n'))
        .unwrap_or(text)
}

/// A non-empty string hint from `request.extra`.
pub(crate) fn extra_str(request: &InferenceRequest, key: &str) -> Option<String> {
    request
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// An integer hint from `request.extra`.
pub(crate) fn extra_i64(request: &InferenceRequest, key: &str) -> Option<i64> {
    request.extra.get(key).and_then(Value::as_i64)
}

/// A floating-point hint from `request.extra`.
pub(crate) fn extra_f64(request: &InferenceRequest, key: &str) -> Option<f64> {
    request.extra.get(key).and_then(Value::as_f64)
}

/// A boolean hint from `request.extra`.
pub(crate) fn extra_bool(request: &InferenceRequest, key: &str) -> Option<bool> {
    request.extra.get(key).and_then(Value::as_bool)
}

/// The absolute deadline for a whole media task, from its stage timeout.
pub(crate) fn deadline(request: &InferenceRequest) -> Instant {
    Instant::now() + Duration::from_secs(request.request_timeout_secs.unwrap_or(DEFAULT_TASK_SECS))
}

/// What one poll of a long task found.
pub(crate) enum Poll {
    /// Finished, with the finished task.
    Done(Value),
    /// Failed, with the reason.
    Failed(String),
    /// Still running.
    Running,
}

/// Poll `check` every `interval` until it is done or fails, or `deadline`
/// passes. `what` names the task in the errors.
pub(crate) async fn poll_until<F, Fut>(
    what: &str,
    deadline: Instant,
    interval: Duration,
    mut check: F,
) -> Result<Value>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Poll>>,
{
    loop {
        match check().await? {
            Poll::Done(task) => return Ok(task),
            Poll::Failed(reason) => {
                return Err(ProviderError::Other(format!("{what} failed: {reason}")));
            }
            Poll::Running => {}
        }
        if Instant::now() >= deadline {
            return Err(ProviderError::Other(format!(
                "{what} did not finish within its deadline; raise the stage's \
                 request_timeout_secs if it needs longer"
            )));
        }
        tokio::time::sleep(interval).await;
    }
}

/// Download `url` without the provider's credential (a signed or public
/// result URL on another host), up to [`DOWNLOAD_CAP`]. Answers the bytes and
/// the content type the host sent.
pub(crate) async fn download(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Vec<u8>, Option<String>)> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| ProviderError::transport("downloading a generated file", &e))?;
    let response = crate::provider::check_http_response(response, None).await?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string());
    let bytes = leviath_net::read_caps::read_body_capped(response, DOWNLOAD_CAP)
        .await
        .map_err(ProviderError::from)?;
    Ok((bytes.to_vec(), content_type))
}

/// A blob of `mime` named `name`, or the error for a type that does not parse.
pub(crate) fn blob(mime: &str, bytes: Vec<u8>, name: &str) -> Result<Blob> {
    let mime_type = MimeType::parse(mime).map_err(|e| {
        ProviderError::InvalidResponse(format!(
            "the provider sent an unreadable type '{mime}': {e}"
        ))
    })?;
    Ok(Blob::new(mime_type, bytes).named(name))
}

/// A file of a type this code names itself (`video/mp4`), so it is known to
/// parse.
pub(crate) fn typed_blob(mime: &'static str, bytes: Vec<u8>, name: &str) -> Blob {
    Blob::new(
        MimeType::parse(mime).expect("a type named in code is a mime type"),
        bytes,
    )
    .named(name)
}

/// A response's body, up to [`DOWNLOAD_CAP`].
pub(crate) async fn body_bytes(response: reqwest::Response) -> Result<Vec<u8>> {
    Ok(
        leviath_net::read_caps::read_body_capped(response, DOWNLOAD_CAP)
            .await
            .map_err(ProviderError::from)?
            .to_vec(),
    )
}

/// The audio type a response's `Content-Type` names, or `audio/mpeg` when it
/// names none or something that is not audio.
pub(crate) fn audio_type(response: &reqwest::Response) -> MimeType {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| MimeType::parse(v.split(';').next().unwrap_or(v).trim()).ok())
        .filter(|m| m.as_str().starts_with("audio/"))
        .unwrap_or_else(|| MimeType::parse("audio/mpeg").expect("audio/mpeg is a mime type"))
}

/// How long an audio file plays, in seconds, when its format says: a WAV by
/// its header's byte rate, and an MP3 by its first frame's bitrate, which
/// holds for the constant-bitrate files the speech routes return (OpenAI's
/// 76 032-byte, 128 kbps clip measured 4.752 s). `None` for anything else.
pub(crate) fn audio_seconds(audio: &Blob) -> Option<f64> {
    let bytes = &audio.bytes;
    match audio.mime_type.as_str() {
        "audio/wav" | "audio/x-wav" => {
            let field = bytes.get(28..32)?;
            let byte_rate = u32::from_le_bytes([field[0], field[1], field[2], field[3]]);
            (byte_rate > 0).then(|| bytes.len().saturating_sub(44) as f64 / f64::from(byte_rate))
        }
        "audio/mpeg" => {
            // An ID3 tag ahead of the audio is skipped by its stated size.
            let start = match bytes.get(..10) {
                Some([b'I', b'D', b'3', _, _, _, a, b, c, d]) => {
                    10 + ((usize::from(*a) << 21)
                        | (usize::from(*b) << 14)
                        | (usize::from(*c) << 7)
                        | usize::from(*d))
                }
                _ => 0,
            };
            let header = bytes.get(start..start + 4)?;
            if header[0] != 0xFF || header[1] & 0xE0 != 0xE0 {
                return None;
            }
            let mpeg1 = header[1] & 0x18 == 0x18;
            let index = usize::from(header[2] >> 4);
            const V1: [u32; 16] = [
                0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
            ];
            const V2: [u32; 16] = [
                0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
            ];
            let kbps = if mpeg1 { V1[index] } else { V2[index] };
            (kbps > 0).then(|| (bytes.len() - start) as f64 * 8.0 / f64::from(kbps * 1000))
        }
        _ => None,
    }
}

/// A JSON part the provider built itself, so its type is known good.
pub(crate) fn json_blob(bytes: Vec<u8>, name: &str) -> Blob {
    Blob::new(
        MimeType::parse("application/json").expect("application/json is a mime type"),
        bytes,
    )
    .named(name)
}

/// The response for a media call: a one-line summary as the text, the files
/// as parts, and the call's cost when it is known.
pub(crate) fn response(
    summary: String,
    parts: Vec<Blob>,
    cost_usd: Option<f64>,
) -> InferenceResponse {
    InferenceResponse {
        content: summary,
        tool_calls: Vec::new(),
        tokens_used: TokenUsage::new(0, 0, 0, 0).with_reported_cost(cost_usd),
        finish_reason: FinishReason::Complete,
        reasoning: None,
        parts,
    }
}

/// `response` as the one chunk a stream carries. The default streaming path
/// drops parts, which for a provider whose whole output is a part would lose
/// the file.
pub(crate) fn one_chunk(response: InferenceResponse) -> crate::rate_limit::ChunkStream {
    let chunk = StreamChunk {
        delta: response.content,
        tool_calls: Vec::new(),
        tokens: Some(response.tokens_used),
        finish_reason: Some(response.finish_reason),
        reasoning: None,
        parts: response.parts,
    };
    Box::pin(tokio_stream::once(Ok(chunk)))
}

/// The summary line for `parts` from `route`.
pub(crate) fn summary(route: &str, parts: &[Blob]) -> String {
    format!(
        "Produced {} part(s) with {route}: {}",
        parts.len(),
        parts
            .iter()
            .map(|p| p.name.clone().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The file extension a person expects for `mime`.
pub(crate) fn extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "application/json" => "json",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, SystemBlock};
    use leviath_core::mime::{MimeRegistry, Part as CorePart};

    fn hydrated(mime: &str, name: &str, bytes: &[u8]) -> ContentBlock {
        carrying(
            mime,
            name,
            bytes,
            &base64::engine::general_purpose::STANDARD.encode(bytes),
        )
    }

    /// A stored part of `bytes` whose block carries `data`.
    fn carrying(mime: &str, name: &str, bytes: &[u8], data: &str) -> ContentBlock {
        let blob = Blob::new(MimeType::parse(mime).unwrap(), bytes.to_vec()).named(name);
        let part = CorePart::stored(blob.describe(&MimeRegistry::builtin())).named(name);
        ContentBlock::Mime {
            part: part.blob().unwrap().clone(),
            data: data.to_string(),
            name: part.name.clone(),
            deliver: None,
            remote: None,
        }
    }

    fn request(blocks: Vec<ContentBlock>) -> InferenceRequest {
        InferenceRequest {
            system: vec![SystemBlock {
                text: "draw a cat".into(),
                cache_hint: leviath_core::CacheHint::Always,
                region: "task".into(),
                volatility: leviath_core::Volatility::Stable,
            }],
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(blocks),
                cache_breakpoint: false,
                reasoning: None,
            }],
            model: "m".into(),
            max_tokens: 0,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({ "n": 2, "size": " 1k ", "blank": " ", "hd": true, "speed": 1.5 }),
            request_timeout_secs: Some(1),
        }
    }

    #[test]
    fn parts_and_text_are_read_apart() {
        let png = hydrated("image/png", "ref.png", b"PNGBYTES");
        let stand_in = png.stand_in().unwrap().to_string();
        let req = request(vec![
            ContentBlock::Text {
                text: format!("[image] {stand_in}"),
            },
            ContentBlock::Text {
                text: "make it blue".into(),
            },
            png,
        ]);
        assert_eq!(request_text(&req), "make it blue");
        let uris = data_uris(&req, |m| m.starts_with("image/"));
        assert_eq!(uris.len(), 1);
        assert!(uris[0].starts_with("data:image/png;base64,"));
        let part = first_part(&req, |m| m.starts_with("image/")).expect("decoded");
        assert_eq!(part.bytes, b"PNGBYTES");
        assert_eq!(part.name.as_deref(), Some("ref.png"));
        assert!(first_part(&req, |m| m.starts_with("audio/")).is_none());
        let torn = carrying("image/png", "torn.png", b"x", "not base64!");
        assert!(
            first_part(&request(vec![torn]), |m| m.starts_with("image/")).is_none(),
            "bytes that do not decode are no part"
        );
        assert_eq!(
            json_blob(b"{}".to_vec(), "a.json").mime_type.as_str(),
            "application/json"
        );
    }

    #[test]
    fn a_request_with_no_conversation_text_falls_back_to_its_system_blocks() {
        let mut req = request(vec![]);
        req.messages.clear();
        assert_eq!(request_text(&req), "draw a cat");
        let mut plain = request(vec![]);
        plain.messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Text("a dog".into()),
            cache_breakpoint: false,
            reasoning: None,
        }];
        assert_eq!(request_text(&plain), "a dog");
        // What the runtime actually sends a stage whose task is pinned: its
        // placeholder turn, with the task in the system blocks.
        let mut opening = request(vec![]);
        opening.messages = vec![Message {
            role: "user".into(),
            content: MessageContent::Text(crate::provider::OPENING_TURN.into()),
            cache_breakpoint: false,
            reasoning: None,
        }];
        assert_eq!(request_text(&opening), "draw a cat");
    }

    /// Lengths read from the files the speech routes return: the MP3 header
    /// is the one OpenAI's and xAI's clips carry (`FF F3 C4`, MPEG-2 at
    /// 128 kbps), so 16 000 bytes play for one second.
    #[test]
    fn audio_length_is_read_from_the_file() {
        let clip = |mime: &str, bytes: Vec<u8>| Blob::new(MimeType::parse(mime).unwrap(), bytes);
        let mut mp3 = vec![0xFF, 0xF3, 0xC4, 0xC4];
        mp3.resize(16_000, 0);
        assert_eq!(audio_seconds(&clip("audio/mpeg", mp3.clone())), Some(1.0));

        // An ID3 tag of 6 bytes ahead of the audio is not counted.
        let mut tagged = vec![b'I', b'D', b'3', 3, 0, 0, 0, 0, 0, 6, 1, 2, 3, 4, 5, 6];
        tagged.extend(&mp3);
        assert_eq!(audio_seconds(&clip("audio/mpeg", tagged)), Some(1.0));

        // MPEG-1 at 128 kbps.
        let mut v1 = vec![0xFF, 0xFB, 0x90, 0x00];
        v1.resize(32_000, 0);
        assert_eq!(audio_seconds(&clip("audio/mpeg", v1)), Some(2.0));

        let mut wav = vec![0u8; 44];
        wav[28..32].copy_from_slice(&48_000u32.to_le_bytes());
        wav.resize(44 + 96_000, 0);
        assert_eq!(audio_seconds(&clip("audio/wav", wav)), Some(2.0));

        assert_eq!(
            audio_seconds(&clip("audio/wav", vec![0; 44])),
            None,
            "no byte rate"
        );
        assert_eq!(
            audio_seconds(&clip("audio/wav", vec![0; 10])),
            None,
            "no header"
        );
        assert_eq!(
            audio_seconds(&clip("audio/mpeg", vec![0; 16])),
            None,
            "no frame"
        );
        assert_eq!(
            audio_seconds(&clip("audio/mpeg", vec![0xFF])),
            None,
            "cut short"
        );
        assert_eq!(
            audio_seconds(&clip("audio/mpeg", vec![0xFF, 0xF3, 0x04, 0])),
            None,
            "a free-format bitrate says nothing"
        );
        assert_eq!(audio_seconds(&clip("audio/ogg", vec![0; 16])), None);
    }

    #[test]
    fn hints_are_read_by_type() {
        let req = request(vec![]);
        assert_eq!(extra_i64(&req, "n"), Some(2));
        assert_eq!(extra_str(&req, "size").as_deref(), Some("1k"));
        assert_eq!(extra_str(&req, "blank"), None);
        assert_eq!(extra_bool(&req, "hd"), Some(true));
        assert_eq!(extra_f64(&req, "speed"), Some(1.5));
    }

    #[tokio::test]
    async fn polling_ends_on_done_on_failure_and_on_the_deadline() {
        let mut calls = 0;
        let done = poll_until(
            "task",
            deadline(&request(vec![])),
            Duration::from_millis(1),
            || {
                calls += 1;
                let finished = calls > 1;
                async move {
                    Ok(match finished {
                        true => Poll::Done(serde_json::json!({ "ok": true })),
                        false => Poll::Running,
                    })
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(done["ok"], true);
        let failed = poll_until("task", Instant::now(), Duration::ZERO, || async {
            Ok(Poll::Failed("moderated".into()))
        })
        .await
        .unwrap_err();
        assert!(failed.to_string().contains("moderated"));
        let late = poll_until("task", Instant::now(), Duration::ZERO, || async {
            Ok(Poll::Running)
        })
        .await
        .unwrap_err();
        assert!(late.to_string().contains("request_timeout_secs"));
        let errored = poll_until("task", Instant::now(), Duration::ZERO, || async {
            Err(ProviderError::Other("boom".into()))
        })
        .await
        .unwrap_err();
        assert!(errored.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn a_download_carries_its_type_and_a_refusal_is_an_error() {
        let url = leviath_testkit::spawn_mock_server_with_headers(
            200,
            "OK",
            "Content-Type: video/mp4; codecs=avc1\r\n",
            b"MP4".to_vec(),
        )
        .await;
        let (bytes, mime) = download(&reqwest::Client::new(), &url).await.unwrap();
        assert_eq!(bytes, b"MP4");
        assert_eq!(mime.as_deref(), Some("video/mp4"));
        let gone = leviath_testkit::spawn_mock_server(404, "Not Found", b"expired".to_vec()).await;
        assert!(download(&reqwest::Client::new(), &gone).await.is_err());
        assert!(
            download(&reqwest::Client::new(), "http://127.0.0.1:1/x")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_response_streams_as_one_chunk_with_its_parts() {
        use tokio_stream::StreamExt as _;
        let parts = vec![blob("image/png", vec![1], "a.png").unwrap()];
        let text = summary("xai/grok-imagine-image", &parts);
        assert_eq!(
            text,
            "Produced 1 part(s) with xai/grok-imagine-image: a.png"
        );
        let response = response(text, parts, Some(0.02));
        assert_eq!(response.tokens_used.reported_cost_usd, Some(0.02));
        let mut stream = one_chunk(response);
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.parts.len(), 1);
        assert!(stream.next().await.is_none());
        assert!(blob("not a type", vec![], "x").is_err());
    }

    #[test]
    fn extensions_follow_the_type() {
        for (mime, ext) in [
            ("image/jpeg", "jpg"),
            ("image/png", "png"),
            ("image/webp", "webp"),
            ("video/mp4", "mp4"),
            ("audio/mpeg", "mp3"),
            ("audio/wav", "wav"),
            ("audio/x-wav", "wav"),
            ("audio/ogg", "ogg"),
            ("application/json", "json"),
            ("model/obj", "bin"),
        ] {
            assert_eq!(extension(mime), ext, "{mime}");
        }
    }

    #[tokio::test]
    async fn a_download_cut_short_is_an_error() {
        let url = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
        assert!(download(&reqwest::Client::new(), &url).await.is_err());
    }
}
