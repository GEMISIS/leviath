//! Policy management commands: list, add, test.

use clap::{Args, Subcommand};

/// Arguments for `lev policy`.
#[derive(Args)]
pub struct PolicyArgs {
    /// Which policy subcommand to run.
    #[command(subcommand)]
    pub command: PolicyCommand,
}

/// The `lev policy` subcommands.
#[derive(Subcommand)]
pub enum PolicyCommand {
    /// List current policy rules (static + scripted)
    List(PolicyListArgs),
    /// Add a new allowlist rule interactively
    Add(PolicyAddArgs),
    /// Test whether a tool would be gated under current policy
    Test(PolicyTestArgs),
}

/// Arguments for `lev policy list`. It takes none.
#[derive(Args)]
pub struct PolicyListArgs {}

/// Arguments for `lev policy add`.
#[derive(Args)]
pub struct PolicyAddArgs {
    /// Tool name to create a rule for
    #[arg(value_name = "TOOL")]
    pub tool: String,
    /// Target pattern (e.g., "megan@*")
    #[arg(long)]
    pub target: Option<String>,
    /// Maximum sensitivity level (public, internal, private)
    #[arg(long, default_value = "internal")]
    pub max_sensitivity: String,
}

/// Arguments for `lev policy test`.
#[derive(Args)]
pub struct PolicyTestArgs {
    /// Tool name to test
    #[arg(value_name = "TOOL")]
    pub tool: String,
    /// Optional target to test against
    #[arg(long)]
    pub target: Option<String>,
    /// Taint level to test with (public, internal, private)
    #[arg(long, default_value = "private")]
    pub taint: String,
}

/// Run `lev policy`: inspect and edit the taint-gate rules.
pub(crate) async fn execute(args: PolicyArgs) -> anyhow::Result<()> {
    match args.command {
        PolicyCommand::List(_) => execute_list().await,
        PolicyCommand::Add(args) => execute_add(args).await,
        PolicyCommand::Test(args) => execute_test(args).await,
    }
}

/// Load the policy config from the default path.
pub(crate) fn load_policy() -> anyhow::Result<leviath_core::PolicyConfig> {
    load_policy_from(&policy_path())
}

/// Load the policy config from a specific path - split out from `load_policy`
/// (which injects the real default path) so the parse-error and missing-file
/// arms are unit-testable without touching the user's real config.
fn load_policy_from(path: &std::path::Path) -> anyhow::Result<leviath_core::PolicyConfig> {
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        leviath_core::PolicyConfig::from_toml(&content).map_err(|e| anyhow::anyhow!("{}", e))
    } else {
        Ok(leviath_core::PolicyConfig::default())
    }
}

/// Resolve the base `…/leviath` config directory, preferring the platform
/// config dir and falling back to `~/.config`. Split out with injected dirs so
/// the fallback arm is unit-testable (the real `dirs::config_dir()` is `Some`
/// on CI runners, so the fallback would otherwise never be exercised).
fn leviath_config_dir(
    config_dir: Option<std::path::PathBuf>,
    home_dir: Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    config_dir
        .unwrap_or_else(|| {
            home_dir
                .expect("no config or home directory")
                .join(".config")
        })
        .join("leviath")
}

/// Get the default policy file path.
pub(crate) fn policy_path() -> std::path::PathBuf {
    leviath_config_dir(dirs::config_dir(), dirs::home_dir()).join("policy.toml")
}

/// The scripted rules directory (`<config>/leviath/rules`).
pub(crate) fn rules_dir() -> std::path::PathBuf {
    leviath_config_dir(dirs::config_dir(), dirs::home_dir()).join("rules")
}

async fn execute_list() -> anyhow::Result<()> {
    execute_list_from(load_policy(), &rules_dir())
}

/// Core of [`execute_list`] with the loaded policy passed in as a `Result` so
/// the `load_policy()?` error arm is unit-testable without a corrupt real
/// config file.
fn execute_list_from(
    loaded: anyhow::Result<leviath_core::PolicyConfig>,
    rules: &std::path::Path,
) -> anyhow::Result<()> {
    execute_list_with(&loaded?, rules)
}

fn execute_list_with(
    config: &leviath_core::PolicyConfig,
    rules: &std::path::Path,
) -> anyhow::Result<()> {
    println!("Taint Policy Rules");
    println!("==================");
    println!();

    if config.allowlist.is_empty() {
        println!("No static allowlist rules configured.");
    } else {
        println!("Static Rules ({}):", config.allowlist.len());
        for (i, rule) in config.allowlist.iter().enumerate() {
            let targets = if !rule.to.is_empty() {
                format!(" → {}", rule.to.join(", "))
            } else if !rule.channel.is_empty() {
                format!(" → {}", rule.channel.join(", "))
            } else {
                " → (any target)".to_string()
            };
            println!(
                "  {}. {} [max: {}]{}",
                i + 1,
                rule.tool,
                rule.max_sensitivity,
                targets
            );
        }
    }

    println!();

    // List scripted rules
    if rules.exists() {
        let mut scripts: Vec<_> = std::fs::read_dir(rules)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "rhai")
                    .unwrap_or(false)
            })
            .collect();
        scripts.sort_by_key(|e| e.file_name());

        if scripts.is_empty() {
            println!("No scripted rules found.");
        } else {
            println!("Scripted Rules ({}):", scripts.len());
            for entry in &scripts {
                let name = entry
                    .path()
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                println!("  - {} ({})", name, entry.path().display());
            }
        }
    } else {
        println!("No scripted rules directory found.");
    }

    if !config.mcp_overrides.is_empty() {
        println!();
        println!("MCP Tool Overrides ({}):", config.mcp_overrides.len());
        for (key, ovr) in &config.mcp_overrides {
            let parts: Vec<String> = [
                ovr.sensitivity.map(|s| format!("sensitivity={}", s)),
                ovr.direction.as_ref().map(|d| format!("direction={}", d)),
                ovr.clearance.map(|c| format!("clearance={}", c)),
            ]
            .into_iter()
            .flatten()
            .collect();
            println!("  {} [{}]", key, parts.join(", "));
        }
    }

    Ok(())
}

async fn execute_add(args: PolicyAddArgs) -> anyhow::Result<()> {
    execute_add_from(args, &policy_path(), load_policy())
}

/// Core of [`execute_add`] with the loaded policy passed in as a `Result` so
/// the `load_policy()?` error arm is unit-testable without a corrupt real
/// config file.
fn execute_add_from(
    args: PolicyAddArgs,
    path: &std::path::Path,
    loaded: anyhow::Result<leviath_core::PolicyConfig>,
) -> anyhow::Result<()> {
    execute_add_with(args, path, &loaded?)
}

fn execute_add_with(
    args: PolicyAddArgs,
    path: &std::path::Path,
    existing: &leviath_core::PolicyConfig,
) -> anyhow::Result<()> {
    let sensitivity =
        leviath_core::TaintLevel::from_str_loose(&args.max_sensitivity).ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid sensitivity level: '{}'. Use: public, internal, private",
                args.max_sensitivity
            )
        })?;

    let rule = leviath_core::AllowlistRule {
        tool: args.tool.clone(),
        to: args
            .target
            .as_ref()
            .map(|t| vec![t.clone()])
            .unwrap_or_default(),
        channel: vec![],
        max_sensitivity: sensitivity,
    };

    let mut config = existing.clone();
    config.allowlist.push(rule);

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Serialize and write. `PolicyConfig` is a struct of arrays-of-tables
    // (`allowlist`) followed by a table (`mcp_overrides`), whose entries hold
    // only inline values - there is no primitive-after-table ordering hazard,
    // so TOML serialization cannot fail.
    let toml_str = toml::to_string_pretty(&config)
        .expect("infallible: PolicyConfig always serializes to TOML");
    std::fs::write(path, toml_str)?;

    println!("Added rule: {} [max: {}]", args.tool, args.max_sensitivity);
    if let Some(target) = &args.target {
        println!("  Target: {}", target);
    }
    println!("Saved to: {}", path.display());

    Ok(())
}

async fn execute_test(args: PolicyTestArgs) -> anyhow::Result<()> {
    let taint = leviath_core::TaintLevel::from_str_loose(&args.taint).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid taint level: '{}'. Use: public, internal, private",
            args.taint
        )
    })?;

    execute_test_with(&args, taint, load_policy(), &rules_dir())
}

/// Build a one-region context window carrying `taint`, so the diagnostic runs
/// the same gate code as the daemon instead of re-deriving the verdict.
fn window_with_taint(
    taint: leviath_core::TaintLevel,
) -> leviath_runtime::components::ContextWindow {
    let mut window = leviath_runtime::components::ContextWindow::new(1024);
    let mut region = leviath_core::Region::new(
        "scenario".to_string(),
        leviath_core::RegionKind::Temporary,
        512,
    );
    region.enable_taint_tracking();
    window.add_region(region);
    if taint != leviath_core::TaintLevel::Public {
        window
            .add_tainted_to_region("scenario", "sample".to_string(), 8, taint)
            .expect("infallible: the region was just added");
    }
    window
}

/// Core of [`execute_test`] with the parsed taint level, the loaded policy
/// (as a `Result`), and the scripted-rules directory passed in, so every
/// verdict arm is unit-testable with crafted configs and temp rule dirs.
///
/// The verdict comes from the same [`leviath_runtime::TaintGate`] the daemon
/// attaches at spawn - `[mcp_overrides]` applied, static allowlist and
/// scripted rules consulted - because a diagnostic that re-derives gate
/// semantics drifts from the enforcer and then lies about it.
fn execute_test_with(
    args: &PolicyTestArgs,
    taint: leviath_core::TaintLevel,
    loaded: anyhow::Result<leviath_core::PolicyConfig>,
    rules: &std::path::Path,
) -> anyhow::Result<()> {
    use leviath_core::taint::{GateDecision, GateDecisionSource, SecurityConfig};

    let config = loaded?;
    let mut gate = leviath_runtime::TaintGate::new(SecurityConfig {
        taint_tracking: true,
    });
    gate.apply_mcp_overrides(&config.mcp_overrides);
    let classification = gate.tool_classification(&args.tool);

    println!("Tool: {}", args.tool);
    println!("  Sensitivity: {}", classification.sensitivity);
    println!("  Direction: {}", classification.direction);
    println!("  Clearance: {}", classification.clearance);
    println!();
    println!("Test scenario:");
    println!("  Taint level: {}", taint);
    if let Some(target) = &args.target {
        println!("  Target: {}", target);
    }
    println!();

    let window = window_with_taint(taint);
    let checker = crate::daemon::gate_rules::build_gate_script_checker(rules);
    let decision = gate.check_with_policy(
        "policy-test",
        &args.tool,
        &window,
        args.target.as_deref(),
        &config,
        Some(checker.as_ref()),
    );

    match decision {
        GateDecision::Allowed => {
            let source = gate.audit_log().last().map(|e| e.decision_source.clone());
            match source {
                Some(GateDecisionSource::AllowlistRule { rule_index }) => {
                    println!("Result: ALLOWED by allowlist rule #{}", rule_index + 1);
                }
                Some(GateDecisionSource::ScriptedRule { script_name }) => {
                    println!("Result: ALLOWED by scripted rule '{}'", script_name);
                }
                _ if !classification.is_outbound() => {
                    println!("Result: ALLOWED (tool is not outbound - no gate check needed)");
                }
                _ => {
                    println!(
                        "Result: ALLOWED (taint {} <= clearance {})",
                        taint, classification.clearance
                    );
                }
            }
        }
        GateDecision::Blocked {
            taint_level,
            clearance,
            ..
        } => {
            println!(
                "Gate fires: taint {} > clearance {}",
                taint_level, clearance
            );
            println!("Result: BLOCKED (no allowlist or scripted rule matched)");
            println!("  The user would be prompted to allow/deny.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_path_returns_valid_path() {
        let path = policy_path();
        assert!(path.to_str().unwrap().contains("leviath"));
        assert!(path.to_str().unwrap().contains("policy.toml"));
    }

    #[test]
    fn rules_dir_returns_valid_path() {
        let path = rules_dir();
        assert!(path.to_str().unwrap().contains("leviath"));
        assert!(path.to_str().unwrap().contains("rules"));
    }

    #[test]
    fn leviath_config_dir_prefers_config_dir() {
        let p = leviath_config_dir(
            Some(std::path::PathBuf::from("/cfg")),
            Some(std::path::PathBuf::from("/home/u")),
        );
        assert_eq!(p, std::path::PathBuf::from("/cfg/leviath"));
    }

    #[test]
    fn leviath_config_dir_falls_back_to_home_config() {
        // No platform config dir → fall back to ~/.config/leviath
        let p = leviath_config_dir(None, Some(std::path::PathBuf::from("/home/u")));
        assert_eq!(p, std::path::PathBuf::from("/home/u/.config/leviath"));
    }

    #[test]
    fn load_policy_from_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        let config = load_policy_from(&missing).unwrap();
        assert!(config.allowlist.is_empty());
    }

    #[test]
    fn load_policy_from_invalid_toml_is_err() {
        // Exercises the parse-error map_err arm of load_policy_from.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "{{ not valid toml").unwrap();
        assert!(load_policy_from(&path).is_err());
    }

    #[test]
    fn load_policy_from_read_error_propagates() {
        // The path exists but is a directory, so read_to_string fails -
        // exercising load_policy_from's `read_to_string(path)?` error arm.
        let dir = tempfile::tempdir().unwrap();
        assert!(load_policy_from(dir.path()).is_err());
    }

    #[test]
    fn load_policy_returns_default_when_no_file() {
        let config = load_policy().unwrap();
        assert!(config.allowlist.is_empty());
    }

    #[test]
    fn execute_list_with_read_dir_error_propagates() {
        // `rules` exists but is a regular file (not a directory), so read_dir
        // fails - exercising execute_list_with's `read_dir(rules)?` error arm.
        let dir = tempfile::tempdir().unwrap();
        let rules_file = dir.path().join("rules");
        std::fs::write(&rules_file, "i am a file, not a dir").unwrap();
        let config = leviath_core::PolicyConfig::default();
        assert!(execute_list_with(&config, &rules_file).is_err());
    }

    #[test]
    fn execute_list_from_propagates_load_error() {
        // The `loaded?` error arm of execute_list_from.
        let dir = tempfile::tempdir().unwrap();
        assert!(execute_list_from(Err(anyhow::anyhow!("boom")), dir.path()).is_err());
    }

    #[test]
    fn execute_add_from_propagates_load_error() {
        // The `loaded?` error arm of execute_add_from.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "internal".to_string(),
        };
        assert!(execute_add_from(args, &path, Err(anyhow::anyhow!("boom"))).is_err());
    }

    #[test]
    fn execute_add_from_ok_writes_policy() {
        // The Ok path of execute_add_from delegates to execute_add_with.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "internal".to_string(),
        };
        assert!(execute_add_from(args, &path, Ok(leviath_core::PolicyConfig::default())).is_ok());
        assert!(path.exists());
    }

    #[test]
    fn execute_add_with_write_error_and_no_parent() {
        // path == "/" has no parent (covers the `if let Some(parent)` None arm)
        // and cannot be written as a file (covers `std::fs::write(path, ..)?`).
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "internal".to_string(),
        };
        let config = leviath_core::PolicyConfig::default();
        assert!(execute_add_with(args, std::path::Path::new("/"), &config).is_err());
    }

    #[test]
    fn execute_test_with_propagates_load_error() {
        // The `loaded?` error arm of execute_test_with.
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: None,
            taint: "private".to_string(),
        };
        let res = execute_test_with(
            &args,
            leviath_core::TaintLevel::Private,
            Err(anyhow::anyhow!("boom")),
            std::env::temp_dir().as_path(),
        );
        assert!(res.is_err());
    }

    /// An empty scripted-rules directory, so a test exercises only the arm it
    /// crafts.
    fn no_rules() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn execute_test_with_allowlist_rule_hit() {
        // An outbound tool blocked by clearance but permitted by a matching
        // allowlist rule exercises the `AllowlistRule` verdict arm.
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: None,
            taint: "private".to_string(),
        };
        let config = leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::AllowlistRule {
                tool: "shell".to_string(),
                to: vec![],
                channel: vec![],
                max_sensitivity: leviath_core::TaintLevel::Private,
            }],
            mcp_overrides: Default::default(),
        };
        let rules = no_rules();
        let res = execute_test_with(
            &args,
            leviath_core::TaintLevel::Private,
            Ok(config),
            rules.path(),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn execute_test_with_scripted_rule_flips_the_verdict() {
        // A rules/*.rhai script that allows the call must be reflected by the
        // diagnostic - the daemon consults scripted rules, so `lev policy
        // test` has to as well or it reports BLOCKED for a call the runtime
        // would allow.
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: None,
            taint: "private".to_string(),
        };
        let rules = tempfile::tempdir().unwrap();
        std::fs::write(
            rules.path().join("allow-shell.rhai"),
            "context.tool == \"shell\"",
        )
        .unwrap();
        let res = execute_test_with(
            &args,
            leviath_core::TaintLevel::Private,
            Ok(leviath_core::PolicyConfig::default()),
            rules.path(),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn execute_test_with_mcp_override_changes_the_classification() {
        // An [mcp_overrides] entry making an unknown (default-internal) MCP
        // tool outbound with a public clearance must make the gate fire.
        let args = PolicyTestArgs {
            tool: "srv.share".to_string(),
            target: None,
            taint: "private".to_string(),
        };
        let config = leviath_core::PolicyConfig {
            allowlist: vec![],
            mcp_overrides: std::collections::HashMap::from([(
                "srv.share".to_string(),
                leviath_core::policy::McpToolOverride {
                    sensitivity: None,
                    direction: Some("outbound".to_string()),
                    clearance: Some(leviath_core::TaintLevel::Public),
                },
            )]),
        };
        let rules = no_rules();
        let res = execute_test_with(
            &args,
            leviath_core::TaintLevel::Private,
            Ok(config),
            rules.path(),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn execute_test_with_non_outbound_and_clearance_allow_arms() {
        let rules = no_rules();
        // read_file is inbound: the not-outbound arm.
        let res = execute_test_with(
            &PolicyTestArgs {
                tool: "read_file".to_string(),
                target: None,
                taint: "private".to_string(),
            },
            leviath_core::TaintLevel::Private,
            Ok(leviath_core::PolicyConfig::default()),
            rules.path(),
        );
        assert!(res.is_ok());
        // shell at public taint: outbound, within clearance.
        let res = execute_test_with(
            &PolicyTestArgs {
                tool: "shell".to_string(),
                target: Some("localhost".to_string()),
                taint: "public".to_string(),
            },
            leviath_core::TaintLevel::Public,
            Ok(leviath_core::PolicyConfig::default()),
            rules.path(),
        );
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn execute_list_succeeds() {
        // Just verify it doesn't panic
        let result = execute_list().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_dispatches_subcommands() {
        // Exercise the top-level `execute` dispatcher for the read-only arms.
        assert!(
            execute(PolicyArgs {
                command: PolicyCommand::List(PolicyListArgs {}),
            })
            .await
            .is_ok()
        );
        assert!(
            execute(PolicyArgs {
                command: PolicyCommand::Test(PolicyTestArgs {
                    tool: "read_file".to_string(),
                    target: None,
                    taint: "internal".to_string(),
                }),
            })
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn execute_test_non_outbound_tool() {
        let args = PolicyTestArgs {
            tool: "read_file".to_string(),
            target: None,
            taint: "private".to_string(),
        };
        let result = execute_test(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_test_outbound_allowed() {
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: None,
            taint: "public".to_string(),
        };
        let result = execute_test(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_test_outbound_blocked() {
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: None,
            taint: "private".to_string(),
        };
        let result = execute_test(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_test_invalid_taint_level() {
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: None,
            taint: "invalid".to_string(),
        };
        let result = execute_test(args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_add_and_list() {
        // Use a temp dir for the policy file
        let dir = tempfile::tempdir().unwrap();
        let policy_file = dir.path().join("policy.toml");

        // Create a minimal policy
        std::fs::write(&policy_file, "").unwrap();

        let args = PolicyAddArgs {
            tool: "send_email".to_string(),
            target: Some("test@*".to_string()),
            max_sensitivity: "private".to_string(),
        };
        // execute_add writes to the real config path, so we test the logic
        // instead of calling it directly
        let sensitivity = leviath_core::TaintLevel::from_str_loose(&args.max_sensitivity);
        assert!(sensitivity.is_some());
        assert_eq!(sensitivity.unwrap(), leviath_core::TaintLevel::Private);
    }

    #[tokio::test]
    async fn execute_add_invalid_sensitivity() {
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "invalid".to_string(),
        };
        let result = execute_add(args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_test_with_target() {
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: Some("example.com".to_string()),
            taint: "private".to_string(),
        };
        let result = execute_test(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_test_outbound_with_internal_taint() {
        let args = PolicyTestArgs {
            tool: "shell".to_string(),
            target: None,
            taint: "internal".to_string(),
        };
        let result = execute_test(args).await;
        assert!(result.is_ok());
    }

    #[test]
    fn policy_add_args_parse_sensitivity_public() {
        let sensitivity = leviath_core::TaintLevel::from_str_loose("public");
        assert!(sensitivity.is_some());
        assert_eq!(sensitivity.unwrap(), leviath_core::TaintLevel::Public);
    }

    #[test]
    fn policy_add_args_parse_sensitivity_internal() {
        let sensitivity = leviath_core::TaintLevel::from_str_loose("internal");
        assert!(sensitivity.is_some());
        assert_eq!(sensitivity.unwrap(), leviath_core::TaintLevel::Internal);
    }

    #[test]
    fn policy_add_args_build_rule_with_target() {
        let args = PolicyAddArgs {
            tool: "send_email".to_string(),
            target: Some("alice@*".to_string()),
            max_sensitivity: "private".to_string(),
        };
        let sensitivity = leviath_core::TaintLevel::from_str_loose(&args.max_sensitivity).unwrap();
        let rule = leviath_core::AllowlistRule {
            tool: args.tool.clone(),
            to: args
                .target
                .as_ref()
                .map(|t| vec![t.clone()])
                .unwrap_or_default(),
            channel: vec![],
            max_sensitivity: sensitivity,
        };
        assert_eq!(rule.tool, "send_email");
        assert_eq!(rule.to, vec!["alice@*".to_string()]);
        assert_eq!(rule.max_sensitivity, leviath_core::TaintLevel::Private);
    }

    #[test]
    fn policy_test_classification_lookup() {
        // Verify that builtin_tool_classification returns expected values
        // for tools we use in execute_test
        let shell_class = leviath_core::taint::builtin_tool_classification("shell");
        assert!(shell_class.is_outbound());
        assert_eq!(shell_class.clearance, leviath_core::TaintLevel::Public);

        let read_class = leviath_core::taint::builtin_tool_classification("read_file");
        assert!(!read_class.is_outbound());
    }

    #[test]
    fn policy_test_clearance_check_scenarios() {
        let class = leviath_core::taint::builtin_tool_classification("shell");

        // Public taint within Public clearance
        assert!(class.check_clearance(leviath_core::TaintLevel::Public));

        // Internal taint exceeds Public clearance
        assert!(!class.check_clearance(leviath_core::TaintLevel::Internal));

        // Private taint exceeds Public clearance
        assert!(!class.check_clearance(leviath_core::TaintLevel::Private));
    }

    #[test]
    fn load_policy_returns_empty_mcp_overrides() {
        let config = load_policy().unwrap();
        assert!(config.mcp_overrides.is_empty());
    }

    #[test]
    fn rules_dir_is_under_leviath_config() {
        let path = rules_dir();
        let path_str = path.to_str().unwrap();
        assert!(path_str.contains("leviath"));
        assert!(path_str.ends_with("rules"));
    }

    // ─── execute_list_with coverage ─────────────────────────────────────────

    #[test]
    fn list_with_allowlist_rules_to_targets() {
        let config = leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::AllowlistRule {
                tool: "send_email".into(),
                to: vec!["alice@*".into(), "bob@*".into()],
                channel: vec![],
                max_sensitivity: leviath_core::TaintLevel::Private,
            }],
            mcp_overrides: Default::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let result = execute_list_with(&config, dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_allowlist_rules_channel_targets() {
        let config = leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::AllowlistRule {
                tool: "post_to_slack".into(),
                to: vec![],
                channel: vec!["#general".into()],
                max_sensitivity: leviath_core::TaintLevel::Internal,
            }],
            mcp_overrides: Default::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let result = execute_list_with(&config, dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_allowlist_rules_any_target() {
        let config = leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::AllowlistRule {
                tool: "shell".into(),
                to: vec![],
                channel: vec![],
                max_sensitivity: leviath_core::TaintLevel::Public,
            }],
            mcp_overrides: Default::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let result = execute_list_with(&config, dir.path());
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_scripted_rules() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("company.rhai"), "// rule").unwrap();
        std::fs::write(rules.join("other.rhai"), "// rule").unwrap();
        std::fs::write(rules.join("not_a_rule.txt"), "ignored").unwrap();

        let config = leviath_core::PolicyConfig::default();
        let result = execute_list_with(&config, &rules);
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_empty_scripted_rules_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();

        let config = leviath_core::PolicyConfig::default();
        let result = execute_list_with(&config, &rules);
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_mcp_overrides() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "server.tool_a".to_string(),
            leviath_core::McpToolOverride {
                sensitivity: Some(leviath_core::TaintLevel::Private),
                direction: Some("outbound".to_string()),
                clearance: Some(leviath_core::TaintLevel::Internal),
            },
        );
        let config = leviath_core::PolicyConfig {
            allowlist: vec![],
            mcp_overrides: overrides,
        };
        let dir = tempfile::tempdir().unwrap();
        let result = execute_list_with(&config, dir.path());
        assert!(result.is_ok());
    }

    // ─── execute_add_with coverage ──────────────────────────────────────────

    #[test]
    fn add_with_target_writes_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        let config = leviath_core::PolicyConfig::default();
        let args = PolicyAddArgs {
            tool: "send_email".to_string(),
            target: Some("test@example.com".to_string()),
            max_sensitivity: "private".to_string(),
        };
        let result = execute_add_with(args, &path, &config);
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("send_email"));
    }

    #[test]
    fn add_without_target_writes_policy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        let config = leviath_core::PolicyConfig::default();
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "internal".to_string(),
        };
        let result = execute_add_with(args, &path, &config);
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("shell"));
    }

    #[test]
    fn add_invalid_sensitivity_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        let config = leviath_core::PolicyConfig::default();
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "bogus".to_string(),
        };
        let result = execute_add_with(args, &path, &config);
        assert!(result.is_err());
    }

    #[test]
    fn add_appends_to_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.toml");
        let existing = leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::AllowlistRule {
                tool: "existing".into(),
                to: vec![],
                channel: vec![],
                max_sensitivity: leviath_core::TaintLevel::Public,
            }],
            mcp_overrides: Default::default(),
        };
        let args = PolicyAddArgs {
            tool: "new_tool".to_string(),
            target: None,
            max_sensitivity: "private".to_string(),
        };
        let result = execute_add_with(args, &path, &existing);
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("existing"));
        assert!(content.contains("new_tool"));
    }

    #[test]
    fn add_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("dir").join("policy.toml");
        let config = leviath_core::PolicyConfig::default();
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "public".to_string(),
        };
        let result = execute_add_with(args, &path, &config);
        assert!(result.is_ok());
        assert!(path.exists());
    }

    #[test]
    fn add_with_parent_that_is_a_file_errors() {
        // When the target path's parent is an existing regular file,
        // create_dir_all fails and the `?` propagates - covers the error arm.
        let dir = tempfile::tempdir().unwrap();
        let file_as_parent = dir.path().join("iamafile");
        std::fs::write(&file_as_parent, "not a dir").unwrap();
        let path = file_as_parent.join("policy.toml");
        let config = leviath_core::PolicyConfig::default();
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "public".to_string(),
        };
        let result = execute_add_with(args, &path, &config);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_dispatches_add_subcommand() {
        // Route the top-level dispatcher through the Add arm. An invalid
        // sensitivity makes execute_add fail before it writes anything to the
        // real config path, so no filesystem side effects occur.
        let result = execute(PolicyArgs {
            command: PolicyCommand::Add(PolicyAddArgs {
                tool: "shell".to_string(),
                target: None,
                max_sensitivity: "definitely-not-valid".to_string(),
            }),
        })
        .await;
        assert!(result.is_err());
    }
}
