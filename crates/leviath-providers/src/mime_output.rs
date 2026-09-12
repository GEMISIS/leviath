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
}
