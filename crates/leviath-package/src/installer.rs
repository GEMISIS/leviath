//! Agent installation from bundle archives.

use flate2::read::GzDecoder;
use std::fs;
use std::path::{Path, PathBuf};

/// Information about an installed agent.
#[derive(Debug, Clone)]
pub struct InstalledAgent {
    /// Agent name
    pub name: String,
    /// Agent version
    pub version: String,
    /// Installation path
    pub path: PathBuf,
    /// Agent description
    pub description: String,
}

/// Installs agents from `.leviath-bundle` packages.
pub struct AgentInstaller {
    /// Installation directory (default ~/.leviath/agents/)
    install_dir: PathBuf,
}

impl AgentInstaller {
    /// Create a new installer using the default installation directory.
    pub fn new() -> Self {
        let install_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".leviath")
            .join("agents");
        Self { install_dir }
    }

    /// Create an installer with a custom installation directory.
    pub fn with_install_dir(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }

    /// Install an agent from a `.leviath-bundle` file.
    pub fn install<P: AsRef<Path>>(&self, package_path: P) -> anyhow::Result<InstalledAgent> {
        let package_path = package_path.as_ref();
        tracing::info!(path = %package_path.display(), "Installing agent from package");

        let data = fs::read(package_path)
            .map_err(|e| anyhow::anyhow!("Failed to read package '{}': {}", package_path.display(), e))?;

        // Derive name from filename (strip .leviath-bundle extension)
        let name = package_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        self.install_from_bytes(&name, &data)
    }

    /// Install an agent from in-memory bytes.
    pub fn install_from_bytes(&self, name: &str, data: &[u8]) -> anyhow::Result<InstalledAgent> {
        tracing::info!(name = %name, "Installing agent from bytes");

        let agent_dir = self.install_dir.join(name);

        // Create installation directory
        fs::create_dir_all(&agent_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create install directory '{}': {}", agent_dir.display(), e))?;

        // Extract tar.gz archive
        let decoder = GzDecoder::new(data);
        let mut archive = tar::Archive::new(decoder);

        archive.unpack(&agent_dir)
            .map_err(|e| anyhow::anyhow!("Failed to extract package: {}", e))?;

        // Read agent.leviath to get metadata
        let manifest_path = agent_dir.join("agent.leviath");
        let (version, description) = if manifest_path.exists() {
            let content = fs::read_to_string(&manifest_path)
                .unwrap_or_default();
            let parsed: toml::Value = toml::from_str(&content).unwrap_or(toml::Value::Table(toml::map::Map::new()));
            let version = parsed
                .get("agent")
                .and_then(|a| a.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or("0.0.0")
                .to_string();
            let description = parsed
                .get("agent")
                .and_then(|a| a.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (version, description)
        } else {
            ("0.0.0".to_string(), String::new())
        };

        tracing::info!(
            name = %name,
            version = %version,
            path = %agent_dir.display(),
            "Agent installed successfully"
        );

        Ok(InstalledAgent {
            name: name.to_string(),
            version,
            path: agent_dir,
            description,
        })
    }

    /// Uninstall an agent by removing its directory.
    pub fn uninstall(&self, agent_name: &str) -> anyhow::Result<()> {
        let agent_dir = self.install_dir.join(agent_name);

        if !agent_dir.exists() {
            anyhow::bail!("Agent '{}' is not installed", agent_name);
        }

        fs::remove_dir_all(&agent_dir)
            .map_err(|e| anyhow::anyhow!("Failed to remove agent '{}': {}", agent_name, e))?;

        tracing::info!(name = %agent_name, "Agent uninstalled");
        Ok(())
    }

    /// List all installed agents.
    pub fn list_installed(&self) -> anyhow::Result<Vec<InstalledAgent>> {
        if !self.install_dir.exists() {
            return Ok(Vec::new());
        }

        let mut agents = Vec::new();

        for entry in fs::read_dir(&self.install_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let manifest_path = path.join("agent.leviath");
                if manifest_path.exists() {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();

                    let content = fs::read_to_string(&manifest_path).unwrap_or_default();
                    let parsed: toml::Value = toml::from_str(&content)
                        .unwrap_or(toml::Value::Table(toml::map::Map::new()));

                    let version = parsed
                        .get("agent")
                        .and_then(|a| a.get("version"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("0.0.0")
                        .to_string();
                    let description = parsed
                        .get("agent")
                        .and_then(|a| a.get("description"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    agents.push(InstalledAgent {
                        name,
                        version,
                        path,
                        description,
                    });
                }
            }
        }

        Ok(agents)
    }

    /// Get information about a specific installed agent.
    pub fn get_installed(&self, name: &str) -> anyhow::Result<Option<InstalledAgent>> {
        let agent_dir = self.install_dir.join(name);

        if !agent_dir.exists() {
            return Ok(None);
        }

        let manifest_path = agent_dir.join("agent.leviath");
        if !manifest_path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&manifest_path).unwrap_or_default();
        let parsed: toml::Value = toml::from_str(&content)
            .unwrap_or(toml::Value::Table(toml::map::Map::new()));

        let version = parsed
            .get("agent")
            .and_then(|a| a.get("version"))
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string();
        let description = parsed
            .get("agent")
            .and_then(|a| a.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(Some(InstalledAgent {
            name: name.to_string(),
            version,
            path: agent_dir,
            description,
        }))
    }
}

impl Default for AgentInstaller {
    fn default() -> Self {
        Self::new()
    }
}
