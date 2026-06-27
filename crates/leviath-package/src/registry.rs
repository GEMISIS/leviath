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
            anyhow::bail!(
                "Registry search failed with status {}",
                response.status()
            );
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

        let url = format!(
            "{}/api/v1/packages/{}/{}/download",
            self.url, name, version
        );
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to download package: {}", e))?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Package download failed with status {}",
                response.status()
            );
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
            anyhow::bail!(
                "Package publish failed with status {}: {}",
                status,
                body
            );
        }

        tracing::info!("Package published successfully");
        Ok(())
    }
}
