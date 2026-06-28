//! `lev uninstall` - Remove an installed agent

use clap::Args;

#[derive(Args)]
pub struct UninstallArgs {
    /// Name of the installed agent to remove
    #[arg(value_name = "NAME")]
    pub name: String,
}

pub async fn execute(args: UninstallArgs) -> anyhow::Result<()> {
    let installer = leviath_package::AgentInstaller::new();

    // Verify it's actually installed first
    let installed = installer.get_installed(&args.name)?;
    if installed.is_none() {
        anyhow::bail!(
            "Agent '{}' is not installed. Use `lev list` to see installed agents.",
            args.name
        );
    }

    installer.uninstall(&args.name)?;
    println!("Uninstalled agent '{}'.", args.name);
    Ok(())
}
