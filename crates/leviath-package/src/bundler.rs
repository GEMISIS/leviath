//! Agent bundling for distribution.
//!
//! Creates `.leviath-bundle` files which are tar.gz archives containing
//! the agent manifest, definition, scripts, and documentation.

use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bundles agents for distribution as `.leviath-bundle` archives.
pub struct AgentBundler {
    /// File patterns to exclude from the bundle
    exclude_patterns: Vec<String>,
}

impl AgentBundler {
    /// Create a new bundler.
    pub fn new() -> Self {
        Self {
            exclude_patterns: vec![
                ".git".to_string(),
                ".DS_Store".to_string(),
                "target".to_string(),
                ".env".to_string(),
                ".env.local".to_string(),
                ".env.production".to_string(),
                "*.key".to_string(),
                "*.pem".to_string(),
                "*.p12".to_string(),
                "config.toml".to_string(),
                ".leviath".to_string(),
                "*.leviath-bundle".to_string(),
            ],
        }
    }

    /// Add an exclusion pattern.
    pub fn with_exclude(mut self, pattern: String) -> Self {
        self.exclude_patterns.push(pattern);
        self
    }

    /// Bundle an agent from a project directory into an in-memory tar.gz archive.
    pub fn bundle<P: AsRef<Path>>(&self, project_path: P) -> anyhow::Result<Vec<u8>> {
        let project_path = project_path.as_ref();
        tracing::info!(path = %project_path.display(), "Bundling agent");

        if !project_path.is_dir() {
            anyhow::bail!("Project path '{}' is not a directory", project_path.display());
        }

        // Verify agent.leviath exists
        let manifest_path = project_path.join("agent.leviath");
        if !manifest_path.exists() {
            anyhow::bail!(
                "No agent.leviath found in '{}'",
                project_path.display()
            );
        }

        let mut buf = Vec::new();
        {
            let encoder = GzEncoder::new(&mut buf, Compression::default());
            let mut tar = tar::Builder::new(encoder);

            // Walk the project directory and add files
            self.add_directory_to_tar(&mut tar, project_path, project_path)?;

            let encoder = tar.into_inner()
                .map_err(|e| anyhow::anyhow!("Failed to finalize tar archive: {}", e))?;
            encoder.finish()
                .map_err(|e| anyhow::anyhow!("Failed to finalize gzip: {}", e))?;
        }

        tracing::info!(
            size_bytes = buf.len(),
            "Bundle created"
        );

        Ok(buf)
    }

    /// Bundle an agent and write to a file.
    pub fn bundle_to_file<P: AsRef<Path>>(
        &self,
        project_path: P,
        output_path: P,
    ) -> anyhow::Result<PathBuf> {
        let data = self.bundle(&project_path)?;
        let output = output_path.as_ref().to_path_buf();

        fs::write(&output, &data)
            .map_err(|e| anyhow::anyhow!("Failed to write bundle to '{}': {}", output.display(), e))?;

        tracing::info!(
            path = %output.display(),
            size_bytes = data.len(),
            "Bundle written to file"
        );

        Ok(output)
    }

    /// Check if a filename should be excluded based on patterns.
    ///
    /// Supports wildcard patterns like `*.key` (matches any file ending in `.key`)
    /// and exact filename matches like `.env`.
    pub fn should_exclude(&self, filename: &str) -> bool {
        self.exclude_patterns.iter().any(|pattern| {
            if let Some(suffix) = pattern.strip_prefix("*.") {
                filename.ends_with(&format!(".{}", suffix))
            } else {
                filename == pattern
            }
        })
    }

    /// Recursively add a directory's contents to the tar archive.
    fn add_directory_to_tar<W: Write>(
        &self,
        tar: &mut tar::Builder<W>,
        dir: &Path,
        base: &Path,
    ) -> anyhow::Result<()> {
        for entry in fs::read_dir(dir)
            .map_err(|e| anyhow::anyhow!("Failed to read directory '{}': {}", dir.display(), e))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();

            // Check exclusion patterns
            if self.should_exclude(&name_str) {
                continue;
            }

            let relative = path
                .strip_prefix(base)
                .map_err(|e| anyhow::anyhow!("Failed to compute relative path: {}", e))?;

            if path.is_dir() {
                self.add_directory_to_tar(tar, &path, base)?;
            } else if path.is_file() {
                tar.append_path_with_name(&path, relative)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to add '{}' to bundle: {}",
                            relative.display(),
                            e
                        )
                    })?;
            }
        }
        Ok(())
    }
}

impl Default for AgentBundler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_exclude_env_files() {
        let bundler = AgentBundler::new();
        assert!(bundler.should_exclude(".env"));
        assert!(bundler.should_exclude(".env.local"));
        assert!(bundler.should_exclude(".env.production"));
    }

    #[test]
    fn test_should_exclude_wildcard_patterns() {
        let bundler = AgentBundler::new();
        assert!(bundler.should_exclude("server.key"));
        assert!(bundler.should_exclude("cert.pem"));
        assert!(bundler.should_exclude("keystore.p12"));
        assert!(bundler.should_exclude("my-agent.leviath-bundle"));
    }

    #[test]
    fn test_should_not_exclude_safe_files() {
        let bundler = AgentBundler::new();
        assert!(!bundler.should_exclude("agent.leviath"));
        assert!(!bundler.should_exclude("README.md"));
        assert!(!bundler.should_exclude("main.rs"));
        assert!(!bundler.should_exclude("keyboard.rs"));
    }

    #[test]
    fn test_should_exclude_exact_matches() {
        let bundler = AgentBundler::new();
        assert!(bundler.should_exclude(".git"));
        assert!(bundler.should_exclude(".DS_Store"));
        assert!(bundler.should_exclude("target"));
        assert!(bundler.should_exclude("config.toml"));
        assert!(bundler.should_exclude(".leviath"));
    }
}
