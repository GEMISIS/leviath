//! `lev install` - Install an agent package

use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct InstallArgs {
    /// Path to agent directory, .leviath-bundle file, or registry package name
    #[arg(value_name = "PACKAGE")]
    pub package: String,

    /// Install from registry (URL override)
    #[arg(short, long)]
    pub registry: Option<String>,
}

pub async fn execute(args: InstallArgs) -> anyhow::Result<()> {
    tracing::info!(package = %args.package, "Installing agent package");

    let installer = leviath_package::AgentInstaller::new();
    let package_path = Path::new(&args.package);

    if package_path.is_dir() {
        // Directory install: copy directory into ~/.leviath/agents/<name>/
        install_from_dir(package_path)?;
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
            .or_else(|| config.registries.first().cloned())
            .unwrap_or_else(|| "https://leviath.dev/registry".to_string());

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

/// Copy a plain agent directory into `~/.leviath/agents/<name>/`.
///
/// The agent name is read from `agent.leviath` in the directory (falling back
/// to the directory's own name).
fn install_from_dir(src: &Path) -> anyhow::Result<()> {
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

    let install_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?
        .join(".leviath")
        .join("agents")
        .join(&name);

    if install_dir.exists() {
        println!("Reinstalling agent '{}' (replacing existing)", name);
        std::fs::remove_dir_all(&install_dir)?;
    }

    copy_dir_recursive(src, &install_dir)?;
    println!("Installed agent '{}' to {}", name, install_dir.display());
    println!("Run with:  lev run {} --task \"...\"", name);
    Ok(())
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
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
