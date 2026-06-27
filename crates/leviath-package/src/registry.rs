//! Package registry client.

/// Client for package registries.
pub struct PackageRegistry {
    /// Registry URL
    url: String,
}

impl PackageRegistry {
    /// Create a new registry client.
    pub fn new(url: String) -> Self {
        Self { url }
    }

    /// Search for packages.
    pub async fn search(&self, query: &str) -> anyhow::Result<Vec<String>> {
        // TODO: Implement search
        tracing::info!(query = %query, registry = %self.url, "Searching registry");
        Ok(Vec::new())
    }

    /// Download a package.
    pub async fn download(&self, name: &str, version: &str) -> anyhow::Result<Vec<u8>> {
        // TODO: Implement download
        tracing::info!(name = %name, version = %version, "Downloading package");
        Ok(Vec::new())
    }
}
