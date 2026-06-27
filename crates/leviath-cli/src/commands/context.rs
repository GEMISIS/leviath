//! `lev context` - Inspect and debug context windows

use clap::Args;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Args)]
pub struct ContextArgs {
    /// Agent ID to inspect
    #[arg(value_name = "AGENT_ID")]
    pub agent_id: Option<String>,

    /// Show detailed region information
    #[arg(short, long)]
    pub detailed: bool,
}

/// Persisted context window state for inspection.
#[derive(Debug, Serialize, Deserialize)]
struct SavedContextWindow {
    agent_id: String,
    max_tokens: usize,
    current_tokens: usize,
    regions: Vec<SavedRegion>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedRegion {
    name: String,
    kind: String,
    max_tokens: usize,
    current_tokens: usize,
    entry_count: usize,
    entries: Vec<SavedEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedEntry {
    content: String,
    tokens: usize,
    timestamp: i64,
}

pub async fn execute(args: ContextArgs) -> anyhow::Result<()> {
    tracing::info!("Inspecting context windows");

    let state_dir = get_state_dir()?;

    if let Some(agent_id) = args.agent_id {
        // Inspect a specific agent's context
        let state_file = state_dir.join(format!("{}.json", agent_id));

        if !state_file.exists() {
            println!("No saved state found for agent: {}", agent_id);
            println!(
                "State files are stored in: {}",
                state_dir.display()
            );
            return Ok(());
        }

        let content = fs::read_to_string(&state_file)?;
        let saved: SavedContextWindow = serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse state file: {}", e))?;

        println!("Agent: {}", saved.agent_id);
        println!(
            "Token usage: {}/{} ({:.1}%)",
            saved.current_tokens,
            saved.max_tokens,
            (saved.current_tokens as f64 / saved.max_tokens as f64) * 100.0
        );
        println!("Regions: {}\n", saved.regions.len());

        for region in &saved.regions {
            println!(
                "  {} [{}]: {}/{} tokens, {} entries",
                region.name,
                region.kind,
                region.current_tokens,
                region.max_tokens,
                region.entry_count
            );

            if args.detailed {
                for (i, entry) in region.entries.iter().enumerate() {
                    let preview: String = entry
                        .content
                        .chars()
                        .take(100)
                        .collect();
                    let suffix = if entry.content.len() > 100 {
                        "..."
                    } else {
                        ""
                    };
                    println!(
                        "    [{}] ({} tokens, ts={}) {}{}",
                        i, entry.tokens, entry.timestamp, preview, suffix
                    );
                }
            }
        }
    } else {
        // List all saved agent states
        if !state_dir.exists() {
            println!("No agent states found.");
            println!(
                "State directory: {}",
                state_dir.display()
            );
            return Ok(());
        }

        let mut found_any = false;
        for entry in fs::read_dir(&state_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(saved) = serde_json::from_str::<SavedContextWindow>(&content) {
                        found_any = true;
                        println!(
                            "  {} - {}/{} tokens ({:.1}%), {} regions",
                            saved.agent_id,
                            saved.current_tokens,
                            saved.max_tokens,
                            (saved.current_tokens as f64 / saved.max_tokens as f64) * 100.0,
                            saved.regions.len()
                        );
                    }
                }
            }
        }

        if !found_any {
            println!("No agent states found.");
        }
    }

    Ok(())
}

fn get_state_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(".leviath").join("state"))
}
