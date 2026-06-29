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
