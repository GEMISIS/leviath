//! What every blob-in, blob-out provider shares: reading a request's prompt
//! and parts, polling a long task, downloading a result, and handing parts
//! back through both the buffered and the streaming path.
//!
//! Meshy (3D), xAI's Imagine and speech routes, and Meta's Muse Image and
//! Voice Transcribe all run the same shape: take the text and the media parts a
//! stage assembled, call an endpoint that is not chat, and answer with files.
//! The runtime stores what comes back in the run's blob store and routes it by
//! the stage's `output_routing`, exactly as for Meshy.

#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "meta",
    feature = "openrouter"
))]
pub(crate) mod images;

#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "meta"
))]
use std::time::Duration;
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google"
))]
use std::time::Instant;

#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "meta",
    feature = "meshy"
))]
use serde_json::Value;

pub(crate) use crate::prompt::request_text;
use crate::provider::InferenceRequest;
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta",
    feature = "meshy",
    feature = "openrouter"
))]
use crate::provider::{ContentBlock, MessageContent};
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
use base64::Engine as _;
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
use {
    crate::provider::{
        FinishReason, InferenceResponse, ProviderError, Result, StreamChunk, TokenUsage,
    },
    leviath_core::cost::{CallCost, CostUnknown},
    leviath_core::mime::{Blob, MimeType},
};

/// The largest body a media download reads: a 15-second 1080p video or a long
/// speech track can pass the 64 MiB a JSON reply is capped at. What is kept is
/// still bounded by `[mime] max_part_bytes` when the runtime stores it.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "meta"
))]
pub(crate) const DOWNLOAD_CAP: usize = 512 * 1024 * 1024;

/// How long a media task may run when the stage names no timeout.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google"
))]
pub(crate) const DEFAULT_TASK_SECS: u64 = 900;

/// Every hydrated mime block whose type passes `want`, as a `data:` URI.
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "meta",
    feature = "meshy",
    feature = "openrouter"
))]
pub(crate) fn data_uris(request: &InferenceRequest, want: impl Fn(&str) -> bool) -> Vec<String> {
    parts(request, want)
        .map(|(part, data, _)| crate::mime::data_uri(&part.mime_type, data))
        .collect()
}

/// Every hydrated mime block whose type passes `want`: the part, its base64
/// bytes, and its name.
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta",
    feature = "meshy",
    feature = "openrouter"
))]
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
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
pub(crate) struct Part {
    /// The part's mime type.
    #[cfg(any(
        feature = "openai",
        feature = "xai",
        feature = "xai-subscription",
        feature = "google"
    ))]
    pub(crate) mime_type: MimeType,
    /// Its bytes.
    pub(crate) bytes: Vec<u8>,
    /// Its name, when it has one.
    #[cfg(any(
        feature = "openai",
        feature = "xai",
        feature = "xai-subscription",
        feature = "meta"
    ))]
    pub(crate) name: Option<String>,
}

/// The first hydrated part whose type passes `want`, decoded. An upload
/// endpoint takes bytes, not a data URI.
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
pub(crate) fn first_part(request: &InferenceRequest, want: impl Fn(&str) -> bool) -> Option<Part> {
    let (part, data, name) = parts(request, want).next()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    // Which fields a part carries depends on the providers built in; the
    // unused halves are bound here so a build without them has nothing to warn
    // about.
    let _ = (&part, &name);
    Some(Part {
        #[cfg(any(
            feature = "openai",
            feature = "xai",
            feature = "xai-subscription",
            feature = "google"
        ))]
        mime_type: part.mime_type.clone(),
        bytes,
        #[cfg(any(
            feature = "openai",
            feature = "xai",
            feature = "xai-subscription",
            feature = "meta"
        ))]
        name: name.map(str::to_string),
    })
}

/// A non-empty string hint from `request.extra`.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "meta",
    feature = "meshy"
))]
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
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "meta",
    feature = "meshy"
))]
pub(crate) fn extra_i64(request: &InferenceRequest, key: &str) -> Option<i64> {
    request.extra.get(key).and_then(Value::as_i64)
}

/// A floating-point hint from `request.extra`.
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "meshy"
))]
pub(crate) fn extra_f64(request: &InferenceRequest, key: &str) -> Option<f64> {
    request.extra.get(key).and_then(Value::as_f64)
}

/// A whole number of seconds from `request.extra`, under the first of `keys`
/// the stage sets: `4`, `4.0`, `"4"` or `"4s"`. `Ok(None)` when it sets none.
///
/// Anything else is refused, naming the key. A length is a paid parameter: one
/// read as "not set" runs at the vendor's default, which can be longer than
/// what was asked for and cost more (Veo dropped `"4s"` and made eight
/// seconds).
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google"
))]
pub(crate) fn extra_seconds(request: &InferenceRequest, keys: &[&str]) -> Result<Option<u64>> {
    let Some((key, value)) = keys
        .iter()
        .find_map(|k| request.extra.get(*k).map(|v| (*k, v)))
    else {
        return Ok(None);
    };
    let seconds = match value {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as u64)),
        Value::String(text) => {
            let text = text.trim();
            text.strip_suffix('s').unwrap_or(text).trim().parse().ok()
        }
        _ => None,
    };
    match seconds {
        Some(seconds) if seconds > 0 => Ok(Some(seconds)),
        _ => Err(ProviderError::unsent(format!(
            "`{key}` must be a whole number of seconds, such as 4, \"4\" or \"4s\", not {value}"
        ))),
    }
}

/// One of `allowed` from `request.extra[key]`, matched without regard to
/// case and answered in `allowed`'s spelling. `Ok(None)` when the stage does
/// not set it; any other value is refused naming the key and what it takes,
/// rather than passed on for the vendor to drop or answer at its default.
#[cfg(feature = "google")]
pub(crate) fn extra_choice(
    request: &InferenceRequest,
    key: &str,
    allowed: &[&'static str],
) -> Result<Option<&'static str>> {
    let Some(value) = request.extra.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(str::trim)
        .and_then(|text| allowed.iter().find(|a| a.eq_ignore_ascii_case(text)))
        .map(|choice| Some(*choice))
        .ok_or_else(|| {
            ProviderError::unsent(format!(
                "`{key}` must be one of {}, not {value}",
                allowed.join(", ")
            ))
        })
}

/// A boolean hint from `request.extra`.
#[cfg(any(feature = "xai", feature = "xai-subscription", feature = "meshy"))]
pub(crate) fn extra_bool(request: &InferenceRequest, key: &str) -> Option<bool> {
    request.extra.get(key).and_then(Value::as_bool)
}

/// The absolute deadline for a whole media task, from its stage timeout.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google"
))]
pub(crate) fn deadline(request: &InferenceRequest) -> Instant {
    Instant::now() + Duration::from_secs(request.request_timeout_secs.unwrap_or(DEFAULT_TASK_SECS))
}

/// What one poll of a long task found.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google"
))]
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
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google"
))]
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
            // The check that reported it was answered, so a request went out.
            Poll::Failed(reason) => {
                return Err(ProviderError::Other(format!("{what} failed: {reason}")).after_send());
            }
            Poll::Running => {}
        }
        if Instant::now() >= deadline {
            // Not a failed job: the provider is still running it and will
            // bill it, so it is kept for the call made again to pick up.
            return Err(crate::jobs::still_running(format!(
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
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "meta",
    feature = "openrouter"
))]
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
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "bedrock",
    feature = "meta",
    feature = "openrouter"
))]
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
#[cfg(any(feature = "openrouter", feature = "openai", feature = "google"))]
pub(crate) fn typed_blob(mime: &'static str, bytes: Vec<u8>, name: &str) -> Blob {
    Blob::new(
        MimeType::parse(mime).expect("a type named in code is a mime type"),
        bytes,
    )
    .named(name)
}

/// A response's body, up to [`DOWNLOAD_CAP`].
#[cfg(any(feature = "openrouter", feature = "openai", feature = "google"))]
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
#[cfg(feature = "openai")]
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
#[cfg(feature = "openai")]
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
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "meta"
))]
pub(crate) fn json_blob(bytes: Vec<u8>, name: &str) -> Blob {
    Blob::new(
        MimeType::parse("application/json").expect("application/json is a mime type"),
        bytes,
    )
    .named(name)
}

/// What `quantity` units cost at `unit`, as a figure this crate computed:
/// no figure where no unit price is known, and none where the reply named no
/// quantity to price (a video with no length, a transcription with no
/// duration). A quantity is what came back, never what was asked for.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
pub(crate) fn unit_cost(
    unit: Option<crate::pricing::UnitPrice>,
    quantity: Option<f64>,
) -> CallCost {
    match (unit, quantity.filter(|q| *q > 0.0)) {
        (None, _) => CallCost::Unknown(CostUnknown::NoPrice),
        (Some(_), None) => CallCost::Unknown(CostUnknown::NoUsage),
        (Some(unit), Some(quantity)) => CallCost::Computed(unit.cost(quantity)),
    }
}

/// The usage of a media call: no tokens, and `cost` as the provider stated
/// it, as this crate computed it, or as unknown with its reason.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
pub(crate) fn usage(cost: CallCost) -> TokenUsage {
    let usage = TokenUsage::new(0, 0, 0, 0);
    match cost {
        CallCost::Reported(usd) => usage.with_reported_cost(Some(usd)),
        CallCost::Computed(usd) => usage.with_computed_cost(usd),
        CallCost::Unknown(reason) => usage.with_unknown_cost(reason),
    }
}

/// Say what a finished job cost before its file is fetched: the provider is
/// done and has billed it, whatever becomes of the download.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription"
))]
pub(crate) fn job_done(cost: CallCost) {
    crate::call_trace::note_whole_usage(&usage(cost));
}

/// How long an MP4 plays, in seconds, by its movie header (`moov/mvhd`):
/// the duration over the timescale. `None` for a file with no such header.
#[cfg(feature = "google")]
pub(crate) fn video_seconds(bytes: &[u8]) -> Option<f64> {
    /// The big-endian number in the `len` bytes at `at`.
    fn number(bytes: &[u8], at: usize, len: usize) -> Option<u64> {
        let digits = bytes.get(at..at + len)?;
        Some(digits.iter().fold(0, |n, b| (n << 8) | u64::from(*b)))
    }
    /// The body of the first box named `name` among the boxes in `bytes`.
    fn find<'a>(mut bytes: &'a [u8], name: &[u8; 4]) -> Option<&'a [u8]> {
        loop {
            let size = number(bytes, 0, 4)?;
            let kind = bytes.get(4..8)?;
            // A size of 1 puts the real size in the next eight bytes; a size
            // of 0 runs to the end of the file.
            let (start, size) = match size {
                1 => (16, number(bytes, 8, 8)?),
                0 => (8, bytes.len() as u64),
                size => (8, size),
            };
            let size = usize::try_from(size).unwrap_or(usize::MAX);
            let body = bytes.get(start..size)?;
            if kind == name {
                return Some(body);
            }
            bytes = &bytes[size..];
        }
    }
    let header = find(find(bytes, b"moov")?, b"mvhd")?;
    // Version 1 writes 64-bit times, version 0 32-bit ones.
    let (timescale, duration) = match *header.first()? {
        1 => (number(header, 20, 4)?, number(header, 24, 8)?),
        _ => (number(header, 12, 4)?, number(header, 16, 4)?),
    };
    (timescale > 0 && duration > 0).then(|| duration as f64 / timescale as f64)
}

/// The response for a media call: a one-line summary as the text, the files
/// as parts, and what the call cost.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
pub(crate) fn response(summary: String, parts: Vec<Blob>, cost: CallCost) -> InferenceResponse {
    InferenceResponse {
        reasoning_summary: None,
        content: summary,
        tool_calls: Vec::new(),
        tokens_used: usage(cost),
        finish_reason: FinishReason::Complete,
        reasoning: None,
        parts,
        content_is_reasoning: false,
    }
}

/// `response` as the one chunk a stream carries. The default streaming path
/// drops parts, which for a provider whose whole output is a part would lose
/// the file.
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
pub(crate) fn one_chunk(response: InferenceResponse) -> crate::rate_limit::ChunkStream {
    let chunk = StreamChunk {
        reasoning_delta: None,
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
#[cfg(any(
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta"
))]
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
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta",
    feature = "openrouter"
))]
pub(crate) fn extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/l16" => "pcm",
        "application/json" => "json",
        _ => "bin",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    /// A minimal MP4: a file type box, then a movie box holding a movie
    /// header that says `duration` units at `timescale` units a second.
    pub(crate) fn mp4(timescale: u32, duration: u32) -> Vec<u8> {
        let boxed = |name: &[u8; 4], body: &[u8]| {
            let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend_from_slice(name);
            out.extend_from_slice(body);
            out
        };
        let mut header = vec![0u8; 12];
        header.extend_from_slice(&timescale.to_be_bytes());
        header.extend_from_slice(&duration.to_be_bytes());
        let mut file = boxed(b"ftyp", b"isom");
        file.extend(boxed(b"moov", &boxed(b"mvhd", &header)));
        file
    }

    use super::*;
    use crate::provider::{ContentBlock, Message, MessageContent, SystemBlock};
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

    fn region_block(region: &str, text: &str) -> SystemBlock {
        SystemBlock {
            text: text.into(),
            cache_hint: leviath_core::CacheHint::Always,
            region: region.into(),
            volatility: leviath_core::Volatility::Stable,
        }
    }

    /// A stage that names its prompt regions sends exactly those regions'
    /// text, in the order named, and nothing the other regions, the
    /// conversation or the runtime's blocks hold.
    #[test]
    fn prompt_regions_are_the_whole_prompt_in_the_order_named() {
        let mut req = request(vec![ContentBlock::Text {
            text: "a message that is not the prompt".into(),
        }]);
        req.system = vec![
            region_block("items", "## items\nboilerplate about the whole project"),
            region_block("brief", "## brief\nA bronze knight, facing left"),
            region_block("style", "## style\nflat pixel art"),
            region_block("stage_instructions", "runtime text"),
            region_block("brief", "   "),
        ];
        req.extra = serde_json::json!({ "prompt_regions": ["brief"] });
        assert_eq!(request_text(&req), "A bronze knight, facing left");

        req.extra = serde_json::json!({ "prompt_regions": ["style", "brief"] });
        assert_eq!(
            request_text(&req),
            "flat pixel art\nA bronze knight, facing left"
        );

        // A region that was never written leaves the prompt empty rather than
        // filling it with some other region.
        req.extra = serde_json::json!({ "prompt_regions": ["missing"] });
        assert_eq!(request_text(&req), "");

        // Unset or empty, the prompt is read as it always was.
        req.extra = serde_json::json!({ "prompt_regions": [] });
        assert_eq!(request_text(&req), "a message that is not the prompt");
        req.extra = serde_json::json!({ "prompt_regions": "brief" });
        assert_eq!(request_text(&req), "a message that is not the prompt");
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
        // The check that reported the failure was answered.
        assert_eq!(failed.request_sent(), Some(true));
        let late = poll_until("task", Instant::now(), Duration::ZERO, || async {
            Ok(Poll::Running)
        })
        .await
        .unwrap_err();
        assert!(late.to_string().contains("request_timeout_secs"));
        assert_eq!(late.request_sent(), Some(true));
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
        let response = response(text, parts, CallCost::Reported(0.02));
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

    #[cfg(feature = "google")]
    #[test]
    fn a_videos_length_is_read_off_its_movie_header() {
        assert_eq!(video_seconds(&mp4(1_000, 3_000)), Some(3.0));
        assert_eq!(video_seconds(&mp4(600, 4_800)), Some(8.0));
        // No header, a header with no timescale, and bytes that are no MP4.
        assert_eq!(video_seconds(b"MP4"), None);
        assert_eq!(video_seconds(&mp4(0, 3_000)), None);
        assert_eq!(video_seconds(&mp4(1_000, 0)), None);
        assert_eq!(video_seconds(&[]), None);

        // A version-1 header writes 64-bit times.
        let mut header = vec![1u8, 0, 0, 0];
        header.extend_from_slice(&[0u8; 16]);
        header.extend_from_slice(&1_000u32.to_be_bytes());
        header.extend_from_slice(&5_000u64.to_be_bytes());
        let boxed = |name: &[u8; 4], body: &[u8]| {
            let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            out.extend_from_slice(name);
            out.extend_from_slice(body);
            out
        };
        let wide = boxed(b"moov", &boxed(b"mvhd", &header));
        assert_eq!(video_seconds(&wide), Some(5.0));
        // Cut short inside its header: in the duration, and in the timescale.
        assert_eq!(
            video_seconds(&boxed(b"moov", &boxed(b"mvhd", &header[..26]))),
            None
        );
        assert_eq!(
            video_seconds(&boxed(b"moov", &boxed(b"mvhd", &header[..22]))),
            None
        );
        // The same for the 32-bit header, and for a header with nothing in it.
        let short = mp4(1_000, 3_000);
        let narrow = &short[short.len() - 20..];
        assert_eq!(
            video_seconds(&boxed(b"moov", &boxed(b"mvhd", narrow))),
            Some(3.0)
        );
        assert_eq!(
            video_seconds(&boxed(b"moov", &boxed(b"mvhd", &narrow[..18]))),
            None
        );
        assert_eq!(
            video_seconds(&boxed(b"moov", &boxed(b"mvhd", &narrow[..14]))),
            None
        );
        assert_eq!(video_seconds(&boxed(b"moov", &boxed(b"mvhd", b""))), None);
        // A movie box with no header in it, a box whose name is cut off, and
        // a box that says it is longer than the file.
        assert_eq!(video_seconds(&boxed(b"moov", &boxed(b"trak", b"x"))), None);
        assert_eq!(video_seconds(&wide[..6]), None);
        assert_eq!(video_seconds(&wide[..wide.len() - 6]), None);

        // A box whose size is in the eight bytes after its name, and one that
        // runs to the end of the file.
        let inner = boxed(b"mvhd", &{
            let mut h = vec![0u8; 12];
            h.extend_from_slice(&1_000u32.to_be_bytes());
            h.extend_from_slice(&2_000u32.to_be_bytes());
            h
        });
        let mut long = 1u32.to_be_bytes().to_vec();
        long.extend_from_slice(b"moov");
        long.extend_from_slice(&((inner.len() + 16) as u64).to_be_bytes());
        long.extend_from_slice(&inner);
        assert_eq!(video_seconds(&long), Some(2.0));
        let mut open = 0u32.to_be_bytes().to_vec();
        open.extend_from_slice(b"moov");
        open.extend_from_slice(&inner);
        assert_eq!(video_seconds(&open), Some(2.0));
        // A wide size cut short, and one too large to be a length.
        assert_eq!(video_seconds(&long[..12]), None);
        let mut huge = 1u32.to_be_bytes().to_vec();
        huge.extend_from_slice(b"moov");
        huge.extend_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(video_seconds(&huge), None);
    }

    #[test]
    fn a_unit_cost_needs_a_price_and_a_quantity_that_came_back() {
        let per_second = crate::pricing::UnitPrice {
            usd: 0.1,
            unit: crate::pricing::PriceUnit::VideoSecond,
        };
        assert_eq!(
            unit_cost(Some(per_second), Some(4.0)),
            CallCost::Computed(0.4)
        );
        assert_eq!(
            unit_cost(Some(per_second), None),
            CallCost::Unknown(CostUnknown::NoUsage),
            "a reply that names no length prices nothing"
        );
        assert_eq!(
            unit_cost(Some(per_second), Some(0.0)),
            CallCost::Unknown(CostUnknown::NoUsage),
            "and a length of nothing is no length"
        );
        assert_eq!(
            unit_cost(None, Some(4.0)),
            CallCost::Unknown(CostUnknown::NoPrice)
        );
    }

    #[test]
    fn a_media_calls_usage_says_how_its_cost_is_known() {
        assert_eq!(
            usage(CallCost::Reported(0.5)).cost(None),
            CallCost::Reported(0.5)
        );
        assert_eq!(
            usage(CallCost::Computed(0.25)).cost(None),
            CallCost::Computed(0.25)
        );
        assert_eq!(
            usage(CallCost::Unknown(CostUnknown::NoUsage)).cost(None),
            CallCost::Unknown(CostUnknown::NoUsage)
        );
        // Computed is never reported.
        assert_eq!(usage(CallCost::Computed(0.25)).reported_cost_usd, None);
    }
}
