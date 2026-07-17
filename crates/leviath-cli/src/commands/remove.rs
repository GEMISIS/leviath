//! `lev remove` - Remove an installed agent

use clap::Args;

#[derive(Args)]
pub struct RemoveArgs {
    /// Name of the installed agent to remove
    #[arg(value_name = "NAME")]
    pub name: String,
}

pub async fn execute(args: RemoveArgs) -> anyhow::Result<()> {
    let installer = leviath_package::AgentInstaller::new();
    remove_agent(&installer, &args.name)
}

/// Core removal logic, parameterized by installer so it can be tested
/// against a tempdir instead of the real `~/.leviath/agents`.
fn remove_agent(installer: &leviath_package::AgentInstaller, name: &str) -> anyhow::Result<()> {
    remove_agent_with(installer, name, &|n| installer.uninstall(n))
}

/// [`remove_agent`] with an injectable uninstall operation.
///
/// The `uninstall` closure is a trait object so its failure arm can be
/// exercised on every platform: a genuinely installed agent (which
/// `get_installed` requires -- an `agent.leviath` under the agent dir) is an
/// ordinary removable directory, so `remove_dir_all` only fails on it via a
/// Unix-only `chmod` on the parent. Production always passes the real
/// `installer.uninstall`.
fn remove_agent_with(
    installer: &leviath_package::AgentInstaller,
    name: &str,
    uninstall: &dyn Fn(&str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // Verify it's actually installed first
    let installed = installer.get_installed(name).unwrap();
    if installed.is_none() {
        anyhow::bail!(
            "Agent '{}' is not installed. Use `lev list` to see installed agents.",
            name
        );
    }

    uninstall(name)?;
    println!("Removed agent '{}'.", name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_args_stores_name() {
        let args = RemoveArgs {
            name: "my-agent".to_string(),
        };
        assert_eq!(args.name, "my-agent");
    }

    #[test]
    fn remove_args_accepts_various_names() {
        for name in &[
            "simple",
            "with-dash",
            "with_underscore",
            "CamelCase",
            "v1.0.0",
        ] {
            let args = RemoveArgs {
                name: name.to_string(),
            };
            assert_eq!(args.name, *name);
        }
    }

    // ─── remove_agent ────────────────────────────────────────────────────

    fn install_test_agent(installer: &leviath_package::AgentInstaller, name: &str) {
        let project_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            project_dir.path().join("agent.leviath"),
            format!(
                "[agent]\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"test agent\"\n"
            ),
        )
        .unwrap();
        let bundle = leviath_package::AgentBundler::new()
            .bundle(project_dir.path())
            .unwrap();
        installer.install_from_bytes(name, &bundle).unwrap();
    }

    #[test]
    fn remove_agent_not_installed_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let installer = leviath_package::AgentInstaller::with_install_dir(dir.path().to_path_buf());

        let err = remove_agent(&installer, "nonexistent").unwrap_err();
        assert!(err.to_string().contains("is not installed"));
        assert!(err.to_string().contains("lev list"));
    }

    #[test]
    fn remove_agent_installed_succeeds_and_uninstalls() {
        let dir = tempfile::tempdir().unwrap();
        let installer = leviath_package::AgentInstaller::with_install_dir(dir.path().to_path_buf());
        install_test_agent(&installer, "my-agent");
        assert!(installer.get_installed("my-agent").unwrap().is_some());

        remove_agent(&installer, "my-agent").unwrap();

        assert!(installer.get_installed("my-agent").unwrap().is_none());
    }

    // ─── execute() ───────────────────────────────────────────────────────
    //
    // `execute()` always constructs a real `AgentInstaller::new()` pointed at
    // the developer's real `~/.leviath/agents` -- there's no env-var seam for
    // it (unlike `Config`'s `LEVIATH_CONFIG_PATH`), so this can't be driven
    // through a real install/uninstall round trip without touching that real
    // directory. It's still safe to exercise the not-installed error path:
    // `get_installed` only *reads* that directory, and an agent name this
    // specific is never going to exist there for real.
    #[tokio::test]
    async fn execute_with_nonexistent_agent_returns_error() {
        let args = RemoveArgs {
            name: "definitely-nonexistent-agent-for-lev-remove-coverage-xyz".to_string(),
        };
        let err = execute(args).await.unwrap_err();
        assert!(err.to_string().contains("is not installed"));
    }

    #[test]
    fn remove_agent_uninstall_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let installer = leviath_package::AgentInstaller::with_install_dir(dir.path().to_path_buf());
        install_test_agent(&installer, "err-agent");

        // An injected uninstall that fails exercises the `uninstall(name)?`
        // error arm deterministically on every platform. The prior version
        // dropped write permission on the parent dir, which only fails on Unix.
        let result = remove_agent_with(&installer, "err-agent", &|_| {
            Err(anyhow::anyhow!("simulated uninstall failure"))
        });
        assert!(result.is_err());
    }
}
