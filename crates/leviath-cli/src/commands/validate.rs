//! `lev validate` - Validate an agent blueprint.

use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct ValidateArgs {
    /// Path to the agent directory or agent.leviath file
    #[arg(default_value = ".")]
    pub(crate) path: String,
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

    let blueprint = leviath_core::manifest::parse_manifest(&content)
        .map_err(|e| ManifestCheckError::Parse(e.to_string()))?;

    blueprint
        .validate()
        .map_err(|e| ManifestCheckError::Validation(e.to_string()))?;

    // Custom regions' Rhai scripts must resolve to readable, compilable
    // files with a well-formed `fn render(ctx)` - the same check a spawn
    // performs, surfaced here where a typo'd path or syntax error is cheap
    // to find.
    crate::daemon::spawn::resolve_region_scripts(&blueprint, &manifest_path.to_string_lossy())
        .map_err(ManifestCheckError::Validation)?;

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

    // Command seeds execute at spawn - surface them before anything else, so
    // `lev validate` is a real audit step before `lev add`.
    for line in command_seed_report(blueprint) {
        println!("{line}");
    }

    // Warnings (non-fatal)
    print_warnings(blueprint);
}

/// The report lines for a blueprint's `seed = { command = "..." }` regions.
///
/// Split out from the printer so it is directly assertable. Empty when the
/// blueprint declares none - the overwhelmingly common case, which should print
/// nothing at all.
fn command_seed_report(blueprint: &leviath_core::Blueprint) -> Vec<String> {
    let seeds: Vec<(&str, &str)> = blueprint
        .context_layout
        .regions
        .iter()
        .filter_map(|r| match &r.seed {
            Some(leviath_core::layout::RegionSeed::Command { command }) => {
                Some((r.name.as_str(), command.as_str()))
            }
            _ => None,
        })
        .collect();
    if seeds.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "  ⚠ {} region(s) run a shell command at spawn, before the first \
         inference and before any tool-approval prompt:",
        seeds.len()
    )];
    lines.extend(
        seeds
            .iter()
            .map(|(region, command)| format!("      {region}: {command}")),
    );
    lines.push(
        "    Disable with `--no-seed-commands`, or machine-wide via \
         `[security] allow_seed_commands = false`."
            .to_string(),
    );
    lines
}

/// Outcome of the real, testable logic in [`execute`]. Kept distinct from
/// the actual `std::process::exit(1)` calls (which would kill the test
/// process if exercised directly) so `execute_reporting_outcome` - and
/// therefore every branch of `check_manifest`'s error handling - can be
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
    print_script_tool_report(&path);
    Ok(ValidateOutcome::Success)
}

/// Validate the agent's own Rhai script tools: discover the agent
/// directory's `tools/` and report how many compiled, warning (non-fatal, like
/// the daemon's own skip-and-warn) about any that failed. A missing `tools/` dir
/// prints nothing.
fn print_script_tool_report(path: &std::path::Path) {
    // The agent dir is the manifest's parent (file path) or the path itself (dir).
    let agent_dir = if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    };
    let tools_dir = agent_dir.join("tools");
    if !tools_dir.is_dir() {
        return;
    }
    let (set, skipped) = leviath_scripting::ScriptToolSet::discover(&[tools_dir]);
    if !set.is_empty() {
        println!("  {} script tool(s) in tools/", set.len());
    }
    // A tool that compiles but whose `@requires` the platform can't satisfy won't
    // load - flag it (this also catches an unknown/typo'd capability name).
    for meta in set.metas() {
        if !crate::daemon::spawn::current_platform_satisfies(&meta.required_caps) {
            println!(
                "  ⚠ Warning: script tool '{}' won't load here (unsatisfiable @requires: {})",
                meta.name,
                meta.required_caps.join(", ")
            );
        }
    }
    for s in &skipped {
        println!(
            "  ⚠ Warning: script tool '{}' skipped: {}",
            s.path.display(),
            s.reason
        );
    }
}

/// Report `[read_paths]` declarations: what the agent asks to read beyond its
/// workdir, plus a sharper warning for entries so broad they amount to "my
/// whole home directory" or "any absolute path". Pure over the blueprint so
/// the wording is testable without capturing stdout.
fn read_path_warning_lines(blueprint: &leviath_core::Blueprint) -> Vec<String> {
    let Some(rp) = blueprint
        .read_paths
        .as_ref()
        .filter(|rp| !rp.allow.is_empty())
    else {
        return Vec::new();
    };
    let mut lines = vec![format!(
        "  ⚠ Note: declares [read_paths] (reads outside the run workdir): {} - refused \
         unless your config grants them",
        rp.allow.join(", ")
    )];
    for entry in &rp.allow {
        if read_path_entry_is_broad(entry) {
            lines.push(format!(
                "  ⚠ Warning: read_paths entry '{entry}' is very broad - it can match your \
                 entire home directory or any path on this machine"
            ));
        }
    }
    lines
}

/// Whether a `[read_paths]` entry grants effectively unlimited read access:
/// the home directory itself, a filesystem root, or a pattern whose first
/// component already matches anything.
fn read_path_entry_is_broad(entry: &str) -> bool {
    let pattern = entry
        .strip_prefix("glob:")
        .or_else(|| entry.strip_prefix("regex:"))
        .unwrap_or(entry);
    let pattern = pattern.replace('\\', "/");
    let trimmed = pattern.trim_end_matches('/');
    matches!(trimmed, "~" | "")
        || trimmed == "/**"
        || pattern.starts_with("**")
        || pattern.starts_with("/.*")
        || trimmed == "/.+"
}

pub async fn execute(args: ValidateArgs) -> anyhow::Result<()> {
    match execute_reporting_outcome(&args)? {
        ValidateOutcome::Success => Ok(()),
        ValidateOutcome::ParseError(e) => anyhow::bail!("✗ Parse error: {}", e),
        ValidateOutcome::ValidationError(e) => anyhow::bail!("✗ Validation failed: {}", e),
    }
}

fn print_warnings(blueprint: &leviath_core::Blueprint) {
    // Before the graph-only checks below: `[read_paths]` applies to any
    // blueprint shape, so it is reported ahead of the `is_graph` early return.
    for line in read_path_warning_lines(blueprint) {
        println!("{line}");
    }

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
        let Some(stage) = blueprint.find_stage(&name) else {
            continue;
        };
        let Some(ref transitions) = stage.transitions else {
            continue;
        };
        for target in transitions.keys() {
            if !reachable.contains(target.as_str()) && stage_names.contains(target.as_str()) {
                queue.push_back(target.clone());
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
        let Some(ref transitions) = stage.transitions else {
            continue;
        };
        for target in transitions.keys() {
            if target == &stage.name {
                continue;
            }
            // Check if target can reach back to this stage (cycle)
            let Some(target_stage) = blueprint.find_stage(target) else {
                continue;
            };
            let Some(ref t2) = target_stage.transitions else {
                continue;
            };
            if t2.contains_key(&stage.name) && target_stage.max_revisits.is_none() {
                #[rustfmt::skip]
                println!("  ⚠ Warning: stage '{}' is in a cycle but has no max_revisits set", target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;

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
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}
"#,
            stages_toml
        )
    }

    fn parse(toml: &str) -> leviath_core::Blueprint {
        leviath_core::manifest::parse_manifest(toml).unwrap()
    }

    /// The `[read_paths]` note fires for any blueprint shape - including the
    /// linear ones the graph warnings skip - and lists the entries verbatim.
    #[test]
    fn read_path_declarations_are_noted_for_linear_blueprints() {
        let toml = format!(
            "{}\n[read_paths]\nallow = [\"~/.leviath/runs\"]\n",
            make_blueprint_toml("[stages.plan]\nmode = \"autonomous\"")
        );
        let lines = read_path_warning_lines(&parse(&toml));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("~/.leviath/runs"), "{lines:?}");
        assert!(
            lines[0].contains("refused unless your config grants"),
            "{lines:?}"
        );
    }

    #[test]
    fn no_read_paths_means_no_note() {
        let toml = make_blueprint_toml("[stages.plan]\nmode = \"autonomous\"");
        assert!(read_path_warning_lines(&parse(&toml)).is_empty());
    }

    /// Entries that amount to "everything" get the sharper warning; scoped
    /// ones do not.
    #[test]
    fn broad_read_path_entries_get_their_own_warning() {
        let toml = format!(
            "{}\n[read_paths]\nallow = [\"~\", \"~/.leviath/runs\"]\n",
            make_blueprint_toml("[stages.plan]\nmode = \"autonomous\"")
        );
        let lines = read_path_warning_lines(&parse(&toml));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].contains("very broad"), "{lines:?}");
        assert!(lines[1].contains("'~'"), "{lines:?}");
    }

    #[test]
    fn broad_entry_heuristic_covers_each_shape() {
        for entry in [
            "~",
            "~/",
            "/",
            "glob:**",
            "glob:/**",
            "regex:/.*",
            "regex:/.+",
        ] {
            assert!(read_path_entry_is_broad(entry), "{entry}");
        }
        for entry in [
            "~/.leviath/runs",
            "glob:~/docs/**",
            "regex:/data/.*",
            "../shared",
            r"C:\data",
        ] {
            assert!(!read_path_entry_is_broad(entry), "{entry}");
        }
    }

    #[test]
    fn check_manifest_verifies_custom_region_scripts() {
        // A custom region's script must exist and compile; the same failure a
        // spawn would hit, surfaced by `lev validate`.
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("agent.leviath");
        let toml = r#"
[agent]
name = "custom-validate"
version = "0.1.0"
description = "d"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
description = "Main stage"

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
brain = { kind = "custom", script = "hooks/brain.rhai", max_tokens = 1000 }
"#;
        std::fs::write(&manifest_path, toml).unwrap();

        // Missing script file → validation error naming region + path.
        let err = format!("{:?}", check_manifest(&manifest_path).unwrap_err());
        assert!(err.starts_with("Validation"), "{err}");
        assert!(err.contains("region 'brain'"), "{err}");

        // Present + compilable → passes.
        std::fs::create_dir(dir.path().join("hooks")).unwrap();
        std::fs::write(
            dir.path().join("hooks/brain.rhai"),
            "fn render(ctx) { \"ok\" }",
        )
        .unwrap();
        let bp = check_manifest(&manifest_path).unwrap();
        assert_eq!(bp.name, "custom-validate");
    }

    #[test]
    fn command_seed_report_is_empty_without_command_seeds() {
        let bp = parse(&make_blueprint_toml(
            r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
description = "Main stage"
"#,
        ));
        assert!(command_seed_report(&bp).is_empty());
    }

    #[test]
    fn command_seed_report_names_every_region_and_command() {
        let toml = r#"
[agent]
name = "scanner"
version = "0.1.0"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
description = "Main stage"

[context.regions]
facts = { kind = "pinned", max_tokens = 1000, seed = { command = "git ls-files" } }
tests = { kind = "pinned", max_tokens = 1000, seed = { command = "ls tests" } }
plain = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;
        let report = command_seed_report(&parse(toml)).join("\n");
        assert!(report.contains("2 region(s)"), "got: {report}");
        assert!(report.contains("facts: git ls-files"), "got: {report}");
        assert!(report.contains("tests: ls tests"), "got: {report}");
        // The escape hatches are named so the reader knows how to refuse.
        assert!(report.contains("--no-seed-commands"), "got: {report}");
        assert!(report.contains("allow_seed_commands"), "got: {report}");
        // A region without a command seed is not reported.
        assert!(!report.contains("plain"), "got: {report}");
        // And the printer itself runs.
        print_success(&parse(toml));
    }

    #[test]
    fn print_warnings_linear_mode_no_panic() {
        let toml = format!(
            "{}\n[read_paths]\nallow = [\"~/.leviath/runs\", \"~\"]\n",
            make_blueprint_toml(
                r#"
[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main stage"
max_iterations = 10
"#,
            )
        );
        let bp = parse(&toml);
        // Should not panic even though there's no graph, and the read_paths
        // note (plus the broad-entry warning for "~") is printed before the
        // graph-only checks bail.
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
        // No unreachable stages - should run without issues
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

    // ─── print_warnings: self-loop cycle (target == stage.name skip) ────

    #[test]
    fn print_warnings_self_loop_with_max_revisits_no_panic() {
        let toml = make_blueprint_toml(
            r#"
[stages.a]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Stage A"
max_iterations = 5
entry = true
max_revisits = 3
[stages.a.transitions]
a = "true"
"#,
        );
        let bp = parse(&toml);
        // Self-loop transition should hit the `target == stage.name` skip in
        // the cycle-detection loop without panicking or false-warning.
        print_warnings(&bp);
    }

    // ─── print_warnings: Blueprint constructed directly (not via parse +
    // validate), so it can carry invariants `Blueprint::validate` would
    // normally reject. `print_warnings` takes a bare `&Blueprint` and has
    // no way to know whether its caller validated it first, so these
    // "malformed but structurally valid Rust" shapes are reachable through
    // its public API even though `execute_reporting_outcome` (the only
    // production caller) always validates first. ────────────────────────

    fn make_model() -> leviath_core::blueprint::ModelConfig {
        leviath_core::blueprint::ModelConfig::new(
            "anthropic".to_string(),
            "claude-sonnet-4-6".to_string(),
        )
    }

    #[test]
    fn print_warnings_entry_stage_missing_no_panic() {
        use leviath_core::{Blueprint, ContextLayout, Stage};

        // Stage "a" is valid on its own, but the blueprint's entry_stage
        // points at a name that doesn't exist among `stages` - impossible
        // via `Blueprint::validate`, but not impossible via this struct's
        // public fields/constructors.
        let mut stage_a = Stage::new("a".to_string(), make_model());
        stage_a.transitions = Some(std::collections::HashMap::new());

        let layout = ContextLayout::new(Vec::new(), 1000);
        let mut bp = Blueprint::new(
            "test".to_string(),
            "test".to_string(),
            vec![stage_a],
            layout,
        );
        bp.entry_stage = Some("ghost".to_string());

        // Hits the BFS's `find_stage(&name) else { continue }` arm: "ghost"
        // is queued as the entry but resolves to no real stage.
        print_warnings(&bp);
    }

    #[test]
    fn print_warnings_transition_target_missing_no_panic() {
        use leviath_core::{Blueprint, ContextLayout, Stage, TransitionEdge};

        // Stage "a" transitions to "ghost", a name with no corresponding
        // Stage entry - impossible via `Blueprint::validate` (which
        // requires every transition target to exist), but constructible
        // directly since `transitions` is a public field.
        let mut transitions = std::collections::HashMap::new();
        transitions.insert(
            "ghost".to_string(),
            TransitionEdge {
                target: "ghost".to_string(),
                condition: Default::default(),
                hint: None,
                transform: Default::default(),
                gate: None,
                stuck: None,
            },
        );
        let mut stage_a = Stage::new("a".to_string(), make_model());
        stage_a.transitions = Some(transitions);

        let layout = ContextLayout::new(Vec::new(), 1000);
        let bp = Blueprint::new(
            "test".to_string(),
            "test".to_string(),
            vec![stage_a],
            layout,
        );

        // Hits the cycle-check loop's `find_stage(target) else { continue }`
        // arm: "ghost" is a transition target but not a real stage.
        print_warnings(&bp);
    }

    // ─── execute: parse and validation error arms ──────────────────────────
    //
    // These call execute() directly (not execute_reporting_outcome) so the
    // ParseError and ValidationError match arms in execute() are exercised.

    #[tokio::test]
    async fn execute_parse_error_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let err = execute(args).await.unwrap_err();
        assert!(err.to_string().contains("Parse error"));
    }

    #[tokio::test]
    async fn execute_validation_error_returns_error() {
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
        let err = execute(args).await.unwrap_err();
        assert!(err.to_string().contains("Validation failed"));
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
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
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
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;
        write_test_agent(dir.path(), manifest);

        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    // ─── execute_reporting_outcome: Parse/Validation paths ──────────────
    //
    // `execute()` itself calls `std::process::exit(1)` on these two
    // branches, which would kill the test process - `execute_reporting_outcome`
    // exists precisely so these can be exercised without that.

    fn assert_is_parse_error(outcome: &ValidateOutcome) {
        assert!(matches!(outcome, ValidateOutcome::ParseError(_)));
    }

    #[test]
    #[should_panic(expected = "assertion failed")]
    fn assert_is_parse_error_panics_on_non_parse_error() {
        assert_is_parse_error(&ValidateOutcome::Success);
    }

    #[test]
    fn execute_reporting_outcome_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let outcome = execute_reporting_outcome(&args).unwrap();
        assert_is_parse_error(&outcome);
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
        assert_is_validation_error(&outcome);
    }

    fn assert_is_validation_error(outcome: &ValidateOutcome) {
        assert!(matches!(outcome, ValidateOutcome::ValidationError(_)));
    }

    #[test]
    #[should_panic(expected = "assertion failed")]
    fn assert_is_validation_error_panics_on_non_validation_error() {
        assert_is_validation_error(&ValidateOutcome::Success);
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
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;
        write_manifest(dir.path(), manifest);
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let outcome = execute_reporting_outcome(&args).unwrap();
        assert_is_success(&outcome);
    }

    fn assert_is_success(outcome: &ValidateOutcome) {
        assert!(matches!(outcome, ValidateOutcome::Success));
    }

    #[test]
    fn execute_reporting_outcome_reports_agent_script_tools() {
        // A valid agent whose `tools/` dir holds one good and one broken script:
        // validation still succeeds, and the script report's count + warning
        // branches both run.
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "with-tools"
version = "0.1.0"
description = "has script tools"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_manifest(dir.path(), manifest);
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("ok.rhai"), "// @tool ok\nparams.x").unwrap();
        std::fs::write(tools.join("bad.rhai"), "no directive\nlet").unwrap();
        // Compiles but requires an unsatisfiable capability → the won't-load warning.
        std::fs::write(tools.join("gpu.rhai"), "// @tool gpu\n// @requires gpu\n1").unwrap();
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let outcome = execute_reporting_outcome(&args).unwrap();
        assert_is_success(&outcome);
    }

    #[test]
    fn print_script_tool_report_no_tools_dir_is_silent() {
        // No `tools/` dir → the early return (covered by most success tests, but
        // asserted here directly against a file path, which exercises the
        // `path.is_file()` → parent arm).
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_manifest(dir.path(), "unused");
        print_script_tool_report(&manifest);
    }

    #[test]
    fn print_script_tool_report_only_broken_scripts_warns_without_count() {
        // A `tools/` dir with only a broken script: `set` is empty (no count
        // line - the `!set.is_empty()` false arm) but the skipped warning runs.
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("bad.rhai"), "no directive\nlet").unwrap();
        print_script_tool_report(dir.path());
    }

    #[test]
    #[should_panic(expected = "assertion failed")]
    fn assert_is_success_panics_on_non_success() {
        assert_is_success(&ValidateOutcome::ParseError("x".to_string()));
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
    // transition to `d` - `d` gets queued twice, so the *second* pop hits
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

    /// Extract the inner `anyhow::Error` from a `ManifestCheckError::Io`,
    /// panicking with a diagnostic message for any other variant.
    fn unwrap_io_err(err: ManifestCheckError) -> anyhow::Error {
        let ManifestCheckError::Io(e) = err else {
            panic!("expected ManifestCheckError::Io, got {err:?}");
        };
        e
    }

    #[test]
    #[should_panic(expected = "expected ManifestCheckError::Io")]
    fn unwrap_io_err_panics_on_parse_variant() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        let err = check_manifest(dir.path()).unwrap_err();
        // err is ManifestCheckError::Parse - this should panic
        unwrap_io_err(err);
    }

    #[test]
    fn check_manifest_missing_directory_manifest_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_manifest(dir.path()).unwrap_err();
        let e = unwrap_io_err(err);
        assert!(e.to_string().contains("No agent.leviath found"));
    }

    #[test]
    fn check_manifest_unreadable_file_path_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        // Pass a path to a file that doesn't exist directly (is_file() is
        // false, and it's not a directory either) - falls through to the
        // "join agent.leviath" branch, which also won't exist.
        let missing = dir.path().join("nonexistent-subdir");
        let err = check_manifest(&missing).unwrap_err();
        unwrap_io_err(err);
    }

    // Distinct from the two "file doesn't exist" IO-error cases above: this
    // exercises `std::fs::read_to_string`'s own `Err` arm (a manifest file
    // that *is* found via `path.is_file()`/`.exists()`, but can't actually
    // be read), which no other test reaches.
    #[test]
    fn check_manifest_unreadable_file_is_io_error() {
        // `agent.leviath` exists but is a *directory*, so it's found via
        // `.exists()` yet `read_to_string` fails on every platform, exercising
        // the read_to_string map_err arm.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agent.leviath")).unwrap();

        let result = check_manifest(dir.path());

        let err = result.unwrap_err();
        let e = unwrap_io_err(err);
        assert!(e.to_string().contains("Failed to read"));
    }

    #[test]
    fn check_manifest_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        let err = check_manifest(dir.path()).unwrap_err();
        assert_is_manifest_parse_error(&err);
    }

    fn assert_is_manifest_parse_error(err: &ManifestCheckError) {
        assert!(matches!(err, ManifestCheckError::Parse(_)));
    }

    #[test]
    #[should_panic(expected = "assertion failed")]
    fn assert_is_manifest_parse_error_panics_on_non_parse_error() {
        assert_is_manifest_parse_error(&ManifestCheckError::Io(anyhow::anyhow!("x")));
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
