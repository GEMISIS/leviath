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
    pub fn load(path: &Path) -> anyhow::Result<Self> {
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
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::{ModelConfig, Stage};
    use leviath_core::layout::RegionDefinition;
    use leviath_core::region::RegionKind;
    use leviath_core::{Blueprint, ContextLayout};

    fn make_blueprint() -> Blueprint {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout = ContextLayout::new(regions, 10000);
        let stages = vec![Stage::new(
            "analyze".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        )];
        Blueprint::new(
            "test-agent".to_string(),
            "A test agent".to_string(),
            stages,
            layout,
        )
    }

    fn make_valid_manifest() -> PackageManifest {
        PackageManifest {
            package: PackageMetadata {
                name: "my-agent".to_string(),
                version: "1.0.0".to_string(),
                description: "A useful agent".to_string(),
                authors: vec!["Alice <alice@example.com>".to_string()],
                license: "MIT".to_string(),
            },
            blueprint: make_blueprint(),
        }
    }

    #[test]
    fn valid_manifest_passes_validation() {
        let manifest = make_valid_manifest();
        assert!(manifest.validate().is_ok());
    }

    fn assert_validation_err_contains(err: &anyhow::Error, needle: &str, label: &str) {
        assert!(
            err.to_string().contains(needle),
            "expected {label} error, got: {err}"
        );
    }

    #[test]
    #[should_panic(expected = "expected widget error, got: unrelated failure")]
    fn assert_validation_err_contains_panics_when_missing() {
        assert_validation_err_contains(&anyhow::anyhow!("unrelated failure"), "needle", "widget");
    }

    #[test]
    fn empty_name_fails_validation() {
        let mut manifest = make_valid_manifest();
        manifest.package.name = String::new();
        let err = manifest.validate().unwrap_err();
        assert_validation_err_contains(&err, "empty", "empty name");
    }

    #[test]
    fn name_too_long_fails_validation() {
        let mut manifest = make_valid_manifest();
        manifest.package.name = "a".repeat(65);
        let err = manifest.validate().unwrap_err();
        assert_validation_err_contains(&err, "64", "max-length");
    }

    #[test]
    fn name_with_invalid_chars_fails_validation() {
        let mut manifest = make_valid_manifest();
        manifest.package.name = "my_agent!".to_string();
        let err = manifest.validate().unwrap_err();
        assert_validation_err_contains(&err, "alphanumeric", "invalid-chars");
    }

    #[test]
    fn invalid_semver_fails_validation() {
        let mut manifest = make_valid_manifest();
        manifest.package.version = "1.0".to_string();
        let err = manifest.validate().unwrap_err();
        assert_validation_err_contains(&err, "semver", "semver");
    }

    #[test]
    fn empty_description_fails_validation() {
        let mut manifest = make_valid_manifest();
        manifest.package.description = String::new();
        let err = manifest.validate().unwrap_err();
        assert_validation_err_contains(&err, "description", "description");
    }

    #[test]
    fn no_authors_fails_validation() {
        let mut manifest = make_valid_manifest();
        manifest.package.authors = Vec::new();
        let err = manifest.validate().unwrap_err();
        assert_validation_err_contains(&err, "author", "authors");
    }

    #[test]
    fn empty_license_fails_validation() {
        let mut manifest = make_valid_manifest();
        manifest.package.license = String::new();
        let err = manifest.validate().unwrap_err();
        assert_validation_err_contains(&err, "license", "license");
    }

    #[test]
    fn invalid_blueprint_fails_validation() {
        // Package-level fields are all valid, but the blueprint itself isn't
        // (empty stage name) — validate() must forward that error.
        let mut manifest = make_valid_manifest();
        manifest.blueprint.stages[0].name = String::new();
        assert!(manifest.validate().is_err());
    }

    // ─── PackageManifest::load ──────────────────────────────────────────────

    #[test]
    fn load_reads_and_parses_a_valid_manifest_file() {
        // Round-trip a real PackageManifest through TOML instead of
        // hand-writing the schema, so this test doesn't drift if Blueprint's
        // fields change.
        let original = make_valid_manifest();
        let toml_content = toml::to_string(&original).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leviath.toml");
        std::fs::write(&path, toml_content).unwrap();

        let manifest = PackageManifest::load(&path).unwrap();
        assert_eq!(manifest.package.name, "my-agent");
        assert_eq!(manifest.package.version, "1.0.0");
    }

    #[test]
    fn load_missing_file_returns_error() {
        let result = PackageManifest::load(Path::new("/nonexistent/path/leviath.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn load_malformed_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leviath.toml");
        std::fs::write(&path, "not valid toml [[[").unwrap();
        let result = PackageManifest::load(&path);
        assert!(result.is_err());
    }
}
