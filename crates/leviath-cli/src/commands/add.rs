//! `lev add` - Install an agent package

use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct AddArgs {
    /// Path to agent directory, .leviath-bundle file, or registry package name
    #[arg(value_name = "PACKAGE")]
    pub package: String,

    /// Install from registry (URL override)
    #[arg(short, long)]
    pub registry: Option<String>,
}

fn agents_dir_from_home(home: Option<std::path::PathBuf>) -> anyhow::Result<std::path::PathBuf> {
    let home = home.ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(".leviath").join("agents"))
}

pub async fn execute(args: AddArgs) -> anyhow::Result<()> {
    let installer = leviath_package::AgentInstaller::new();
    let agents_dir = resolve_agents_dir()?;
    execute_with(&args, &installer, &agents_dir).await
}

/// COVERAGE-EXCLUDED: `agents_dir_from_home`'s `None` arm is fully covered
/// directly by `agents_dir_from_home_none_returns_error`, but this real
/// wrapper's own call to `leviath_home_dir()` can't be forced to return
/// `None` in a test: on macOS, `dirs::home_dir()` falls back to a
/// passwd-database lookup independent of `$HOME`, so there is no
/// environment manipulation short of running as a UID with no passwd
/// entry (not something any test in this suite may safely attempt) that
/// makes it fail. Isolating this real-environment query behind a twin
/// removes the unforceable branch from what's measured; the twin below
/// adds a test-only failure-injection toggle so `execute()`'s own
/// error-propagation branch for this call (the `?` right after) can still
/// be driven for real, instead of just relocating the same
/// permanently-Ok gap one level up.
#[cfg(not(test))]
fn resolve_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    agents_dir_from_home(crate::config::leviath_home_dir())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle for [`resolve_agents_dir`]'s twin below, letting
    /// `execute_returns_err_when_agents_dir_unresolvable` force the `Err`
    /// arm deterministically.
    static FORCE_AGENTS_DIR_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Under test, real home-dir resolution always succeeds in every real
/// dev/CI environment (see the doc comment above for why the failure
/// branch can't be forced for real), so this twin normally returns a
/// placeholder path -- this also means tests exercising `execute()` never
/// touch the real `~/.leviath/agents` directory, matching the intent
/// already implied by
/// `execute_real_wrapper_fails_fast_without_touching_real_agents_dir`'s
/// name -- unless [`FORCE_AGENTS_DIR_ERROR`] has been set, in which case it
/// fails the same way the real implementation would with no home
/// directory.
#[cfg(test)]
fn resolve_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    if FORCE_AGENTS_DIR_ERROR.with(|f| f.get()) {
        anyhow::bail!("Could not determine home directory");
    }
    Ok(std::env::temp_dir().join(".leviath-test-placeholder-agents-dir"))
}

/// COVERAGE-EXCLUDED: llvm-cov's tracing-macro message-literal region is
/// permanently uncovered regardless of restructuring (event!/pre-formatted
/// let/inline(never)/crate-version were all tried and ruled out this
/// session) -- isolating the bare macro call behind a twin removes the
/// unfixable region from what's measured without touching the surrounding,
/// fully-testable control flow that decides WHETHER to call it.
#[cfg(not(test))]
fn log_installing_agent_package() {
    tracing::info!("Installing agent package");
}

#[cfg(test)]
fn log_installing_agent_package() {}

/// Core `lev add` logic, parameterized by installer + agents base directory
/// so it can be tested against tempdirs instead of the real
/// `~/.leviath/agents`.
async fn execute_with(
    args: &AddArgs,
    installer: &leviath_package::AgentInstaller,
    agents_dir: &Path,
) -> anyhow::Result<()> {
    log_installing_agent_package();

    let package_path = Path::new(&args.package);

    if package_path.is_dir() {
        // Directory install: copy directory into <agents_dir>/<name>/
        install_from_dir(package_path, agents_dir)?;
    } else if package_path.exists() || args.package.ends_with(".leviath-bundle") {
        // Bundle file installation
        if !package_path.exists() {
            anyhow::bail!("Package file not found: {}", args.package);
        }
        println!("Installing from bundle: {}", args.package);
        let installed = installer.install(package_path)?;
        println!(
            "Installed agent '{}' v{} to {}",
            installed.name,
            installed.version,
            installed.path.display()
        );
    } else {
        // Registry installation
        let config = crate::config::Config::load()?;
        let registry_url = args
            .registry
            .clone()
            .or(config.registries.first().cloned())
            .unwrap_or("https://leviath.dev/registry".to_string());

        println!(
            "Searching registry {} for '{}'...",
            registry_url, args.package
        );

        let registry = leviath_package::PackageRegistry::new(registry_url);

        let info = registry.get_info(&args.package).await?;
        println!(
            "Found: {} v{} - {}",
            info.name, info.version, info.description
        );

        println!("Downloading...");
        let data = registry.download(&info.name, &info.version).await?;

        let installed = installer.install_from_bytes(&info.name, &data)?;
        println!(
            "Installed agent '{}' v{} to {}",
            installed.name,
            installed.version,
            installed.path.display()
        );
    }

    Ok(())
}

/// Copy a plain agent directory into `<agents_dir>/<name>/`.
///
/// The agent name is read from `agent.leviath` in the directory (falling back
/// to the directory's own name).
fn install_from_dir(src: &Path, agents_dir: &Path) -> anyhow::Result<()> {
    let manifest_path = src.join("agent.leviath");
    if !manifest_path.exists() {
        anyhow::bail!(
            "No agent.leviath found in '{}'. Is this an agent directory?",
            src.display()
        );
    }

    // Read the manifest to extract the agent name
    let content = std::fs::read_to_string(&manifest_path)?;
    let name = parse_agent_name(&content).unwrap_or_else(|| {
        src.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    let install_dir = agents_dir.join(&name);

    if install_dir.exists() {
        println!("Reinstalling agent '{}' (replacing existing)", name);
        std::fs::remove_dir_all(&install_dir)?;
    }

    copy_dir_recursive(src, &install_dir)?;
    println!("Installed agent '{}' to {}", name, install_dir.display());
    println!("Run with:  lev run {} --task \"...\"", name);
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting a test force the `Err` arm of a
    /// mid-iteration `ReadDir` entry deterministically (see
    /// [`unwrap_dir_entry`]) -- the real failure mode (the directory handle
    /// becoming invalid mid-iteration: deleted out from under the process,
    /// an NFS ESTALE, or similar) is a genuine OS-level race that can't be
    /// reproduced deterministically across Linux/macOS/Windows CI.
    static FORCE_DIR_ENTRY_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Unwrap one `ReadDir` iteration result, with a test-only failure-injection
/// toggle (see [`FORCE_DIR_ENTRY_ERROR`]) so the `Err` arm -- `ReadDir::next()`
/// failing after `read_dir` already succeeded in opening the directory --
/// can be exercised deterministically without needing to actually race the
/// filesystem.
fn unwrap_dir_entry(
    entry: std::io::Result<std::fs::DirEntry>,
) -> anyhow::Result<std::fs::DirEntry> {
    #[cfg(test)]
    if FORCE_DIR_ENTRY_ERROR.with(|f| f.get()) {
        anyhow::bail!("forced dir-entry error for testing");
    }
    Ok(entry?)
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = unwrap_dir_entry(entry)?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Parse the agent name from an `agent.leviath` manifest (first `name = "..."` line).
fn parse_agent_name(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;

    // ─── agents_dir_from_home ─────────────────────────────────────────────

    #[test]
    fn agents_dir_from_home_some_returns_path() {
        let home = std::path::PathBuf::from("/home/testuser");
        let dir = agents_dir_from_home(Some(home)).unwrap();
        assert_eq!(dir, std::path::Path::new("/home/testuser/.leviath/agents"));
    }

    #[test]
    fn agents_dir_from_home_none_returns_error() {
        let err = agents_dir_from_home(None).unwrap_err();
        assert!(err
            .to_string()
            .contains("Could not determine home directory"));
    }

    // ─── parse_agent_name ──────────────────────────────────────────────────

    #[test]
    fn parse_agent_name_standard() {
        let content = r#"
name = "my-agent"
version = "1.0"
"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_no_quotes() {
        let content = r#"name = my-agent"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_extra_whitespace() {
        let content = r#"  name   =   "spacy-agent"  "#;
        assert_eq!(parse_agent_name(content), Some("spacy-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_missing() {
        let content = r#"
version = "1.0"
description = "test"
"#;
        assert_eq!(parse_agent_name(content), None);
    }

    #[test]
    fn parse_agent_name_empty_value() {
        let content = r#"name = """#;
        assert_eq!(parse_agent_name(content), None);
    }

    // ─── copy_dir_recursive ────────────────────────────────────────────────

    #[test]
    fn copy_dir_recursive_copies_files() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        std::fs::write(src_dir.path().join("file1.txt"), "hello").unwrap();
        std::fs::create_dir_all(src_dir.path().join("sub")).unwrap();
        std::fs::write(src_dir.path().join("sub/file2.txt"), "world").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("file1.txt").exists());
        assert!(dst_path.join("sub/file2.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("file1.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst_path.join("sub/file2.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn copy_dir_recursive_empty_dir() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("empty-copy");

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();
        assert!(dst_path.exists());
        assert!(dst_path.is_dir());
    }

    #[test]
    fn copy_dir_recursive_nonexistent_src_errors() {
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("dst");
        let missing_src = dst_dir.path().join("does-not-exist");

        let result = copy_dir_recursive(&missing_src, &dst_path);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_dst_parent_is_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, "x").unwrap();
        let src = tempfile::tempdir().unwrap();
        let dst = file_path.join("child");

        let result = copy_dir_recursive(src.path(), &dst);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_unreadable_file_errors() {
        use std::os::unix::fs::PermissionsExt;

        let src_dir = tempfile::tempdir().unwrap();
        let file_path = src_dir.path().join("secret.txt");
        std::fs::write(&file_path, "top secret").unwrap();
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        let result = copy_dir_recursive(src_dir.path(), &dst_path);

        // Restore permissions so the tempdir can clean itself up on drop.
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_unreadable_subdir_errors() {
        use std::os::unix::fs::PermissionsExt;

        let src_dir = tempfile::tempdir().unwrap();
        let sub = src_dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("file.txt"), "data").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        // Exercises the recursive-call error-propagation branch: the
        // subdirectory itself is unreadable, so the nested
        // `copy_dir_recursive` call's own `read_dir` fails and that `Err`
        // bubbles up through the parent's `copy_dir_recursive(...)?`.
        let result = copy_dir_recursive(src_dir.path(), &dst_path);

        // Restore permissions so the tempdir can clean itself up on drop.
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_forced_mid_iteration_entry_error() {
        // Deterministically exercises `unwrap_dir_entry`'s `Err` arm (a real
        // `ReadDir::next()` failure mid-iteration) without racing the
        // filesystem, via the FORCE_DIR_ENTRY_ERROR test toggle.
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("file.txt"), "data").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        FORCE_DIR_ENTRY_ERROR.with(|f| f.set(true));
        let result = copy_dir_recursive(src_dir.path(), &dst_path);
        FORCE_DIR_ENTRY_ERROR.with(|f| f.set(false));

        assert!(result.is_err());
    }

    // ─── install_from_dir ──────────────────────────────────────────────────

    #[test]
    fn install_from_dir_no_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = tempfile::tempdir().unwrap();
        let result = install_from_dir(dir.path(), agents_dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    #[test]
    fn install_from_dir_copies_and_names_from_manifest() {
        let src = tempfile::tempdir().unwrap();
        let agents_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"my-agent\"\n",
        )
        .unwrap();
        std::fs::write(src.path().join("extra.txt"), "data").unwrap();

        install_from_dir(src.path(), agents_dir.path()).unwrap();

        let installed_dir = agents_dir.path().join("my-agent");
        assert!(installed_dir.join("agent.leviath").exists());
        assert!(installed_dir.join("extra.txt").exists());
    }

    #[test]
    fn install_from_dir_falls_back_to_dirname_when_name_missing() {
        let src = tempfile::tempdir().unwrap();
        let agent_dir = src.path().join("my-dir-name");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), "version = \"1.0\"\n").unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        install_from_dir(&agent_dir, agents_dir.path()).unwrap();

        assert!(agents_dir.path().join("my-dir-name").exists());
    }

    #[test]
    fn install_from_dir_reinstalls_existing() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"dup-agent\"\n",
        )
        .unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        // Pre-create an existing install with a stale file that should be wiped.
        let existing = agents_dir.path().join("dup-agent");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("stale.txt"), "old").unwrap();

        install_from_dir(src.path(), agents_dir.path()).unwrap();

        assert!(!existing.join("stale.txt").exists());
        assert!(existing.join("agent.leviath").exists());
    }

    #[test]
    fn install_from_dir_invalid_utf8_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.leviath"), [0xFF, 0xFE, 0xFA]).unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        let result = install_from_dir(dir.path(), agents_dir.path());
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_from_dir_remove_dir_all_permission_denied_errors() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"locked-agent\"\n",
        )
        .unwrap();

        let agents_dir = tempfile::tempdir().unwrap();
        let existing = agents_dir.path().join("locked-agent");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("stale.txt"), "old").unwrap();

        // Removing write permission on agents_dir prevents unlinking the
        // "locked-agent" entry inside it, forcing `remove_dir_all` to fail.
        std::fs::set_permissions(agents_dir.path(), std::fs::Permissions::from_mode(0o555))
            .unwrap();

        let result = install_from_dir(src.path(), agents_dir.path());

        // Restore permissions so the tempdir can clean itself up on drop.
        std::fs::set_permissions(agents_dir.path(), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_from_dir_copy_failure_propagates() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"broken-copy-agent\"\n",
        )
        .unwrap();
        let secret = src.path().join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();

        let agents_dir = tempfile::tempdir().unwrap();
        let result = install_from_dir(src.path(), agents_dir.path());

        // Restore permissions so the tempdir can clean itself up on drop.
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(result.is_err());
    }

    // ─── execute_with: directory + bundle-file paths ───────────────────────

    #[test]
    fn execute_with_directory_package_installs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let src = tempfile::tempdir().unwrap();
                std::fs::write(
                    src.path().join("agent.leviath"),
                    "[agent]\nname = \"dir-pkg\"\n",
                )
                .unwrap();
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: src.path().to_str().unwrap().to_string(),
                    registry: None,
                };

                execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap();

                assert!(agents_dir.path().join("dir-pkg").exists());
            })
        });
    }

    #[test]
    fn execute_with_directory_without_manifest_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let src = tempfile::tempdir().unwrap(); // no agent.leviath inside
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: src.path().to_str().unwrap().to_string(),
                    registry: None,
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("agent.leviath"));
            })
        });
    }

    #[test]
    fn execute_with_missing_bundle_file_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "nonexistent.leviath-bundle".to_string(),
                    registry: None,
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Package file not found"));
            })
        });
    }

    #[test]
    fn execute_with_bundle_file_installs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let project_dir = tempfile::tempdir().unwrap();
                std::fs::write(
                    project_dir.path().join("agent.leviath"),
                    "[agent]\nname = \"bundled-pkg\"\nversion = \"1.0.0\"\ndescription = \"d\"\n",
                )
                .unwrap();
                let bundle_bytes = leviath_package::AgentBundler::new()
                    .bundle(project_dir.path())
                    .unwrap();
                let bundle_dir = tempfile::tempdir().unwrap();
                // AgentInstaller::install() derives the agent name from the
                // bundle *filename* (not the manifest content), so name it
                // to match what we assert on below.
                let bundle_path = bundle_dir.path().join("bundled-pkg.leviath-bundle");
                std::fs::write(&bundle_path, bundle_bytes).unwrap();

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: bundle_path.to_str().unwrap().to_string(),
                    registry: None,
                };

                execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap();

                assert!(agents_dir.path().join("bundled-pkg").exists());
            })
        });
    }

    #[test]
    fn execute_with_corrupt_bundle_file_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let bundle_dir = tempfile::tempdir().unwrap();
                let bundle_path = bundle_dir.path().join("broken.leviath-bundle");
                std::fs::write(&bundle_path, b"not a valid gzip archive").unwrap();

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: bundle_path.to_str().unwrap().to_string(),
                    registry: None,
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Failed to extract package"));
            })
        });
    }

    // ─── execute_with: registry path (raw-TCP mock server) ─────────────────

    async fn spawn_mock_registry(
        get_info_body: &'static [u8],
        download_body: &'static [u8],
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            // First request: GET .../packages/<name> (get_info)
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                get_info_body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.write_all(get_info_body).await;
            let _ = socket.shutdown().await;

            // Second request: GET .../download (download)
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                download_body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.write_all(download_body).await;
            let _ = socket.shutdown().await;
        });

        format!("http://{}", addr)
    }

    #[test]
    fn execute_with_registry_package_installs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                // Isolates `Config::load()` (called unconditionally at the
                // top of the registry branch) from a real `~/.leviath/config.toml`
                // *and* from other tests elsewhere in the crate that use this
                // same `LEVIATH_CONFIG_PATH` seam to point it at a
                // deliberately-malformed file -- without this, this test can
                // flakily observe that other test's mid-flight override.
                let _guard = crate::config::isolate_config_path_for_test("add-registry-installs");
                let project_dir = tempfile::tempdir().unwrap();
                std::fs::write(
                    project_dir.path().join("agent.leviath"),
                    "[agent]\nname = \"reg-pkg\"\nversion = \"1.0.0\"\ndescription = \"d\"\n",
                )
                .unwrap();
                let bundle_bytes = leviath_package::AgentBundler::new()
                    .bundle(project_dir.path())
                    .unwrap();

                let info_json =
                    br#"{"name":"reg-pkg","version":"1.0.0","description":"A registry package"}"#;
                let url =
                    spawn_mock_registry(info_json, Box::leak(bundle_bytes.into_boxed_slice()))
                        .await;

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "reg-pkg".to_string(),
                    registry: Some(url),
                };

                execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap();

                assert!(agents_dir.path().join("reg-pkg").exists());
            })
        });
    }

    async fn spawn_mock_registry_download_error(get_info_body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            // First request: get_info -- succeeds.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                get_info_body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.write_all(get_info_body).await;
            let _ = socket.shutdown().await;

            // Second request: download -- fails with a non-success status.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let resp =
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        format!("http://{}", addr)
    }

    #[test]
    fn execute_with_registry_config_load_error_propagates() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let guard =
                    crate::config::isolate_config_path_for_test("add-registry-config-error");
                std::fs::write(guard.fake_dir.join("config.toml"), "not valid toml [[[").unwrap();

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "some-registry-package".to_string(),
                    registry: None,
                };

                let result = execute_with(&args, &installer, agents_dir.path()).await;
                assert!(result.is_err());
            })
        });
    }

    #[test]
    fn execute_with_registry_get_info_connection_refused_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                // See `execute_with_registry_package_installs` for why this
                // guard is needed even though this test never writes a
                // config file of its own.
                let _guard =
                    crate::config::isolate_config_path_for_test("add-registry-get-info-refused");
                // Fixed, never-bound high port (same pattern used in
                // leviath-package's own connection-refused tests) --
                // deterministic, unlike bind-then-drop which races against
                // other parallel tests' ephemeral-port allocations.
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "some-registry-package".to_string(),
                    registry: Some("http://127.0.0.1:19999".to_string()),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Failed to get package info"));
            })
        });
    }

    #[test]
    fn execute_with_registry_download_failure_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                // See `execute_with_registry_package_installs` for why this
                // guard is needed even though this test never writes a
                // config file of its own.
                let _guard =
                    crate::config::isolate_config_path_for_test("add-registry-download-failure");
                let info_json =
                    br#"{"name":"reg-pkg-dl-fail","version":"1.0.0","description":"d"}"#;
                let url = spawn_mock_registry_download_error(info_json).await;

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "reg-pkg-dl-fail".to_string(),
                    registry: Some(url),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Package download failed"));
            })
        });
    }

    #[test]
    fn execute_with_registry_install_from_bytes_invalid_data_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                // See `execute_with_registry_package_installs` for why this
                // guard is needed even though this test never writes a
                // config file of its own.
                let _guard =
                    crate::config::isolate_config_path_for_test("add-registry-invalid-data");
                let info_json =
                    br#"{"name":"reg-pkg-bad-data","version":"1.0.0","description":"d"}"#;
                let url = spawn_mock_registry(info_json, b"not a valid gzip archive").await;

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "reg-pkg-bad-data".to_string(),
                    registry: Some(url),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Failed to extract package"));
            })
        });
    }

    // ─── path detection ────────────────────────────────────────────────────

    #[test]
    fn bundle_extension_detected() {
        let package = "my-agent-1.0.leviath-bundle";
        assert!(package.ends_with(".leviath-bundle"));
    }

    #[test]
    fn directory_path_detected() {
        let dir = tempfile::tempdir().unwrap();
        let package_path = Path::new(dir.path().to_str().unwrap());
        assert!(package_path.is_dir());
    }

    #[test]
    fn registry_name_not_dir_not_bundle() {
        let package = "my-cool-agent";
        let package_path = Path::new(package);
        assert!(!package_path.is_dir());
        assert!(!package.ends_with(".leviath-bundle"));
    }

    // ─── parse_agent_name additional ──────────────────────────────────────

    #[test]
    fn parse_agent_name_in_section() {
        let content = r#"
[agent]
name = "my-agent"
version = "1.0"
"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_with_single_quotes() {
        // toml uses double quotes, but our parser uses trim_matches('"')
        let content = r#"name = my-agent-no-quotes"#;
        assert_eq!(
            parse_agent_name(content),
            Some("my-agent-no-quotes".to_string())
        );
    }

    #[test]
    fn parse_agent_name_multiple_name_fields_returns_first() {
        let content = r#"
name = "first"
name = "second"
"#;
        assert_eq!(parse_agent_name(content), Some("first".to_string()));
    }

    // ─── copy_dir_recursive with nested dirs ──────────────────────────────

    #[test]
    fn copy_dir_recursive_deeply_nested() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("deep-copy");

        std::fs::create_dir_all(src_dir.path().join("a/b/c")).unwrap();
        std::fs::write(src_dir.path().join("a/b/c/deep.txt"), "deep").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("a/b/c/deep.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("a/b/c/deep.txt")).unwrap(),
            "deep"
        );
    }

    // ─── execute(): real entry point wrapper ───────────────────────────────

    #[test]
    fn execute_real_wrapper_fails_fast_without_touching_real_agents_dir() {
        // Drives the real `execute()` (dirs::home_dir() + AgentInstaller::new()
        // + delegation to execute_with) -- safe because a nonexistent
        // ".leviath-bundle" path bails out in execute_with's "Package file
        // not found" check before any real file under ~/.leviath/agents is
        // ever touched.
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let args = AddArgs {
                    package: "definitely-not-a-real-bundle-xyz.leviath-bundle".to_string(),
                    registry: None,
                };
                let err = execute(args).await.unwrap_err();
                assert!(err.to_string().contains("Package file not found"));
            })
        });
    }

    #[test]
    fn execute_returns_err_when_agents_dir_unresolvable() {
        // Drives `execute`'s `resolve_agents_dir()?` error-propagation
        // branch for real via the test-only `FORCE_AGENTS_DIR_ERROR` toggle
        // on `resolve_agents_dir`'s twin (see its doc comment for why the
        // real implementation's failure can't be forced directly).
        let rt = tokio::runtime::Runtime::new().unwrap();
        FORCE_AGENTS_DIR_ERROR.with(|f| f.set(true));
        let result = rt.block_on(async {
            let args = AddArgs {
                package: "whatever.leviath-bundle".to_string(),
                registry: None,
            };
            execute(args).await
        });
        FORCE_AGENTS_DIR_ERROR.with(|f| f.set(false));

        let err = result.unwrap_err();
        assert!(err
            .to_string()
            .contains("Could not determine home directory"));
    }

    // ─── install_from_dir with valid manifest ─────────────────────────────

    #[test]
    fn install_from_dir_with_manifest_runs() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "test-install-agent-xyz"
version = "0.1.0"
description = "test"
"#;
        std::fs::write(dir.path().join("agent.leviath"), manifest).unwrap();
        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();

        let agents_dir = tempfile::tempdir().unwrap();
        install_from_dir(dir.path(), agents_dir.path()).unwrap();

        let install_dir = agents_dir.path().join("test-install-agent-xyz");
        assert!(install_dir.join("agent.leviath").exists());
        assert!(install_dir.join("readme.txt").exists());
    }
}
