//! Agent installation from bundle archives.

use flate2::read::GzDecoder;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest decompressed size a bundle may unpack to.
///
/// Bounds a decompression bomb: gzip reaches ratios well past 1000:1, so a
/// bundle small enough to look unremarkable could fill the disk. 256 MiB is far
/// above any real agent bundle (they are manifests, prompts, and a few `.rhai`
/// files) and far below "fills the disk".
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;

/// What an entry is, as far as the symlink check cares.
///
/// A three-way answer rather than a `FileType`, because `FileType` cannot be
/// constructed without a real file of that kind - and a real symlink is exactly
/// what a test cannot create on Windows without a privilege CI runners lack.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Entry {
    Dir,
    File,
    /// A symlink, or an entry that could not be stat'd at all - the same
    /// refusal either way, since neither can be certified.
    Refused,
}

/// Classify a directory entry by its own metadata, not its target's.
///
/// `symlink_metadata` does not follow the link, which is the point.
fn classify(path: &Path) -> Entry {
    match fs::symlink_metadata(path).map(|m| m.file_type()).ok() {
        Some(t) if t.is_dir() => Entry::Dir,
        Some(t) if !t.is_symlink() => Entry::File,
        _ => Entry::Refused,
    }
}

/// Refuse a bundle containing any symlink, at any depth, with the entry
/// classifier injected.
///
/// tar-rs blocks entries that *extract* outside the destination, but a symlink
/// entry lands inside it perfectly legally - and then points wherever it likes.
/// Since the installed tree is later scanned for `.rhai` tool scripts and read
/// by the file tools, a link is a way to smuggle content in (or to have a later
/// write follow it out). Nothing in a legitimate agent bundle needs one.
///
/// A `fn` pointer (not `impl Fn`) so there is one monomorphization, matching the
/// seam idiom used elsewhere in the workspace. The seam exists because the
/// refusal cannot be reached otherwise on every platform: it needs a real
/// symlink on disk, and creating one on Windows requires a privilege CI runners
/// do not have. The `#[cfg(unix)]` tests still prove the real behaviour end to
/// end through a genuine symlink in a genuine archive.
fn reject_symlinks_with(dir: &Path, classify: fn(&Path) -> Entry) -> anyhow::Result<()> {
    // `into_iter().flatten().flatten()` rather than a `match` on `read_dir`: this
    // directory was created and unpacked into moments ago, so an unreadable one
    // has no reachable test - and it surfaces anyway on the manifest read that
    // follows. Collapsing it to "no entries" keeps the semantics with no branch
    // nothing can exercise.
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match classify(&path) {
            Entry::Dir => reject_symlinks_with(&path, classify)?,
            Entry::File => {}
            Entry::Refused => anyhow::bail!(
                "Package contains a symlink or unreadable entry ('{}'), which is not \
                 permitted in an agent bundle",
                path.display()
            ),
        }
    }
    Ok(())
}

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

/// The version and description an `agent.leviath` declares, with the
/// defaults the catalogue shows when the file is missing, unreadable or not
/// TOML: `0.0.0` and an empty description. Three listings used to read and
/// parse the file each with these same two lookups inline.
fn manifest_meta(manifest_path: &Path) -> (String, String) {
    let content = fs::read_to_string(manifest_path).unwrap_or_default();
    let parsed: toml::Value =
        toml::from_str(&content).unwrap_or(toml::Value::Table(toml::map::Map::new()));
    let field = |key: &str, default: &str| -> String {
        parsed
            .get("agent")
            .and_then(|a| a.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    };
    (field("version", "0.0.0"), field("description", ""))
}

impl AgentInstaller {
    /// Create a new installer using the default installation directory.
    ///
    /// The install root comes from the shared `LEVIATH_HOME`-aware resolver
    /// in [`leviath_core::paths`], so this crate installs into exactly the
    /// tree every other component reads.
    pub fn new() -> Self {
        // Panic (rather than silently falling back to ".") when no home
        // resolves: a system with no home directory is a misconfigured
        // environment, and failing loudly is better than installing into an
        // unexpected relative path.
        let install_dir =
            leviath_core::paths::agents_dir().expect("could not determine home directory");
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
    ///
    /// `name` becomes a directory under the install dir, so it must be a single
    /// safe path component. `install` derives it from `file_stem()` (which
    /// already strips directories), but this is `pub` and any future caller
    /// passing a downloaded or user-supplied name would otherwise get a
    /// traversal for free - `Path::join` does not normalize, and an absolute
    /// name replaces the base entirely.
    pub fn install_from_bytes(&self, name: &str, data: &[u8]) -> anyhow::Result<InstalledAgent> {
        self.install_from_bytes_with(name, data, classify)
    }

    /// Extract `data` into `dest` and validate what came out.
    ///
    /// `Read::take` bounds the *decompressed* stream. Without it a ~1 MB bundle
    /// could expand to fill the disk - the classic decompression bomb - and
    /// nothing downstream would notice until the write failed.
    ///
    /// `set_preserve_permissions(false)` and `set_unpack_xattrs(false)`:
    /// otherwise an attacker-authored archive chooses the modes and extended
    /// attributes of the files it drops into the user's home.
    ///
    /// tar-rs already rejects `..` components and validates every entry against
    /// the destination (including hard links), so classic zip-slip is covered by
    /// the dependency - which is a reason to keep `cargo audit` watching it, not
    /// a reason to assume it always will.
    fn unpack_into(dest: &Path, data: &[u8], classify: fn(&Path) -> Entry) -> anyhow::Result<()> {
        let decoder = GzDecoder::new(data).take(MAX_UNPACKED_BYTES);
        let mut archive = tar::Archive::new(decoder);
        archive.set_preserve_permissions(false);
        archive.set_unpack_xattrs(false);
        archive.unpack(dest).map_err(|e| {
            anyhow::anyhow!(
                "Failed to extract package: {}. (Bundles are limited to {} MiB \
                 uncompressed.)",
                e,
                MAX_UNPACKED_BYTES / (1024 * 1024)
            )
        })?;
        reject_symlinks_with(dest, classify)
    }

    /// Move a validated staging directory over the install destination.
    ///
    /// Remove-then-rename rather than rename-over: Windows refuses to rename
    /// onto an existing directory. That leaves a window where a previous
    /// install is gone and the new one is not yet in place, so a failure here
    /// says so plainly rather than reporting a generic install error.
    fn swap_into_place(staging: &Path, dest: &Path) -> anyhow::Result<()> {
        // One error arm for both steps: the remedy is the same either way, and
        // splitting them would mean two messages saying "reinstall the agent".
        let swap = || -> std::io::Result<()> {
            if dest.exists() {
                fs::remove_dir_all(dest)?;
            }
            fs::rename(staging, dest)
        };
        swap().map_err(|e| {
            anyhow::anyhow!(
                "Failed to install into '{}': {}. Any previous install there has been removed - \
                 reinstall the agent.",
                dest.display(),
                e
            )
        })
    }

    /// Core of [`install_from_bytes`](Self::install_from_bytes) with the entry
    /// classifier injected - see [`reject_symlinks_with`] for why the seam
    /// exists. A `fn` pointer, so there is one monomorphization.
    fn install_from_bytes_with(
        &self,
        name: &str,
        data: &[u8],
        classify: fn(&Path) -> Entry,
    ) -> anyhow::Result<InstalledAgent> {
        tracing::info!(name = %name, "Installing agent from bytes");

        if !leviath_core::is_safe_path_component(name) {
            anyhow::bail!(
                "invalid agent name '{name}': names may contain only letters, digits, \
                 '.', '_' and '-'"
            );
        }
        let agent_dir = self.install_dir.join(name);

        // Unpack into a staging directory and swap it in only once the contents
        // have passed every check.
        //
        // Extracting straight into `agent_dir` meant a bundle that failed
        // validation still left its files there - including the symlinks
        // `reject_symlinks_with` had just refused, which `discover_blueprints`
        // would then list as a runnable agent. And because this path
        // `create_dir_all`s over an existing install, a failed *re-install*
        // would leave a working agent half-overwritten.
        //
        // Staged *beside* the agents directory, not inside it, and still on the
        // same filesystem so the swap is a rename rather than a copy.
        //
        // Inside would be simpler and is wrong. Blueprint discovery scans every
        // subdirectory of the agents directory, filters on `is_dir()` rather
        // than skipping dotted names, sorts, and keeps the *first* entry for a
        // given blueprint name. `.staging-…` sorts before every letter, so a
        // staging tree declaring `name = "coder"` does not appear alongside the
        // real `coder` - it **shadows** it. And a crash or SIGKILL between the
        // unpack and the rename leaves that tree behind permanently, holding
        // pre-validation content: the symlinks `reject_symlinks_with` was about
        // to refuse, still discoverable, still shadowing.
        // Built by suffixing the agents directory's own name rather than by
        // walking to its parent: `<...>/agents.staging-coder-123` is a sibling
        // of `agents`, so it is outside what discovery scans, and there is no
        // "what if there is no parent" branch nothing could ever exercise.
        // `OsString` rather than `format!` on a `Display`, so a non-UTF-8 home
        // survives the round trip.
        let mut staging = self.install_dir.clone().into_os_string();
        staging.push(format!(".staging-{name}-{}", std::process::id()));
        let staging = PathBuf::from(staging);
        // The agents directory itself may not exist on a first install. The
        // previous shape created it implicitly by unpacking into it; now that
        // staging happens beside it, the rename needs it to be there already.
        // One fallible step and one error arm: staging is a sibling of the
        // agents directory, so if that directory could be created this one can
        // too - a second message would describe a failure nothing can reach.
        let prepare = || -> std::io::Result<()> {
            fs::create_dir_all(&self.install_dir)?;
            // A same-pid leftover from a crashed run would otherwise be
            // unpacked *into*, mixing two bundles.
            let _ = fs::remove_dir_all(&staging);
            fs::create_dir_all(&staging)
        };
        prepare().map_err(|e| {
            anyhow::anyhow!(
                "Failed to create install directory '{}': {}",
                self.install_dir.display(),
                e
            )
        })?;
        // Every early return from here on goes through this, so a refused
        // bundle leaves nothing behind.
        let staged = Self::unpack_into(&staging, data, classify);
        let result = staged.and_then(|()| Self::swap_into_place(&staging, &agent_dir));
        if let Err(e) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }

        let (version, description) = manifest_meta(&agent_dir.join("agent.leviath"));

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
            fs::read_dir(&self.install_dir).expect("install_dir exists - read_dir should not fail")
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

                    let (version, description) = manifest_meta(&manifest_path);

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

    /// Get information about a specific installed agent, or `None` if it is not
    /// installed.
    ///
    /// Infallible on purpose, and the signature now says so. A manifest that
    /// cannot be read or parsed still means *installed* - the directory and the
    /// file are both there - so it reports the agent with whatever metadata it
    /// could recover rather than failing. That is the state you would run
    /// `lev remove` to fix, and an error here would be the one thing standing
    /// between the user and the fix.
    ///
    /// It previously returned `anyhow::Result` and never once returned `Err`,
    /// which left its only production caller `.unwrap()`-ing an infallible
    /// result inside a function that returns `Result` - a panic waiting for
    /// whoever made this propagate.
    pub fn get_installed(&self, name: &str) -> Option<InstalledAgent> {
        let agent_dir = self.install_dir.join(name);

        if !agent_dir.exists() {
            return None;
        }

        let manifest_path = agent_dir.join("agent.leviath");
        if !manifest_path.exists() {
            return None;
        }

        let (version, description) = manifest_meta(&manifest_path);

        Some(InstalledAgent {
            name: name.to_string(),
            version,
            path: agent_dir,
            description,
        })
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
    use flate2::Compression;
    use flate2::write::GzEncoder;

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

    /// `install_from_bytes` is `pub` and joins `name` onto the install dir.
    /// `Path::join` does not normalize `..` and an absolute name replaces the
    /// base entirely, so an unvalidated name reached anywhere on the filesystem.
    #[test]
    fn install_from_bytes_rejects_traversing_names() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let bundle = make_bundle("x", "1.0.0", "d");
        for name in ["../escape", "../../tmp/escape", "/tmp/escape", "a/b", ".."] {
            let err = installer
                .install_from_bytes(name, &bundle)
                .expect_err("{name} must be refused");
            assert!(err.to_string().contains("invalid agent name"), "{err}");
        }
        assert!(
            !std::path::Path::new("/tmp/escape").exists(),
            "nothing may be created outside the install dir"
        );
    }

    /// A gzip bomb: a small archive that expands without bound. `Read::take`
    /// stops it mid-stream, so the unpack fails instead of filling the disk.
    #[test]
    fn install_from_bytes_refuses_a_decompression_bomb() {
        // 512 MiB of zeros, which gzip compresses to a few hundred KiB - past
        // the 256 MiB cap, so extraction must fail.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut archive = tar::Builder::new(&mut encoder);
            let size = 512 * 1024 * 1024u64;
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "big.bin", std::io::repeat(0).take(size))
                .unwrap();
            archive.finish().unwrap();
        }
        let bomb = encoder.finish().unwrap();
        // The length is bound first: a *call* inside `assert!`'s format
        // arguments is a region only the failing path reaches.
        let compressed = bomb.len();
        assert!(
            compressed < 5 * 1024 * 1024,
            "precondition: the bomb is small on disk"
        );

        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let err = installer
            .install_from_bytes("bomb", &bomb)
            .expect_err("an oversized bundle must be refused");
        assert!(err.to_string().contains("Failed to extract"), "{err}");
    }

    /// A bundle with a `tools/` subdirectory - the realistic shape, and the one
    /// that exercises the recursive descent rather than only the flat case.
    #[test]
    fn install_from_bytes_accepts_a_nested_directory() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut archive = tar::Builder::new(&mut encoder);
            for (path, body) in [
                (
                    "agent.leviath",
                    "[agent]\nname = \"n\"\nversion = \"1.0.0\"\n",
                ),
                ("tools/web_fetch.rhai", "// @tool web_fetch\n"),
            ] {
                let bytes = body.as_bytes();
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                archive.append_data(&mut header, path, bytes).unwrap();
            }
            archive.finish().unwrap();
        }
        let bundle = encoder.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let installed = installer.install_from_bytes("nested", &bundle).unwrap();
        assert!(installed.path.join("tools/web_fetch.rhai").exists());
    }

    /// A symlink hidden one directory down is refused too - the scan descends
    /// rather than checking only the top level, which is where a bundle would
    /// naturally put one (`tools/`).
    #[cfg(unix)]
    #[test]
    fn install_from_bytes_refuses_a_nested_symlink_entry() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut archive = tar::Builder::new(&mut encoder);
            let manifest = "[agent]\nname = \"n\"\nversion = \"1.0.0\"\n";
            let bytes = manifest.as_bytes();
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "agent.leviath", bytes)
                .unwrap();

            let mut link = tar::Header::new_gnu();
            link.set_size(0);
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_mode(0o777);
            archive
                .append_link(&mut link, "tools/escape", "/etc/passwd")
                .unwrap();
            archive.finish().unwrap();
        }
        let bundle = encoder.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let err = installer
            .install_from_bytes("nested-link", &bundle)
            .expect_err("a nested symlink must be refused");
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    /// tar-rs blocks entries that *extract* outside the destination, but a
    /// symlink entry lands inside it legally and then points wherever it likes.
    /// The installed tree is later scanned for `.rhai` tool scripts, so a link
    /// is a way to smuggle content in.
    /// The refusal itself, driven through the injected classifier so it runs on
    /// every platform. The `#[cfg(unix)]` tests below prove the same refusal
    /// against a genuine symlink in a genuine archive; this one proves the arm
    /// fires on Windows too, where a test cannot create one.
    #[test]
    fn reject_symlinks_refuses_an_entry_it_cannot_certify() {
        fn all_refused(_: &Path) -> Entry {
            Entry::Refused
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("thing"), b"x").unwrap();

        let err = reject_symlinks_with(dir.path(), all_refused)
            .expect_err("an entry that cannot be certified is refused");
        assert!(err.to_string().contains("symlink or unreadable"), "{err}");
    }

    /// The refusal has to propagate out of a *nested* directory too - a bundle
    /// plants its `tools/` subdirectory, not its root.
    #[test]
    fn reject_symlinks_refuses_an_entry_nested_in_a_subdirectory() {
        /// Refuses only the leaf, so the recursion has to reach it.
        fn refuse_the_leaf(path: &Path) -> Entry {
            match path.file_name().and_then(|n| n.to_str()) {
                Some("web_fetch.rhai") => Entry::Refused,
                _ => classify(path),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("tools");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("web_fetch.rhai"), b"x").unwrap();

        let err = reject_symlinks_with(dir.path(), refuse_the_leaf)
            .expect_err("a refused entry one level down is still refused");
        assert!(err.to_string().contains("web_fetch.rhai"), "{err}");
    }

    /// And the refusal fails the *install*, rather than being computed and
    /// discarded - the bundle must not be left in place.
    #[test]
    fn install_refuses_a_bundle_whose_entries_cannot_be_certified() {
        fn all_refused(_: &Path) -> Entry {
            Entry::Refused
        }

        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let bundle = make_bundle("probe", "1.0.0", "a probe");

        let err = installer
            .install_from_bytes_with("probe", &bundle, all_refused)
            .expect_err("an uncertifiable bundle must not install");
        assert!(err.to_string().contains("symlink or unreadable"), "{err}");
    }

    /// Ordinary files and nested directories pass, so the test above is not
    /// passing merely because everything is refused.
    #[test]
    fn reject_symlinks_admits_ordinary_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("tools");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("web_fetch.rhai"), b"x").unwrap();
        std::fs::write(dir.path().join("agent.leviath"), b"x").unwrap();

        reject_symlinks_with(dir.path(), classify).expect("an ordinary bundle passes");
        // And the classifier itself agrees about what it saw.
        assert_eq!(classify(&nested), Entry::Dir);
        assert_eq!(classify(&nested.join("web_fetch.rhai")), Entry::File);
        assert_eq!(classify(&dir.path().join("no-such-entry")), Entry::Refused);
    }

    #[cfg(unix)]
    #[test]
    fn install_from_bytes_refuses_a_symlink_entry() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut archive = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            archive
                .append_link(&mut header, "escape", "/etc/passwd")
                .unwrap();
            archive.finish().unwrap();
        }
        let bundle = encoder.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let err = installer
            .install_from_bytes("linky", &bundle)
            .expect_err("a symlink entry must be refused");
        assert!(err.to_string().contains("symlink"), "{err}");
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

        // A regular file (not a dir) - covers the `if path.is_dir()` false branch
        fs::write(dir.path().join("not-an-agent.txt"), "hello").unwrap();

        // A dir without an agent.leviath manifest - covers the `if manifest_path.exists()` false branch
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

        let agent = installer.get_installed("findme").unwrap();
        assert_eq!(agent.name, "findme");
        assert_eq!(agent.version, "3.2.1");
        assert_eq!(agent.description, "Find this agent");
    }

    #[test]
    fn get_installed_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        assert!(installer.get_installed("nope").is_none());
    }

    #[test]
    fn get_installed_dir_exists_but_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        // Create directory but no agent.leviath
        fs::create_dir_all(dir.path().join("empty-agent")).unwrap();
        assert!(installer.get_installed("empty-agent").is_none());
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
        // Make a plain file where a directory needs to exist, so creating the
        // agents directory under it fails.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();

        let installer = AgentInstaller::with_install_dir(blocker.join("agents"));
        let bundle = make_bundle("blocked", "1.0.0", "desc");
        let err = installer
            .install_from_bytes("blocked", &bundle)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to create install directory"),
            "got: {err}"
        );
    }

    /// A bundle that fails validation must leave nothing on disk. Extracting
    /// straight into the destination meant the symlinks `reject_symlinks_with`
    /// had just refused stayed there, and `discover_blueprints` would list the
    /// half-extracted tree as a runnable agent.
    #[test]
    fn a_rejected_bundle_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let bundle = make_bundle("evil", "1.0.0", "desc");

        installer
            .install_from_bytes_with("evil", &bundle, |_| Entry::Refused)
            .expect_err("a bundle full of symlinks must be refused");

        // Counted rather than named: the assertion is that there is nothing to
        // name, so a closure building the names would never run.
        let leftovers = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(
            leftovers, 0,
            "a refused install left {leftovers} entries behind"
        );
    }

    /// The swap can fail on its own - a stray *file* sitting where the agent
    /// directory belongs cannot be removed as a directory. The message has to
    /// say the install did not happen rather than reporting success.
    #[test]
    fn a_blocked_destination_reports_a_failed_install() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("blocked"), b"a file, not a directory").unwrap();

        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let err = installer
            .install_from_bytes("blocked", &make_bundle("blocked", "1.0.0", "desc"))
            .expect_err("a file in the way must not be silently replaced");
        assert!(err.to_string().contains("Failed to install into"), "{err}");

        // And the staging directory is not left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".staging-"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?} behind");
    }

    /// Staging must not land inside the directory blueprint discovery scans.
    ///
    /// Discovery filters on `is_dir()` rather than skipping dotted names, sorts,
    /// and keeps the *first* entry per blueprint name - and `.` sorts before
    /// every letter. So a staging tree inside the agents directory would not sit
    /// alongside the real agent, it would shadow it; and a crash between the
    /// unpack and the rename would leave that tree there permanently, holding
    /// exactly the pre-validation content the symlink check was about to refuse.
    #[test]
    fn staging_never_lands_inside_the_scanned_agents_directory() {
        let home = tempfile::tempdir().unwrap();
        let agents = home.path().join("agents");
        let installer = AgentInstaller::with_install_dir(agents.clone());

        installer
            .install_from_bytes("coder", &make_bundle("coder", "1.0.0", "real"))
            .expect("install succeeds");

        // Only the agent itself is in the scanned directory - nothing dotted,
        // nothing that would sort ahead of it.
        let mut entries: Vec<String> = fs::read_dir(&agents)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, ["coder"]);

        // And a refused install leaves nothing beside it either, so a crash
        // window is the only way to strand a staging tree at all.
        installer
            .install_from_bytes_with("coder", &make_bundle("coder", "2.0.0", "evil"), |_| {
                Entry::Refused
            })
            .expect_err("a symlink bundle is refused");
        let stranded = fs::read_dir(home.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".staging-"))
            .count();
        assert_eq!(stranded, 0, "a refused install stranded a staging tree");
    }

    /// A failed re-install must not destroy the agent that was already there.
    /// This is why the fix is a staged swap and not a `remove_dir_all` on the
    /// error path - that would have introduced exactly this bug.
    #[test]
    fn a_failed_reinstall_keeps_the_previous_install() {
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());

        installer
            .install_from_bytes("keeper", &make_bundle("keeper", "1.0.0", "original"))
            .expect("the first install succeeds");

        installer
            .install_from_bytes_with("keeper", &make_bundle("keeper", "2.0.0", "evil"), |_| {
                Entry::Refused
            })
            .expect_err("the second install is refused");

        let manifest = fs::read_to_string(dir.path().join("keeper").join("agent.leviath"))
            .expect("the original install is still readable");
        assert!(
            manifest.contains("1.0.0"),
            "the working install was replaced by a refused one: {manifest}"
        );
    }

    #[test]
    fn install_from_bytes_corrupt_tar_after_valid_gzip_returns_extract_error() {
        // Valid gzip framing wrapping bytes that are NOT a valid tar
        // archive - `GzDecoder` decompresses fine, but `Archive::unpack`
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

    #[test]
    fn uninstall_remove_dir_all_failure_returns_error() {
        // The installed "agent" entry is a regular file rather than a
        // directory: `exists()` passes the guard, but `remove_dir_all`
        // requires a directory and fails on every platform (NotADirectory),
        // exercising the "Failed to remove agent" error arm.
        let dir = tempfile::tempdir().unwrap();
        let installer = AgentInstaller::with_install_dir(dir.path().to_path_buf());
        let agent_path = dir.path().join("not-a-dir");
        fs::write(&agent_path, b"i am a file, not a directory").unwrap();

        let result = installer.uninstall("not-a-dir");

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to remove agent")
        );
    }
}
