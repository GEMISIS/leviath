//! Agent installation.

use std::path::Path;

/// Installs agents from packages.
pub struct AgentInstaller {}

impl AgentInstaller {
    /// Create a new installer.
    pub fn new() -> Self {
        Self {}
    }

    /// Install an agent from a package file.
    pub fn install<P: AsRef<Path>>(&self, package_path: P) -> anyhow::Result<()> {
        // TODO: Implement installation
        tracing::info!(path = %package_path.as_ref().display(), "Installing agent");
        Ok(())
    }
}

impl Default for AgentInstaller {
    fn default() -> Self {
        Self::new()
    }
}
