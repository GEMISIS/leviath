//! Putting a stored part's bytes into a request, and the shapes vendors take
//! them in.
//!
//! Assembly never reads a file: it emits a [`ContentBlock::Mime`] per stored
//! part with the bytes left out, so the assembled request, the journal and
//! every snapshot stay small. Right before a request is sent,
//! [`hydrate_request`] decides per block what the model gets:
//!
//! - the bytes, base64, when the model's [`ModelMime`] covers the type;
//! - the bytes as text, when the registry says the type is text (a `model/obj`
//!   file is UTF-8) or the part asks for text, whatever the model declares;
//! - the stand-in line otherwise.
//!
//! A vendor encoder then turns a hydrated block into that vendor's shape. A
//! block that reaches an encoder unhydrated is written as its stand-in, so a
//! lane that never hydrates (routing, compaction) still sends a correct
//! request.

use std::sync::Arc;

use base64::Engine;
use leviath_core::mime::{BlobRef, Delivery, MimeRegistry, MimeType};

use crate::capabilities::ModelMime;
use crate::provider::{ContentBlock, InferenceRequest, MessageContent};

/// The families the built-in encoders know how to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `image/*`.
    Image,
    /// `audio/*`.
    Audio,
    /// `video/*`.
    Video,
    /// `application/pdf`.
    Document,
    /// Anything else, which no built-in encoder has a shape for.
    Other,
}

/// The encoder family a type belongs to, by the vendors' own grouping.
pub fn family_of(mime_type: &MimeType) -> Family {
    match (mime_type.kind(), mime_type.subtype()) {
        ("image", _) => Family::Image,
        ("audio", _) => Family::Audio,
        ("video", _) => Family::Video,
        ("application", "pdf") => Family::Document,
        _ => Family::Other,
    }
}

/// The request shapes this crate encodes a mime block into, and so what each
/// can carry beside text.
///
/// A vendor may take video, and its published modality table says so, but a
/// request shape with no slot for a family can only send the stand-in. A
/// model's mime is passed through its shape so the runtime never base64s,
/// charges and caps bytes that the encoder would throw away at the last step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireShape {
    /// OpenAI Chat Completions: `image_url`, `input_audio`, `file`.
    OpenAi,
    /// Anthropic Messages: `image`, `document`.
    Anthropic,
    /// OpenAI Responses (Codex and the providers that share its client):
    /// `input_image`, `input_file`.
    Responses,
    /// The Responses shape on a route that also takes `input_audio` and
    /// `input_video` (Meta's Model API).
    ResponsesAv,
    /// Gemini's Interactions API: `image`, `audio`, `video`, `document`.
    Gemini,
    /// Bedrock Converse: `image`, `document`.
    Bedrock,
}

impl WireShape {
    /// Whether this shape has a block for `family`.
    fn carries(self, family: Family) -> bool {
        matches!(
            (self, family),
            (_, Family::Image | Family::Document)
                | (WireShape::OpenAi, Family::Audio)
                | (
                    WireShape::ResponsesAv | WireShape::Gemini,
                    Family::Audio | Family::Video
                )
        )
    }

    /// `mime` with every input this shape has no block for taken out. Text is
    /// not a family and always travels; outputs are left alone, since what a
    /// model hands back is the reply's business, not the request's.
    pub fn carried(
        self,
        mut mime: crate::capabilities::ModelMime,
    ) -> crate::capabilities::ModelMime {
        mime.input
            .retain(|pattern| family_of_pattern(pattern).is_none_or(|f| self.carries(f)));
        mime
    }
}

/// The family a mime pattern names: `None` for text and for `*/*`, which
/// every shape carries; a pattern that names no built-in family is `Other`.
fn family_of_pattern(pattern: &str) -> Option<Family> {
    let (kind, _) = pattern.split_once('/')?;
    match kind {
        "text" | "*" => None,
        "image" => Some(Family::Image),
        "audio" => Some(Family::Audio),
        "video" => Some(Family::Video),
        _ => Some(
            MimeType::parse(pattern)
                .map(|t| family_of(&t))
                .unwrap_or(Family::Other),
        ),
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;
    use crate::capabilities::ModelMime;

    fn everything() -> ModelMime {
        ModelMime::new(
            &[
                "text/*",
                "image/*",
                "audio/*",
                "video/*",
                "application/pdf",
                "model/*",
            ],
            &["text/*", "video/*"],
        )
    }

    #[test]
    fn a_shape_keeps_only_the_inputs_it_has_a_block_for() {
        let open_ai = WireShape::OpenAi.carried(everything());
        assert_eq!(
            open_ai.input,
            ["text/*", "image/*", "audio/*", "application/pdf"]
        );
        let anthropic = WireShape::Anthropic.carried(everything());
        assert_eq!(anthropic.input, ["text/*", "image/*", "application/pdf"]);
        assert_eq!(
            WireShape::Responses.carried(everything()).input,
            anthropic.input
        );
        assert_eq!(
            WireShape::Bedrock.carried(everything()).input,
            anthropic.input
        );
        // Outputs are the reply's business and are left alone.
        assert_eq!(anthropic.output, ["text/*", "video/*"]);
        // A wildcard input, and a pattern that names no family, read the
        // same way whatever the shape.
        let odd = WireShape::Anthropic.carried(ModelMime::new(
            &["*/*", "x-thing/*", "application/zip", "noslash"],
            &["text/*"],
        ));
        assert_eq!(odd.input, ["*/*", "noslash"]);
    }
}

/// `data:<type>;base64,<bytes>` for a hydrated mime block.
pub fn data_uri(mime_type: &MimeType, data: &str) -> String {
    format!("data:{mime_type};base64,{data}")
}

/// How much stored media a request may carry, and what fetches its bytes.
pub struct Hydration<'a> {
    /// What the model takes.
    pub mime: &'a ModelMime,
    /// The registry, for the text flag.
    pub registry: &'a MimeRegistry,
    /// The bytes of stored media the request may carry before the oldest parts
    /// become stand-ins. This is the raw part size, not the base64 on the wire,
    /// and it is a backstop for vendor request-size limits a token budget
    /// cannot see: an image's token estimate is the same whatever its byte
    /// size, so a request can sit inside its context window and still be
    /// megabytes of media.
    pub max_media_bytes: u64,
    /// The bytes for a reference, or `None` when they are missing.
    pub fetch: &'a dyn Fn(&BlobRef) -> Option<Arc<[u8]>>,
    /// What the provider documents. A part over its inline limit for its type
    /// becomes its stand-in with the reason, rather than a refused request.
    pub limits: crate::files::MediaLimits,
    /// Why a part went inline rather than by file id, said beside a part the
    /// inline limit held back: zero data retention, or uploads switched off.
    /// Empty when the provider has no file storage to name.
    pub why_inline: &'a str,
}

/// What hydration did, for the run's log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HydrationReport {
    /// Blocks sent with their bytes.
    pub sent: usize,
    /// Blocks sent as text, through the bypass.
    pub as_text: usize,
    /// Blocks sent as their stand-in because the model does not take them.
    pub stand_ins: usize,
    /// Blocks sent as their stand-in because the request was over its
    /// media-byte cap.
    pub capped: usize,
    /// Hashes whose bytes could not be read.
    pub missing: Vec<String>,
    /// Blocks sent by the id of the vendor's copy.
    pub by_file: usize,
    /// Blocks sent as their stand-in because they were over the provider's
    /// inline limit for their type.
    pub too_large: usize,
}

/// What one block should become.
enum Fate {
    Bytes,
    Remote,
    TooLarge(u64),
    Text,
    StandIn,
}

/// Whether the model is sent `block`'s bytes natively (inline or by file id),
/// as opposed to text or its stand-in. What the runtime asks before it
/// uploads anything.
pub fn sends_natively(block: &ContentBlock, mime: &ModelMime) -> bool {
    let ContentBlock::Mime { part, deliver, .. } = block else {
        return false;
    };
    match deliver {
        Some(Delivery::Text) | Some(Delivery::StandIn) => false,
        Some(Delivery::Native) | None => mime.accepts(&part.mime_type),
    }
}

fn fate(block: &ContentBlock, h: &Hydration<'_>) -> Fate {
    let ContentBlock::Mime {
        part,
        deliver,
        remote,
        ..
    } = block
    else {
        return Fate::StandIn;
    };
    if sends_natively(block, h.mime) {
        return match (remote, h.limits.inline_part_limit(&part.mime_type)) {
            (Some(_), _) => Fate::Remote,
            (None, Some(max)) if part.size > max => Fate::TooLarge(max),
            (None, _) => Fate::Bytes,
        };
    }
    match deliver {
        Some(Delivery::StandIn) => Fate::StandIn,
        _ if h.registry.info(&part.mime_type).text => Fate::Text,
        Some(Delivery::Text) => Fate::Text,
        _ => Fate::StandIn,
    }
}

/// A byte count the way a person reads one: `5.0 MiB`.
fn human_bytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / crate::files::MIB as f64)
}

/// Fill every mime block in `request` with what the model should get.
pub fn hydrate_request(request: &mut InferenceRequest, h: &Hydration<'_>) -> HydrationReport {
    let mut report = HydrationReport::default();
    // Sum the raw bytes that would be sent natively, so the cap can shed the
    // oldest first: messages are in conversation order, so the first blocks
    // seen are the oldest. The newest media is the media a stage most likely
    // still needs, so it is what the cap keeps.
    let total_bytes: u64 = request
        .messages
        .iter()
        .flat_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => blocks.iter().collect::<Vec<_>>(),
            MessageContent::Text(_) => Vec::new(),
        })
        .map(|b| match (fate(b, h), b) {
            (Fate::Bytes, ContentBlock::Mime { part, .. }) => part.size,
            _ => 0,
        })
        .sum();
    let mut to_shed = total_bytes.saturating_sub(h.max_media_bytes);
    for message in &mut request.messages {
        let MessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        for block in blocks.iter_mut() {
            let ContentBlock::Mime {
                part,
                name,
                deliver,
                ..
            } = &*block
            else {
                continue;
            };
            let (part, name, deliver) = (part.clone(), name.clone(), *deliver);
            let outcome = match fate(block, h) {
                Fate::Bytes if to_shed > 0 => {
                    to_shed = to_shed.saturating_sub(part.size);
                    report.capped += 1;
                    Fate::StandIn
                }
                other => other,
            };
            match outcome {
                Fate::Remote => report.by_file += 1,
                Fate::TooLarge(max) => {
                    report.too_large += 1;
                    let why = match h.why_inline {
                        "" => String::new(),
                        why => format!("; {why}"),
                    };
                    *block = ContentBlock::Text {
                        text: format!(
                            "{} [not sent: {} is over the {} this provider takes inline{why}]",
                            part.stand_in,
                            human_bytes(part.size),
                            human_bytes(max)
                        ),
                    };
                }
                Fate::Bytes => match (h.fetch)(&part) {
                    Some(bytes) => {
                        *block = ContentBlock::Mime {
                            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                            part,
                            name,
                            deliver,
                            remote: None,
                        };
                        report.sent += 1;
                    }
                    None => {
                        report.missing.push(part.sha256.clone());
                        *block = ContentBlock::Text {
                            text: part.stand_in.clone(),
                        };
                    }
                },
                Fate::Text => match (h.fetch)(&part) {
                    Some(bytes) => match std::str::from_utf8(&bytes) {
                        Ok(text) => {
                            *block = ContentBlock::Text {
                                text: text.to_string(),
                            };
                            report.as_text += 1;
                        }
                        Err(_) => {
                            report.stand_ins += 1;
                            *block = ContentBlock::Text {
                                text: part.stand_in.clone(),
                            };
                        }
                    },
                    None => {
                        report.missing.push(part.sha256.clone());
                        *block = ContentBlock::Text {
                            text: part.stand_in.clone(),
                        };
                    }
                },
                Fate::StandIn => {
                    report.stand_ins += 1;
                    *block = ContentBlock::Text {
                        text: part.stand_in.clone(),
                    };
                }
            }
        }
    }
    report
}

/// One content block's share of a request's token estimate: text by the
/// byte heuristic, a mime block by its registry estimate whether or not it
/// carries bytes yet.
pub fn block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => leviath_core::estimate_tokens(text),
        ContentBlock::ToolUse { name, input, .. } => {
            leviath_core::estimate_tokens(name) + leviath_core::estimate_tokens(&input.to_string())
        }
        ContentBlock::ToolResult { content, .. } => leviath_core::estimate_tokens(content),
        ContentBlock::Mime { part, .. } => part.tokens,
    }
}

/// The tokens the mime blocks of `request` are estimated to cost, from
/// their registry estimates. The text guard counts only text; this is the
/// rest.
pub fn mime_tokens(request: &InferenceRequest) -> usize {
    request
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::Mime {
                part, data, remote, ..
            } if !data.is_empty() || remote.is_some() => Some(part.tokens),
            _ => None,
        })
        .sum()
}

/// A mime block as OpenAI's Chat Completions content part, or a text part
/// carrying the stand-in for a family that shape has no slot for.
pub fn openai_part(block: &ContentBlock) -> Option<serde_json::Value> {
    let ContentBlock::Mime {
        part,
        data,
        name,
        remote,
        ..
    } = block
    else {
        return None;
    };
    if let (Some(file), Family::Document) = (remote, family_of(&part.mime_type)) {
        return Some(serde_json::json!({ "type": "file", "file": { "file_id": file.id } }));
    }
    if data.is_empty() {
        return Some(serde_json::json!({ "type": "text", "text": part.stand_in }));
    }
    Some(match family_of(&part.mime_type) {
        Family::Image => serde_json::json!({
            "type": "image_url",
            "image_url": { "url": data_uri(&part.mime_type, data) },
        }),
        Family::Audio => serde_json::json!({
            "type": "input_audio",
            "input_audio": { "data": data, "format": audio_format(&part.mime_type) },
        }),
        Family::Document => serde_json::json!({
            "type": "file",
            "file": {
                "filename": name.clone().unwrap_or_else(|| "document.pdf".to_string()),
                "file_data": data_uri(&part.mime_type, data),
            },
        }),
        Family::Video | Family::Other => {
            serde_json::json!({ "type": "text", "text": part.stand_in })
        }
    })
}

/// A mime block as an Anthropic content block: `image` or `document`, or a
/// `text` block carrying the stand-in for a family it has no block for.
pub fn anthropic_block(block: &ContentBlock) -> Option<serde_json::Value> {
    let ContentBlock::Mime {
        part, data, remote, ..
    } = block
    else {
        return None;
    };
    let source = match remote {
        Some(file) => serde_json::json!({ "type": "file", "file_id": file.id }),
        None if data.is_empty() => {
            return Some(serde_json::json!({ "type": "text", "text": part.stand_in }));
        }
        None => serde_json::json!({
            "type": "base64",
            "media_type": part.mime_type,
            "data": data,
        }),
    };
    Some(match family_of(&part.mime_type) {
        Family::Image => serde_json::json!({ "type": "image", "source": source }),
        Family::Document => serde_json::json!({ "type": "document", "source": source }),
        Family::Audio | Family::Video | Family::Other => {
            serde_json::json!({ "type": "text", "text": part.stand_in })
        }
    })
}

/// A mime block as a Responses content part: `input_image`,
/// `input_file`, or `input_text` carrying the stand-in.
pub fn responses_part(block: &ContentBlock) -> Option<serde_json::Value> {
    let ContentBlock::Mime {
        part,
        data,
        name,
        remote,
        ..
    } = block
    else {
        return None;
    };
    if let Some(file) = remote {
        let kind = match family_of(&part.mime_type) {
            Family::Image => "input_image",
            Family::Audio => "input_audio",
            Family::Video => "input_video",
            Family::Document | Family::Other => "input_file",
        };
        return Some(serde_json::json!({ "type": kind, "file_id": file.id }));
    }
    if data.is_empty() {
        return Some(serde_json::json!({ "type": "input_text", "text": part.stand_in }));
    }
    Some(match family_of(&part.mime_type) {
        Family::Image => serde_json::json!({
            "type": "input_image",
            "image_url": data_uri(&part.mime_type, data),
        }),
        Family::Document => serde_json::json!({
            "type": "input_file",
            "filename": name.clone().unwrap_or_else(|| "document.pdf".to_string()),
            "file_data": data_uri(&part.mime_type, data),
        }),
        // Only a route whose shape carries these is ever handed their bytes
        // (Meta's); every other Responses route gets the stand-in, because its
        // shape takes audio and video out of what the model is said to accept
        // before anything is read.
        Family::Audio => serde_json::json!({
            "type": "input_audio",
            "input_audio": { "data": data, "format": audio_format(&part.mime_type) },
        }),
        Family::Video => serde_json::json!({
            "type": "input_video",
            "video_url": data_uri(&part.mime_type, data),
        }),
        Family::Other => serde_json::json!({ "type": "input_text", "text": part.stand_in }),
    })
}

/// A mime block as a Gemini Interactions content item: the part by the
/// vendor's uri when it was uploaded, else its bytes, else a text item
/// carrying the stand-in.
pub fn gemini_part(block: &ContentBlock) -> Option<serde_json::Value> {
    let ContentBlock::Mime {
        part, data, remote, ..
    } = block
    else {
        return None;
    };
    let kind = match family_of(&part.mime_type) {
        Family::Image => "image",
        Family::Audio => "audio",
        Family::Video => "video",
        Family::Document => "document",
        Family::Other => return Some(serde_json::json!({ "type": "text", "text": part.stand_in })),
    };
    match (
        remote.as_ref().and_then(|f| f.uri.as_deref()),
        data.is_empty(),
    ) {
        (Some(uri), _) => Some(serde_json::json!({
            "type": kind, "uri": uri, "mime_type": part.mime_type,
        })),
        (None, false) => Some(serde_json::json!({
            "type": kind, "data": data, "mime_type": part.mime_type,
        })),
        (None, true) => Some(serde_json::json!({ "type": "text", "text": part.stand_in })),
    }
}

/// The `format` word OpenAI's `input_audio` part takes for a type.
fn audio_format(mime_type: &MimeType) -> &'static str {
    match mime_type.subtype() {
        "mpeg" | "mp3" => "mp3",
        _ => "wav",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use leviath_core::mime::{Blob, Part};

    fn reg() -> MimeRegistry {
        MimeRegistry::builtin()
    }

    fn png_part() -> Part {
        let blob = Blob::new(
            MimeType::parse("image/png").unwrap(),
            b"\x89PNG\r\n\x1a\nbody".to_vec(),
        )
        .named("hero.png");
        Part::stored(blob.describe(&reg())).named("hero.png")
    }

    fn obj_part() -> Part {
        let blob = Blob::new(MimeType::parse("model/obj").unwrap(), b"v 1 2 3\n".to_vec())
            .named("cube.obj");
        Part::stored(blob.describe(&reg())).named("cube.obj")
    }

    fn request(blocks: Vec<ContentBlock>) -> InferenceRequest {
        InferenceRequest {
            system: Vec::new(),
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: MessageContent::Text("plain".to_string()),
                    cache_breakpoint: false,
                    reasoning: None,
                },
                Message {
                    role: "user".to_string(),
                    content: MessageContent::Blocks(blocks),
                    cache_breakpoint: false,
                    reasoning: None,
                },
            ],
            model: "m".to_string(),
            max_tokens: 10,
            temperature: 0.0,
            tools: Vec::new(),
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    /// The blocks of a message, or none for plain text.
    fn blocks_of(content: &MessageContent) -> &[ContentBlock] {
        match content {
            MessageContent::Blocks(blocks) => blocks,
            MessageContent::Text(_) => &[],
        }
    }

    /// `block` carrying `data`, as hydration would leave it.
    fn with_data(block: ContentBlock, data: &str) -> ContentBlock {
        match block {
            ContentBlock::Mime {
                part,
                name,
                deliver,
                ..
            } => ContentBlock::Mime {
                part,
                data: data.to_string(),
                name,
                deliver,
                remote: None,
            },
            other => other,
        }
    }

    fn bytes_of(parts: &[Part]) -> impl Fn(&BlobRef) -> Option<Arc<[u8]>> + '_ {
        move |r| {
            parts
                .iter()
                .find(|p| p.blob().is_some_and(|b| b.sha256 == r.sha256))
                .map(|p| match p.name.as_deref() {
                    Some("cube.obj") => Arc::from(b"v 1 2 3\n".as_slice()),
                    _ => Arc::from(b"\x89PNG\r\n\x1a\nbody".as_slice()),
                })
        }
    }

    #[test]
    fn every_block_kind_has_a_token_share() {
        assert_eq!(
            block_tokens(&ContentBlock::Text {
                text: "abcd".into()
            }),
            1
        );
        assert_eq!(
            block_tokens(&ContentBlock::ToolUse {
                id: "i".into(),
                name: "name".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            }),
            2
        );
        assert_eq!(
            block_tokens(&ContentBlock::ToolResult {
                tool_use_id: "i".into(),
                content: "abcdefgh".into(),
                is_error: false,
            }),
            2
        );
        assert_eq!(
            block_tokens(&ContentBlock::mime(&png_part()).unwrap()),
            1600
        );
    }

    #[test]
    fn families_and_uris() {
        assert_eq!(
            family_of(&MimeType::parse("image/webp").unwrap()),
            Family::Image
        );
        assert_eq!(
            family_of(&MimeType::parse("audio/wav").unwrap()),
            Family::Audio
        );
        assert_eq!(
            family_of(&MimeType::parse("video/mp4").unwrap()),
            Family::Video
        );
        assert_eq!(
            family_of(&MimeType::parse("application/pdf").unwrap()),
            Family::Document
        );
        assert_eq!(
            family_of(&MimeType::parse("model/obj").unwrap()),
            Family::Other
        );
        assert_eq!(
            data_uri(&MimeType::parse("image/png").unwrap(), "AAAA"),
            "data:image/png;base64,AAAA"
        );
        assert_eq!(audio_format(&MimeType::parse("audio/mpeg").unwrap()), "mp3");
        assert_eq!(audio_format(&MimeType::parse("audio/wav").unwrap()), "wav");
    }

    #[test]
    fn a_vision_model_gets_the_bytes_and_a_text_model_the_stand_in() {
        let parts = [png_part()];
        let block = ContentBlock::mime(&parts[0]).unwrap();
        assert!(!block.is_hydrated_mime());
        assert_eq!(block.stand_in(), Some("[image/png, 12 B] hero.png"));
        let fetch = bytes_of(&parts);
        let vision = ModelMime::new(&["text/*", "image/*"], &["text/*"]);
        let mut req = request(vec![
            ContentBlock::Text { text: "see".into() },
            block.clone(),
        ]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &vision,
                registry: &reg(),
                max_media_bytes: 1024,
                fetch: &fetch,
            },
        );
        assert_eq!(report.sent, 1);
        assert!(req.messages[1].content.as_text().contains("see"));
        let blocks = blocks_of(&req.messages[1].content);
        assert!(blocks[1].is_hydrated_mime());
        assert_eq!(mime_tokens(&req), 1600);
        assert!(blocks[1].stand_in().is_some());
        assert_eq!(ContentBlock::Text { text: "x".into() }.stand_in(), None);

        let text_only = ModelMime::text_only();
        let mut req = request(vec![block.clone()]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &text_only,
                registry: &reg(),
                max_media_bytes: 1024,
                fetch: &fetch,
            },
        );
        assert_eq!(report.stand_ins, 1);
        assert_eq!(
            req.messages[1].content.as_text(),
            "[image/png, 12 B] hero.png"
        );
        assert_eq!(mime_tokens(&req), 0);
    }

    #[test]
    fn the_text_bypass_and_the_delivery_overrides() {
        let parts = [obj_part(), png_part()];
        let fetch = bytes_of(&parts);
        let text_only = ModelMime::text_only();
        let obj = ContentBlock::mime(&parts[0]).unwrap();
        let mut req = request(vec![obj]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &text_only,
                registry: &reg(),
                max_media_bytes: 1024,
                fetch: &fetch,
            },
        );
        assert_eq!(report.as_text, 1);
        assert_eq!(req.messages[1].content.as_text(), "v 1 2 3\n");

        // A PNG forced to text is not UTF-8, so it degrades to the stand-in.
        let forced = ContentBlock::mime(&parts[1].clone().delivered(Delivery::Text)).unwrap();
        let mut req = request(vec![forced]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &text_only,
                registry: &reg(),
                max_media_bytes: 1024,
                fetch: &fetch,
            },
        );
        assert_eq!(report.stand_ins, 1);

        // Stand-in on request, even for a model that sees.
        let vision = ModelMime::new(&["text/*", "image/*"], &["text/*"]);
        let quiet = ContentBlock::mime(&parts[1].clone().delivered(Delivery::StandIn)).unwrap();
        let mut req = request(vec![quiet]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &vision,
                registry: &reg(),
                max_media_bytes: 1024,
                fetch: &fetch,
            },
        );
        assert_eq!(report.stand_ins, 1);

        // Native on request behaves like the default.
        let native = ContentBlock::mime(&parts[1].clone().delivered(Delivery::Native)).unwrap();
        let mut req = request(vec![native]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &vision,
                registry: &reg(),
                max_media_bytes: 1024,
                fetch: &fetch,
            },
        );
        assert_eq!(report.sent, 1);
    }

    #[test]
    fn missing_bytes_and_the_cap_degrade_to_stand_ins() {
        let parts = [png_part(), obj_part()];
        let none = |_: &BlobRef| -> Option<Arc<[u8]>> { None };
        let vision = ModelMime::new(&["text/*", "image/*"], &["text/*"]);
        let mut req = request(vec![
            ContentBlock::mime(&parts[0]).unwrap(),
            ContentBlock::mime(&parts[1]).unwrap(),
        ]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &vision,
                registry: &reg(),
                max_media_bytes: 1024,
                fetch: &none,
            },
        );
        assert_eq!(report.missing.len(), 2);
        assert!(req.messages[1].content.as_text().contains("hero.png"));

        let fetch = bytes_of(&parts);
        let mut req = request(vec![
            ContentBlock::mime(&parts[0]).unwrap(),
            ContentBlock::mime(&parts[0]).unwrap(),
            ContentBlock::mime(&parts[0]).unwrap(),
        ]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &vision,
                registry: &reg(),
                max_media_bytes: 12,
                fetch: &fetch,
            },
        );
        assert_eq!(report.capped, 2);
        assert_eq!(report.sent, 1);
        let blocks = blocks_of(&req.messages[1].content);
        assert!(!blocks[0].is_hydrated_mime() && blocks[2].is_hydrated_mime());
    }

    #[test]
    fn the_cap_sheds_by_bytes_not_by_count() {
        use leviath_core::mime::Blob;
        // An accepted part of a chosen byte size, so the byte cap can be tested
        // against real sizes rather than a count.
        let sized = |n: usize| {
            let blob =
                Blob::new(MimeType::parse("image/png").unwrap(), vec![7u8; n]).named("m.png");
            Part::stored(blob.describe(&reg())).named("m.png")
        };
        let fetch = |r: &BlobRef| -> Option<Arc<[u8]>> {
            Some(Arc::from(vec![7u8; r.size as usize].as_slice()))
        };
        let vision = ModelMime::new(&["text/*", "image/*"], &["text/*"]);
        let small = sized(12);
        let big = sized(1000);

        // A cap below the large part: the small old part is shed to make room,
        // the large new part still does not fit, so both go as stand-ins. A
        // count cap of one would have kept the newest; the byte cap keeps none.
        let mut req = request(vec![
            ContentBlock::mime(&small).unwrap(),
            ContentBlock::mime(&big).unwrap(),
        ]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &vision,
                registry: &reg(),
                max_media_bytes: 500,
                fetch: &fetch,
            },
        );
        assert_eq!(report.capped, 2);
        assert_eq!(report.sent, 0);

        // A cap above the large part but below the sum: the small old part is
        // shed and the large new part is kept - oldest first, by bytes.
        let mut req = request(vec![
            ContentBlock::mime(&small).unwrap(),
            ContentBlock::mime(&big).unwrap(),
        ]);
        let report = hydrate_request(
            &mut req,
            &Hydration {
                limits: crate::files::MediaLimits::NONE,
                why_inline: "",
                mime: &vision,
                registry: &reg(),
                max_media_bytes: 1000,
                fetch: &fetch,
            },
        );
        assert_eq!(report.capped, 1);
        assert_eq!(report.sent, 1);
        let blocks = blocks_of(&req.messages[1].content);
        assert!(!blocks[0].is_hydrated_mime() && blocks[1].is_hydrated_mime());
    }

    #[test]
    fn vendor_shapes() {
        let parts = [png_part(), obj_part()];
        let png = ContentBlock::mime(&parts[0]).unwrap();
        assert_eq!(openai_part(&png).unwrap()["type"], "text");
        assert_eq!(anthropic_block(&png).unwrap()["type"], "text");
        assert_eq!(responses_part(&png).unwrap()["type"], "input_text");
        assert_eq!(blocks_of(&MessageContent::Text("t".into())).len(), 0);
        let text = ContentBlock::Text { text: "t".into() };
        assert_eq!(with_data(text.clone(), "x"), text);
        let png = with_data(png, "AAAA");
        assert_eq!(openai_part(&png).unwrap()["type"], "image_url");
        assert_eq!(
            openai_part(&png).unwrap()["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(anthropic_block(&png).unwrap()["type"], "image");
        // Anthropic's field is `media_type`; any other name is refused with
        // "source.media_type: Field required".
        assert_eq!(
            anthropic_block(&png).unwrap()["source"]["media_type"],
            "image/png"
        );
        assert!(
            anthropic_block(&png).unwrap()["source"]
                .get("mime_type")
                .is_none()
        );
        assert_eq!(responses_part(&png).unwrap()["type"], "input_image");
        assert!(openai_part(&ContentBlock::Text { text: "x".into() }).is_none());
        assert!(anthropic_block(&ContentBlock::Text { text: "x".into() }).is_none());
        assert!(responses_part(&ContentBlock::Text { text: "x".into() }).is_none());

        let make = |t: &str, name: &str| {
            let blob = Blob::new(MimeType::parse(t).unwrap(), vec![1, 2, 3]).named(name);
            let part = Part::stored(blob.describe(&reg())).named(name);
            with_data(ContentBlock::mime(&part).unwrap(), "AQID")
        };
        let wav = make("audio/wav", "clip.wav");
        assert_eq!(openai_part(&wav).unwrap()["input_audio"]["format"], "wav");
        assert_eq!(anthropic_block(&wav).unwrap()["type"], "text");
        assert_eq!(responses_part(&wav).unwrap()["type"], "input_audio");
        assert_eq!(
            responses_part(&wav).unwrap()["input_audio"]["format"],
            "wav"
        );
        let pdf = make("application/pdf", "spec.pdf");
        assert_eq!(openai_part(&pdf).unwrap()["file"]["filename"], "spec.pdf");
        assert_eq!(anthropic_block(&pdf).unwrap()["type"], "document");
        assert_eq!(responses_part(&pdf).unwrap()["type"], "input_file");
        let mp4 = make("video/mp4", "a.mp4");
        assert_eq!(openai_part(&mp4).unwrap()["type"], "text");
        assert_eq!(responses_part(&mp4).unwrap()["type"], "input_video");
        assert_eq!(
            responses_part(&mp4).unwrap()["video_url"],
            "data:video/mp4;base64,AQID"
        );
        let other = make("model/gltf-binary", "a.glb");
        assert_eq!(responses_part(&other).unwrap()["type"], "input_text");
        let unnamed_pdf = {
            let blob = Blob::new(MimeType::parse("application/pdf").unwrap(), vec![1]);
            with_data(
                ContentBlock::mime(&Part::stored(blob.describe(&reg()))).unwrap(),
                "AQ==",
            )
        };
        assert_eq!(
            openai_part(&unnamed_pdf).unwrap()["file"]["filename"],
            "document.pdf"
        );
        assert_eq!(
            responses_part(&unnamed_pdf).unwrap()["filename"],
            "document.pdf"
        );
        assert!(ContentBlock::mime(&Part::text("t")).is_none());
    }
}

#[cfg(test)]
#[path = "mime/remote_tests.rs"]
mod remote_tests;
