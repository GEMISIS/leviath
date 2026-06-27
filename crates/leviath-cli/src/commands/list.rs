//! `lev list` - List available agents and blueprints

use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use super::run::parse_manifest_public;

#[derive(Args)]
pub struct ListArgs {
    /// Filter by type (agents, blueprints, all)
    #[arg(short, long, default_value = "all")]
    pub filter: String,
}

/// Info parsed from an agent manifest for display.
struct AgentInfo {
    name: String,
    version: String,
    description: String,
}

fn read_agent_info(manifest_path: &Path) -> Option<AgentInfo> {
    let content = fs::read_to_string(manifest_path).ok()?;
    let blueprint = parse_manifest_public(&content).ok()?;
    Some(AgentInfo {
        name: blueprint.name,
        version: blueprint.version,
        description: blueprint.description,
    })
}

fn scan_directory_for_agents(dir: &Path) -> Vec<(PathBuf, AgentInfo)> {
    let mut agents = Vec::new();
    if !dir.exists() {
        return agents;
    }

    // Check if this directory itself has an agent.leviath
    let direct_manifest = dir.join("agent.leviath");
    if direct_manifest.exists() {
        if let Some(info) = read_agent_info(&direct_manifest) {
            agents.push((dir.to_path_buf(), info));
        }
    }

    // Check subdirectories
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let manifest_path = path.join("agent.leviath");
                if manifest_path.exists() {
                    if let Some(info) = read_agent_info(&manifest_path) {
                        agents.push((path, info));
                    }
                }
            }
        }
    }

    agents
}

pub async fn execute(_args: ListArgs) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let mut found_anything = false;

    // 1. Installed agents (~/.leviath/agents/)
    let agents_dir = get_agents_dir()?;
    let installed = scan_directory_for_agents(&agents_dir);
    if !installed.is_empty() {
        found_anything = true;
        println!("Installed agents (~/.leviath/agents/):");
        for (_path, info) in &installed {
            let desc = if info.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", info.description)
            };
            println!("  {} (v{}){}", info.name, info.version, desc);
        }
        println!();
    }

    // 2. Local (current directory)
    let cwd = std::env::current_dir().unwrap_or_default();
    let local_manifest = cwd.join("agent.leviath");
    if local_manifest.exists() {
        if let Some(info) = read_agent_info(&local_manifest) {
            found_anything = true;
            let desc = if info.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", info.description)
            };
            println!("Local (current directory):");
            println!("  {} (v{}){}", info.name, info.version, desc);
            println!();
        }
    }

    // 3. Config's agent_paths directories
    let mut config_agents = Vec::new();
    for agent_path in &config.agent_paths {
        let found = scan_directory_for_agents(agent_path);
        config_agents.extend(found);
    }
    if !config_agents.is_empty() {
        found_anything = true;
        println!("From configured paths:");
        for (_path, info) in &config_agents {
            let desc = if info.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", info.description)
            };
            println!("  {} (v{}){}", info.name, info.version, desc);
        }
        println!();
    }

    // 4. Built-in agents (relative to the binary or known locations)
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    if let Some(ref exe_dir) = exe_dir {
        let builtin_dir = exe_dir.join("agents");
        let builtins = scan_directory_for_agents(&builtin_dir);
        if !builtins.is_empty() {
            found_anything = true;
            let names: Vec<&str> = builtins.iter().map(|(_, i)| i.name.as_str()).collect();
            println!("Built-in agents:");
            println!("  {}", names.join(", "));
            println!();
        }
    }

    if !found_anything {
        println!("No agents found.");
        println!();
        println!("To create a new agent:");
        println!("  lev init my-agent");
        println!();
        println!("To install an agent:");
        println!("  lev install <package>");
    }

    Ok(())
}

fn get_agents_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(".leviath").join("agents"))
}
