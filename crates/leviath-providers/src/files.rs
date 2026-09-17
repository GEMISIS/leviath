//! Vendor file storage: a stored part uploaded once and sent by its id after.
//!
//! A provider with a Files API takes a large part once, keeps it, and lets
//! every later request name it, so a 30 MB PDF crosses the wire once for a
//! whole run rather than on every turn and every retry. The runtime decides
//! when to upload (never under zero data retention, never with `[providers]
//! file_uploads` off) and keeps the ids; this module holds what the
//! providers share for it: the limits each vendor documents, the upload and
//! the delete for the routes that take the same multipart shape, and the
//! reading of what the vendor answered.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use leviath_core::mime::MimeType;

use crate::provider::{ProviderError, Result};

/// One mebibyte.
pub const MIB: u64 = 1024 * 1024;

/// The shortest file lifetime any vendor takes: one hour.
pub const MIN_TTL_SECS: u64 = 3_600;

/// What a vendor documents about the media one request may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaLimits {
    /// Bytes of inline media one request may carry, when documented.
    pub inline_request_bytes: Option<u64>,
    /// The largest inline part, by type pattern; the first match wins.
    pub inline_part_bytes: &'static [(&'static str, u64)],
    /// The largest file the vendor stores, when this model can be sent a
    /// file by id.
    pub file_bytes: Option<u64>,
    /// The types this model can be sent by file id.
    pub file_types: &'static [&'static str],
    /// The longest lifetime an upload may ask for, in seconds.
    pub file_ttl_max_secs: u64,
}

impl MediaLimits {
    /// Nothing documented: the Leviath settings bound everything, and nothing
    /// is uploaded.
    pub const NONE: MediaLimits = MediaLimits {
        inline_request_bytes: None,
        inline_part_bytes: &[],
        file_bytes: None,
        file_types: &[],
        file_ttl_max_secs: MIN_TTL_SECS,
    };

    /// These limits with the file side taken away: what a model whose
    /// request shape cannot name a file is held to.
    pub const fn inline_only(self) -> MediaLimits {
        MediaLimits {
            file_bytes: None,
            file_types: &[],
            ..self
        }
    }

    /// The largest inline part of `mime_type`, when the vendor names one.
    pub fn inline_part_limit(&self, mime_type: &MimeType) -> Option<u64> {
        self.inline_part_bytes
            .iter()
            .find(|(pattern, _)| mime_type.matches(pattern))
            .map(|(_, bytes)| *bytes)
    }

    /// Whether a part of `mime_type` and `size` bytes can go by file id.
    pub fn by_file(&self, mime_type: &MimeType, size: u64) -> bool {
        self.file_bytes.is_some_and(|max| size <= max) && mime_type.matches_any(self.file_types)
    }

    /// The largest single part these limits let through, by file or inline,
    /// when anything is documented.
    pub fn largest_part(&self) -> Option<u64> {
        let inline = self.inline_part_bytes.iter().map(|(_, b)| *b);
        self.file_bytes
            .into_iter()
            .chain(self.inline_request_bytes)
            .chain(inline)
            .max()
    }

    /// `ttl_secs` within what the vendor takes.
    pub fn clamp_ttl(&self, ttl_secs: u64) -> u64 {
        ttl_secs.clamp(MIN_TTL_SECS, self.file_ttl_max_secs.max(MIN_TTL_SECS))
    }
}

const DAY: u64 = 86_400;

/// Anthropic: images, PDFs and plain text by file id; 500 MB a file, kept up
/// to 90 days; 32 MB a request inline and 5 MB an image.
const ANTHROPIC: MediaLimits = MediaLimits {
    inline_request_bytes: Some(32 * MIB),
    inline_part_bytes: &[("image/*", 5 * MIB)],
    file_bytes: Some(500 * MIB),
    file_types: &[
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "application/pdf",
        "text/plain",
    ],
    file_ttl_max_secs: 90 * DAY,
};

/// OpenAI's Responses API: images and PDFs by file id; 512 MB a file, kept up
/// to 30 days; 50 MB a file inline and 20 MB an image.
const OPENAI: MediaLimits = MediaLimits {
    inline_request_bytes: None,
    inline_part_bytes: &[("image/*", 20 * MIB), ("*/*", 50 * MIB)],
    file_bytes: Some(512 * MIB),
    file_types: &["image/*", "application/pdf"],
    file_ttl_max_secs: 30 * DAY,
};

/// Google's Gemini API: images, audio, video and PDFs by file uri; 2 GB a
/// file, kept 48 hours; 100 MB a request inline and 50 MB a PDF.
const GOOGLE: MediaLimits = MediaLimits {
    inline_request_bytes: Some(100 * MIB),
    inline_part_bytes: &[("application/pdf", 50 * MIB)],
    file_bytes: Some(2 * 1024 * MIB),
    file_types: &["image/*", "audio/*", "video/*", "application/pdf"],
    file_ttl_max_secs: 2 * DAY,
};

/// xAI (and Grok, the same API): documents by file id; 512 MB a file, kept up
/// to 30 days; 20 MB an image inline.
const XAI: MediaLimits = MediaLimits {
    inline_request_bytes: None,
    inline_part_bytes: &[("image/*", 20 * MIB)],
    file_bytes: Some(512 * MIB),
    file_types: &["application/pdf", "text/plain"],
    file_ttl_max_secs: 30 * DAY,
};

/// Meta's Model API: images, video, audio and PDFs by file id; 1 GiB a file,
/// kept up to 30 days; 50 MB a request inline.
const META: MediaLimits = MediaLimits {
    inline_request_bytes: Some(50 * MIB),
    inline_part_bytes: &[],
    file_bytes: Some(1024 * MIB),
    file_types: &["image/*", "video/*", "audio/*", "application/pdf"],
    file_ttl_max_secs: 30 * DAY,
};

/// Amazon Bedrock's Converse API: no file storage; 3.75 MB an image, 4.5 MB a
/// document and 25 MB a video inline.
const BEDROCK: MediaLimits = MediaLimits {
    inline_request_bytes: None,
    inline_part_bytes: &[
        ("image/*", 3_750_000),
        ("video/*", 25 * MIB),
        ("*/*", 4_500_000),
    ],
    file_bytes: None,
    file_types: &[],
    file_ttl_max_secs: MIN_TTL_SECS,
};

/// What `provider` documents for its chat models, by registry name. A name
/// with nothing documented answers [`MediaLimits::NONE`].
pub fn provider_limits(provider: &str) -> MediaLimits {
    match provider {
        "anthropic" => ANTHROPIC,
        "openai" => OPENAI,
        "google" => GOOGLE,
        "xai" | "grok" => XAI,
        "meta" => META,
        "bedrock" => BEDROCK,
        _ => MediaLimits::NONE,
    }
}

/// A stored part to upload.
#[derive(Clone)]
pub struct FileUpload {
    /// The bytes.
    pub bytes: Arc<[u8]>,
    /// Its mime type.
    pub mime_type: String,
    /// The file name the vendor is told.
    pub name: String,
    /// The lifetime to ask for, in seconds, before the vendor's own clamp.
    pub ttl_secs: u64,
}

impl std::fmt::Debug for FileUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileUpload")
            .field("bytes", &self.bytes.len())
            .field("mime_type", &self.mime_type)
            .field("name", &self.name)
            .field("ttl_secs", &self.ttl_secs)
            .finish()
    }
}

/// A file a vendor holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFile {
    /// The vendor's id for it.
    pub id: String,
    /// The uri a request names it by, for a vendor that uses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// When the vendor deletes it, Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl RemoteFile {
    /// Whether it still has `margin_secs` to live at `now`. A file with no
    /// expiry lives until it is deleted.
    pub fn usable(&self, now: i64, margin_secs: i64) -> bool {
        self.expires_at.is_none_or(|at| at - margin_secs > now)
    }
}

/// The bytes as a multipart file part, typed and named, without a copy.
pub(crate) fn file_part(upload: &FileUpload) -> Result<reqwest::multipart::Part> {
    let len = upload.bytes.len() as u64;
    let body = reqwest::Body::from(bytes::Bytes::from_owner(ArcBytes(upload.bytes.clone())));
    reqwest::multipart::Part::stream_with_length(body, len)
        .file_name(upload.name.clone())
        .mime_str(&upload.mime_type)
        // A mime type the multipart builder will not take is this machine's
        // mistake, found before anything was sent: not a network failure, and
        // not worth a retry.
        .map_err(|e| ProviderError::Other(format!("building the upload: {e}")))
}

/// An `Arc<[u8]>` the `bytes` crate can own.
struct ArcBytes(Arc<[u8]>);

impl AsRef<[u8]> for ArcBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Unix seconds now.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The file a vendor's upload answer describes: its `id`, and its expiry as
/// Unix seconds or an RFC 3339 time, else `ttl_secs` from now.
pub(crate) fn remote_from(body: &serde_json::Value, ttl_secs: u64) -> Result<RemoteFile> {
    let id = body
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ProviderError::InvalidResponse(format!("an upload with no id: {body}")))?;
    let expires_at = match body.get("expires_at") {
        Some(serde_json::Value::Number(n)) => n.as_i64(),
        Some(serde_json::Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp()),
        _ => None,
    }
    .or(Some(now_secs() + ttl_secs as i64));
    Ok(RemoteFile {
        id: id.to_string(),
        uri: None,
        expires_at,
    })
}

/// A vendor's answer to an upload or a delete, as its JSON body or the error
/// its status names.
pub(crate) async fn read_answer(response: reqwest::Response) -> Result<serde_json::Value> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(crate::responses::client::api_error(status.as_u16(), &body));
    }
    crate::provider::decode_json(response).await
}

/// Whether a failed request failed because a file it named is gone: expired,
/// deleted, or held by another account. The runtime uploads again and retries
/// once on this.
pub fn names_a_missing_file(error: &ProviderError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("file")
        && (text.contains("not found") || text.contains("not_found") || text.contains("404"))
}

/// Upload to an OpenAI-shaped `POST /files`: a `file` part, a `purpose`, and
/// `expires_after` from creation. OpenAI, xAI and Meta all take it.
pub(crate) async fn upload_openai_shape(
    endpoint: &crate::responses::client::Endpoint,
    upload: &FileUpload,
    purpose: &str,
    limits: &MediaLimits,
) -> Result<RemoteFile> {
    let ttl = limits.clamp_ttl(upload.ttl_secs);
    let url = endpoint.url("/files");
    // Checked once here, so the builder below cannot fail.
    file_part(upload)?;
    let response = endpoint
        .send(|client| {
            let form = reqwest::multipart::Form::new()
                .text("purpose", purpose.to_string())
                .text("expires_after[anchor]", "created_at")
                .text("expires_after[seconds]", ttl.to_string())
                .part("file", file_part(upload).expect("checked above"));
            client.post(&url).multipart(form)
        })
        .await?;
    remote_from(&read_answer(response).await?, ttl)
}

/// Delete from an OpenAI-shaped `DELETE /files/{id}`. A file already gone is
/// deleted.
pub(crate) async fn delete_openai_shape(
    endpoint: &crate::responses::client::Endpoint,
    file: &RemoteFile,
) -> Result<()> {
    let url = endpoint.url(&format!("/files/{}", file.id));
    let response = endpoint.send(|client| client.delete(&url)).await?;
    match response.status().as_u16() {
        404 => Ok(()),
        _ => read_answer(response).await.map(|_| ()),
    }
}

#[cfg(test)]
mod tests;
