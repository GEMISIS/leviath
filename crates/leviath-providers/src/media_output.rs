//! Media a model hands back, read off an OpenAI-shaped message.
//!
//! The counterpart to [`crate::media`], which puts media into a request.
//! A model that draws answers with data URIs: OpenRouter puts them under
//! `message.images` (and the same key on a streamed `delta`), and an
//! endpoint that speaks the content-array form puts `image_url` items in
//! `content`. Both are read here, decoded, and typed by the URI's own
//! media type, so a provider hands the runtime bytes it can store.

use base64::Engine;
use leviath_core::media::{Blob, MediaRegistry, MediaType};

/// The blob a `data:<type>;base64,<bytes>` URI carries, or `None` for a
/// URI of any other shape, an undecodable payload, or no bytes at all. A
/// plain `https://` URL is not fetched: the provider is the one thing that
/// must not start reading arbitrary addresses on a model's say-so.
pub fn decode_data_uri(uri: &str) -> Option<Blob> {
    let rest = uri.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let media_type = header.strip_suffix(";base64")?;
    let media_type = MediaType::parse(media_type).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(Blob::new(media_type, bytes))
}

/// Every image an OpenAI-shaped `message` (or streamed `delta`) carries,
/// named `image-<n>.<ext>` in order, from `images[].image_url.url` and from
/// `image_url` items in a content array. Text content carries none.
pub fn message_blobs(message: &serde_json::Value) -> Vec<Blob> {
    let registry = MediaRegistry::builtin();
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
            .info(&blob.media_type)
            .extensions
            .first()
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        blobs.push(blob.named(format!("image-{}{ext}", blobs.len() + 1)));
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
        assert_eq!(blob.media_type.as_str(), "image/png");
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
    fn a_message_yields_its_images_named_in_order() {
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
        assert_eq!(blobs[0].name.as_deref(), Some("image-1.webp"));
        assert_eq!(blobs[1].name.as_deref(), Some("image-2.png"));
        assert!(message_blobs(&serde_json::json!({"content": "just text"})).is_empty());
        // A type the registry has no extension for is named bare.
        let odd = serde_json::json!({
            "images": [{"image_url": {"url": "data:image/x-odd;base64,AQID"}}]
        });
        assert_eq!(message_blobs(&odd)[0].name.as_deref(), Some("image-1"));
    }
}
