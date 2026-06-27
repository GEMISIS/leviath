//! `lev init` - Create a new agent project

use clap::Args;

#[derive(Args)]
pub struct InitArgs {
    /// Project name
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Project template (default, coding, research)
    #[arg(short, long, default_value = "default")]
    pub template: String,
}

pub async fn execute(args: InitArgs) -> anyhow::Result<()> {
    tracing::info!(name = %args.name, template = %args.template, "Initializing agent project");
    
    // TODO: Implement project scaffolding
    println!("Creating agent project: {}", args.name);
    println!("Template: {}", args.template);
    
    Ok(())
}
