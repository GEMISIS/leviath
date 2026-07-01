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

        let data = fs::read(package_path).map_err(|e| {
            anyhow::anyhow!("Failed to read package '{}': {}", package_path.display(), e)
        })?;

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
        fs::create_dir_all(&agent_dir).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create install directory '{}': {}",
                agent_dir.display(),
                e
            )
        })?;

        // Extract tar.gz archive
        let decoder = GzDecoder::new(data);
        let mut archive = tar::Archive::new(decoder);

        archive
            .unpack(&agent_dir)
            .map_err(|e| anyhow::anyhow!("Failed to extract package: {}", e))?;

        // Read agent.leviath to get metadata
        let manifest_path = agent_dir.join("agent.leviath");
        let (version, description) = if manifest_path.exists() {
            let content = fs::read_to_string(&manifest_path).unwrap_or_default();
            let parsed: toml::Value =
                toml::from_str(&content).unwrap_or(toml::Value::Table(toml::map::Map::new()));
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
        let parsed: toml::Value =
            toml::from_str(&content).unwrap_or(toml::Value::Table(toml::map::Map::new()));

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

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    /// Create a minimal tar.gz bundle with an agent.leviath manifest.
    fn make_bundle(name: &str, version: &str, description: &str) -> Vec<u8> {
        let manifest = format!(
            r#"[agent]
name = "{}"
version = "{}"
description = "{}"
"#,
            name, version, description
        );

        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut archive = tar::Builder::new(&mut encoder);
            let manifest_bytes = manifest.as_bytes();
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest_bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "agent.leviath", manifest_bytes)
                .unwrap();
            archive.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[test]
    fn with_install_dir_sets_dir() {
        let dir = PathBuf::from("/tmp/test-installer");
        let installer = AgentInstaller::with_install_dir(dir.clone());
        assert_eq!(installer.install_dir, dir);
    }

    #[test]
    fn install_from_bytes_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let bundle = make_bundle("test-agent", "1.0.0", "A test agent");
        let result = installer.install_from_bytes("test-agent", &bundle).unwrap();

        assert_eq!(result.name, "test-agent");
        assert_eq!(result.version, "1.0.0");
        assert_eq!(result.description, "A test agent");
        assert!(result.path.exists());
        assert!(result.path.join("agent.leviath").exists());
    }

    #[test]
    fn install_from_bytes_no_manifest_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        // Create a bundle with no agent.leviath
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut archive = tar::Builder::new(&mut encoder);
            let data = b"hello";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "readme.txt", &data[..])
                .unwrap();
            archive.finish().unwrap();
        }
        let bundle = encoder.finish().unwrap();

        let result = installer
            .install_from_bytes("no-manifest", &bundle)
            .unwrap();
        assert_eq!(result.version, "0.0.0");
        assert_eq!(result.description, "");
    }

    #[test]
    fn uninstall_removes_directory() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let bundle = make_bundle("to-remove", "1.0.0", "remove me");
        installer.install_from_bytes("to-remove", &bundle).unwrap();

        assert!(dir.path().join("to-remove").exists());
        installer.uninstall("to-remove").unwrap();
        assert!(!dir.path().join("to-remove").exists());
    }

    #[test]
    fn uninstall_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let err = installer.uninstall("no-such-agent").unwrap_err();
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn list_installed_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let agents = installer.list_installed().unwrap();
        assert!(agents.is_empty());
    }

    #[test]
    fn list_installed_nonexistent_dir() {
        let installer =
            AgentInstaller::with_install_dir(PathBuf::from("/tmp/nonexistent-leviath-test-dir"));
        let agents = installer.list_installed().unwrap();
        assert!(agents.is_empty());
    }

    #[test]
    fn list_installed_returns_installed_agents() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let bundle1 = make_bundle("agent-a", "1.0.0", "Agent A");
        let bundle2 = make_bundle("agent-b", "2.0.0", "Agent B");
        installer.install_from_bytes("agent-a", &bundle1).unwrap();
        installer.install_from_bytes("agent-b", &bundle2).unwrap();

        let agents = installer.list_installed().unwrap();
        assert_eq!(agents.len(), 2);
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"agent-a"));
        assert!(names.contains(&"agent-b"));
    }

    #[test]
    fn get_installed_found() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let bundle = make_bundle("findme", "3.2.1", "Find this agent");
        installer.install_from_bytes("findme", &bundle).unwrap();

        let agent = installer.get_installed("findme").unwrap().unwrap();
        assert_eq!(agent.name, "findme");
        assert_eq!(agent.version, "3.2.1");
        assert_eq!(agent.description, "Find this agent");
    }

    #[test]
    fn get_installed_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        assert!(installer.get_installed("nope").unwrap().is_none());
    }

    #[test]
    fn get_installed_dir_exists_but_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        // Create directory but no agent.leviath
        fs::create_dir_all(dir.path().join("empty-agent")).unwrap();
        assert!(installer.get_installed("empty-agent").unwrap().is_none());
    }

    // ─── AgentInstaller::new / Default ─────────────────────────────────

    #[test]
    fn new_derives_install_dir_from_home() {
        let installer = AgentInstaller::new();
        assert!(installer.install_dir.ends_with(".leviath/agents"));
    }

    #[test]
    fn default_matches_new() {
        let installer = AgentInstaller::default();
        assert!(installer.install_dir.ends_with(".leviath/agents"));
    }

    // ─── install() (file-based) ────────────────────────────────────────

    #[test]
    fn install_from_file_path_derives_name_from_filename() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().join("agents"));

        let bundle = make_bundle("file-agent", "1.2.3", "Installed from a file");
        let package_path = dir.path().join("file-agent.leviath-bundle");
        fs::write(&package_path, &bundle).unwrap();

        let result = installer.install(&package_path).unwrap();
        assert_eq!(result.name, "file-agent");
        assert_eq!(result.version, "1.2.3");
        assert_eq!(result.description, "Installed from a file");
        assert!(result.path.exists());
    }

    #[test]
    fn install_from_file_path_missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let err = installer
            .install(dir.path().join("does-not-exist.leviath-bundle"))
            .unwrap_err();
        assert!(err.to_string().contains("Failed to read package"));
    }

    // ─── install_from_bytes: create_dir_all failure ────────────────────

    #[test]
    fn install_from_bytes_create_dir_failure_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // Make a plain file where a directory needs to exist, so
        // create_dir_all(install_dir.join(name)) fails.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();

        let installer = AgentInstaller::with_install_dir(blocker.join("agents"));
        let bundle = make_bundle("blocked", "1.0.0", "desc");
        let err = installer
            .install_from_bytes("blocked", &bundle)
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to create install directory"));
    }
}
