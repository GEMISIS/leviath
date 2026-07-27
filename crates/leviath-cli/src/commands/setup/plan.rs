//! What setup decided, as plain data, and applying it.
//!
//! This is the contract between the wizard and the world, and the reason the
//! terminal UI is a *front-end* rather than the feature itself. Everything the
//! user chose lands in a [`SetupPlan`]; [`apply`] is the only thing that
//! touches disk. The `--non-interactive` flag path builds the same struct, and
//! a future mobile or web host would build it a third way with nothing
//! downstream changing.
//!
//! Keeping it separate also means the interesting logic — what actually
//! changes, and what to warn about — is testable without a terminal.

use std::path::{Path, PathBuf};

use crate::bundled::BundledAgent;
use crate::config::Config;

/// Everything `lev setup` decided to do.
pub struct SetupPlan {
    /// The config to write, fully resolved. MCP imports are already merged into
    /// its `mcp_servers`.
    pub config: Config,
    /// Blueprints to install or update.
    pub agents: Vec<&'static BundledAgent>,
}

/// What actually happened, for the closing summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub config_path: PathBuf,
    pub agents_installed: Vec<String>,
    /// Non-fatal problems worth telling the user about.
    pub warnings: Vec<String>,
}

/// Write the config and install the chosen blueprints.
///
/// Config first, and it is the only fallible-and-fatal step: a blueprint that
/// fails to install is reported as a warning rather than aborting, because a
/// written config plus nine of ten agents is a far better place to leave
/// someone than an abandoned run with nothing saved.
pub fn apply(plan: &SetupPlan, config_path: &Path, agents_dir: &Path) -> anyhow::Result<Applied> {
    plan.config.save_to_path_public(config_path)?;

    let mut agents_installed = Vec::new();
    let mut warnings = Vec::new();
    for agent in &plan.agents {
        match crate::bundled::install_bundled(agent, agents_dir) {
            Ok(()) => agents_installed.push(agent.name.to_string()),
            Err(e) => warnings.push(format!("could not install {}: {e}", agent.name)),
        }
    }

    Ok(Applied {
        config_path: config_path.to_path_buf(),
        agents_installed,
        warnings,
    })
}

/// A human-readable list of what this plan changes against `before`, for the
/// review screen. Empty means nothing would change.
///
/// Credentials are described as "set" / "changed" / "cleared" and never
/// printed — the review screen is exactly the moment a shoulder-surfer is
/// looking, and a key the user cannot read back is not a real loss when the
/// wizard just verified it works.
pub fn changes(before: &Config, plan: &SetupPlan) -> Vec<String> {
    let after = &plan.config;
    let mut out = Vec::new();

    for provider in super::catalog::providers() {
        let old = super::catalog::stored_credential(before, provider.id);
        let new = super::catalog::stored_credential(after, provider.id);
        let label = provider.display;
        match (old, new) {
            (None, Some(_)) => out.push(format!("{label}: credential set")),
            (Some(_), None) => out.push(format!("{label}: credential cleared")),
            (Some(a), Some(b)) if a != b => out.push(format!("{label}: credential changed")),
            _ => {}
        }
    }

    if before.providers.claude_code_enabled != after.providers.claude_code_enabled {
        out.push(format!(
            "Claude Code transport: {}",
            if after.providers.claude_code_enabled {
                "enabled"
            } else {
                "disabled"
            }
        ));
    }
    push_if_changed(
        &mut out,
        "default provider",
        Some(&before.default_provider),
        Some(&after.default_provider),
    );
    push_if_changed(
        &mut out,
        "default model",
        before.default_model.as_ref(),
        after.default_model.as_ref(),
    );
    push_if_changed(
        &mut out,
        "max concurrent inferences",
        before.limits.max_concurrent_inferences.as_ref(),
        after.limits.max_concurrent_inferences.as_ref(),
    );
    push_if_changed(
        &mut out,
        "max concurrent tools",
        Some(&before.limits.max_concurrent_tools),
        Some(&after.limits.max_concurrent_tools),
    );
    push_if_changed(
        &mut out,
        "default max iterations",
        before.limits.default_max_iterations.as_ref(),
        after.limits.default_max_iterations.as_ref(),
    );
    push_if_changed(
        &mut out,
        "exact token counting",
        Some(&before.limits.exact_token_counting),
        Some(&after.limits.exact_token_counting),
    );
    push_if_changed(
        &mut out,
        "batch tool hint",
        Some(&before.batch_tool_hint),
        Some(&after.batch_tool_hint),
    );

    let added = after
        .mcp_servers
        .len()
        .saturating_sub(before.mcp_servers.len());
    if added > 0 {
        out.push(format!("MCP servers: {added} imported"));
    }
    if !plan.agents.is_empty() {
        out.push(format!("agents: {} to install", plan.agents.len()));
    }
    out
}

/// Append a `field: old → new` line when the two differ.
fn push_if_changed<T: PartialEq + std::fmt::Display>(
    out: &mut Vec<String>,
    label: &str,
    before: Option<&T>,
    after: Option<&T>,
) {
    let describe = |v: Option<&T>| match v {
        Some(v) => v.to_string(),
        None => "(unset)".to_string(),
    };
    if before != after {
        out.push(format!(
            "{label}: {} → {}",
            describe(before),
            describe(after)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundled::BUNDLED_AGENTS;

    fn plan_of(config: Config) -> SetupPlan {
        SetupPlan {
            config,
            agents: Vec::new(),
        }
    }

    // ─── apply ──────────────────────────────────────────────────────────────

    #[test]
    fn apply_writes_the_config_and_installs_the_chosen_agents() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let agents_dir = dir.path().join("agents");
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("sk-ant-x".to_string());
        let plan = SetupPlan {
            config,
            agents: vec![&BUNDLED_AGENTS[0]],
        };

        let applied = apply(&plan, &config_path, &agents_dir).unwrap();

        assert_eq!(applied.config_path, config_path);
        assert_eq!(applied.agents_installed, vec![BUNDLED_AGENTS[0].name]);
        assert!(applied.warnings.is_empty());
        let written = Config::load_from_path_public(&config_path).unwrap();
        assert_eq!(
            written.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-x")
        );
        assert!(
            agents_dir
                .join(BUNDLED_AGENTS[0].name)
                .join("agent.leviath")
                .exists()
        );
    }

    #[test]
    fn apply_with_nothing_to_install_still_writes_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let applied = apply(
            &plan_of(Config::default()),
            &config_path,
            &dir.path().join("agents"),
        )
        .unwrap();

        assert!(applied.agents_installed.is_empty());
        assert!(config_path.exists());
    }

    #[test]
    fn a_blueprint_that_fails_to_install_warns_rather_than_aborting() {
        // A written config and most of the agents beats an abandoned run that
        // saved nothing.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        // `agents_dir` is a file, so every install fails.
        let agents_dir = dir.path().join("blocked");
        std::fs::write(&agents_dir, "").unwrap();
        let plan = SetupPlan {
            config: Config::default(),
            agents: vec![&BUNDLED_AGENTS[0]],
        };

        let applied = apply(&plan, &config_path, &agents_dir).unwrap();

        assert!(applied.agents_installed.is_empty());
        assert_eq!(applied.warnings.len(), 1);
        assert!(applied.warnings[0].contains(BUNDLED_AGENTS[0].name));
        assert!(config_path.exists(), "the config was still written");
    }

    #[test]
    fn a_config_that_cannot_be_written_is_a_hard_error() {
        // Nothing else in the plan matters if the config did not land.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, "").unwrap();

        let result = apply(
            &plan_of(Config::default()),
            &blocked.join("config.toml"),
            &dir.path().join("agents"),
        );

        assert!(result.is_err());
    }

    // ─── changes ────────────────────────────────────────────────────────────

    #[test]
    fn an_unchanged_plan_lists_nothing() {
        assert!(changes(&Config::default(), &plan_of(Config::default())).is_empty());
    }

    #[test]
    fn credential_changes_are_described_but_never_printed() {
        // The review screen is exactly when someone is reading over a shoulder.
        let mut before = Config::default();
        before.providers.openai_api_key = Some("sk-old-secret".to_string());
        before.openrouter_api_key = Some("sk-or-doomed".to_string());
        let mut after = before.clone();
        after.providers.anthropic_api_key = Some("sk-ant-brand-new".to_string());
        after.providers.openai_api_key = Some("sk-new-secret".to_string());
        after.openrouter_api_key = None;

        let lines = changes(&before, &plan_of(after));

        assert!(lines.contains(&"Anthropic: credential set".to_string()));
        assert!(lines.contains(&"OpenAI: credential changed".to_string()));
        assert!(lines.contains(&"OpenRouter: credential cleared".to_string()));
        for line in &lines {
            assert!(!line.contains("secret"), "a credential leaked: {line}");
            assert!(!line.contains("sk-"), "a credential leaked: {line}");
        }
    }

    #[test]
    fn an_unchanged_credential_is_not_listed() {
        let mut before = Config::default();
        before.providers.anthropic_api_key = Some("sk-ant-same".to_string());

        assert!(changes(&before, &plan_of(before.clone())).is_empty());
    }

    #[test]
    fn the_claude_code_toggle_is_reported_both_ways() {
        let before = Config::default();
        let mut on = before.clone();
        on.providers.claude_code_enabled = true;

        assert!(
            changes(&before, &plan_of(on.clone()))
                .contains(&"Claude Code transport: enabled".to_string())
        );
        assert!(
            changes(&on, &plan_of(before)).contains(&"Claude Code transport: disabled".to_string())
        );
    }

    #[test]
    fn scalar_settings_are_shown_as_old_to_new() {
        let before = Config::default();
        let mut after = before.clone();
        after.default_provider = "ollama".to_string();
        after.default_model = Some("llama3".to_string());
        after.limits.max_concurrent_inferences = Some(1);
        after.limits.max_concurrent_tools = 4;
        after.limits.default_max_iterations = None;
        after.limits.exact_token_counting = true;
        after.batch_tool_hint = false;

        let lines = changes(&before, &plan_of(after));

        assert!(lines.contains(&"default provider: anthropic → ollama".to_string()));
        assert!(lines.contains(&"default model: (unset) → llama3".to_string()));
        assert!(lines.contains(&"max concurrent inferences: 8 → 1".to_string()));
        assert!(lines.contains(&"max concurrent tools: 8 → 4".to_string()));
        assert!(lines.contains(&"default max iterations: 50 → (unset)".to_string()));
        assert!(lines.contains(&"exact token counting: false → true".to_string()));
        assert!(lines.contains(&"batch tool hint: true → false".to_string()));
    }

    #[test]
    fn imported_servers_and_pending_agents_are_counted() {
        let before = Config::default();
        let mut after = before.clone();
        after.mcp_servers = vec![
            leviath_mcp::MCPServerConfig::stdio("a", "x", vec![]),
            leviath_mcp::MCPServerConfig::stdio("b", "y", vec![]),
        ];
        let plan = SetupPlan {
            config: after,
            agents: vec![&BUNDLED_AGENTS[0], &BUNDLED_AGENTS[1]],
        };

        let lines = changes(&before, &plan);

        assert!(lines.contains(&"MCP servers: 2 imported".to_string()));
        assert!(lines.contains(&"agents: 2 to install".to_string()));
    }

    #[test]
    fn removing_servers_is_not_reported_as_an_import() {
        // `saturating_sub` must not turn a shrink into a bogus positive count.
        let before = Config {
            mcp_servers: vec![leviath_mcp::MCPServerConfig::stdio("a", "x", vec![])],
            ..Config::default()
        };
        let after = Config::default();

        let lines = changes(&before, &plan_of(after));

        assert!(
            lines.is_empty(),
            "a shrink must not be reported as an import"
        );
    }
}
