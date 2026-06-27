//! `lev run` - Run an agent

use clap::Args;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct RunArgs {
    /// Path to agent project or agent.leviath
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
    
    // Find agent.leviath manifest
    let manifest_path = find_manifest(&path)?;
    println!("📄 Loading agent from: {}", manifest_path.display());
    
    // For now, we'll just demonstrate the pipeline structure
    // In a full implementation, this would:
    // 1. Parse agent.leviath into a Blueprint
    // 2. Create a ContextWindow from the blueprint's layout
    // 3. Initialize the provider based on the model
    // 4. Execute stages in sequence
    // 5. Handle tool calls and eviction
    
    println!("📋 Task: {}", args.task);
    println!();
    println!("🔧 Pipeline demonstration:");
    println!("  ✓ Manifest loaded");
    println!("  ✓ Context window initialized");
    println!("  ✓ Provider configured");
    println!();
    println!("💭 Mock execution:");
    println!("  [Stage: main]");
    println!("  Running inference with task...");
    println!();
    println!("✅ Agent execution complete!");
    println!();
    println!("Note: This is a mock execution demonstrating the pipeline.");
    println!("To enable real LLM calls, configure API keys in ~/.leviath/config.toml");
    
    Ok(())
}

fn find_manifest(path: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(path);
    
    // If path is a file named agent.leviath, use it directly
    if path.is_file() && path.file_name() == Some(std::ffi::OsStr::new("agent.leviath")) {
        return Ok(path.to_path_buf());
    }
    
    // If path is a directory, look for agent.leviath inside it
    if path.is_dir() {
        let manifest = path.join("agent.leviath");
        if manifest.exists() {
            return Ok(manifest);
        }
    }
    
    // Fall back to current directory
    let current_manifest = PathBuf::from("agent.leviath");
    if current_manifest.exists() {
        return Ok(current_manifest);
    }
    
    anyhow::bail!("Could not find agent.leviath in {} or current directory", path.display())
}
