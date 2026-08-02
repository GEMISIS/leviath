//! `lev validate` - Validate an agent blueprint.

use clap::Args;
use std::path::PathBuf;

use crate::lint::{LintEnv, LintFinding, LintSeverity, lint_manifest};

#[derive(Args)]
pub struct ValidateArgs {
    /// Path to the agent directory or agent.leviath file
    #[arg(default_value = ".")]
    pub(crate) path: String,

    /// Fail on warnings too, not only errors. Notes never fail.
    #[arg(long)]
    pub(crate) deny_warnings: bool,
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

/// A manifest that parsed and validated, kept alongside the text it came from
/// so the linter can ask what the author actually wrote.
#[derive(Debug)]
struct CheckedManifest {
    blueprint: leviath_core::Blueprint,
    content: String,
    /// The directory holding the manifest: where its `tools/` live.
    agent_dir: PathBuf,
}

fn check_manifest(path: &std::path::Path) -> Result<CheckedManifest, ManifestCheckError> {
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

    let agent_dir = manifest_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    Ok(CheckedManifest {
        blueprint,
        content,
        agent_dir,
    })
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
}

/// Outcome of the real, testable logic in [`execute`]. Kept distinct from
/// the actual failure reporting so `execute_reporting_outcome` - and therefore
/// every branch of `check_manifest`'s error handling - can be unit tested.
#[derive(Debug)]
enum ValidateOutcome {
    Success,
    ParseError(String),
    ValidationError(String),
    /// The manifest is structurally fine but the lint found something fatal:
    /// how many errors, and how many warnings (which only count when
    /// `--deny-warnings` was passed).
    LintFailed {
        errors: usize,
        warnings: usize,
    },
}

/// Print `findings` worst-first, one per line with its fix indented under it.
///
/// Returns the counts so the caller can decide the exit status without walking
/// the list again.
fn print_findings(findings: &[LintFinding]) -> (usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    for finding in findings {
        match finding.severity {
            LintSeverity::Error => errors += 1,
            LintSeverity::Warning => warnings += 1,
            LintSeverity::Note => {}
        }
        println!(
            "  {} {} [{}]",
            finding.severity.label(),
            finding.one_line(),
            finding.code
        );
        if let Some(fix) = &finding.fix {
            println!("       {fix}");
        }
    }
    (errors, warnings)
}

/// The command core. `config` is the user's configuration when it could be
/// loaded, and is only used to answer "can this install reach the providers
/// this blueprint names" - a config that will not load is not a reason to
/// refuse to lint, it only means that one check has nothing to say. Taking it
/// as an argument keeps this function hermetic; the real
/// [`Config::load`](crate::config::Config::load) happens in [`execute`].
fn execute_reporting_outcome(
    args: &ValidateArgs,
    config: Option<&crate::config::Config>,
) -> anyhow::Result<ValidateOutcome> {
    let path = PathBuf::from(&args.path);

    let checked = match check_manifest(&path) {
        Ok(c) => c,
        Err(ManifestCheckError::Io(e)) => return Err(e),
        Err(ManifestCheckError::Parse(e)) => return Ok(ValidateOutcome::ParseError(e)),
        Err(ManifestCheckError::Validation(e)) => return Ok(ValidateOutcome::ValidationError(e)),
    };

    print_success(&checked.blueprint);
    print_script_tool_report(&path);

    let mut env = LintEnv::offline(&checked.agent_dir);
    if let Some(config) = config {
        env = env.with_providers(&checked.blueprint, config);
    }
    let findings = lint_manifest(&checked.content, &checked.blueprint, &env);
    let (errors, warnings) = print_findings(&findings);

    if errors > 0 || (args.deny_warnings && warnings > 0) {
        return Ok(ValidateOutcome::LintFailed { errors, warnings });
    }
    Ok(ValidateOutcome::Success)
}

/// The failure line for a lint that came back fatal. Split out so its
/// pluralization is assertable without capturing stdout.
fn lint_failure_message(errors: usize, warnings: usize, deny_warnings: bool) -> String {
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(format!("{errors} error{}", plural(errors)));
    }
    if deny_warnings && warnings > 0 {
        parts.push(format!(
            "{warnings} warning{} (--deny-warnings)",
            plural(warnings)
        ));
    }
    format!("✗ Blueprint has {}", parts.join(" and "))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
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

pub async fn execute(args: ValidateArgs) -> anyhow::Result<()> {
    let config = crate::config::Config::load().ok();
    match execute_reporting_outcome(&args, config.as_ref())? {
        ValidateOutcome::Success => Ok(()),
        ValidateOutcome::ParseError(e) => anyhow::bail!("✗ Parse error: {}", e),
        ValidateOutcome::ValidationError(e) => anyhow::bail!("✗ Validation failed: {}", e),
        ValidateOutcome::LintFailed { errors, warnings } => {
            anyhow::bail!(lint_failure_message(errors, warnings, args.deny_warnings))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;

    /// A minimal manifest that lints clean, so a test can add exactly the one
    /// defect it is about.
    ///
    /// Ollama is last in the models list because it registers with no
    /// credential: under the isolated config these tests run against, a
    /// blueprint naming only keyed providers would (correctly) warn that
    /// nothing in its list is reachable.
    const CLEAN_MANIFEST: &str = r#"
[agent]
name = "ok-agent"
version = "0.1.0"
description = "Valid"

[stages.main]
mode = "autonomous"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }, { provider = "ollama", model = "qwen3.5:9b" }] }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;

    fn write_manifest(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("agent.leviath");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn args_for(dir: &std::path::Path) -> ValidateArgs {
        ValidateArgs {
            path: dir.to_str().unwrap().to_string(),
            deny_warnings: false,
        }
    }

    // ─── print_success ───────────────────────────────────────────────────

    fn parse(toml: &str) -> leviath_core::Blueprint {
        leviath_core::manifest::parse_manifest(toml).unwrap()
    }

    /// Helper to create a minimal valid blueprint TOML with given stages.
    fn make_blueprint_toml(stages_toml: &str) -> String {
        format!(
            r#"
[agent]
name = "test"
version = "0.1.0"
description = "test blueprint"

{stages_toml}

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}
"#
        )
    }

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
        print_success(&parse(&toml));
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
        // Exercises: graph mode header, an edge with a target ("-> b"), and
        // stage "b" which has transitions = None ("(linear)" branch) as well
        // as the max_revisits formatting on stage "a".
        print_success(&parse(&toml));
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

    // ─── print_findings ──────────────────────────────────────────────────

    /// One finding of each severity: the counts returned are errors and
    /// warnings only, because a note must never fail anything.
    #[test]
    fn print_findings_counts_errors_and_warnings_but_not_notes() {
        let findings = [
            (LintSeverity::Error, "e"),
            (LintSeverity::Error, "e2"),
            (LintSeverity::Warning, "w"),
            (LintSeverity::Note, "n"),
        ]
        .map(|(severity, code)| LintFinding {
            severity,
            code,
            stage: Some("main".to_string()),
            message: "something".to_string(),
            // Alternating so both the with-fix and without-fix print arms run.
            fix: (code == "e").then(|| "do the thing".to_string()),
        });
        assert_eq!(print_findings(&findings), (2, 1));
    }

    #[test]
    fn print_findings_on_an_empty_list_reports_nothing() {
        assert_eq!(print_findings(&[]), (0, 0));
    }

    // ─── lint_failure_message ────────────────────────────────────────────

    #[test]
    fn lint_failure_message_pluralizes_and_names_the_flag() {
        assert_eq!(lint_failure_message(1, 0, false), "✗ Blueprint has 1 error");
        assert_eq!(
            lint_failure_message(2, 5, false),
            "✗ Blueprint has 2 errors",
            "warnings are not counted unless they were asked to be"
        );
        assert_eq!(
            lint_failure_message(0, 1, true),
            "✗ Blueprint has 1 warning (--deny-warnings)"
        );
        assert_eq!(
            lint_failure_message(1, 2, true),
            "✗ Blueprint has 1 error and 2 warnings (--deny-warnings)"
        );
    }

    // ─── execute ─────────────────────────────────────────────────────────
    //
    // `execute` loads the real config, so each of these runs inside
    // `with_isolated_config_path_async`: it points the load at a scratch
    // directory and takes the same process-wide lock every other env-touching
    // test holds.

    #[tokio::test]
    async fn execute_parse_error_returns_error() {
        crate::config::with_isolated_config_path_async("validate-parse-error", |_| async {
            let dir = tempfile::tempdir().unwrap();
            write_manifest(dir.path(), "not valid toml [[[");
            let err = execute(args_for(dir.path())).await.unwrap_err();
            assert!(err.to_string().contains("Parse error"));
        })
        .await;
    }

    #[tokio::test]
    async fn execute_validation_error_returns_error() {
        crate::config::with_isolated_config_path_async("validate-validation-error", |_| async {
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
            let err = execute(args_for(dir.path())).await.unwrap_err();
            assert!(err.to_string().contains("Validation failed"));
        })
        .await;
    }

    /// A tool name matching nothing is fatal, and the failure line says so.
    #[tokio::test]
    async fn execute_lint_error_fails_the_command() {
        crate::config::with_isolated_config_path_async("validate-lint-error", |_| async {
            let dir = tempfile::tempdir().unwrap();
            write_manifest(
                dir.path(),
                &CLEAN_MANIFEST.replace(
                    "max_iterations = 5",
                    "max_iterations = 5\navailable_tools = [\"raed_file\"]",
                ),
            );
            let err = execute(args_for(dir.path())).await.unwrap_err();
            assert_eq!(err.to_string(), "✗ Blueprint has 1 error");
        })
        .await;
    }

    /// A warning alone exits zero, and the same manifest fails under
    /// `--deny-warnings`. Asserted as a pair, since the whole point of the flag
    /// is the difference between the two.
    #[tokio::test]
    async fn warnings_only_fail_when_denied() {
        crate::config::with_isolated_config_path_async("validate-deny-warnings", |_| async {
            let dir = tempfile::tempdir().unwrap();
            // No max_iterations on the one stage: exactly one warning, no errors.
            write_manifest(
                dir.path(),
                &CLEAN_MANIFEST.replace("max_iterations = 5", ""),
            );

            let mut args = args_for(dir.path());
            assert!(execute_reporting_outcome(&args, None).unwrap().is_success());

            args.deny_warnings = true;
            let err = execute(args).await.unwrap_err();
            assert_eq!(
                err.to_string(),
                "✗ Blueprint has 1 warning (--deny-warnings)"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn execute_no_manifest_errors() {
        crate::config::with_isolated_config_path_async("validate-no-manifest", |_| async {
            let dir = tempfile::tempdir().unwrap();
            assert!(execute(args_for(dir.path())).await.is_err());
        })
        .await;
    }

    /// The manifest may be named directly rather than by its directory.
    #[tokio::test]
    async fn execute_valid_manifest_file_path() {
        crate::config::with_isolated_config_path_async("validate-file-path", |_| async {
            let dir = tempfile::tempdir().unwrap();
            let manifest_path = write_manifest(dir.path(), CLEAN_MANIFEST);
            let args = ValidateArgs {
                path: manifest_path.to_str().unwrap().to_string(),
                deny_warnings: false,
            };
            assert!(execute(args).await.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn execute_valid_manifest_directory_path() {
        crate::config::with_isolated_config_path_async("validate-dir-path", |_| async {
            let dir = tempfile::tempdir().unwrap();
            write_test_agent(dir.path(), CLEAN_MANIFEST);
            assert!(execute(args_for(dir.path())).await.is_ok());
        })
        .await;
    }

    // ─── execute_reporting_outcome ───────────────────────────────────────

    impl ValidateOutcome {
        /// Whether this is [`ValidateOutcome::Success`]. A method rather than a
        /// `matches!` in each test: the never-taken arm of an inline `matches!`
        /// reads to llvm-cov as an uncovered region.
        fn is_success(&self) -> bool {
            matches!(self, Self::Success)
        }

        fn is_parse_error(&self) -> bool {
            matches!(self, Self::ParseError(_))
        }

        fn is_validation_error(&self) -> bool {
            matches!(self, Self::ValidationError(_))
        }
    }

    #[test]
    fn outcome_predicates_distinguish_the_variants() {
        assert!(ValidateOutcome::Success.is_success());
        assert!(!ValidateOutcome::Success.is_parse_error());
        assert!(!ValidateOutcome::Success.is_validation_error());
        assert!(ValidateOutcome::ParseError(String::new()).is_parse_error());
        assert!(ValidateOutcome::ValidationError(String::new()).is_validation_error());
        assert!(
            !ValidateOutcome::LintFailed {
                errors: 1,
                warnings: 0
            }
            .is_success()
        );
    }

    #[test]
    fn execute_reporting_outcome_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None)
                .unwrap()
                .is_parse_error()
        );
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
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None)
                .unwrap()
                .is_validation_error()
        );
    }

    #[test]
    fn execute_reporting_outcome_missing_manifest_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(execute_reporting_outcome(&args_for(dir.path()), None).is_err());
    }

    #[test]
    fn execute_reporting_outcome_valid_manifest_is_success() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), CLEAN_MANIFEST);
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None)
                .unwrap()
                .is_success()
        );
    }

    /// A blueprint whose regions run shell commands at spawn: the note lands in
    /// the findings, and does not fail the command.
    #[test]
    fn command_seed_regions_are_noted_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "scanner"
version = "0.1.0"

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-5" }
description = "Main stage"
max_iterations = 5

[context.regions]
facts = { kind = "pinned", max_tokens = 1000, seed = { command = "git ls-files" } }
conversation = { kind = "sliding_window", max_items = 50, max_tokens = 10000 }
"#;
        write_manifest(dir.path(), manifest);
        // Even under --deny-warnings, a note is not a warning.
        let args = ValidateArgs {
            path: dir.path().to_str().unwrap().to_string(),
            deny_warnings: true,
        };
        assert!(execute_reporting_outcome(&args, None).unwrap().is_success());
    }

    #[test]
    fn execute_reporting_outcome_reports_agent_script_tools() {
        // A valid agent whose `tools/` dir holds one good and one broken script:
        // validation still succeeds, and the script report's count + warning
        // branches both run.
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), CLEAN_MANIFEST);
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("ok.rhai"), "// @tool ok\nparams.x").unwrap();
        std::fs::write(tools.join("bad.rhai"), "no directive\nlet").unwrap();
        // Compiles but requires an unsatisfiable capability → the won't-load warning.
        std::fs::write(tools.join("gpu.rhai"), "// @tool gpu\n// @requires gpu\n1").unwrap();
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None)
                .unwrap()
                .is_success()
        );
    }

    /// A tool the agent defines itself resolves, so granting it is not an
    /// unknown-tool error. This is the reason the lint env is built from the
    /// agent's own directory rather than from the built-ins alone.
    #[test]
    fn an_agents_own_script_tool_resolves() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(
            dir.path(),
            &CLEAN_MANIFEST.replace(
                "max_iterations = 5",
                "max_iterations = 5\navailable_tools = [\"stub_search\"]",
            ),
        );
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(
            tools.join("stub_search.rhai"),
            "// @tool stub_search\n// @description searches\n\"found\"",
        )
        .unwrap();
        assert!(
            execute_reporting_outcome(&args_for(dir.path()), None)
                .unwrap()
                .is_success()
        );
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

    // ─── check_manifest ──────────────────────────────────────────────────

    #[test]
    fn check_manifest_verifies_custom_region_scripts() {
        // A custom region's script must exist and compile; the same failure a
        // spawn would hit, surfaced by `lev validate`.
        let dir = tempfile::tempdir().unwrap();
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
        let manifest_path = write_manifest(dir.path(), toml);

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
        let checked = check_manifest(&manifest_path).unwrap();
        assert_eq!(checked.blueprint.name, "custom-validate");
        // The text is carried through for the linter, and the agent dir points
        // at the manifest's own directory rather than the manifest file.
        assert!(checked.content.contains("custom-validate"));
        assert_eq!(checked.agent_dir, dir.path());
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

        let err = check_manifest(dir.path()).unwrap_err();
        let e = unwrap_io_err(err);
        assert!(e.to_string().contains("Failed to read"));
    }

    impl ManifestCheckError {
        /// Whether this is a parse failure. A method rather than an inline
        /// `matches!` in the test: the arm the passing run does not take reads
        /// to llvm-cov as an uncovered region, and so does a `{err:?}` argument
        /// that only a failing assertion would format.
        fn is_parse(&self) -> bool {
            matches!(self, Self::Parse(_))
        }
    }

    #[test]
    fn check_manifest_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "not valid toml [[[");
        assert!(check_manifest(dir.path()).unwrap_err().is_parse());
        // And the other arm: a missing manifest is an I/O failure, not a parse
        // one, so the predicate is deciding rather than always agreeing.
        let empty = tempfile::tempdir().unwrap();
        assert!(!check_manifest(empty.path()).unwrap_err().is_parse());
    }

    #[test]
    fn check_manifest_direct_file_path_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = write_manifest(dir.path(), CLEAN_MANIFEST);
        // Pass the *file* path directly, not the directory.
        let checked = check_manifest(&manifest_path).unwrap();
        assert_eq!(checked.blueprint.name, "ok-agent");
    }
}
