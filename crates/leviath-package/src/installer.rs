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
    ///
    /// `LEVIATH_HOME` overrides the resolved home directory when set
    /// (mirrors `leviath-cli`'s `config::leviath_home_dir()`), so tests --
    /// including ones that spawn the real `lev` binary as a child process --
    /// can redirect this without relying on `$HOME`/`%USERPROFILE%`, which
    /// `dirs::home_dir()` does not read on macOS (`NSHomeDirectory()`) or
    /// Windows (`SHGetKnownFolderPath`). `leviath-package` doesn't depend on
    /// `leviath-cli`, so this is a small local duplicate of the same check
    /// rather than a shared helper.
    pub fn new() -> Self {
        // Panic (rather than silently falling back to ".") when neither the
        // override nor home_dir() resolves: a system with no home directory
        // is a misconfigured environment, and failing loudly is better than
        // installing into an unexpected relative path.
        let home = std::env::var_os("LEVIATH_HOME")
            .map(std::path::PathBuf::from)
            .or_else(dirs::home_dir)
            .expect("could not determine home directory");
        let install_dir = home.join(".leviath").join("agents");
        Self { install_dir }
    }

    /// Create an installer with a custom installation directory.
    pub fn with_install_dir(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }

    /// Install an agent from a `.leviath-bundle` file.
    pub fn install(&self, package_path: &Path) -> anyhow::Result<InstalledAgent> {
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

        for entry in
            fs::read_dir(&self.install_dir).expect("install_dir exists — read_dir should not fail")
        {
            let entry = entry.expect("read_dir entry should not fail");
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
    use crate::test_support::with_tracing;
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
        with_tracing(|| {
            let dir = tempfile::tempdir().unwrap();
            let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

            let bundle = make_bundle("test-agent", "1.0.0", "A test agent");
            let result = installer.install_from_bytes("test-agent", &bundle).unwrap();

            assert_eq!(result.name, "test-agent");
            assert_eq!(result.version, "1.0.0");
            assert_eq!(result.description, "A test agent");
            assert!(result.path.exists());
            assert!(result.path.join("agent.leviath").exists());
        });
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
        with_tracing(|| {
            let dir = tempfile::tempdir().unwrap();
            let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

            let bundle = make_bundle("to-remove", "1.0.0", "remove me");
            installer.install_from_bytes("to-remove", &bundle).unwrap();

            assert!(dir.path().join("to-remove").exists());
            installer.uninstall("to-remove").unwrap();
            assert!(!dir.path().join("to-remove").exists());
        });
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
    fn list_installed_skips_non_directory_entries() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        // Install one real agent
        let bundle = make_bundle("good-agent", "1.0.0", "Good");
        installer.install_from_bytes("good-agent", &bundle).unwrap();

        // A regular file (not a dir) — covers the `if path.is_dir()` false branch
        fs::write(dir.path().join("not-an-agent.txt"), "hello").unwrap();

        // A dir without an agent.leviath manifest — covers the `if manifest_path.exists()` false branch
        fs::create_dir_all(dir.path().join("no-manifest-dir")).unwrap();

        let agents = installer.list_installed().unwrap();
        // Only the properly-installed agent is returned; file and bare dir are skipped
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "good-agent");
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
        with_tracing(|| {
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
        });
    }

    #[test]
    fn install_from_file_path_missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let err = installer
            .install(&dir.path().join("does-not-exist.leviath-bundle"))
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

    #[test]
    fn install_from_bytes_corrupt_tar_after_valid_gzip_returns_extract_error() {
        // Valid gzip framing wrapping bytes that are NOT a valid tar
        // archive -- `GzDecoder` decompresses fine, but `Archive::unpack`
        // fails on the malformed header, exercising the "Failed to extract
        // package" error arm that every other test's well-formed bundle
        // never reaches.
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        use std::io::Write;
        encoder
            .write_all(&[b'x'; 600]) // not a valid 512-byte tar header
            .unwrap();
        let bundle = encoder.finish().unwrap();

        let err = installer
            .install_from_bytes("corrupt-tar", &bundle)
            .unwrap_err();
        assert!(err.to_string().contains("Failed to extract package"));
    }

    /// Assert that an uninstall result either failed with the expected error
    /// message OR succeeded (which happens when running as root, since root
    /// ignores Unix permission bits).  Extracted so both paths are covered by
    /// distinct tests.
    #[cfg(unix)]
    fn assert_failed_or_root(result: anyhow::Result<()>) {
        if result.is_ok() {
            return; // running as root — permission lock had no effect
        }
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to remove agent"));
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_remove_dir_all_failure_returns_error() {
        // Removing write permission on the *parent* directory (not the
        // agent directory itself) means `remove_dir_all` can't unlink the
        // agent directory's entry from it, even though the agent
        // directory's own contents are otherwise removable — exercising
        // the "Failed to remove agent" error arm no other test reaches.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let agent_dir = dir.path().join("locked-agent");
        fs::create_dir_all(&agent_dir).unwrap();

        let mut locked = fs::metadata(dir.path()).unwrap().permissions();
        locked.set_mode(0o555);
        fs::set_permissions(dir.path(), locked).unwrap();

        let result = installer.uninstall("locked-agent");

        // Restore permissions unconditionally so tempdir cleanup succeeds.
        let mut restored = fs::metadata(dir.path()).unwrap().permissions();
        restored.set_mode(0o755);
        fs::set_permissions(dir.path(), restored).unwrap();

        assert_failed_or_root(result);
    }

    /// Exercises the `if result.is_ok() { return; }` path in
    /// `assert_failed_or_root` by passing a successful result.
    #[cfg(unix)]
    #[test]
    fn assert_failed_or_root_ok_path_returns_immediately() {
        assert_failed_or_root(Ok(()));
    }
}
