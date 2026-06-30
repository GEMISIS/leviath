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

    tracing::info!(path = %project_path.display(), "Packing agent");

    // Find and parse agent.leviath to get name + version
    let manifest_path = find_manifest(project_path)?;
    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest_public(&manifest_content)?;

    println!("Packing agent: {} v{}", blueprint.name, blueprint.version);

    // Determine output path
    let output_path = if let Some(ref out) = args.output {
        PathBuf::from(out)
    } else {
        PathBuf::from(format!(
            "{}-{}.leviath-bundle",
            blueprint.name, blueprint.version
        ))
    };

    // Bundle the project
    let bundler = AgentBundler::new();
    let project_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));

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
    let file_count = count_files(project_dir)?;
    println!("  {} files bundled", file_count);
    println!("  Manifest: agent.leviath");

    let scripts_dir = project_dir.join("scripts");
    if scripts_dir.exists() {
        let script_count = count_files(&scripts_dir)?;
        println!("  Scripts: {} files", script_count);
    }

    let tests_dir = project_dir.join("tests");
    if tests_dir.exists() {
        let test_count = count_files(&tests_dir)?;
        println!("  Tests: {} files", test_count);
    }

    println!("\nDone! Install with: lev add {}", output_path.display());

    Ok(())
}

fn find_manifest(project_path: &Path) -> anyhow::Result<PathBuf> {
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

    let current_manifest = PathBuf::from("agent.leviath");
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

fn count_files(dir: &Path) -> anyhow::Result<usize> {
    let mut count = 0;
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                count += 1;
            } else if path.is_dir() {
                count += count_files(&path)?;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(count_files(dir.path()).unwrap(), 0);
    }

    #[test]
    fn count_files_with_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();
        assert_eq!(count_files(dir.path()).unwrap(), 2);
    }

    #[test]
    fn count_files_nested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("top.txt"), "t").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/nested.txt"), "n").unwrap();
        assert_eq!(count_files(dir.path()).unwrap(), 2);
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
        // No agent.leviath in the dir
        let result = find_manifest(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    // ─── output path determination ─────────────────────────────────────────

    #[test]
    fn output_path_from_args() {
        let output = Some("my-output.leviath-bundle".to_string());
        let output_path = if let Some(ref out) = output {
            PathBuf::from(out)
        } else {
            PathBuf::from("default.leviath-bundle")
        };
        assert_eq!(output_path, PathBuf::from("my-output.leviath-bundle"));
    }

    #[test]
    fn output_path_default() {
        let name = "my-agent";
        let version = "1.0.0";
        let output_path = PathBuf::from(format!("{}-{}.leviath-bundle", name, version));
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
        // 1536 = 1.5 KB
        assert_eq!(format_size(1536), "1.5 KB");
        // 2560 = 2.5 KB
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
        assert_eq!(count_files(dir.path()).unwrap(), 4);
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
        let name = "my-agent";
        let version = "1.0.0-beta.1";
        let output_path = PathBuf::from(format!("{}-{}.leviath-bundle", name, version));
        assert_eq!(
            output_path,
            PathBuf::from("my-agent-1.0.0-beta.1.leviath-bundle")
        );
    }
}
