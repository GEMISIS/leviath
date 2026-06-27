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
        // TODO: Implement validation
        self.blueprint.validate()
            .map_err(|e| anyhow::anyhow!("Blueprint validation failed: {}", e))
    }
}
