//! Anthropic's Files API: `POST /v1/files` to upload, `DELETE /v1/files/{id}`
//! to delete, and the file named by id in an `image` or `document` block.
//!
//! No beta header: the API is out of beta. Uploads are not eligible for zero
//! data retention, which is why the runtime never uploads with the switch on.

use crate::files::{FileUpload, MediaLimits, RemoteFile, file_part, read_answer, remote_from};
use crate::provider::{ProviderError, Result};

use super::AnthropicProvider;

impl AnthropicProvider {
    /// A request to `path` under the base URL with the key, the version and
    /// the operator's headers.
    fn files_request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        crate::provider::with_extra_headers(
            self.client
                .request(method, format!("{}{path}", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01"),
            &self.extra_headers,
        )
    }

    /// Upload `upload`, asking Anthropic to delete it after its lifetime.
    pub(super) async fn upload(
        &self,
        upload: &FileUpload,
        limits: &MediaLimits,
    ) -> Result<RemoteFile> {
        let ttl = limits.clamp_ttl(upload.ttl_secs);
        let form = reqwest::multipart::Form::new()
            .text("expires_in_seconds", ttl.to_string())
            .part("file", file_part(upload)?);
        let response = self
            .files_request(reqwest::Method::POST, "/files")
            .multipart(form)
            .send()
            .await
            .map_err(|e| ProviderError::transport("uploading a file", &e))?;
        remote_from(&read_answer(response).await?, ttl)
    }

    /// Delete `file`. One already gone is deleted.
    pub(super) async fn delete(&self, file: &RemoteFile) -> Result<()> {
        let response = self
            .files_request(reqwest::Method::DELETE, &format!("/files/{}", file.id))
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
mod tests {
    use std::sync::Arc;

    use super::*;
    use leviath_testkit::spawn_mock_recorder;

    fn upload() -> FileUpload {
        FileUpload {
            bytes: Arc::from(&b"%PDF-1.7"[..]),
            mime_type: "application/pdf".into(),
            name: "report.pdf".into(),
            ttl_secs: 10,
        }
    }

    #[tokio::test]
    async fn an_upload_sends_the_file_and_its_lifetime_and_reads_the_expiry() {
        let (url, seen) = spawn_mock_recorder(
            200,
            "OK",
            br#"{"id":"file_1","expires_at":"2026-09-17T00:00:00Z"}"#.to_vec(),
        )
        .await;
        let provider = AnthropicProvider::new(reqwest::Client::new(), "sk-ant".into())
            .with_base_url(Some(url));
        let limits = crate::files::provider_limits("anthropic");
        let file = provider.upload(&upload(), &limits).await.unwrap();
        assert_eq!(file.id, "file_1");
        assert_eq!(file.expires_at, Some(1_789_603_200));
        let raw = seen.lock().unwrap().join("");
        assert!(raw.contains("POST /files"), "{raw}");
        assert!(raw.contains("x-api-key: sk-ant"), "{raw}");
        assert!(
            raw.contains("name=\"expires_in_seconds\"\r\n\r\n3600"),
            "{raw}"
        );
        assert!(raw.contains("filename=\"report.pdf\""), "{raw}");
        assert!(
            !raw.to_ascii_lowercase().contains("anthropic-beta"),
            "{raw}"
        );
    }

    #[tokio::test]
    async fn a_delete_of_a_gone_file_is_done_and_a_refusal_is_an_error() {
        let file = RemoteFile {
            id: "file_1".into(),
            uri: None,
            expires_at: None,
        };
        let (gone, seen) = spawn_mock_recorder(404, "Not Found", b"{}".to_vec()).await;
        let provider =
            AnthropicProvider::new(reqwest::Client::new(), "k".into()).with_base_url(Some(gone));
        provider.delete(&file).await.unwrap();
        assert!(
            seen.lock()
                .unwrap()
                .join("")
                .contains("DELETE /files/file_1")
        );

        let (refused, _) = spawn_mock_recorder(403, "Forbidden", b"{}".to_vec()).await;
        let provider =
            AnthropicProvider::new(reqwest::Client::new(), "k".into()).with_base_url(Some(refused));
        assert!(provider.delete(&file).await.is_err());
        let (ok, _) = spawn_mock_recorder(200, "OK", b"{\"id\":\"file_1\"}".to_vec()).await;
        let provider =
            AnthropicProvider::new(reqwest::Client::new(), "k".into()).with_base_url(Some(ok));
        provider.delete(&file).await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_host_is_a_transport_error() {
        let provider = AnthropicProvider::new(reqwest::Client::new(), "k".into())
            .with_base_url(Some("http://127.0.0.1:9".into()));
        let limits = crate::files::provider_limits("anthropic");
        assert!(provider.upload(&upload(), &limits).await.is_err());
        let file = RemoteFile {
            id: "f".into(),
            uri: None,
            expires_at: None,
        };
        assert!(provider.delete(&file).await.is_err());
    }

    #[tokio::test]
    async fn the_trait_reaches_the_files_api_and_a_refusal_or_a_bad_type_is_an_error() {
        use crate::provider::Provider;
        let (url, _) = spawn_mock_recorder(200, "OK", br#"{"id":"file_9"}"#.to_vec()).await;
        let provider =
            AnthropicProvider::new(reqwest::Client::new(), "k".into()).with_base_url(Some(url));
        let pdf = leviath_core::mime::MimeType::parse("application/pdf").unwrap();
        assert!(provider.media_limits("claude-sonnet-5").by_file(&pdf, 1));
        assert_eq!(provider.upload_file(&upload()).await.unwrap().id, "file_9");
        let (gone, _) = spawn_mock_recorder(404, "Not Found", b"{}".to_vec()).await;
        let provider =
            AnthropicProvider::new(reqwest::Client::new(), "k".into()).with_base_url(Some(gone));
        let file = RemoteFile {
            id: "file_9".into(),
            uri: None,
            expires_at: None,
        };
        provider.delete_file(&file).await.unwrap();

        let (refused, _) = spawn_mock_recorder(413, "Too Large", b"{}".to_vec()).await;
        let provider =
            AnthropicProvider::new(reqwest::Client::new(), "k".into()).with_base_url(Some(refused));
        let limits = crate::files::provider_limits("anthropic");
        assert!(provider.upload(&upload(), &limits).await.is_err());
        let bad = FileUpload {
            mime_type: "not a type\n".into(),
            ..upload()
        };
        assert!(provider.upload(&bad, &limits).await.is_err());
    }
}
