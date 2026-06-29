//! `lev validate` - Validate an agent blueprint.

use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct ValidateArgs {
    /// Path to the agent directory or agent.leviath file
    #[arg(default_value = ".")]
    path: String,
}

pub async fn execute(args: ValidateArgs) -> anyhow::Result<()> {
    let path = PathBuf::from(&args.path);

    // Resolve manifest path
    let manifest_path = if path.is_file() {
        path.clone()
    } else {
        let p = path.join("agent.leviath");
        if !p.exists() {
            anyhow::bail!("No agent.leviath found at {}", path.display());
        }
        p
    };

    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", manifest_path.display(), e))?;

    // Parse
    let blueprint = match super::run::parse_manifest_public(&content) {
        Ok(bp) => bp,
        Err(e) => {
            eprintln!("✗ Parse error: {}", e);
            std::process::exit(1);
        }
    };

    // Validate
    match blueprint.validate() {
        Ok(()) => {
            println!("✓ Blueprint '{}' is valid.", blueprint.name);
            println!(
                "  {} stages, version {}",
                blueprint.stages.len(),
                blueprint.version
            );

            // Check if graph mode
            let is_graph = blueprint.stages.iter().any(|s| s.transitions.is_some());
            if is_graph {
                let entry = blueprint.resolve_entry_stage_name();
                println!("  Graph mode: entry stage '{}'", entry);

                // List stages and their transitions
                for stage in &blueprint.stages {
                    let transitions_info = match &stage.transitions {
                        Some(t) if !t.is_empty() => {
                            let targets: Vec<&str> = t.keys().map(|k| k.as_str()).collect();
                            format!(" → {}", targets.join(", "))
                        }
                        Some(_) => " (terminal)".to_string(),
                        None => " (linear)".to_string(),
                    };
                    let revisits = stage
                        .max_revisits
                        .map(|n| format!(" (max_revisits: {})", n))
                        .unwrap_or_default();
                    println!("  - {}{}{}", stage.name, transitions_info, revisits);
                }
            } else {
                println!(
                    "  Linear mode: {}",
                    blueprint
                        .stages
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                );
            }

            // Warnings (non-fatal)
            print_warnings(&blueprint);
        }
        Err(e) => {
            eprintln!("✗ Validation failed: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_warnings(blueprint: &leviath_core::Blueprint) {
    let stage_names: std::collections::HashSet<&str> =
        blueprint.stages.iter().map(|s| s.name.as_str()).collect();

    let is_graph = blueprint.stages.iter().any(|s| s.transitions.is_some());
    if !is_graph {
        return;
    }

    let entry = blueprint.resolve_entry_stage_name();

    // Check reachability via BFS from entry stage
    let mut reachable = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(entry.clone());
    while let Some(name) = queue.pop_front() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        if let Some(stage) = blueprint.find_stage(&name) {
            if let Some(ref transitions) = stage.transitions {
                for target in transitions.keys() {
                    if !reachable.contains(target.as_str()) && stage_names.contains(target.as_str())
                    {
                        queue.push_back(target.clone());
                    }
                }
            }
        }
    }

    for stage in &blueprint.stages {
        if !reachable.contains(stage.name.as_str()) {
            println!(
                "  ⚠ Warning: stage '{}' is unreachable from entry stage '{}'",
                stage.name, entry
            );
        }
    }

    // Check for loops without max_revisits
    for stage in &blueprint.stages {
        if let Some(ref transitions) = stage.transitions {
            for target in transitions.keys() {
                if target != &stage.name {
                    // Check if target can reach back to this stage (cycle)
                    if let Some(target_stage) = blueprint.find_stage(target) {
                        if let Some(ref t2) = target_stage.transitions {
                            if t2.contains_key(&stage.name) && target_stage.max_revisits.is_none() {
                                println!(
                                    "  ⚠ Warning: stage '{}' is in a cycle but has no max_revisits set",
                                    target
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
