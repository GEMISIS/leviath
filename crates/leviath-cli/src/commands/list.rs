//! `lev list` - List available agents and blueprints

use clap::Args;

#[derive(Args)]
pub struct ListArgs {
    /// Filter by type (agents, blueprints, all)
    #[arg(short, long, default_value = "all")]
    pub filter: String,
}

pub async fn execute(args: ListArgs) -> anyhow::Result<()> {
    tracing::info!(filter = %args.filter, "Listing resources");
    
    // TODO: Implement listing
    println!("Listing: {}", args.filter);
    
    Ok(())
}
