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
            anyhow::bail!(
                "Project path '{}' is not a directory",
                project_path.display()
            );
        }

        // Verify agent.leviath exists
        let manifest_path = project_path.join("agent.leviath");
        if !manifest_path.exists() {
            anyhow::bail!("No agent.leviath found in '{}'", project_path.display());
        }

        let mut buf = Vec::new();
        {
            let encoder = GzEncoder::new(&mut buf, Compression::default());
            let mut tar = tar::Builder::new(encoder);

            // Walk the project directory and add files
            self.add_directory_to_tar(&mut tar, project_path, project_path)?;

            let encoder = tar
                .into_inner()
                .map_err(|e| anyhow::anyhow!("Failed to finalize tar archive: {}", e))?;
            encoder
                .finish()
                .map_err(|e| anyhow::anyhow!("Failed to finalize gzip: {}", e))?;
        }

        tracing::info!(size_bytes = buf.len(), "Bundle created");

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

        fs::write(&output, &data).map_err(|e| {
            anyhow::anyhow!("Failed to write bundle to '{}': {}", output.display(), e)
        })?;

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
                tar.append_path_with_name(&path, relative).map_err(|e| {
                    anyhow::anyhow!("Failed to add '{}' to bundle: {}", relative.display(), e)
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

    /// Without a registered `tracing::Subscriber`, `tracing::info!`'s macro
    /// expansion short-circuits field-expression evaluation before the
    /// call's "is this level enabled" check even runs -- so a multi-line
    /// `tracing::info!` call's field-list lines show as uncovered even
    /// though the surrounding branch demonstrably executes. This bare
    /// subscriber reports every callsite enabled, forcing real evaluation.
    struct AlwaysOnSubscriber;
    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
        tracing::subscriber::with_default(AlwaysOnSubscriber, f)
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        use tracing::Subscriber;
        let sub = AlwaysOnSubscriber;
        let span = tracing::span::Id::from_u64(1);
        with_tracing(|| {
            let s = tracing::info_span!("test-span", field = tracing::field::Empty);
            s.record("field", 1);
            s.in_scope(|| {});
        });
        sub.enter(&span);
        sub.exit(&span);
        sub.record_follows_from(&span, &span);
    }

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

    // ─── AgentBundler::default ──────────────────────────────────────────

    #[test]
    fn test_bundler_default() {
        let bundler = AgentBundler::default();
        // Default should have exclusion patterns
        assert!(bundler.should_exclude(".git"));
        assert!(bundler.should_exclude(".env"));
    }

    // ─── with_exclude ───────────────────────────────────────────────────

    #[test]
    fn test_with_exclude_adds_pattern() {
        let bundler = AgentBundler::new().with_exclude("*.log".to_string());
        assert!(bundler.should_exclude("app.log"));
        assert!(bundler.should_exclude("error.log"));
        assert!(!bundler.should_exclude("readme.txt"));
    }

    #[test]
    fn test_with_exclude_chaining() {
        let bundler = AgentBundler::new()
            .with_exclude("*.log".to_string())
            .with_exclude("*.tmp".to_string())
            .with_exclude("node_modules".to_string());
        assert!(bundler.should_exclude("app.log"));
        assert!(bundler.should_exclude("test.tmp"));
        assert!(bundler.should_exclude("node_modules"));
    }

    // ─── should_exclude: edge cases ─────────────────────────────────────

    #[test]
    fn test_should_exclude_empty_filename() {
        let bundler = AgentBundler::new();
        assert!(!bundler.should_exclude(""));
    }

    #[test]
    fn test_should_exclude_dot_files() {
        let bundler = AgentBundler::new();
        // .git is excluded but .gitignore is not
        assert!(bundler.should_exclude(".git"));
        assert!(!bundler.should_exclude(".gitignore"));
    }

    #[test]
    fn test_should_exclude_wildcard_multiple_dots() {
        let bundler = AgentBundler::new();
        assert!(bundler.should_exclude("server.private.key"));
        assert!(bundler.should_exclude("my.cert.pem"));
    }

    // ─── bundle: not a directory ────────────────────────────────────────

    #[test]
    fn test_bundle_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not_a_dir.txt");
        fs::write(&file, "content").unwrap();

        let bundler = AgentBundler::new();
        let result = bundler.bundle(&file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not a directory"));
    }

    // ─── bundle: missing manifest ───────────────────────────────────────

    #[test]
    fn test_bundle_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let bundler = AgentBundler::new();
        let result = bundler.bundle(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    // ─── bundle: valid project ──────────────────────────────────────────

    #[test]
    fn test_bundle_valid_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        // Create minimal agent project
        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();
        fs::write(project.join("README.md"), "# Test Agent").unwrap();

        let bundler = AgentBundler::new();
        let result = bundler.bundle(project);
        assert!(result.is_ok());
        let data = result.unwrap();
        assert!(!data.is_empty());
    }

    // ─── bundle: excludes sensitive files ───────────────────────────────

    #[test]
    fn test_bundle_excludes_sensitive_files() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();
        fs::write(project.join(".env"), "SECRET=123").unwrap();
        fs::write(project.join("server.key"), "private key").unwrap();
        fs::write(project.join("cert.pem"), "certificate").unwrap();
        fs::write(project.join("safe.txt"), "this is fine").unwrap();

        let bundler = AgentBundler::new();
        let data = bundler.bundle(project).unwrap();

        // Decompress and check: .env, server.key, cert.pem should not be in the archive
        let decoder = flate2::read::GzDecoder::new(&data[..]);
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(names.iter().any(|n| n.contains("agent.leviath")));
        assert!(names.iter().any(|n| n.contains("safe.txt")));
        assert!(!names.iter().any(|n| n.contains(".env")));
        assert!(!names.iter().any(|n| n.contains("server.key")));
        assert!(!names.iter().any(|n| n.contains("cert.pem")));
    }

    // ─── bundle_to_file ─────────────────────────────────────────────────

    #[test]
    fn test_bundle_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();

        let output = dir.path().join("output.leviath-bundle");
        let bundler = AgentBundler::new();
        let result = with_tracing(|| bundler.bundle_to_file(&project, &output));
        assert!(result.is_ok());
        assert!(output.exists());
        let file_size = fs::metadata(&output).unwrap().len();
        assert!(file_size > 0);
    }

    #[test]
    fn test_bundle_to_file_write_failure_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();

        // Output path inside a directory that doesn't exist — fs::write must fail.
        let output = dir
            .path()
            .join("no-such-parent-dir")
            .join("output.leviath-bundle");
        let bundler = AgentBundler::new();
        let result = bundler.bundle_to_file(&project, &output);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to write bundle"));
    }

    #[cfg(unix)]
    #[test]
    fn test_bundle_unreadable_file_returns_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();

        let secret = project.join("secret.dat");
        fs::write(&secret, b"cant read me").unwrap();
        // Remove all permissions so opening the file for the tar archive fails.
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

        let bundler = AgentBundler::new();
        let result = bundler.bundle(project);

        // Restore permissions so tempdir cleanup can remove the file.
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to add"));
    }

    // ─── bundle: with subdirectories ────────────────────────────────────

    #[test]
    fn test_bundle_with_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();
        let sub = project.join("scripts");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("init.rhai"), "// init script").unwrap();

        let bundler = AgentBundler::new();
        let data = bundler.bundle(project).unwrap();
        assert!(!data.is_empty());

        // Verify subdirectory file is included
        let decoder = flate2::read::GzDecoder::new(&data[..]);
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(names.iter().any(|n| n.contains("init.rhai")));
    }

    // ─── bundle: excluded subdirectory ──────────────────────────────────

    #[test]
    fn test_bundle_excludes_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();
        let git = project.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main").unwrap();

        let bundler = AgentBundler::new();
        let data = bundler.bundle(project).unwrap();

        let decoder = flate2::read::GzDecoder::new(&data[..]);
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(!names.iter().any(|n| n.contains(".git")));
    }
}
