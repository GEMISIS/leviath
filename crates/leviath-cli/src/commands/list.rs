//! `lev list` - List available agents and blueprints

use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};

use super::run::parse_manifest_public;
use crate::config::Config;

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
        println!("  lev add <package>");
    }

    Ok(())
}

fn get_agents_dir() -> anyhow::Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(".leviath").join("agents"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, name: &str) {
        let content = format!(
            r#"[agent]
name = "{}"
version = "1.0.0"
description = "Test agent"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Main"
max_iterations = 5

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
"#,
            name
        );
        fs::write(dir.join("agent.leviath"), content).unwrap();
    }

    #[test]
    fn read_agent_info_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "my-agent");
        let info = read_agent_info(&dir.path().join("agent.leviath")).unwrap();
        assert_eq!(info.name, "my-agent");
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.description, "Test agent");
    }

    #[test]
    fn read_agent_info_missing_file_returns_none() {
        let result = read_agent_info(Path::new("/nonexistent/agent.leviath"));
        assert!(result.is_none());
    }

    #[test]
    fn read_agent_info_invalid_toml_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("agent.leviath"), "not valid toml {{{{").unwrap();
        let result = read_agent_info(&dir.path().join("agent.leviath"));
        assert!(result.is_none());
    }

    #[test]
    fn scan_directory_nonexistent_returns_empty() {
        let agents = scan_directory_for_agents(Path::new("/nonexistent/path"));
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_directory_with_direct_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "direct-agent");
        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.name, "direct-agent");
    }

    #[test]
    fn scan_directory_with_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let sub1 = dir.path().join("agent-a");
        let sub2 = dir.path().join("agent-b");
        fs::create_dir_all(&sub1).unwrap();
        fs::create_dir_all(&sub2).unwrap();
        write_manifest(&sub1, "agent-a");
        write_manifest(&sub2, "agent-b");

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 2);
        let names: Vec<&str> = agents.iter().map(|a| a.1.name.as_str()).collect();
        assert!(names.contains(&"agent-a"));
        assert!(names.contains(&"agent-b"));
    }

    #[test]
    fn scan_directory_ignores_subdirs_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("no-manifest");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("readme.txt"), "not a manifest").unwrap();

        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    #[test]
    fn list_args_default_filter() {
        let args = ListArgs {
            filter: "all".to_string(),
        };
        assert_eq!(args.filter, "all");
    }
}
