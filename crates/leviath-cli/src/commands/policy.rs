//! Policy management commands: list, add, test.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct PolicyArgs {
    #[command(subcommand)]
    pub command: PolicyCommand,
}

#[derive(Subcommand)]
pub enum PolicyCommand {
    /// List current policy rules (static + scripted)
    List(PolicyListArgs),
    /// Add a new allowlist rule interactively
    Add(PolicyAddArgs),
    /// Test whether a tool would be gated under current policy
    Test(PolicyTestArgs),
}

#[derive(Args)]
pub struct PolicyListArgs {}

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

pub async fn execute(args: PolicyArgs) -> anyhow::Result<()> {
    match args.command {
        PolicyCommand::List(_) => execute_list().await,
        PolicyCommand::Add(args) => execute_add(args).await,
        PolicyCommand::Test(args) => execute_test(args).await,
    }
}

/// Load the policy config from the default path.
fn load_policy() -> anyhow::Result<leviath_core::PolicyConfig> {
    let policy_path = policy_path();
    if policy_path.exists() {
        let content = std::fs::read_to_string(&policy_path)?;
        leviath_core::PolicyConfig::from_toml(&content).map_err(|e| anyhow::anyhow!("{}", e))
    } else {
        Ok(leviath_core::PolicyConfig::default())
    }
}

/// Get the default policy file path.
fn policy_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".config"))
        .join("leviath")
        .join("policy.toml")
}

/// List the scripted rules directory.
fn rules_dir() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".config"))
        .join("leviath")
        .join("rules")
}

async fn execute_list() -> anyhow::Result<()> {
    execute_list_with(&load_policy()?, &rules_dir())
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
    execute_add_with(args, &policy_path(), &load_policy()?)
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

    // Serialize and write
    let toml_str =
        toml::to_string_pretty(&config).map_err(|e| anyhow::anyhow!("TOML error: {}", e))?;
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

    let config = load_policy()?;
    let classification = leviath_core::taint::builtin_tool_classification(&args.tool);

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

    if !classification.is_outbound() {
        println!();
        println!("Result: ALLOWED (tool is not outbound — no gate check needed)");
        return Ok(());
    }

    if classification.check_clearance(taint) {
        println!();
        println!(
            "Result: ALLOWED (taint {} ≤ clearance {})",
            taint, classification.clearance
        );
        return Ok(());
    }

    // Would be blocked — check allowlist
    println!();
    println!(
        "Gate would fire: taint {} > clearance {}",
        taint, classification.clearance
    );

    if let Some(rule_idx) = config.check_allowlist(&args.tool, args.target.as_deref(), taint) {
        println!("Result: ALLOWED by allowlist rule #{}", rule_idx + 1);
    } else {
        println!("Result: BLOCKED (no matching allowlist rule)");
        println!("  The user would be prompted to allow/deny.");
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
    fn load_policy_returns_default_when_no_file() {
        let config = load_policy().unwrap();
        assert!(config.allowlist.is_empty());
    }

    #[tokio::test]
    async fn execute_list_succeeds() {
        // Just verify it doesn't panic
        let result = execute_list().await;
        assert!(result.is_ok());
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
    fn policy_add_args_build_rule_without_target() {
        let args = PolicyAddArgs {
            tool: "shell".to_string(),
            target: None,
            max_sensitivity: "internal".to_string(),
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
        assert!(rule.to.is_empty());
        assert_eq!(rule.max_sensitivity, leviath_core::TaintLevel::Internal);
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
}
