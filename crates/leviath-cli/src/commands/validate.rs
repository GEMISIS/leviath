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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a minimal valid blueprint TOML with given stages.
    fn make_blueprint_toml(stages_toml: &str) -> String {
        format!(
            r#"
[agent]
name = "test"
version = "0.1.0"
description = "test blueprint"

{}

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
"#,
            stages_toml
        )
    }

    fn parse(toml: &str) -> leviath_core::Blueprint {
        super::super::run::parse_manifest_public(toml).unwrap()
    }

    #[test]
    fn print_warnings_linear_mode_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main stage"
max_iterations = 10
"#,
        );
        let bp = parse(&toml);
        // Should not panic even though there's no graph
        print_warnings(&bp);
    }

    #[test]
    fn print_warnings_graph_all_reachable() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage A"
max_iterations = 5
entry = true
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage B"
max_iterations = 5
"#,
        );
        let bp = parse(&toml);
        // No unreachable stages — should run without issues
        print_warnings(&bp);
    }

    #[test]
    fn validate_args_default_path() {
        // ValidateArgs can be constructed with default path
        let args = ValidateArgs {
            path: ".".to_string(),
        };
        assert_eq!(args.path, ".");
    }

    // ─── print_warnings: unreachable stage ──────────────────────────────

    #[test]
    fn print_warnings_unreachable_stage_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage A"
max_iterations = 5
entry = true
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage B"
max_iterations = 5

[stages.orphan]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Unreachable stage"
max_iterations = 5
"#,
        );
        let bp = parse(&toml);
        // Should not panic; orphan stage is unreachable
        print_warnings(&bp);
    }

    // ─── print_warnings: cycle without max_revisits ─────────────────────

    #[test]
    fn print_warnings_cycle_without_max_revisits_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage A"
max_iterations = 5
entry = true
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage B"
max_iterations = 5
[stages.b.transitions]
a = "true"
"#,
        );
        let bp = parse(&toml);
        // Should print warning about cycle but not panic
        print_warnings(&bp);
    }

    // ─── print_warnings: cycle with max_revisits set ────────────────────

    #[test]
    fn print_warnings_cycle_with_max_revisits_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage A"
max_iterations = 5
entry = true
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage B"
max_iterations = 5
max_revisits = 3
[stages.b.transitions]
a = "true"
"#,
        );
        let bp = parse(&toml);
        print_warnings(&bp);
    }

    // ─── print_warnings: terminal stage with empty transitions ──────────

    #[test]
    fn print_warnings_terminal_stage_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage A"
max_iterations = 5
entry = true
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Terminal stage"
max_iterations = 5
[stages.b.transitions]
"#,
        );
        let bp = parse(&toml);
        print_warnings(&bp);
    }

    // ─── execute: no manifest ──────────────────────────────────────────

    #[tokio::test]
    async fn execute_no_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let result = execute(args).await;
        assert!(result.is_err());
    }

    // ─── execute: with file path pointing to manifest ───────────────────

    #[tokio::test]
    async fn execute_valid_manifest_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "test-agent"
version = "0.1.0"
description = "A test agent"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        let manifest_path = dir.path().join("agent.leviath");
        std::fs::write(&manifest_path, manifest).unwrap();

        let args = ValidateArgs {
            path: manifest_path.to_str().unwrap().to_string(),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    // ─── execute: with directory path ───────────────────────────────────

    #[tokio::test]
    async fn execute_valid_manifest_directory_path() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "dir-agent"
version = "0.2.0"
description = "A directory agent"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        std::fs::write(dir.path().join("agent.leviath"), manifest).unwrap();

        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    // ─── print_warnings: multiple stages all reachable ──────────────────

    #[test]
    fn print_warnings_chain_all_reachable() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "A"
max_iterations = 5
entry = true
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "B"
max_iterations = 5
[stages.b.transitions]
c = "true"

[stages.c]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "C"
max_iterations = 5
"#,
        );
        let bp = parse(&toml);
        print_warnings(&bp);
    }
}
