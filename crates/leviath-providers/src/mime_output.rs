//! Mime a model hands back, read off an OpenAI-shaped message.
//!
//! The counterpart to [`crate::mime`], which puts mime into a request.
//! A model that draws answers with data URIs: OpenRouter puts them under
//! `message.images` (and the same key on a streamed `delta`), and an
//! endpoint that speaks the content-array form puts `image_url` items in
//! `content`. Both are read here, decoded, and typed by the URI's own
//! mime type, so a provider hands the runtime bytes it can store.

use base64::Engine;
use leviath_core::mime::{Blob, MimeRegistry, MimeType, sha256_hex};

/// The blob a `data:<type>;base64,<bytes>` URI carries, or `None` for a
/// URI of any other shape, an undecodable payload, or no bytes at all. A
/// plain `https://` URL is not fetched: the provider is the one thing that
/// must not start reading arbitrary addresses on a model's say-so.
pub fn decode_data_uri(uri: &str) -> Option<Blob> {
    let rest = uri.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let mime_type = header.strip_suffix(";base64")?;
    let mime_type = MimeType::parse(mime_type).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(Blob::new(mime_type, bytes))
}

/// Every image an OpenAI-shaped `message` (or streamed `delta`) carries,
/// named `image-<sha12>.<ext>` after the first twelve hex of the bytes'
/// sha256, from `images[].image_url.url` and from `image_url` items in a
/// content array. Text content carries none.
///
/// Naming by content, not by position, gives each produced image a stable,
/// unique handle a later stage can point at (pick this one, drop that one):
/// two images the model draws in different turns never collide on
/// `image-1.png`, and byte-identical images share one name, which is what
/// the run's blob store already dedupes them to.
pub fn message_blobs(message: &serde_json::Value) -> Vec<Blob> {
    let registry = MimeRegistry::builtin();
    let mut blobs = Vec::new();
    let items = message
        .get("images")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .chain(
            message
                .get("content")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten(),
        );
    for item in items {
        let Some(url) = item
            .get("image_url")
            .and_then(|i| i.get("url"))
            .and_then(|u| u.as_str())
        else {
            continue;
        };
        let Some(blob) = decode_data_uri(url) else {
            continue;
        };
        let ext = registry
            .info(&blob.mime_type)
            .extensions
            .first()
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        // The first twelve hex of the sha, taken char by char: no string
        // slice (which clippy forbids for its UTF-8 panic) and no Option whose
        // None arm no 64-char hex string could ever reach.
        let sha12: String = sha256_hex(&blob.bytes).chars().take(12).collect();
        blobs.push(blob.named(format!("image-{sha12}{ext}")));
    }
    blobs
}

/// Decode an OpenRouter `message.audio.data` payload, naming the result by
/// its content hash so multiple generated parts do not collide in a run. The
/// stage's declared audio MIME takes precedence, allowing custom and future
/// output types without adding them to this provider crate.
pub fn message_audio_blob(
    message: &serde_json::Value,
    requested_mime: Option<&str>,
) -> Option<Blob> {
    let audio = message.get("audio")?;
    let data = audio.get("data")?.as_str()?;
    let returned_mime = audio
        .get("mime_type")
        .or_else(|| audio.get("content_type"))
        .and_then(serde_json::Value::as_str)
        .filter(|mime| mime.starts_with("audio/") && !mime.ends_with("/*"));
    let declared_mime =
        requested_mime.filter(|mime| mime.starts_with("audio/") && !mime.ends_with("/*"));
    let format_mime = audio
        .get("format")
        .and_then(serde_json::Value::as_str)
        .filter(|format| !format.trim().is_empty())
        .map(|format| {
            let format = format.trim();
            if format.contains('/') {
                format.to_string()
            } else {
                format!("audio/{format}")
            }
        });
    let said = returned_mime
        .or(declared_mime)
        .map(str::to_string)
        .or(format_mime);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    // The MIME may be user supplied or introduced by a future model.
    let mime_type = audio_type_of(&bytes, said.as_deref())?;
    let sha12: String = sha256_hex(&bytes).chars().take(12).collect();
    Some(Blob::new(mime_type, bytes).named(format!("audio-{sha12}")))
}

/// Assemble base64 audio pieces from an OpenAI-shaped audio stream into one
/// stored part. Providers may emit each piece as its own padded base64 value,
/// or split one base64 value across several deltas; accept either framing.
pub fn streamed_audio_blob(
    chunks: &[String],
    requested_mime: Option<&str>,
    returned_mime: Option<&str>,
    format: Option<&str>,
) -> Option<Blob> {
    if chunks.is_empty() {
        return None;
    }

    let returned_mime =
        returned_mime.filter(|mime| mime.starts_with("audio/") && !mime.ends_with("/*"));
    let declared_mime =
        requested_mime.filter(|mime| mime.starts_with("audio/") && !mime.ends_with("/*"));
    let format_mime = format
        .filter(|format| !format.trim().is_empty())
        .map(|format| {
            let format = format.trim();
            if format.contains('/') {
                format.to_string()
            } else {
                format!("audio/{format}")
            }
        });
    let said = returned_mime
        .or(declared_mime)
        .map(str::to_string)
        .or(format_mime);

    let individually_decoded = chunks
        .iter()
        .map(|chunk| base64::engine::general_purpose::STANDARD.decode(chunk.trim()))
        .collect::<Result<Vec<_>, _>>()
        .ok();
    let bytes = individually_decoded
        .map(|decoded| decoded.into_iter().flatten().collect())
        .or_else(|| {
            base64::engine::general_purpose::STANDARD
                .decode(chunks.concat().trim())
                .ok()
        })?;
    if bytes.is_empty() {
        return None;
    }
    let mime_type = audio_type_of(&bytes, said.as_deref())?;
    let sha12: String = sha256_hex(&bytes).chars().take(12).collect();
    Some(Blob::new(mime_type, bytes).named(format!("audio-{sha12}")))
}

/// The type audio bytes are: what their leading bytes say when they say an
/// audio type, else `said` (the reply's label, the stage's declared type, or
/// the format Leviath asked for), else `None`. A gateway asked for `wav` can
/// answer with an MP3, and a part labelled by the request came out
/// `audio/wav` over MPEG bytes.
fn audio_type_of(bytes: &[u8], said: Option<&str>) -> Option<MimeType> {
    sniff_audio(bytes).or_else(|| MimeType::parse(said?).ok())
}

/// The audio type the leading bytes identify: a builtin registry row's
/// magic (WAV, ID3-tagged MP3, Ogg, FLAC), or a bare MPEG audio frame,
/// which has no fixed prefix: eleven sync bits, then any layer but `00`
/// (that is AAC's ADTS framing, not MPEG audio). `None` for anything else.
pub fn sniff_audio(bytes: &[u8]) -> Option<MimeType> {
    if let Some(sniffed) = MimeRegistry::builtin()
        .sniff(bytes)
        .filter(|t| t.as_str().starts_with("audio/"))
    {
        return Some(sniffed);
    }
    match bytes {
        [0xFF, second, ..] if second & 0xE0 == 0xE0 && second & 0x06 != 0 => {
            MimeType::parse("audio/mpeg").ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "data:image/png;base64,iVBORw0KGgo=";

    #[test]
    fn a_data_uri_decodes_to_a_typed_blob_and_anything_else_does_not() {
        let blob = decode_data_uri(PNG).unwrap();
        assert_eq!(blob.mime_type.as_str(), "image/png");
        assert_eq!(blob.bytes, b"\x89PNG\r\n\x1a\n");
        for bad in [
            "https://example.com/a.png",
            "data:image/png,plain",
            "data:image/png;base64,!!",
            "data:image/png;base64,",
            "data:not a type;base64,AQID",
            "data:image/png;base64",
        ] {
            assert!(decode_data_uri(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_message_yields_its_images_named_by_content_sha() {
        let message = serde_json::json!({
            "content": [
                {"type": "text", "text": "here"},
                {"type": "image_url", "image_url": {"url": PNG}},
                {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
                {"type": "image_url"}
            ],
            "images": [
                {"type": "image_url", "image_url": {"url": "data:image/webp;base64,AQID"}}
            ]
        });
        let blobs = message_blobs(&message);
        assert_eq!(blobs.len(), 2);
        // images first, then content: the webp, then the png. Each name is
        // image-<first 12 hex of the bytes' sha256>.<ext>.
        assert_eq!(blobs[0].name.as_deref(), Some("image-039058c6f2c0.webp"));
        assert_eq!(blobs[1].name.as_deref(), Some("image-4c4b6a3be131.png"));
        assert!(message_blobs(&serde_json::json!({"content": "just text"})).is_empty());
        // A type the registry has no extension for is named bare (no dot).
        let odd = serde_json::json!({
            "images": [{"image_url": {"url": "data:image/x-odd;base64,AQID"}}]
        });
        assert_eq!(
            message_blobs(&odd)[0].name.as_deref(),
            Some("image-039058c6f2c0")
        );
    }

    #[test]
    fn byte_identical_images_share_a_name_and_differing_ones_do_not() {
        // Same bytes in two turns collapse to one handle (the store dedupes
        // them); different bytes never collide, whatever their position.
        let same = serde_json::json!({
            "images": [
                {"image_url": {"url": "data:image/png;base64,AQID"}},
                {"image_url": {"url": "data:image/png;base64,AQID"}}
            ]
        });
        let blobs = message_blobs(&same);
        assert_eq!(blobs[0].name, blobs[1].name);
        let differ = serde_json::json!({
            "images": [
                {"image_url": {"url": "data:image/png;base64,AQID"}},
                {"image_url": {"url": PNG}}
            ]
        });
        let blobs = message_blobs(&differ);
        assert_ne!(blobs[0].name, blobs[1].name);
    }

    #[test]
    fn a_message_yields_its_generated_audio_as_a_typed_part() {
        let blob = message_audio_blob(
            &serde_json::json!({ "audio": { "data": "UklGRg==" } }),
            Some("audio/wav"),
        )
        .unwrap();
        assert_eq!(blob.mime_type.as_str(), "audio/wav");
        assert_eq!(blob.bytes, b"RIFF");
        assert!(blob.name.as_deref().unwrap().starts_with("audio-"));
    }

    #[test]
    fn custom_audio_mimes_are_taken_from_the_stage_or_response() {
        let requested = message_audio_blob(
            &serde_json::json!({ "audio": { "data": "AQID", "format": "vendor-codec" } }),
            Some("audio/x-future-model"),
        )
        .unwrap();
        assert_eq!(requested.mime_type.as_str(), "audio/x-future-model");

        let returned = message_audio_blob(
            &serde_json::json!({
                "audio": { "data": "AQID", "mime_type": "audio/vnd.vendor.codec" }
            }),
            None,
        )
        .unwrap();
        assert_eq!(returned.mime_type.as_str(), "audio/vnd.vendor.codec");

        let content_type = message_audio_blob(
            &serde_json::json!({
                "audio": { "data": "AQID", "content_type": "audio/vnd.other.codec" }
            }),
            None,
        )
        .unwrap();
        assert_eq!(content_type.mime_type.as_str(), "audio/vnd.other.codec");

        let format_mime = message_audio_blob(
            &serde_json::json!({ "audio": { "data": "AQID", "format": "audio/x-direct" } }),
            None,
        )
        .unwrap();
        assert_eq!(format_mime.mime_type.as_str(), "audio/x-direct");

        let generic = message_audio_blob(
            &serde_json::json!({ "audio": { "data": "AQID", "format": "x-future-codec" } }),
            Some("audio/*"),
        )
        .unwrap();
        assert_eq!(generic.mime_type.as_str(), "audio/x-future-codec");
    }

    #[test]
    fn streamed_audio_joins_independent_or_split_base64_chunks() {
        let independent = streamed_audio_blob(
            &["UklG".to_string(), "Rg==".to_string()],
            Some("audio/wav"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(independent.bytes, b"RIFF");
        assert_eq!(independent.mime_type.as_str(), "audio/wav");

        let split = streamed_audio_blob(
            &["Ukl".to_string(), "GRg==".to_string()],
            None,
            None,
            Some("wav"),
        )
        .unwrap();
        assert_eq!(split.bytes, b"RIFF");
        assert_eq!(split.mime_type.as_str(), "audio/wav");

        let direct_mime =
            streamed_audio_blob(&["AQID".to_string()], None, None, Some("audio/x-direct")).unwrap();
        assert_eq!(direct_mime.mime_type.as_str(), "audio/x-direct");

        let returned = streamed_audio_blob(
            &["AQID".to_string()],
            Some("audio/x-requested"),
            Some("audio/x-returned"),
            Some("wav"),
        )
        .unwrap();
        assert_eq!(returned.mime_type.as_str(), "audio/x-returned");
    }

    #[test]
    fn audio_is_typed_by_its_bytes_not_by_the_format_asked_for() {
        // An MP3 frame answered to a request for wav is an MP3.
        let streamed = streamed_audio_blob(
            &["//uQZAA=".to_string()],
            Some("audio/wav"),
            Some("audio/wav"),
            Some("wav"),
        )
        .unwrap();
        assert_eq!(streamed.mime_type.as_str(), "audio/mpeg");
        let buffered = message_audio_blob(
            &serde_json::json!({ "audio": { "data": "//uQZAA=", "format": "wav" } }),
            Some("audio/wav"),
        )
        .unwrap();
        assert_eq!(buffered.mime_type.as_str(), "audio/mpeg");
        // Bytes that say what they are need no label at all.
        let unlabelled = streamed_audio_blob(&["SUQzBAA=".to_string()], None, None, None).unwrap();
        assert_eq!(unlabelled.mime_type.as_str(), "audio/mpeg");
        let unlabelled = message_audio_blob(
            &serde_json::json!({ "audio": { "data": "T2dnUwA=" } }),
            None,
        )
        .unwrap();
        assert_eq!(unlabelled.mime_type.as_str(), "audio/ogg");
    }

    #[test]
    fn audio_bytes_are_sniffed_by_magic_or_mpeg_frame_sync() {
        let sniff = |bytes: &[u8]| sniff_audio(bytes).map(|t| t.as_str().to_string());
        assert_eq!(
            sniff(b"RIFF\x24\0\0\0WAVEfmt ").as_deref(),
            Some("audio/wav")
        );
        assert_eq!(sniff(b"ID3\x04\0").as_deref(), Some("audio/mpeg"));
        assert_eq!(sniff(b"\xFF\xFB\x90\x64").as_deref(), Some("audio/mpeg"));
        assert_eq!(sniff(b"\xFF\xF3\x40").as_deref(), Some("audio/mpeg"));
        assert_eq!(sniff(b"OggS\0").as_deref(), Some("audio/ogg"));
        assert_eq!(sniff(b"fLaC\0").as_deref(), Some("audio/flac"));
        // AAC's ADTS sync (layer 00), no sync at all, a RIFF that is not
        // WAVE, and bytes too short or unknown say no audio type.
        assert_eq!(sniff(b"\xFF\xF1\x50"), None);
        assert_eq!(sniff(b"\xFF\x00"), None);
        assert_eq!(sniff(b"RIFF\x24\0\0\0WEBPVP8 "), None);
        assert_eq!(sniff(b"\xFF"), None);
        assert_eq!(sniff(b"\x01\x02\x03"), None);
    }

    #[test]
    fn streamed_audio_rejects_empty_untyped_and_malformed_chunks() {
        assert!(streamed_audio_blob(&[], Some("audio/wav"), None, None).is_none());
        assert!(streamed_audio_blob(&["AQID".to_string()], None, None, None).is_none());
        assert!(streamed_audio_blob(&["!!".to_string()], Some("audio/wav"), None, None).is_none());
        assert!(streamed_audio_blob(&["".to_string()], Some("audio/wav"), None, None).is_none());
        assert!(streamed_audio_blob(&["AQID".to_string()], None, None, Some(" ")).is_none());
        assert!(streamed_audio_blob(&["AQID".to_string()], None, Some("audio/"), None).is_none());
    }

    #[test]
    fn unusable_or_untyped_audio_payloads_are_ignored() {
        for message in [
            serde_json::json!({}),
            serde_json::json!({ "audio": {} }),
            serde_json::json!({ "audio": { "data": 1 } }),
            serde_json::json!({ "audio": { "data": "!!", "format": "x-test" } }),
            serde_json::json!({ "audio": { "data": "", "format": "x-test" } }),
            serde_json::json!({ "audio": { "data": "AQID" } }),
            serde_json::json!({ "audio": { "data": "AQID", "mime_type": "audio/" } }),
        ] {
            assert!(message_audio_blob(&message, None).is_none(), "{message}");
        }
    }
}
