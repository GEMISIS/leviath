//! Package manifest (leviath.toml) parsing and validation.

use leviath_core::Blueprint;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Package manifest loaded from leviath.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifest {
    /// Package metadata
    pub package: PackageMetadata,
    /// Agent blueprint
    pub blueprint: Blueprint,
}

/// Package metadata section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    /// Package name
    pub name: String,
    /// Version
    pub version: String,
    /// Description
    pub description: String,
    /// Authors
    pub authors: Vec<String>,
    /// License
    pub license: String,
}

impl PackageManifest {
    /// Load a manifest from a file.
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let manifest: Self = toml::from_str(&content)?;
        Ok(manifest)
    }

    /// Validate the manifest.
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate package name: non-empty, alphanumeric + hyphens, max 64 chars
        if self.package.name.is_empty() {
            anyhow::bail!("Package name cannot be empty");
        }
        if self.package.name.len() > 64 {
            anyhow::bail!("Package name cannot exceed 64 characters");
        }
        if !self
            .package
            .name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-')
        {
            anyhow::bail!("Package name must contain only alphanumeric characters and hyphens");
        }

        // Validate version: must be valid semver (X.Y.Z)
        let version_parts: Vec<&str> = self.package.version.split('.').collect();
        if version_parts.len() != 3
            || !version_parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        {
            anyhow::bail!(
                "Version '{}' is not valid semver (expected X.Y.Z)",
                self.package.version
            );
        }

        // Validate description: non-empty
        if self.package.description.is_empty() {
            anyhow::bail!("Package description cannot be empty");
        }

        // Validate authors: at least one entry
        if self.package.authors.is_empty() {
            anyhow::bail!("Package must have at least one author");
        }

        // Validate license: non-empty
        if self.package.license.is_empty() {
            anyhow::bail!("Package license cannot be empty");
        }

        // Validate blueprint
        self.blueprint
            .validate()
            .map_err(|e| anyhow::anyhow!("Blueprint validation failed: {}", e))?;

        Ok(())
    }
}
