//! `lev run` - Run an agent

use clap::Args;

#[derive(Args)]
pub struct RunArgs {
    /// Path to agent project or leviath.toml
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt
    #[arg(short, long)]
    pub task: String,

    /// Model override
    #[arg(short, long)]
    pub model: Option<String>,
}

pub async fn execute(args: RunArgs) -> anyhow::Result<()> {
    let path = args.path.unwrap_or_else(|| ".".to_string());
    tracing::info!(path = %path, task = %args.task, "Running agent");
    
    // TODO: Implement agent execution
    println!("Running agent from: {}", path);
    println!("Task: {}", args.task);
    
    Ok(())
}
