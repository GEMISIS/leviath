//! `lev install` - Install an agent package

use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct InstallArgs {
    /// Package name or path to .leviath-bundle file
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

    // Check if this is a local file path (ends with .leviath-bundle or file exists)
    if package_path.exists()
        || args
            .package
            .ends_with(".leviath-bundle")
    {
        // Local file installation
        if !package_path.exists() {
            anyhow::bail!("Package file not found: {}", args.package);
        }

        println!("Installing from local file: {}", args.package);
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

        // Get package info
        let info = registry.get_info(&args.package).await?;
        println!(
            "Found: {} v{} - {}",
            info.name, info.version, info.description
        );

        // Download
        println!("Downloading...");
        let data = registry.download(&info.name, &info.version).await?;

        // Install from bytes
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
