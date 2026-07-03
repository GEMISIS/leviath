//! `lev pack` - Bundle an agent project for distribution.

use clap::Args;
use leviath_package::AgentBundler;
use std::path::{Path, PathBuf};

use super::run::parse_manifest_public;

#[derive(Args)]
pub struct PackArgs {
    /// Path to agent project (default: current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Output file path (default: {name}-{version}.leviath-bundle)
    #[arg(short, long)]
    pub output: Option<String>,
}

pub async fn execute(args: PackArgs) -> anyhow::Result<()> {
    let path = args.path.unwrap_or_else(|| ".".to_string());
    let project_path = Path::new(&path);

    tracing::info!("Packing agent");

    // Find and parse agent.leviath to get name + version
    let manifest_path = find_manifest(project_path)?;
    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest_public(&manifest_content)?;

    println!("Packing agent: {} v{}", blueprint.name, blueprint.version);

    // Determine output path
    let output_path =
        determine_output_path(args.output.as_deref(), &blueprint.name, &blueprint.version);

    // Bundle the project
    let bundler = AgentBundler::new();
    let project_dir = manifest_path.parent().unwrap_or(Path::new("."));

    let data = bundler.bundle(project_dir)?;
    let bundle_size = data.len();

    std::fs::write(&output_path, &data).map_err(|e| {
        anyhow::anyhow!(
            "Failed to write bundle to '{}': {}",
            output_path.display(),
            e
        )
    })?;

    // Print summary
    println!("Bundle written to: {}", output_path.display());
    println!("Bundle size: {}", format_size(bundle_size));

    // List contents summary
    println!("\nContents:");
    let file_count = count_files(project_dir);
    println!("  {} files bundled", file_count);
    println!("  Manifest: agent.leviath");

    let scripts_dir = project_dir.join("scripts");
    if scripts_dir.exists() {
        let script_count = count_files(&scripts_dir);
        println!("  Scripts: {} files", script_count);
    }

    let tests_dir = project_dir.join("tests");
    if tests_dir.exists() {
        let test_count = count_files(&tests_dir);
        println!("  Tests: {} files", test_count);
    }

    println!("\nDone! Install with: lev add {}", output_path.display());

    Ok(())
}

/// Resolves the bundle output path.
fn determine_output_path(output: Option<&str>, name: &str, version: &str) -> PathBuf {
    match output {
        Some(out) => PathBuf::from(out),
        None => PathBuf::from(format!("{}-{}.leviath-bundle", name, version)),
    }
}

fn find_manifest(project_path: &Path) -> anyhow::Result<PathBuf> {
    find_manifest_with_cwd(project_path, &std::env::current_dir().unwrap_or_default())
}

fn find_manifest_with_cwd(project_path: &Path, cwd: &Path) -> anyhow::Result<PathBuf> {
    if project_path.is_file()
        && project_path.file_name() == Some(std::ffi::OsStr::new("agent.leviath"))
    {
        return Ok(project_path.to_path_buf());
    }

    if project_path.is_dir() {
        let manifest = project_path.join("agent.leviath");
        if manifest.exists() {
            return Ok(manifest);
        }
    }

    let current_manifest = cwd.join("agent.leviath");
    if current_manifest.exists() {
        return Ok(current_manifest);
    }

    anyhow::bail!(
        "Could not find agent.leviath in {} or current directory",
        project_path.display()
    )
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Count files in `dir` recursively; returns 0 on I/O errors.
fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    if dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_file() {
                    count += 1;
                } else if path.is_dir() {
                    count += count_files(&path);
                }
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;

    // ─── tracing subscriber ────────────────────────────────────────────────
    //
    // Without a registered subscriber, `tracing::info!`'s macro expansion
    // short-circuits field evaluation before the "is level enabled" check
    // runs, so field-expression lines show as uncovered even though the
    // surrounding branch executes. Each test below calls `with_tracing(|| {})`
    // once as a bare statement (rather than wrapping the whole test body) to
    // install the shared `AlwaysOnSubscriber` (see `crate::test_support`) as
    // the process-wide default before the rest of the test runs.

    #[test]
    fn diagnose_tracing_setup() {
        with_tracing(|| {});
        tracing::callsite::rebuild_interest_cache();
        let lvl = tracing::level_filters::LevelFilter::current();
        println!("LevelFilter::current() = {:?}", lvl);
        let set = tracing::dispatcher::has_been_set();
        println!("dispatcher has_been_set = {}", set);
        tracing::info!("test event");
        println!("info! executed successfully");
    }

    // ─── format_size ───────────────────────────────────────────────────────

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    // ─── count_files ───────────────────────────────────────────────────────

    #[test]
    fn count_files_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(count_files(dir.path()), 0);
    }

    #[test]
    fn count_files_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();
        assert_eq!(count_files(dir.path()), 2);
    }

    #[test]
    fn count_files_nested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("top.txt"), "t").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/nested.txt"), "n").unwrap();
        assert_eq!(count_files(dir.path()), 2);
    }

    #[test]
    fn count_files_non_directory_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        assert_eq!(count_files(&file), 0);
    }

    #[cfg(unix)]
    #[test]
    fn count_files_unreadable_dir_returns_zero() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = count_files(dir.path());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result, 0);
    }

    #[cfg(unix)]
    #[test]
    fn count_files_skips_non_file_non_dir_entries() {
        // A broken symlink is neither is_file() nor is_dir(), so count_files
        // skips it.  This covers the implicit "else" branch of the
        // if/else-if in count_files.
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        // Create a dangling symlink: points to a target that does not exist.
        let link = dir.path().join("dangling.link");
        symlink("/tmp/this_target_does_not_exist_leviath_test", &link).unwrap();
        std::fs::write(dir.path().join("real.txt"), "x").unwrap();
        // The symlink is neither a file nor a dir, so only real.txt is counted.
        assert_eq!(count_files(dir.path()), 1);
    }

    // ─── find_manifest ─────────────────────────────────────────────────────

    #[test]
    fn find_manifest_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, "name = \"test\"").unwrap();
        let result = find_manifest(dir.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), manifest);
    }

    #[test]
    fn find_manifest_direct_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, "name = \"test\"").unwrap();
        let result = find_manifest(&manifest);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), manifest);
    }

    #[test]
    fn find_manifest_not_found_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_manifest(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    // ─── find_manifest_with_cwd ────────────────────────────────────────────

    #[test]
    fn find_manifest_with_cwd_finds_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, "name = \"test\"").unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let result = find_manifest_with_cwd(dir.path(), cwd.path());
        assert_eq!(result.unwrap(), manifest);
    }

    #[test]
    fn find_manifest_with_cwd_finds_direct_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.leviath");
        std::fs::write(&manifest, "name = \"test\"").unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let result = find_manifest_with_cwd(&manifest, cwd.path());
        assert_eq!(result.unwrap(), manifest);
    }

    #[test]
    fn find_manifest_with_cwd_falls_back_to_cwd() {
        let empty_dir = tempfile::tempdir().unwrap();
        let cwd_dir = tempfile::tempdir().unwrap();
        let cwd_manifest = cwd_dir.path().join("agent.leviath");
        std::fs::write(&cwd_manifest, "name = \"test\"").unwrap();
        let result = find_manifest_with_cwd(empty_dir.path(), cwd_dir.path());
        assert_eq!(result.unwrap(), cwd_manifest);
    }

    #[test]
    fn find_manifest_with_cwd_errors_when_not_found() {
        let empty_project = tempfile::tempdir().unwrap();
        let empty_cwd = tempfile::tempdir().unwrap();
        let result = find_manifest_with_cwd(empty_project.path(), empty_cwd.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    // ─── output path determination ─────────────────────────────────────────

    #[test]
    fn output_path_from_args() {
        let output_path =
            determine_output_path(Some("my-output.leviath-bundle"), "my-agent", "1.0.0");
        assert_eq!(output_path, PathBuf::from("my-output.leviath-bundle"));
    }

    #[test]
    fn output_path_default() {
        let output_path = determine_output_path(None, "my-agent", "1.0.0");
        assert_eq!(output_path, PathBuf::from("my-agent-1.0.0.leviath-bundle"));
    }

    // ─── format_size edge cases ───────────────────────────────────────────

    #[test]
    fn format_size_boundary_kb() {
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
    }

    #[test]
    fn format_size_boundary_mb() {
        assert_eq!(format_size(1024 * 1024 - 1), "1024.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn format_size_fractional_kb() {
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(2560), "2.5 KB");
    }

    // ─── count_files edge cases ───────────────────────────────────────────

    #[test]
    fn count_files_nested_multiple_levels() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        std::fs::write(dir.path().join("root.txt"), "r").unwrap();
        std::fs::write(dir.path().join("a/level1.txt"), "1").unwrap();
        std::fs::write(dir.path().join("a/b/level2.txt"), "2").unwrap();
        std::fs::write(dir.path().join("a/b/c/level3.txt"), "3").unwrap();
        assert_eq!(count_files(dir.path()), 4);
    }

    // ─── find_manifest edge cases ─────────────────────────────────────────

    #[test]
    fn find_manifest_nonexistent_path_errors() {
        let result = find_manifest(Path::new("/tmp/nonexistent-leviath-test-dir"));
        assert!(result.is_err());
    }

    // ─── output path generation ───────────────────────────────────────────

    #[test]
    fn output_path_with_special_chars() {
        let output_path = determine_output_path(None, "my-agent", "1.0.0-beta.1");
        assert_eq!(
            output_path,
            PathBuf::from("my-agent-1.0.0-beta.1.leviath-bundle")
        );
    }

    // ─── execute ─────────────────────────────────────────────────────────

    fn make_project_dir(with_scripts: bool, with_tests: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.leviath"),
            "[agent]\nname = \"packed-agent\"\nversion = \"1.0.0\"\ndescription = \"d\"\n",
        )
        .unwrap();
        if with_scripts {
            std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
            std::fs::write(dir.path().join("scripts/run.sh"), "#!/bin/sh\n").unwrap();
        }
        if with_tests {
            std::fs::create_dir_all(dir.path().join("tests")).unwrap();
            std::fs::write(dir.path().join("tests/test1.txt"), "test").unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn execute_packs_project_to_explicit_output() {
        with_tracing(|| {});
        let project = make_project_dir(false, false);
        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("out.leviath-bundle");
        let args = PackArgs {
            path: Some(project.path().to_str().unwrap().to_string()),
            output: Some(output_path.to_str().unwrap().to_string()),
        };
        execute(args).await.unwrap();
        assert!(output_path.exists());
        assert!(std::fs::metadata(&output_path).unwrap().len() > 0);
    }

    #[tokio::test]
    async fn execute_with_scripts_and_tests_dirs() {
        with_tracing(|| {});
        let project = make_project_dir(true, true);
        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("out.leviath-bundle");
        let args = PackArgs {
            path: Some(project.path().to_str().unwrap().to_string()),
            output: Some(output_path.to_str().unwrap().to_string()),
        };
        execute(args).await.unwrap();
        assert!(output_path.exists());
    }

    #[tokio::test]
    async fn execute_missing_manifest_errors() {
        with_tracing(|| {});
        let project = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("out.leviath-bundle");
        let args = PackArgs {
            path: Some(project.path().to_str().unwrap().to_string()),
            output: Some(output_path.to_str().unwrap().to_string()),
        };
        let err = execute(args).await.unwrap_err();
        assert!(err.to_string().contains("Could not find agent.leviath"));
    }

    #[tokio::test]
    async fn execute_unwritable_output_path_errors() {
        with_tracing(|| {});
        let project = make_project_dir(false, false);
        let output_path = project
            .path()
            .join("nonexistent-subdir")
            .join("out.leviath-bundle");
        let args = PackArgs {
            path: Some(project.path().to_str().unwrap().to_string()),
            output: Some(output_path.to_str().unwrap().to_string()),
        };
        let err = execute(args).await.unwrap_err();
        assert!(err.to_string().contains("Failed to write bundle"));
    }

    #[tokio::test]
    async fn execute_with_path_none_falls_back_to_dot() {
        // args.path = None triggers the unwrap_or_else closure on line 21.
        with_tracing(|| {});
        let args = PackArgs {
            path: None,
            output: None,
        };
        let err = execute(args).await.unwrap_err();
        assert!(err.to_string().contains("agent.leviath"));
    }

    #[tokio::test]
    async fn execute_invalid_manifest_toml_errors() {
        // Manifest exists but is invalid TOML — covers parse_manifest_public ? on line 30.
        with_tracing(|| {});
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("agent.leviath"), "not valid toml ][").unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("out.leviath-bundle");
        let args = PackArgs {
            path: Some(project.path().to_str().unwrap().to_string()),
            output: Some(output_path.to_str().unwrap().to_string()),
        };
        execute(args).await.unwrap_err();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_unreadable_manifest_errors() {
        // Manifest exists but is chmod 000 — covers map_err on lines 28-29.
        // This test assumes it is not running as root (chmod 000 blocks reads).
        use std::os::unix::fs::PermissionsExt;
        with_tracing(|| {});
        let project = tempfile::tempdir().unwrap();
        let manifest = project.path().join("agent.leviath");
        std::fs::write(
            &manifest,
            "[agent]\nname = \"x\"\nversion = \"0.1.0\"\ndescription = \"d\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o000)).unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("out.leviath-bundle");
        let args = PackArgs {
            path: Some(project.path().to_str().unwrap().to_string()),
            output: Some(output_path.to_str().unwrap().to_string()),
        };
        let result = execute(args).await;
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = result.unwrap_err();
        assert!(e.to_string().contains("Failed to read manifest"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_bundle_error_propagated() {
        // An unreadable file in the project causes bundler.bundle to fail —
        // covers the ? on bundler.bundle(project_dir)? on line 42.
        // This test assumes it is not running as root (chmod 000 blocks reads).
        use std::os::unix::fs::PermissionsExt;
        with_tracing(|| {});
        let project = make_project_dir(false, false);
        let secret = project.path().join("secret.txt");
        std::fs::write(&secret, "secret data").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("out.leviath-bundle");
        let args = PackArgs {
            path: Some(project.path().to_str().unwrap().to_string()),
            output: Some(output_path.to_str().unwrap().to_string()),
        };
        let result = execute(args).await;
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Non-root: bundler cannot open the file → propagated as an error.
        let e = result.unwrap_err();
        assert!(e.to_string().contains("Failed to add"));
    }
}
