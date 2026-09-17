//! Gemini's Files API: a resumable upload in two requests, a wait while a
//! large video is processed, and a delete.
//!
//! The upload starts at `POST /upload/v1beta/files`, which answers with the
//! URL to send the bytes to in `x-goog-upload-url`; the bytes then go there
//! with `upload, finalize`. The file is named by its `uri` in a request, and
//! Google deletes it after 48 hours whatever lifetime is asked for.

use std::time::Duration;

use serde_json::{Value, json};

use crate::files::{FileUpload, RemoteFile, read_answer};
use crate::provider::{ProviderError, Result};

use super::GeminiProvider;

/// How long a processing file is waited for before the upload is given up
/// and the part goes inline instead.
const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// The base URL with `/upload` after the host: where uploads go.
pub(super) fn upload_base(base_url: &str) -> String {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("", base_url));
    let prefix = match scheme {
        "" => String::new(),
        scheme => format!("{scheme}://"),
    };
    match rest.split_once('/') {
        Some((host, path)) => format!("{prefix}{host}/upload/{path}"),
        None => format!("{prefix}{rest}/upload"),
    }
}

/// The file an answer describes, with its expiry.
fn remote_of(file: &Value) -> Result<RemoteFile> {
    let name = file
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ProviderError::InvalidResponse(format!("an upload with no name: {file}")))?;
    Ok(RemoteFile {
        id: name.to_string(),
        uri: file.get("uri").and_then(Value::as_str).map(str::to_string),
        expires_at: file
            .get("expirationTime")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp()),
    })
}

/// The file an answer carries: the upload wraps it in `file`, a read of the
/// file is the file itself.
fn unwrapped(answer: Value) -> Value {
    match answer.get("file") {
        Some(file) => file.clone(),
        None => answer,
    }
}

/// A file's processing state: `ACTIVE`, `PROCESSING` or `FAILED`.
fn state_of(file: &Value) -> &str {
    file.get("state")
        .and_then(Value::as_str)
        .unwrap_or("ACTIVE")
}

impl GeminiProvider {
    /// A request to `url` with the key and the operator's headers.
    fn files_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        crate::provider::with_extra_headers(
            self.client
                .request(method, url)
                .header("x-goog-api-key", &self.api_key),
            &self.extra_headers,
        )
    }

    /// Upload `upload` and wait until Google can read it.
    pub(super) async fn upload(&self, upload: &FileUpload, poll: Duration) -> Result<RemoteFile> {
        let len = upload.bytes.len();
        let started = self
            .files_request(
                reqwest::Method::POST,
                &format!("{}/files", upload_base(&self.base_url)),
            )
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header("X-Goog-Upload-Header-Content-Length", len.to_string())
            .header("X-Goog-Upload-Header-Content-Type", &upload.mime_type)
            .json(&json!({ "file": { "display_name": upload.name } }))
            .send()
            .await
            .map_err(|e| ProviderError::transport("starting an upload", &e))?;
        let started = crate::provider::check_http_response(started, None).await?;
        let target = started
            .headers()
            .get("x-goog-upload-url")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("an upload start with no upload URL".into())
            })?;
        let body = reqwest::Body::from(bytes::Bytes::from(upload.bytes.to_vec()));
        let finished = self
            .files_request(reqwest::Method::POST, &target)
            .header("X-Goog-Upload-Offset", "0")
            .header("X-Goog-Upload-Command", "upload, finalize")
            .header("Content-Length", len.to_string())
            .body(body)
            .send()
            .await
            .map_err(|e| ProviderError::transport("uploading a file", &e))?;
        let mut file = unwrapped(read_answer(finished).await?);
        let remote = remote_of(&file)?;
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        while state_of(&file) == "PROCESSING" {
            if tokio::time::Instant::now() >= deadline {
                return Err(ProviderError::Other(format!(
                    "{} was still processing after {}s",
                    remote.id,
                    READY_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(poll).await;
            // The file's own uri is its resource; the name under the base is
            // the same resource for an answer that carried no uri.
            let resource = remote
                .uri
                .clone()
                .unwrap_or_else(|| format!("{}/{}", self.base_url, remote.id));
            let response = self
                .files_request(reqwest::Method::GET, &resource)
                .send()
                .await
                .map_err(|e| ProviderError::transport("checking an upload", &e))?;
            file = unwrapped(read_answer(response).await?);
        }
        match state_of(&file) {
            "FAILED" => Err(ProviderError::ApiError(format!(
                "Google could not process {}: {file}",
                remote.id
            ))),
            _ => Ok(remote),
        }
    }

    /// Delete `file`. One already gone is deleted.
    pub(super) async fn delete(&self, file: &RemoteFile) -> Result<()> {
        let response = self
            .files_request(
                reqwest::Method::DELETE,
                &format!("{}/{}", self.base_url, file.id),
            )
            .send()
            .await
            .map_err(|e| ProviderError::transport("deleting a file", &e))?;
        match response.status().as_u16() {
            404 => Ok(()),
            _ => read_answer(response).await.map(|_| ()),
        }
    }
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
