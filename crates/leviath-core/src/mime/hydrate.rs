//! Deciding what a model is sent for each stored part: its bytes, its bytes
//! as text, or its stand-in.
//!
//! Assembly never reads a file: a stored part travels as its [`BlobRef`],
//! so an assembled request, a journal and every snapshot stay small. Right
//! before a request is sent, [`hydrate`] decides per part what the model
//! gets:
//!
//! - the bytes as text, when the registry says the type is text (a `model/obj`
//!   file is UTF-8, so is `text/markdown`) or the part asks for text, whatever
//!   the model declares. A model whose input patterns name `text/*` has no
//!   native block for a text file: an encoder would write its stand-in, and a
//!   vendor handed one as an uploaded file refuses it (Meta's Responses API
//!   answers a `text/markdown` `input_file` with 400 "Invalid upload request");
//! - the bytes, base64, when the model's input patterns cover the type;
//! - the stand-in line otherwise, with the reason when a documented limit is
//!   what held it back.
//!
//! The decision is all this module holds. It takes the model's input mime
//! patterns as a plain list and a function that fetches bytes, and returns one
//! [`Outcome`] per part. Writing an outcome into a request, and the
//! vendor-specific block shapes (an Anthropic `image`, an OpenAI `image_url`,
//! a Responses `input_image`), are the business of whoever owns the request
//! type: `leviath-providers` for a Leviath run.

use std::sync::Arc;

use base64::Engine;

use super::{BlobRef, Delivery, MimeRegistry, MimeType};

/// One mebibyte, for saying a byte count the way a person reads one.
const MIB: u64 = 1024 * 1024;

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

/// The family a mime pattern names: `None` for text and for `*/*`, which
/// every request shape carries; a pattern that names no built-in family is
/// `Other`.
pub fn family_of_pattern(pattern: &str) -> Option<Family> {
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

/// `data:<type>;base64,<bytes>` for a hydrated part.
pub fn data_uri(mime_type: &MimeType, data: &str) -> String {
    format!("data:{mime_type};base64,{data}")
}

/// The largest inline part `table` allows for `mime_type`: the first pattern
/// that matches wins, and `None` means nothing is documented.
pub fn inline_limit(table: &[(&str, u64)], mime_type: &MimeType) -> Option<u64> {
    table
        .iter()
        .find(|(pattern, _)| mime_type.matches(pattern))
        .map(|(_, bytes)| *bytes)
}

/// How much stored media a request may carry, and what fetches its bytes.
pub struct Hydration<'a> {
    /// The mime patterns the model accepts in a request (`text/*`,
    /// `image/*`).
    pub input: &'a [String],
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
    /// The largest inline part the provider documents, by type pattern; the
    /// first match wins. A part over it becomes its stand-in with the reason,
    /// rather than a refused request.
    pub inline_part_bytes: &'a [(&'a str, u64)],
    /// Types the provider takes only by file id: a part of one that was not
    /// uploaded becomes its stand-in with the reason, never inline bytes.
    pub file_only: &'a [&'a str],
    /// Why a part went inline rather than by file id, said beside a part the
    /// inline limit held back: zero data retention, or uploads switched off.
    /// Empty when the provider has no file storage to name.
    pub why_inline: &'a str,
}

/// One stored part of a request, as hydration sees it.
#[derive(Debug, Clone, Copy)]
pub struct MimeSlot<'a> {
    /// What the part is.
    pub part: &'a BlobRef,
    /// The part's delivery override, when it has one.
    pub deliver: Option<Delivery>,
    /// Whether the vendor already holds a copy that the request names by id.
    pub remote: bool,
}

/// What one stored part becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its bytes, base64.
    Data(String),
    /// The vendor's copy, named by id: the part is sent as it is.
    Remote,
    /// Text in its place: the part's own UTF-8, its stand-in, or its stand-in
    /// with why it was not sent.
    Text(String),
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
    /// Blocks sent as their stand-in because the provider takes their type
    /// only by file id and they were not uploaded.
    pub not_uploaded: usize,
}

/// What one part should become, before its bytes are read.
enum Fate {
    Bytes,
    Remote,
    TooLarge(u64),
    NotUploaded,
    Text,
    StandIn,
}

/// Whether a model accepting `input` is sent a part of this type natively
/// (inline or by file id), as opposed to text or its stand-in. What a caller
/// asks before it uploads anything.
///
/// A type `registry` says is text never is, whatever `input` names and even
/// when the part asks to go natively: no request shape has a native block
/// for a text file, so its text is what the model can read.
pub fn sends_natively(
    part: &BlobRef,
    deliver: Option<Delivery>,
    input: &[String],
    registry: &MimeRegistry,
) -> bool {
    match deliver {
        Some(Delivery::Text) | Some(Delivery::StandIn) => false,
        _ if registry.info(&part.mime_type).text => false,
        Some(Delivery::Native) | None => part.mime_type.matches_any(input),
    }
}

fn fate(slot: &MimeSlot<'_>, h: &Hydration<'_>) -> Fate {
    let part = slot.part;
    if sends_natively(part, slot.deliver, h.input, h.registry) {
        return match (
            slot.remote,
            inline_limit(h.inline_part_bytes, &part.mime_type),
        ) {
            (true, _) => Fate::Remote,
            // A type the provider takes only by file id never goes inline: it
            // is refused with its reason instead. Muse Spark's audio is one,
            // because Meta answers inline audio with a 200 and drops it unread
            // (https://github.com/meta-models/meta-model-cookbook/issues/59).
            (false, _) if part.mime_type.matches_any(h.file_only) => Fate::NotUploaded,
            (false, Some(max)) if part.size > max => Fate::TooLarge(max),
            (false, _) => Fate::Bytes,
        };
    }
    match slot.deliver {
        Some(Delivery::StandIn) => Fate::StandIn,
        _ if h.registry.info(&part.mime_type).text => Fate::Text,
        Some(Delivery::Text) => Fate::Text,
        _ => Fate::StandIn,
    }
}

/// A byte count the way a person reads one: `5.0 MiB`.
fn human_bytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / MIB as f64)
}

/// Decide what each of `slots` becomes, in order, and fetch the bytes of
/// those that are sent.
///
/// `slots` are in conversation order, so the first are the oldest. When the
/// media sent natively adds up to more than
/// [`max_media_bytes`](Hydration::max_media_bytes), the oldest become
/// stand-ins first: the newest media is the media a stage most likely still
/// needs, so it is what the cap keeps.
pub fn hydrate(slots: &[MimeSlot<'_>], h: &Hydration<'_>) -> (Vec<Outcome>, HydrationReport) {
    let mut report = HydrationReport::default();
    let total_bytes: u64 = slots
        .iter()
        .map(|s| match fate(s, h) {
            Fate::Bytes => s.part.size,
            _ => 0,
        })
        .sum();
    let mut to_shed = total_bytes.saturating_sub(h.max_media_bytes);
    let mut outcomes = Vec::with_capacity(slots.len());
    for slot in slots {
        let part = slot.part;
        let fate = match fate(slot, h) {
            Fate::Bytes if to_shed > 0 => {
                to_shed = to_shed.saturating_sub(part.size);
                report.capped += 1;
                Fate::StandIn
            }
            other => other,
        };
        let outcome = match fate {
            Fate::Remote => {
                report.by_file += 1;
                Outcome::Remote
            }
            Fate::TooLarge(max) => {
                report.too_large += 1;
                let why = match h.why_inline {
                    "" => String::new(),
                    why => format!("; {why}"),
                };
                Outcome::Text(format!(
                    "{} [not sent: {} is over the {} this provider takes inline{why}]",
                    part.stand_in,
                    human_bytes(part.size),
                    human_bytes(max)
                ))
            }
            Fate::NotUploaded => {
                report.not_uploaded += 1;
                let why = match h.why_inline {
                    "" => "it could not be uploaded",
                    why => why,
                };
                Outcome::Text(format!(
                    "{} [not sent: this provider takes {} only as an uploaded file, \
                     because inline it is dropped unread \
                     (meta-model-cookbook#59), and {why}]",
                    part.stand_in, part.mime_type
                ))
            }
            Fate::Bytes => match (h.fetch)(part) {
                Some(bytes) => {
                    report.sent += 1;
                    Outcome::Data(base64::engine::general_purpose::STANDARD.encode(&bytes))
                }
                None => {
                    report.missing.push(part.sha256.clone());
                    Outcome::Text(part.stand_in.clone())
                }
            },
            Fate::Text => match (h.fetch)(part) {
                Some(bytes) => match std::str::from_utf8(&bytes) {
                    Ok(text) => {
                        report.as_text += 1;
                        Outcome::Text(text.to_string())
                    }
                    Err(_) => {
                        report.stand_ins += 1;
                        Outcome::Text(part.stand_in.clone())
                    }
                },
                None => {
                    report.missing.push(part.sha256.clone());
                    Outcome::Text(part.stand_in.clone())
                }
            },
            Fate::StandIn => {
                report.stand_ins += 1;
                Outcome::Text(part.stand_in.clone())
            }
        };
        outcomes.push(outcome);
    }
    (outcomes, report)
}

#[cfg(test)]
#[path = "hydrate_tests.rs"]
mod tests;
