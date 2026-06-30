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
}
