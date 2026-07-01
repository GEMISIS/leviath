//! Package registry client for searching, downloading, and publishing agent packages.

use serde::{Deserialize, Serialize};

/// Information about a package in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    /// Package name
    pub name: String,
    /// Latest version
    pub version: String,
    /// Package description
    pub description: String,
    /// Package authors
    #[serde(default)]
    pub authors: Vec<String>,
    /// Total downloads
    #[serde(default)]
    pub downloads: u64,
}

/// Client for interacting with a package registry over HTTP.
pub struct PackageRegistry {
    /// Registry base URL
    url: String,
    /// HTTP client
    client: reqwest::Client,
}

impl PackageRegistry {
    /// Create a new registry client.
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }

    /// Search for packages matching a query.
    pub async fn search(&self, query: &str) -> anyhow::Result<Vec<PackageInfo>> {
        tracing::info!(query = %query, registry = %self.url, "Searching registry");

        let url = format!("{}/api/v1/search", self.url);
        let response = self
            .client
            .get(&url)
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to search registry: {}", e))?;

        if !response.status().is_success() {
            anyhow::bail!("Registry search failed with status {}", response.status());
        }

        let packages: Vec<PackageInfo> = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse search results: {}", e))?;

        Ok(packages)
    }

    /// Download a package by name and version.
    pub async fn download(&self, name: &str, version: &str) -> anyhow::Result<Vec<u8>> {
        tracing::info!(name = %name, version = %version, registry = %self.url, "Downloading package");

        let url = format!("{}/api/v1/packages/{}/{}/download", self.url, name, version);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to download package: {}", e))?;

        if !response.status().is_success() {
            anyhow::bail!("Package download failed with status {}", response.status());
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read package bytes: {}", e))?;

        Ok(bytes.to_vec())
    }

    /// Get information about a package.
    pub async fn get_info(&self, name: &str) -> anyhow::Result<PackageInfo> {
        tracing::info!(name = %name, registry = %self.url, "Getting package info");

        let url = format!("{}/api/v1/packages/{}", self.url, name);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get package info: {}", e))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Package info request failed with status {}",
                response.status()
            );
        }

        let info: PackageInfo = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse package info: {}", e))?;

        Ok(info)
    }

    /// Publish a package bundle to the registry.
    pub async fn publish(&self, bundle: &[u8], token: &str) -> anyhow::Result<()> {
        tracing::info!(registry = %self.url, size = bundle.len(), "Publishing package");

        let url = format!("{}/api/v1/packages", self.url);
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/octet-stream")
            .body(bundle.to_vec())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to publish package: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Package publish failed with status {}: {}", status, body);
        }

        tracing::info!("Package published successfully");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_info_serde_roundtrip() {
        let info = PackageInfo {
            name: "my-agent".to_string(),
            version: "1.2.3".to_string(),
            description: "A cool agent".to_string(),
            authors: vec!["Alice".to_string(), "Bob".to_string()],
            downloads: 42,
        };

        let json = serde_json::to_string(&info).expect("should serialize");
        let deserialized: PackageInfo = serde_json::from_str(&json).expect("should deserialize");

        assert_eq!(deserialized.name, "my-agent");
        assert_eq!(deserialized.version, "1.2.3");
        assert_eq!(deserialized.description, "A cool agent");
        assert_eq!(deserialized.authors, vec!["Alice", "Bob"]);
        assert_eq!(deserialized.downloads, 42);
    }

    #[test]
    fn package_info_default_fields() {
        // authors and downloads have #[serde(default)], so they should
        // deserialize to empty vec and 0 when omitted.
        let json = r#"{
            "name": "minimal",
            "version": "0.1.0",
            "description": "Minimal package"
        }"#;
        let info: PackageInfo = serde_json::from_str(json).expect("should deserialize");

        assert_eq!(info.name, "minimal");
        assert!(info.authors.is_empty());
        assert_eq!(info.downloads, 0);
    }

    #[test]
    fn package_registry_new_creates_instance() {
        let registry = PackageRegistry::new("https://registry.example.com".to_string());
        assert_eq!(registry.url, "https://registry.example.com");
    }

    // ─── PackageInfo serialization ──────────────────────────────────────

    #[test]
    fn package_info_empty_authors() {
        let info = PackageInfo {
            name: "agent".to_string(),
            version: "1.0.0".to_string(),
            description: "desc".to_string(),
            authors: vec![],
            downloads: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: PackageInfo = serde_json::from_str(&json).unwrap();
        assert!(back.authors.is_empty());
        assert_eq!(back.downloads, 0);
    }

    #[test]
    fn package_info_large_downloads() {
        let info = PackageInfo {
            name: "popular".to_string(),
            version: "10.0.0".to_string(),
            description: "Very popular".to_string(),
            authors: vec!["Author".to_string()],
            downloads: 1_000_000,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: PackageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.downloads, 1_000_000);
    }

    #[test]
    fn package_info_clone() {
        let info = PackageInfo {
            name: "agent".to_string(),
            version: "1.0.0".to_string(),
            description: "desc".to_string(),
            authors: vec!["alice".to_string()],
            downloads: 5,
        };
        let cloned = info.clone();
        assert_eq!(cloned.name, "agent");
        assert_eq!(cloned.authors.len(), 1);
    }

    #[test]
    fn package_info_debug() {
        let info = PackageInfo {
            name: "agent".to_string(),
            version: "1.0.0".to_string(),
            description: "desc".to_string(),
            authors: vec![],
            downloads: 0,
        };
        let debug = format!("{:?}", info);
        assert!(debug.contains("agent"));
    }

    // ─── PackageRegistry URL construction ───────────────────────────────

    #[test]
    fn package_registry_preserves_url() {
        let registry = PackageRegistry::new("https://custom-registry.io".to_string());
        assert_eq!(registry.url, "https://custom-registry.io");
    }

    #[test]
    fn package_registry_url_without_trailing_slash() {
        let registry = PackageRegistry::new("https://registry.example.com".to_string());
        // URL should be stored as-is without trailing slash
        assert!(!registry.url.ends_with('/'));
    }

    #[test]
    fn package_registry_url_with_trailing_slash() {
        let registry = PackageRegistry::new("https://registry.example.com/".to_string());
        // URL stored as-is (trailing slash preserved by constructor)
        assert!(registry.url.ends_with('/'));
    }

    // ─── PackageInfo from JSON with extra fields ────────────────────────

    #[test]
    fn package_info_ignores_unknown_fields() {
        let json = r#"{
            "name": "test",
            "version": "1.0.0",
            "description": "Test package",
            "authors": [],
            "downloads": 10,
            "extra_field": "ignored"
        }"#;
        // serde by default ignores unknown fields
        let info: Result<PackageInfo, _> = serde_json::from_str(json);
        assert!(info.is_ok());
    }

    // ─── PackageInfo with unicode ───────────────────────────────────────

    #[test]
    fn package_info_unicode_description() {
        let info = PackageInfo {
            name: "unicode-agent".to_string(),
            version: "1.0.0".to_string(),
            description: "An agent for processing text".to_string(),
            authors: vec!["Author Name".to_string()],
            downloads: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: PackageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.description, "An agent for processing text");
    }

    // ─── PackageRegistry HTTP error paths ──────────────────────────────

    #[tokio::test]
    async fn search_connection_refused_returns_error() {
        // Use a port that's unlikely to be listening
        let registry = PackageRegistry::new("http://127.0.0.1:19999".to_string());
        let result = registry.search("test-query").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to search"),
            "Expected search error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn download_connection_refused_returns_error() {
        let registry = PackageRegistry::new("http://127.0.0.1:19999".to_string());
        let result = registry.download("my-package", "1.0.0").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to download"),
            "Expected download error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn get_info_connection_refused_returns_error() {
        let registry = PackageRegistry::new("http://127.0.0.1:19999".to_string());
        let result = registry.get_info("my-package").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to get package info"),
            "Expected get_info error, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn publish_connection_refused_returns_error() {
        let registry = PackageRegistry::new("http://127.0.0.1:19999".to_string());
        let bundle = b"fake bundle data";
        let result = registry.publish(bundle, "my-token").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Failed to publish"),
            "Expected publish error, got: {}",
            err
        );
    }

    // ─── Minimal raw-TCP mock HTTP server (no new dependency needed) ───────
    //
    // Binds to an OS-assigned localhost port, accepts exactly one connection,
    // discards the request, and writes back a fixed raw HTTP/1.1 response.
    // Good enough for exercising PackageRegistry's response-parsing paths
    // without a mocking crate.

    async fn spawn_mock_server(status: u16, reason: &str, body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status,
            reason,
            body.len()
        )
        .into_bytes();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(&response).await;
                let _ = socket.write_all(body).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });

        format!("http://{}", addr)
    }

    /// Like `spawn_mock_server`, but declares a `Content-Length` larger than
    /// the bytes actually sent before closing the connection -- forces a
    /// genuine mid-body I/O error on `.bytes()`/`.text()`/`.json()`, as
    /// opposed to a well-formed-but-wrong body.
    async fn spawn_mock_server_truncated_body(status: u16, reason: &str, body: &[u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let declared_len = body.len() + 4096;
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status, reason, declared_len
        )
        .into_bytes();
        let body = body.to_vec();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(&response).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.flush().await;
                // Close without ever sending the remaining declared bytes.
                let _ = socket.shutdown().await;
            }
        });

        format!("http://{}", addr)
    }

    /// llvm-cov reports `tracing::info!(...)` call sites' field-expression
    /// sub-regions as uncovered when no `tracing::Subscriber` is registered
    /// during tests -- the macro short-circuits field evaluation before the
    /// "is this level enabled" check even runs, even though the surrounding
    /// branch genuinely executes. Setting this as the default subscriber for
    /// a test's duration makes those field expressions actually evaluate.
    struct AlwaysOnSubscriber;
    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn always_on_tracing_guard() -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(AlwaysOnSubscriber)
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        let _guard = always_on_tracing_guard();
        let span = tracing::info_span!("test-span", field = 1);
        span.record("field", 2);
        span.follows_from(&span);
        span.in_scope(|| {
            tracing::info!("inside span");
        });
    }

    #[tokio::test]
    async fn search_success_returns_packages() {
        let _guard = always_on_tracing_guard();
        let body = br#"[{"name":"pkg-a","version":"1.0.0","description":"desc"}]"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let registry = PackageRegistry::new(url);
        let packages = registry.search("pkg").await.unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "pkg-a");
    }

    #[tokio::test]
    async fn search_non_success_status_returns_error() {
        let url = spawn_mock_server(404, "Not Found", b"").await;
        let registry = PackageRegistry::new(url);
        let err = registry.search("pkg").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn search_malformed_json_returns_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let registry = PackageRegistry::new(url);
        let err = registry.search("pkg").await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse search results"));
    }

    #[tokio::test]
    async fn download_success_returns_bytes() {
        let _guard = always_on_tracing_guard();
        let url = spawn_mock_server(200, "OK", b"binary bundle content").await;
        let registry = PackageRegistry::new(url);
        let bytes = registry.download("pkg-a", "1.0.0").await.unwrap();
        assert_eq!(bytes, b"binary bundle content");
    }

    #[tokio::test]
    async fn download_body_read_error_returns_error() {
        // A truncated body (declared Content-Length exceeds what's actually
        // sent) forces `.bytes()` itself to fail, exercising the
        // `map_err(|e| ... "Failed to read package bytes" ...)` arm --
        // distinct from a non-success status or a well-formed-but-wrong body.
        let url = spawn_mock_server_truncated_body(200, "OK", b"partial").await;
        let registry = PackageRegistry::new(url);
        let err = registry.download("pkg-a", "1.0.0").await.unwrap_err();
        assert!(err.to_string().contains("Failed to read package bytes"));
    }

    #[tokio::test]
    async fn download_non_success_status_returns_error() {
        let url = spawn_mock_server(500, "Internal Server Error", b"").await;
        let registry = PackageRegistry::new(url);
        let err = registry.download("pkg-a", "1.0.0").await.unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn get_info_success_returns_info() {
        let _guard = always_on_tracing_guard();
        let body = br#"{"name":"pkg-a","version":"1.0.0","description":"desc"}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let registry = PackageRegistry::new(url);
        let info = registry.get_info("pkg-a").await.unwrap();
        assert_eq!(info.name, "pkg-a");
        assert_eq!(info.version, "1.0.0");
    }

    #[tokio::test]
    async fn get_info_non_success_status_returns_error() {
        let url = spawn_mock_server(404, "Not Found", b"").await;
        let registry = PackageRegistry::new(url);
        let err = registry.get_info("pkg-a").await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn get_info_malformed_json_returns_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let registry = PackageRegistry::new(url);
        let err = registry.get_info("pkg-a").await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse package info"));
    }

    #[tokio::test]
    async fn publish_success_returns_ok() {
        let _guard = always_on_tracing_guard();
        let url = spawn_mock_server(200, "OK", b"").await;
        let registry = PackageRegistry::new(url);
        let result = registry.publish(b"bundle bytes", "token").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn publish_non_success_status_returns_error_with_body() {
        let url = spawn_mock_server(403, "Forbidden", b"invalid token").await;
        let registry = PackageRegistry::new(url);
        let err = registry
            .publish(b"bundle bytes", "bad-token")
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("invalid token"));
    }

    // ─── PackageInfo various field combinations ─────────────────────────

    #[test]
    fn package_info_multiple_authors() {
        let info = PackageInfo {
            name: "multi-author".to_string(),
            version: "1.0.0".to_string(),
            description: "By many authors".to_string(),
            authors: vec![
                "Alice".to_string(),
                "Bob".to_string(),
                "Charlie".to_string(),
            ],
            downloads: 100,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: PackageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.authors.len(), 3);
        assert_eq!(back.authors[0], "Alice");
        assert_eq!(back.authors[2], "Charlie");
    }

    #[test]
    fn package_info_zero_downloads() {
        let json = r#"{
            "name": "new-package",
            "version": "0.1.0",
            "description": "New",
            "downloads": 0
        }"#;
        let info: PackageInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.downloads, 0);
    }

    #[test]
    fn package_registry_new_with_empty_url() {
        // Should not panic even with unusual URLs
        let registry = PackageRegistry::new("".to_string());
        assert_eq!(registry.url, "");
    }

    #[test]
    fn package_registry_new_with_localhost() {
        let registry = PackageRegistry::new("http://localhost:8080".to_string());
        assert_eq!(registry.url, "http://localhost:8080");
    }
}
