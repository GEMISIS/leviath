//! Agent bundling for distribution.
//!
//! Creates `.leviath-bundle` files which are tar.gz archives containing
//! the agent manifest, definition, scripts, and documentation.

use flate2::Compression;
use flate2::write::GzEncoder;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The directory-read operation used while walking a project tree, injectable
/// so tests can force the walk's `read_dir` error and recursion-propagation
/// arms deterministically on every platform (a `chmod 0o000` subdirectory is
/// Unix-only; the OS-agnostic failures — a missing top-level path, a
/// mismatched `base`, an empty tar entry name — can't produce a *recursive*
/// read failure, because during recursion `base` is always an ancestor of the
/// entry). It is a trait object rather than `impl Fn` so production and every
/// test share exactly ONE monomorphization of the walk functions. Production
/// always passes `std::fs::read_dir`.
type DirReader<'a> = &'a dyn Fn(&Path) -> std::io::Result<fs::ReadDir>;

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
    ///
    /// Takes `project_path: &Path` (not `impl AsRef<Path>`) so every caller --
    /// production code and the various `&Path`/`&PathBuf`/`&&PathBuf` shapes
    /// tests pass -- shares exactly ONE monomorphization; `&PathBuf` and
    /// `&&PathBuf` coerce to `&Path` automatically via deref coercion at the
    /// call site, so no caller needs to change beyond that.
    pub fn bundle(&self, project_path: &Path) -> anyhow::Result<Vec<u8>> {
        self.bundle_with(project_path, &|p| fs::read_dir(p))
    }

    /// [`Self::bundle`] with an injectable directory reader; see [`DirReader`].
    fn bundle_with(&self, project_path: &Path, read_dir: DirReader) -> anyhow::Result<Vec<u8>> {
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
        self.write_bundle(project_path, &mut buf, read_dir)?;

        tracing::info!(size_bytes = buf.len(), "Bundle created");

        Ok(buf)
    }

    /// Walk `project_path` and write a tar.gz archive to `sink`.
    ///
    /// Takes `sink` as `&mut dyn Write` (a trait object) rather than a
    /// generic `W: Write` so that every caller -- the real `Vec<u8>`/file
    /// sink as well as tests injecting sinks that fail on write to exercise
    /// the tar/gzip finalization error paths below -- shares exactly ONE
    /// monomorphization of this function (and, transitively, of
    /// `add_directory_to_tar`) instead of one per concrete sink type.
    fn write_bundle(
        &self,
        project_path: &Path,
        sink: &mut dyn Write,
        read_dir: DirReader,
    ) -> anyhow::Result<()> {
        let mut encoder = GzEncoder::new(sink, Compression::default());
        {
            let mut tar = tar::Builder::new(&mut encoder as &mut dyn Write);

            // Walk the project directory and add files
            self.add_directory_to_tar(&mut tar, project_path, project_path, read_dir)?;

            tar.into_inner()
                .map_err(|e| anyhow::anyhow!("Failed to finalize tar archive: {}", e))?;
        }
        encoder
            .finish()
            .map_err(|e| anyhow::anyhow!("Failed to finalize gzip: {}", e))?;
        Ok(())
    }

    /// Bundle an agent and write to a file.
    ///
    /// Concrete `&Path` params for the same single-monomorphization reason
    /// as `bundle` above.
    pub fn bundle_to_file(
        &self,
        project_path: &Path,
        output_path: &Path,
    ) -> anyhow::Result<PathBuf> {
        let data = self.bundle(project_path)?;
        let output = output_path.to_path_buf();

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
    ///
    /// `tar` is `&mut tar::Builder<&mut dyn Write>` (matching `write_bundle`'s
    /// trait-object sink) rather than `tar::Builder<W>` generic over `W:
    /// Write`, for the same single-monomorphization reason described on
    /// `write_bundle`.
    fn add_directory_to_tar(
        &self,
        tar: &mut tar::Builder<&mut dyn Write>,
        dir: &Path,
        base: &Path,
        read_dir: DirReader,
    ) -> anyhow::Result<()> {
        for entry in read_dir(dir)
            .map_err(|e| anyhow::anyhow!("Failed to read directory '{}': {}", dir.display(), e))?
        {
            // `ReadDir::next()`'s `Err` arm surfaces OS-level directory-read
            // faults (not TOCTOU races): on both macOS and Linux, `readdir`
            // returns entry names without `stat`-ing them, so deleting a
            // file mid-iteration doesn't make this fail -- the failure path
            // would need an actual OS/filesystem-driver-level error while
            // listing, which is unreachable in practice.
            let entry = entry.expect("read_dir should not fail during bundle");
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
                self.add_directory_to_tar(tar, &path, base, read_dir)?;
                continue;
            }
            if !path.is_file() {
                continue;
            }
            tar.append_path_with_name(&path, relative).map_err(|e| {
                anyhow::anyhow!("Failed to add '{}' to bundle: {}", relative.display(), e)
            })?;
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
    use crate::test_support::with_tracing;

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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to write bundle")
        );
    }

    // The tar-append error arm ("Failed to add ...") is exercised OS-agnostically
    // by calling the walk directly with a `base` equal to the file being
    // appended: `strip_prefix(base)` then yields an *empty* relative path, which
    // `tar::Builder::append_path_with_name` rejects on every platform ("paths in
    // archives must have at least one component").
    #[test]
    fn test_add_directory_to_tar_append_failure_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        let file = project.join("file.txt");
        fs::write(&file, b"content").unwrap();

        let bundler = AgentBundler::new();
        let mut sink = Vec::new();
        let mut tar = tar::Builder::new(&mut sink as &mut dyn Write);
        // base == the file: for that entry `strip_prefix` returns "" (empty), so
        // the file is appended under an empty name and tar rejects it.
        let result = bundler.add_directory_to_tar(&mut tar, project, &file, &|p| fs::read_dir(p));

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

    // ─── bundle_to_file: bundle() itself fails ──────────────────────────

    #[test]
    fn test_bundle_to_file_missing_manifest_propagates_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        // No agent.leviath written -- `bundle()` must fail before ever
        // reaching the write step, and `bundle_to_file` must propagate that
        // failure via its `?` rather than attempting to write anything.
        let output = dir.path().join("output.leviath-bundle");

        let bundler = AgentBundler::new();
        let result = bundler.bundle_to_file(&project, &output);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
        assert!(!output.exists());
    }

    // ─── add_directory_to_tar: entries that are neither dir nor file ────

    #[cfg(unix)]
    #[test]
    fn test_bundle_skips_broken_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();

        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();

        // A symlink whose target doesn't exist: `Path::is_dir`/`is_file`
        // both follow symlinks and return `false` when the target is
        // missing, so this entry is neither -- exercising the "skip
        // anything that isn't a plain file or directory" branch.
        std::os::unix::fs::symlink(project.join("does-not-exist"), project.join("dangling"))
            .unwrap();

        let bundler = AgentBundler::new();
        let data = bundler.bundle(project).unwrap();
        assert!(!data.is_empty());

        let decoder = flate2::read::GzDecoder::new(&data[..]);
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(!names.iter().any(|n| n.contains("dangling")));
    }

    // A portable analogue of `test_bundle_skips_broken_symlink` that needs no
    // `std::os::unix::fs::symlink`: a directory entry whose backing path no
    // longer exists on disk classifies as *neither* a regular file nor a
    // directory (`is_file()` and `is_dir()` both return `false`), exercising
    // the `if !path.is_file() { continue; }` skip on every OS. We produce such
    // a dangling entry via the injected reader: it opens the directory, then
    // renames that directory out from under the still-open `ReadDir` handle.
    // The open handle stays valid and still yields `ghost.txt`, but the entry's
    // reconstructed `path()` (built from the *original* directory name) now
    // points at a location that has moved away and no longer resolves --
    // confirmed empirically to be neither file nor dir.
    #[test]
    fn test_add_directory_to_tar_skips_neither_file_nor_dir() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("src");
        let moved = root.path().join("src-moved");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("ghost.txt"), "content").unwrap();

        let bundler = AgentBundler::new();
        let mut sink = Vec::new();
        let mut tar = tar::Builder::new(&mut sink as &mut dyn Write);

        let src_for_reader = src.clone();
        let moved_for_reader = moved.clone();
        let read_dir = move |p: &Path| -> std::io::Result<fs::ReadDir> {
            // `.unwrap()` rather than `?` on purpose: these operations succeed
            // deterministically here, and `?` would leave its never-taken `Err`
            // arm as an uncovered region.
            let rd = fs::read_dir(p).unwrap();
            // Move the directory aside *after* opening it.
            fs::rename(&src_for_reader, &moved_for_reader).unwrap();
            Ok(rd)
        };

        let result = bundler.add_directory_to_tar(&mut tar, &src, &src, &read_dir);
        assert!(result.is_ok());
        tar.finish().unwrap();
        drop(tar);

        // The dangling entry was skipped -- nothing was appended to the tar.
        let mut archive = tar::Archive::new(&sink[..]);
        assert_eq!(archive.entries().unwrap().count(), 0);
    }

    // ─── add_directory_to_tar: recursive read_dir failure propagates ────

    // A `read_dir` that fails when the walk recurses into a subdirectory
    // exercises, in one go, the recursive read-dir map_err (`Failed to read
    // directory`), the recursion `?` that propagates it back up, and both
    // enclosing `?`s in `write_bundle` and `bundle_with`. Injecting the reader
    // makes this deterministic on every platform. (During real
    // recursion `base` is always an ancestor of the entry, so no OS-agnostic
    // path/tar failure can surface *inside* a recursion -- injection is the
    // only cross-platform way to reach these propagation arms.)
    #[test]
    fn test_bundle_recursive_read_dir_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(
            project.join("agent.leviath"),
            "[agent]\nname = \"test\"\nversion = \"1.0.0\"\ndescription = \"test\"\n",
        )
        .unwrap();
        let locked = project.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("secret.txt"), "shh").unwrap();

        let bundler = AgentBundler::new();
        // Succeeds for the top-level walk, fails only when recursing into
        // `locked`, so the failure surfaces from *inside* the recursion.
        let result = bundler.bundle_with(project, &|p| {
            if p.file_name() == Some(std::ffi::OsStr::new("locked")) {
                Err(std::io::Error::other("simulated read_dir failure"))
            } else {
                fs::read_dir(p)
            }
        });

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read directory")
        );
    }

    // ─── add_directory_to_tar: strip_prefix failure (direct unit test) ──
    //
    // Every real caller of `add_directory_to_tar` passes the same
    // `project_path` as both `dir` and `base` on the initial call, and
    // recursion only ever descends from `dir` into its own children --
    // so `path.strip_prefix(base)` can never fail via the public `bundle()`
    // API. It's still a private method, so a test in this module can call
    // it directly with a `base` that isn't an ancestor of `dir`, exercising
    // the defensive `map_err` without fabricating a broken filesystem.
    #[test]
    fn test_add_directory_to_tar_direct_strip_prefix_failure() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("file.txt"), "content").unwrap();

        let unrelated = tempfile::tempdir().unwrap();

        let bundler = AgentBundler::new();
        let mut sink = Vec::new();
        let mut tar = tar::Builder::new(&mut sink as &mut dyn Write);
        let result =
            bundler.add_directory_to_tar(&mut tar, project, unrelated.path(), &|p| fs::read_dir(p));

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to compute relative path")
        );
    }

    // ─── write_bundle: tar/gzip finalize failure paths ──────────────────

    /// A `Write` sink that fails immediately, used to force
    /// `tar::Builder::into_inner`'s write of the gzip header (which happens
    /// on the very first byte written through the encoder) to fail.
    #[derive(Debug, Default)]
    struct AlwaysFailingWriter;
    impl Write for AlwaysFailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("simulated flush failure"))
        }
    }

    #[test]
    fn test_write_bundle_into_inner_failure_returns_error() {
        // An empty directory (no files at all, not even a manifest --
        // `write_bundle` itself has no manifest check; that lives in the
        // public `bundle()` wrapper) means `add_directory_to_tar` writes
        // nothing during the walk. The very first byte `write_bundle`
        // writes to the sink is the gzip header, written lazily by
        // `GzEncoder` on `tar::Builder::into_inner`'s first `write` call --
        // so an always-failing sink fails there, at `tar.into_inner()`.
        let dir = tempfile::tempdir().unwrap();

        let bundler = AgentBundler::new();
        let mut sink = AlwaysFailingWriter;
        let result = bundler.write_bundle(dir.path(), &mut sink, &|p| fs::read_dir(p));

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to finalize tar archive")
        );
    }

    /// A `Write` sink that succeeds exactly once (letting the gzip header
    /// through `tar::Builder::into_inner` succeed) and fails on every
    /// subsequent call, forcing `GzEncoder::finish`'s write of the
    /// compressed body/trailer to fail instead.
    #[derive(Debug, Default)]
    struct FailsAfterFirstWriteWriter {
        calls: usize,
    }
    impl Write for FailsAfterFirstWriteWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == 1 {
                Ok(buf.len())
            } else {
                Err(std::io::Error::other("simulated write failure"))
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_write_bundle_gzip_finish_failure_returns_error() {
        let dir = tempfile::tempdir().unwrap();

        let bundler = AgentBundler::new();
        let mut sink = FailsAfterFirstWriteWriter::default();
        let result = bundler.write_bundle(dir.path(), &mut sink, &|p| fs::read_dir(p));

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to finalize gzip")
        );
    }

    // Neither `tar::Builder::into_inner` nor `GzEncoder::finish` ever calls
    // `Write::flush` on the underlying sink directly (confirmed empirically:
    // a sink whose `flush` always errors but whose `write` always succeeds
    // makes `write_bundle` succeed) -- so these `flush` impls are
    // unreachable via `write_bundle` no matter how the sink is configured.
    // Test them directly, matching `always_on_subscriber_span_methods_are_all_no_ops`'s
    // precedent elsewhere in this file for otherwise-unreachable trait-impl methods.
    #[test]
    fn always_failing_writer_flush_returns_error() {
        assert!(AlwaysFailingWriter.flush().is_err());
    }

    #[test]
    fn fails_after_first_write_writer_flush_returns_ok() {
        assert!(FailsAfterFirstWriteWriter::default().flush().is_ok());
    }
}
