//! `lev validate` - Validate an agent blueprint.

use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct ValidateArgs {
    /// Path to the agent directory or agent.leviath file
    #[arg(default_value = ".")]
    path: String,
}

/// Resolve, read, parse, and validate the manifest at `path`. Distinguishes
/// I/O failures (propagated as a normal error) from parse/validation
/// failures (which `execute()` reports specially and exits(1) on) so the
/// core logic can be unit tested without killing the test process.
#[derive(Debug)]
enum ManifestCheckError {
    Io(anyhow::Error),
    Parse(String),
    Validation(String),
}

fn check_manifest(path: &std::path::Path) -> Result<leviath_core::Blueprint, ManifestCheckError> {
    // Resolve manifest path
    let manifest_path = if path.is_file() {
        path.to_path_buf()
    } else {
        let p = path.join("agent.leviath");
        if !p.exists() {
            return Err(ManifestCheckError::Io(anyhow::anyhow!(
                "No agent.leviath found at {}",
                path.display()
            )));
        }
        p
    };

    let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
        ManifestCheckError::Io(anyhow::anyhow!(
            "Failed to read {}: {}",
            manifest_path.display(),
            e
        ))
    })?;

    let blueprint = super::run::parse_manifest_public(&content)
        .map_err(|e| ManifestCheckError::Parse(e.to_string()))?;

    blueprint
        .validate()
        .map_err(|e| ManifestCheckError::Validation(e.to_string()))?;

    Ok(blueprint)
}

/// Print the "valid blueprint" summary + non-fatal warnings.
fn print_success(blueprint: &leviath_core::Blueprint) {
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
    print_warnings(blueprint);
}

/// Outcome of the real, testable logic in [`execute`]. Kept distinct from
/// the actual `std::process::exit(1)` calls (which would kill the test
/// process if exercised directly) so `execute_reporting_outcome` -- and
/// therefore every branch of `check_manifest`'s error handling -- can be
/// unit tested; only the thin `execute` wrapper below ever calls `exit`.
enum ValidateOutcome {
    Success,
    ParseError(String),
    ValidationError(String),
}

fn execute_reporting_outcome(args: &ValidateArgs) -> anyhow::Result<ValidateOutcome> {
    let path = PathBuf::from(&args.path);

    let blueprint = match check_manifest(&path) {
        Ok(bp) => bp,
        Err(ManifestCheckError::Io(e)) => return Err(e),
        Err(ManifestCheckError::Parse(e)) => return Ok(ValidateOutcome::ParseError(e)),
        Err(ManifestCheckError::Validation(e)) => return Ok(ValidateOutcome::ValidationError(e)),
    };

    print_success(&blueprint);
    Ok(ValidateOutcome::Success)
}

pub async fn execute(args: ValidateArgs) -> anyhow::Result<()> {
    match execute_reporting_outcome(&args)? {
        ValidateOutcome::Success => Ok(()),
        ValidateOutcome::ParseError(e) => {
            eprintln!("✗ Parse error: {}", e);
            std::process::exit(1);
        }
        ValidateOutcome::ValidationError(e) => {
            eprintln!("✗ Validation failed: {}", e);
            std::process::exit(1);
        }
    }
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

    // ─── execute_reporting_outcome: Parse/Validation paths ──────────────
    //
    // `execute()` itself calls `std::process::exit(1)` on these two
    // branches, which would kill the test process -- `execute_reporting_outcome`
    // exists precisely so these can be exercised without that.

    #[test]
    fn execute_reporting_outcome_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let outcome = execute_reporting_outcome(&args).unwrap();
        assert!(matches!(outcome, ValidateOutcome::ParseError(_)));
    }

    #[test]
    fn execute_reporting_outcome_bad_entry_stage_is_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "bad-entry-agent"
version = "0.1.0"
description = "Entry stage does not exist"
entry_stage = "does-not-exist"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_manifest(dir.path(), manifest);
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let outcome = execute_reporting_outcome(&args).unwrap();
        assert!(matches!(outcome, ValidateOutcome::ValidationError(_)));
    }

    #[test]
    fn execute_reporting_outcome_missing_manifest_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        assert!(execute_reporting_outcome(&args).is_err());
    }

    #[test]
    fn execute_reporting_outcome_valid_manifest_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "ok-agent"
version = "0.1.0"
description = "Valid"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_manifest(dir.path(), manifest);
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let outcome = execute_reporting_outcome(&args).unwrap();
        assert!(matches!(outcome, ValidateOutcome::Success));
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

    // ─── print_warnings: BFS revisits an already-reached node (diamond) ──
    //
    // `entry` transitions to both `b` and `c`, and both `b` and `c`
    // transition to `d` -- `d` gets queued twice, so the *second* pop hits
    // the `if !reachable.insert(name.clone()) { continue; }` early-exit that
    // a simple linear chain or single-path graph never reaches.

    #[test]
    fn print_warnings_diamond_graph_revisits_shared_target_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.entry]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Entry"
max_iterations = 5
entry = true
[stages.entry.transitions]
b = "true"
c = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "B"
max_iterations = 5
[stages.b.transitions]
d = "true"

[stages.c]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "C"
max_iterations = 5
[stages.c.transitions]
d = "true"

[stages.d]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "D"
max_iterations = 5
"#,
        );
        let bp = parse(&toml);
        // All 4 stages reachable, no unreachable warnings expected; the
        // point of this test is exercising the revisit-skip branch itself.
        print_warnings(&bp);
    }

    // ─── check_manifest ──────────────────────────────────────────────────

    fn write_manifest(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("agent.leviath");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn check_manifest_missing_directory_manifest_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_manifest(dir.path()).unwrap_err();
        assert!(matches!(err, ManifestCheckError::Io(_)));
        if let ManifestCheckError::Io(e) = err {
            assert!(e.to_string().contains("No agent.leviath found"));
        }
    }

    #[test]
    fn check_manifest_unreadable_file_path_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        // Pass a path to a file that doesn't exist directly (is_file() is
        // false, and it's not a directory either) — falls through to the
        // "join agent.leviath" branch, which also won't exist.
        let missing = dir.path().join("nonexistent-subdir");
        let err = check_manifest(&missing).unwrap_err();
        assert!(matches!(err, ManifestCheckError::Io(_)));
    }

    // Distinct from the two "file doesn't exist" IO-error cases above: this
    // exercises `std::fs::read_to_string`'s own `Err` arm (a manifest file
    // that *is* found via `path.is_file()`/`.exists()`, but can't actually
    // be read), which no other test reaches.
    #[cfg(unix)]
    #[test]
    fn check_manifest_permission_denied_file_is_io_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = write_manifest(dir.path(), "irrelevant content");
        std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = check_manifest(&manifest_path);

        // Restore permissions so the tempdir can clean itself up regardless
        // of the assertion outcome below.
        std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = result.unwrap_err();
        assert!(matches!(err, ManifestCheckError::Io(_)));
        if let ManifestCheckError::Io(e) = err {
            assert!(e.to_string().contains("Failed to read"));
        }
    }

    #[test]
    fn check_manifest_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        let err = check_manifest(dir.path()).unwrap_err();
        assert!(matches!(err, ManifestCheckError::Parse(_)));
    }

    #[test]
    fn check_manifest_direct_file_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let toml = make_blueprint_toml(
            r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main stage"
max_iterations = 5
"#,
        );
        let manifest_path = write_manifest(dir.path(), &toml);
        // Pass the *file* path directly, not the directory.
        let blueprint = check_manifest(&manifest_path).unwrap();
        assert_eq!(blueprint.name, "test");
    }

    #[test]
    fn check_manifest_valid_linear_blueprint_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let toml = make_blueprint_toml(
            r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main stage"
max_iterations = 5
"#,
        );
        write_manifest(dir.path(), &toml);
        let blueprint = check_manifest(dir.path()).unwrap();
        assert_eq!(blueprint.name, "test");
        assert_eq!(blueprint.stages.len(), 1);
    }

    // ─── print_success ───────────────────────────────────────────────────

    #[test]
    fn print_success_linear_mode_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main stage"
max_iterations = 5

[stages.review]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Review stage"
max_iterations = 5
"#,
        );
        let bp = parse(&toml);
        print_success(&bp);
    }

    #[test]
    fn print_success_graph_mode_with_terminal_and_revisits_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "A"
max_iterations = 5
entry = true
max_revisits = 3
[stages.a.transitions]
b = "true"

[stages.b]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "B"
max_iterations = 5
"#,
        );
        let bp = parse(&toml);
        // Exercises: graph mode header, an edge with a target ("-> b"), and
        // stage "b" which has transitions = None ("(linear)" branch) as well
        // as the max_revisits formatting on stage "a".
        print_success(&bp);
    }

    #[test]
    fn print_success_graph_mode_terminal_stage_no_panic() {
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
"#,
        );
        let bp = parse(&toml);
        // Stage "b" has an explicitly-empty transitions table -> Some(empty
        // map) -> exercises the "(terminal)" formatting branch.
        let b = bp.find_stage("b").unwrap();
        assert!(matches!(&b.transitions, Some(t) if t.is_empty()));
        print_success(&bp);
    }
}
