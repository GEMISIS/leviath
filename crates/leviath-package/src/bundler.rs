//! Agent bundling for distribution.

use std::path::Path;

/// Bundles agents for distribution.
pub struct AgentBundler {}

impl AgentBundler {
    /// Create a new bundler.
    pub fn new() -> Self {
        Self {}
    }

    /// Bundle an agent from a project directory.
    pub fn bundle<P: AsRef<Path>>(&self, project_path: P) -> anyhow::Result<Vec<u8>> {
        // TODO: Implement bundling
        tracing::info!(path = %project_path.as_ref().display(), "Bundling agent");
        Ok(Vec::new())
    }
}

impl Default for AgentBundler {
    fn default() -> Self {
        Self::new()
    }
}
