//! `lev list` - List available agents and blueprints

use clap::Args;
use std::fs;
use std::path::PathBuf;

#[derive(Args)]
pub struct ListArgs {
    /// Filter by type (agents, blueprints, all)
    #[arg(short, long, default_value = "all")]
    pub filter: String,
}

pub async fn execute(args: ListArgs) -> anyhow::Result<()> {
    tracing::info!(filter = %args.filter, "Listing resources");
    
    let agents_dir = get_agents_dir()?;
    
    if !agents_dir.exists() {
        println!("No agents installed yet.");
        println!("\nTo create a new agent:");
        println!("  lev init my-agent");
        return Ok(());
    }
    
    println!("Installed agents:\n");
    
    let mut found_any = false;
    for entry in fs::read_dir(&agents_dir)? {
        let entry = entry?;
        let path = entry.path();
        
        if path.is_dir() {
            let manifest_path = path.join("agent.leviath");
            if manifest_path.exists() {
                found_any = true;
                let name = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                println!("  {} ({})", name, path.display());
            }
        }
    }
    
    if !found_any {
        println!("  (none)");
        println!("\nTo create a new agent:");
        println!("  lev init my-agent");
    }
    
    Ok(())
}

fn get_agents_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(".leviath").join("agents"))
}
