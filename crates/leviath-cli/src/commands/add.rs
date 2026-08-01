//! `lev add` - Install an agent package

use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct AddArgs {
    /// Path to an agent directory or a .leviath-bundle file
    #[arg(value_name = "PACKAGE")]
    pub package: String,
}

fn agents_dir_or_error(dir: Option<std::path::PathBuf>) -> anyhow::Result<std::path::PathBuf> {
    dir.ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))
}

pub async fn execute(args: AddArgs) -> anyhow::Result<()> {
    let installer = leviath_package::AgentInstaller::new();
    let agents_dir = resolve_agents_dir()?;
    execute_with(&args, &installer, &agents_dir).await
}

/// Resolve `~/.leviath/agents`, the install root for `lev add`.
///
/// A thin wrapper over [`agents_dir_or_error`] supplying the real resolved
/// directory. The `#[cfg(test)]` guard below only lets tests force the
/// "no home directory" error arm of `execute()` deterministically - the real
/// the shared resolver can't be made to return `None` in any environment a
/// test may safely create (on macOS `dirs::home_dir()` falls back to a
/// passwd-database lookup independent of `$HOME`). It does NOT hide the real
/// body from coverage: with the toggle off, `agents_dir_or_error(
/// leviath_core::paths::agents_dir())` runs (and is measured) in every ordinary test. This
/// only computes a `PathBuf`; the `None` arm of `agents_dir_or_error` is
/// covered directly by `agents_dir_or_error_none_returns_error`.
fn resolve_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    #[cfg(test)]
    if FORCE_AGENTS_DIR_ERROR.with(|f| f.get()) {
        anyhow::bail!("Could not determine home directory");
    }
    agents_dir_or_error(leviath_core::paths::agents_dir())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting `execute_returns_err_when_agents_dir_unresolvable`
    /// force `resolve_agents_dir`'s `Err` arm deterministically.
    static FORCE_AGENTS_DIR_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Core `lev add` logic, parameterized by installer + agents base directory
/// so it can be tested against tempdirs instead of the real
/// `~/.leviath/agents`.
async fn execute_with(
    args: &AddArgs,
    installer: &leviath_package::AgentInstaller,
    agents_dir: &Path,
) -> anyhow::Result<()> {
    tracing::info!("Installing agent package");

    let package_path = Path::new(&args.package);

    if package_path.is_dir() {
        // Directory install: copy directory into <agents_dir>/<name>/
        install_from_dir(package_path, agents_dir)?;
    } else if package_path.exists() || args.package.ends_with(".leviath-bundle") {
        // Bundle file installation
        if !package_path.exists() {
            anyhow::bail!("Package file not found: {}", args.package);
        }
        println!("Installing from bundle: {}", args.package);
        let installed = installer.install(package_path)?;
        println!(
            "Installed agent '{}' v{} to {}",
            installed.name,
            installed.version,
            installed.path.display()
        );
        print_capabilities(&installed.name, &installed.path);
    } else {
        // Only local installs exist: agent directories and .leviath-bundle
        // files. Fail with a clear message rather than guessing at intent.
        anyhow::bail!(
            "'{}' is not a local agent directory or a .leviath-bundle file - \
             pass a path to one of those instead.",
            args.package
        );
    }

    Ok(())
}

/// The security-relevant things an agent package carries, as human-readable
/// lines.
///
/// A bare "Installed agent 'x' to …" would never tell the user that the
/// package ships executable `.rhai` tool scripts, pre-approves its own `shell`,
/// turns the sandbox off, or runs a command at spawn before any prompt. Every
/// one of those is a decision the user is making by installing, so `lev add`
/// must surface them.
///
/// Empty means the package declares nothing unusual - a plain prompt-and-stages
/// agent - in which case there is nothing to warn about and we stay quiet.
///
/// Pure over `(manifest_toml, dir_entries)` so the whole table is testable
/// without a filesystem or an installed agent.
pub(crate) fn describe_capabilities(manifest_toml: &str, script_tools: &[String]) -> Vec<String> {
    let mut findings = Vec::new();
    // `toml::from_str`, not `manifest_toml.parse::<toml::Value>()`. In toml 1.x
    // `FromStr for Value` parses a single *value*, not a document - so a real
    // manifest starting with `[agent]` reads as an array literal followed by
    // junk and fails. It still compiles, so the change is silent; the tests are
    // what caught it.
    let Ok(value) = toml::from_str::<toml::Value>(manifest_toml) else {
        // An unparseable manifest is reported by the installer itself; there is
        // nothing to inventory.
        return findings;
    };

    if !script_tools.is_empty() {
        findings.push(format!(
            "ships {} executable script tool(s): {}",
            script_tools.len(),
            script_tools.join(", ")
        ));
    }

    // Tool permissions the package grants itself, at agent or stage level.
    let mut granted: Vec<String> = Vec::new();
    let mut collect_grants = |table: Option<&toml::Value>| {
        if let Some(t) = table.and_then(|v| v.as_table()) {
            for (tool, policy) in t {
                if policy.as_str() == Some("allow") && !granted.contains(tool) {
                    granted.push(tool.clone());
                }
            }
        }
    };
    collect_grants(value.get("tool_permissions"));
    if let Some(stages) = value.get("stages").and_then(|v| v.as_table()) {
        for stage in stages.values() {
            collect_grants(stage.get("tool_permissions"));
        }
    }
    if !granted.is_empty() {
        granted.sort();
        findings.push(format!(
            "pre-approves these tools (no prompt at run time): {}",
            granted.join(", ")
        ));
    }

    // Script host functions it grants itself.
    if let Some(t) = value
        .get("tool_script_permissions")
        .and_then(|v| v.as_table())
    {
        let mut allowed: Vec<&String> = t
            .iter()
            .filter(|(_, v)| v.as_str() == Some("allow"))
            .map(|(k, _)| k)
            .collect();
        if !allowed.is_empty() {
            allowed.sort();
            findings.push(format!(
                "requests script host access: {}",
                allowed
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    // A sandbox opt-out.
    if let Some(kind) = value
        .get("sandbox")
        .and_then(|v| v.get("kind"))
        .and_then(|v| v.as_str())
        && kind == "none"
    {
        findings.push("asks to run tools directly on the host (sandbox = none)".to_string());
    }

    // Read paths beyond the workdir. Declaring is not granting - the entries
    // are inert until the user's config grants them - but the ask itself is
    // exactly what this inventory exists to surface.
    if let Some(entries) = value
        .get("read_paths")
        .and_then(|v| v.get("allow"))
        .and_then(|v| v.as_array())
        && !entries.is_empty()
    {
        let listed: Vec<String> = entries
            .iter()
            .filter_map(|e| e.as_str().map(str::to_string))
            .collect();
        findings.push(format!(
            "asks to read outside its workdir (read-only): {}; inert unless you grant it \
             via [security] read_paths / allow_blueprint_read_paths or \
             [agent_read_paths.<name>] in your config",
            listed.join(", ")
        ));
    }

    // Command seeds run at spawn, before the first inference and therefore
    // before any approval prompt - the one place a manifest executes something
    // without being asked.
    let seed_commands = collect_seed_commands(&value);
    for command in seed_commands {
        findings.push(format!(
            "runs this command at startup, before any prompt: `{command}`"
        ));
    }

    findings
}

/// Every `seed = { command = "..." }` in a manifest, from agent-level and
/// stage-level `[context.regions]` blocks alike.
fn collect_seed_commands(value: &toml::Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut scan = |regions: Option<&toml::Value>| {
        if let Some(t) = regions.and_then(|v| v.as_table()) {
            for region in t.values() {
                if let Some(cmd) = region
                    .get("seed")
                    .and_then(|s| s.get("command"))
                    .and_then(|c| c.as_str())
                {
                    out.push(cmd.to_string());
                }
            }
        }
    };
    scan(value.get("context").and_then(|c| c.get("regions")));
    if let Some(stages) = value.get("stages").and_then(|v| v.as_table()) {
        for stage in stages.values() {
            scan(stage.get("context").and_then(|c| c.get("regions")));
        }
    }
    out
}

/// Print the capability inventory for a freshly installed agent, if it has one.
fn print_capabilities(name: &str, install_dir: &Path) {
    let manifest = std::fs::read_to_string(install_dir.join("agent.leviath")).unwrap_or_default();
    let scripts = script_tool_names(install_dir);
    let findings = describe_capabilities(&manifest, &scripts);
    if findings.is_empty() {
        return;
    }
    println!("\n  '{name}' asks for the following. Review before running it:");
    for finding in &findings {
        println!("    - {finding}");
    }
    println!("  Inspect it with:  lev validate {name}");
}

/// Names of the `.rhai` tool scripts an installed agent ships.
fn script_tool_names(install_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(install_dir.join("tools"))
        .into_iter()
        .flatten()
        .flatten()
        // `DirEntry::file_name` rather than `path().file_name()`: the latter
        // returns an `Option` that a directory entry can never actually be
        // missing, leaving an arm no test can reach.
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".rhai").then_some(name)
        })
        .collect();
    names.sort();
    names
}

/// Copy a plain agent directory into `<agents_dir>/<name>/`.
///
/// The agent name is read from `agent.leviath` in the directory (falling back
/// to the directory's own name).
fn install_from_dir(src: &Path, agents_dir: &Path) -> anyhow::Result<()> {
    let manifest_path = src.join("agent.leviath");
    if !manifest_path.exists() {
        anyhow::bail!(
            "No agent.leviath found in '{}'. Is this an agent directory?",
            src.display()
        );
    }

    // Read the manifest to extract the agent name
    let content = std::fs::read_to_string(&manifest_path)?;
    let name = parse_agent_name(&content).unwrap_or_else(|| {
        src.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    let install_dir = agents_dir.join(&name);

    if install_dir.exists() {
        println!("Reinstalling agent '{}' (replacing existing)", name);
        std::fs::remove_dir_all(&install_dir)?;
    }

    copy_dir_recursive(src, &install_dir)?;
    println!("Installed agent '{}' to {}", name, install_dir.display());
    print_capabilities(&name, &install_dir);
    println!("Run with:  lev run {} --task \"...\"", name);
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting a test force the `Err` arm of a
    /// mid-iteration `ReadDir` entry deterministically (see
    /// [`unwrap_dir_entry`]) - the real failure mode (the directory handle
    /// becoming invalid mid-iteration: deleted out from under the process,
    /// an NFS ESTALE, or similar) is a genuine OS-level race that can't be
    /// reproduced deterministically across Linux/macOS/Windows CI.
    static FORCE_DIR_ENTRY_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Unwrap one `ReadDir` iteration result, with a test-only failure-injection
/// toggle (see [`FORCE_DIR_ENTRY_ERROR`]) so the `Err` arm - `ReadDir::next()`
/// failing after `read_dir` already succeeded in opening the directory --
/// can be exercised deterministically without needing to actually race the
/// filesystem.
fn unwrap_dir_entry(
    entry: std::io::Result<std::fs::DirEntry>,
) -> anyhow::Result<std::fs::DirEntry> {
    #[cfg(test)]
    if FORCE_DIR_ENTRY_ERROR.with(|f| f.get()) {
        anyhow::bail!("forced dir-entry error for testing");
    }
    Ok(entry?)
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = unwrap_dir_entry(entry)?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Parse the agent name from an `agent.leviath` manifest (first `name = "..."` line).
fn parse_agent_name(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod capability_tests {
    use super::describe_capabilities;

    /// A plain agent declares nothing unusual, so the inventory stays quiet -
    /// a warning that fires on everything teaches people to skip it.
    #[test]
    fn an_ordinary_agent_has_nothing_to_report() {
        let manifest = "[agent]\nname = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n\
                        [stages.main]\nprompt = \"p\"\n";
        assert!(describe_capabilities(manifest, &[]).is_empty());
    }

    #[test]
    fn script_tools_are_listed_by_name() {
        let findings = describe_capabilities(
            "[agent]\nname = \"x\"\n",
            &["web_fetch.rhai".to_string(), "post.rhai".to_string()],
        );
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("2 executable script tool"));
        assert!(findings[0].contains("web_fetch.rhai"));
    }

    /// The case that matters most: a package that pre-approves its own shell.
    /// Under the permission floor a user's explicit config still wins, but where
    /// the user has said nothing this is a real grant they should see.
    #[test]
    fn self_granted_tool_permissions_are_reported() {
        let manifest = "[agent]\nname = \"x\"\n\n\
                        [tool_permissions]\nshell = \"allow\"\nread_file = \"ask\"\n";
        let findings = describe_capabilities(manifest, &[]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("pre-approves"));
        assert!(findings[0].contains("shell"));
        // `ask` is the default posture, not a grant.
        assert!(!findings[0].contains("read_file"));
    }

    /// A `[tool_script_permissions]` table that only *tightens* is not a grant,
    /// so it must not appear in the inventory - the same "quiet unless there is
    /// something to say" rule the ordinary-agent case establishes.
    #[test]
    fn a_script_permission_table_that_grants_nothing_is_not_reported() {
        let manifest = "[agent]\nname = \"x\"\n\n\
                        [tool_script_permissions]\nenv_var = \"deny\"\nhttp_get = \"ask\"\n";
        assert!(
            describe_capabilities(manifest, &[]).is_empty(),
            "denying host access is not a capability to warn about"
        );
    }

    #[test]
    fn stage_level_grants_are_reported_too() {
        let manifest = "[agent]\nname = \"x\"\n\n\
                        [stages.build.tool_permissions]\nwrite_file = \"allow\"\n";
        let findings = describe_capabilities(manifest, &[]);
        assert!(findings[0].contains("write_file"), "{findings:?}");
    }

    #[test]
    fn script_host_grants_and_sandbox_opt_out_are_reported() {
        let manifest = "[agent]\nname = \"x\"\n\n\
                        [tool_script_permissions]\nshell = \"allow\"\nhttp_post = \"allow\"\n\n\
                        [sandbox]\nkind = \"none\"\n";
        let findings = describe_capabilities(manifest, &[]);
        let joined = findings.join(" | ");
        assert!(joined.contains("script host access"), "{joined}");
        assert!(joined.contains("http_post"), "{joined}");
        assert!(joined.contains("sandbox = none"), "{joined}");
    }

    /// A command seed runs at spawn - before the first inference and therefore
    /// before any approval prompt. It is the one thing a manifest executes
    /// without being asked, so the exact command is shown.
    #[test]
    fn command_seeds_are_reported_verbatim() {
        let manifest = "[agent]\nname = \"x\"\n\n\
                        [context.regions]\n\
                        repo = { kind = \"pinned\", seed = { command = \"git ls-files\" } }\n";
        let findings = describe_capabilities(manifest, &[]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("before any prompt"), "{findings:?}");
        assert!(findings[0].contains("git ls-files"), "{findings:?}");
    }

    #[test]
    fn stage_level_command_seeds_are_reported() {
        let manifest = "[agent]\nname = \"x\"\n\n\
                        [stages.discover.context.regions]\n\
                        env = { kind = \"pinned\", seed = { command = \"curl https://evil\" } }\n";
        let findings = describe_capabilities(manifest, &[]);
        assert!(findings[0].contains("curl https://evil"), "{findings:?}");
    }

    /// A sandbox the manifest *opts into* is not a warning - only opting out is.
    #[test]
    fn opting_into_a_sandbox_is_not_reported() {
        let manifest = "[agent]\nname = \"x\"\n\n[sandbox]\nkind = \"container\"\n";
        assert!(describe_capabilities(manifest, &[]).is_empty());
    }

    /// `[read_paths]` is an ask to see beyond the workdir - listed verbatim,
    /// with the reminder that it stays inert until the user's config grants it.
    #[test]
    fn read_path_declarations_are_reported() {
        let manifest = "[agent]\nname = \"x\"\n\n\
                        [read_paths]\n\
                        allow = [\"~/.leviath/runs\", \"glob:~/design-docs/**\"]\n";
        let findings = describe_capabilities(manifest, &[]);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].contains("read outside its workdir"),
            "{findings:?}"
        );
        assert!(findings[0].contains("~/.leviath/runs"), "{findings:?}");
        assert!(
            findings[0].contains("glob:~/design-docs/**"),
            "{findings:?}"
        );
        assert!(
            findings[0].contains("inert unless you grant it"),
            "{findings:?}"
        );
    }

    /// An empty `allow` array asks for nothing - stay quiet.
    #[test]
    fn an_empty_read_paths_block_is_not_reported() {
        let manifest = "[agent]\nname = \"x\"\n\n[read_paths]\nallow = []\n";
        assert!(describe_capabilities(manifest, &[]).is_empty());
    }

    /// The `tools/` scan that feeds the inventory: only `.rhai` files count, and
    /// they come back sorted so the message is stable between runs.
    #[test]
    fn script_tool_names_lists_only_rhai_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir(&tools).unwrap();
        for name in ["zeta.rhai", "alpha.rhai", "README.md", "notes.txt"] {
            std::fs::write(tools.join(name), "x").unwrap();
        }
        assert_eq!(
            super::script_tool_names(dir.path()),
            vec!["alpha.rhai".to_string(), "zeta.rhai".to_string()]
        );
    }

    /// An agent with no `tools/` directory at all - the common case.
    #[test]
    fn script_tool_names_is_empty_without_a_tools_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::script_tool_names(dir.path()).is_empty());
    }

    /// The end-to-end printer, over a directory rather than a string: it must
    /// stay silent for an ordinary agent and speak for a demanding one.
    #[test]
    fn print_capabilities_reads_the_installed_directory() {
        crate::test_support::with_tracing(|| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("agent.leviath"),
                "[agent]\nname = \"q\"\n\n[tool_permissions]\nshell = \"allow\"\n",
            )
            .unwrap();
            let tools = dir.path().join("tools");
            std::fs::create_dir(&tools).unwrap();
            std::fs::write(tools.join("t.rhai"), "// @tool t\n").unwrap();
            super::print_capabilities("q", dir.path());

            // And the quiet path: a plain agent prints nothing.
            let plain = tempfile::tempdir().unwrap();
            std::fs::write(
                plain.path().join("agent.leviath"),
                "[agent]\nname = \"p\"\n\n[stages.main]\nprompt = \"p\"\n",
            )
            .unwrap();
            super::print_capabilities("p", plain.path());
        });
    }

    #[test]
    fn an_unparseable_manifest_reports_nothing() {
        assert!(describe_capabilities("{ not toml", &[]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{with_tracing, write_test_agent};

    // ─── agents_dir_or_error ─────────────────────────────────────────────

    #[test]
    fn agents_dir_or_error_some_returns_path() {
        let dir = std::path::PathBuf::from("/home/testuser/.leviath/agents");
        assert_eq!(agents_dir_or_error(Some(dir.clone())).unwrap(), dir);
    }

    #[test]
    fn agents_dir_or_error_none_returns_error() {
        let err = agents_dir_or_error(None).unwrap_err();
        assert!(
            err.to_string()
                .contains("Could not determine home directory")
        );
    }

    // ─── parse_agent_name ──────────────────────────────────────────────────

    #[test]
    fn parse_agent_name_standard() {
        let content = r#"
name = "my-agent"
version = "1.0"
"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_no_quotes() {
        let content = r#"name = my-agent"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_extra_whitespace() {
        let content = r#"  name   =   "spacy-agent"  "#;
        assert_eq!(parse_agent_name(content), Some("spacy-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_missing() {
        let content = r#"
version = "1.0"
description = "test"
"#;
        assert_eq!(parse_agent_name(content), None);
    }

    #[test]
    fn parse_agent_name_empty_value() {
        let content = r#"name = """#;
        assert_eq!(parse_agent_name(content), None);
    }

    // ─── copy_dir_recursive ────────────────────────────────────────────────

    #[test]
    fn copy_dir_recursive_copies_files() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        std::fs::write(src_dir.path().join("file1.txt"), "hello").unwrap();
        std::fs::create_dir_all(src_dir.path().join("sub")).unwrap();
        std::fs::write(src_dir.path().join("sub/file2.txt"), "world").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("file1.txt").exists());
        assert!(dst_path.join("sub/file2.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("file1.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst_path.join("sub/file2.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn copy_dir_recursive_empty_dir() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("empty-copy");

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();
        assert!(dst_path.exists());
        assert!(dst_path.is_dir());
    }

    #[test]
    fn copy_dir_recursive_nonexistent_src_errors() {
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("dst");
        let missing_src = dst_dir.path().join("does-not-exist");

        let result = copy_dir_recursive(&missing_src, &dst_path);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_dst_parent_is_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, "x").unwrap();
        let src = tempfile::tempdir().unwrap();
        let dst = file_path.join("child");

        let result = copy_dir_recursive(src.path(), &dst);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_file_over_existing_dir_errors() {
        // Copying a file onto a destination path that already exists as a
        // directory fails on every platform (EISDIR / ERROR_ACCESS_DENIED),
        // exercising the `std::fs::copy(...)?` error arm.
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("clash"), "top secret").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");
        // Pre-create dst/clash as a directory so the file copy collides.
        std::fs::create_dir_all(dst_path.join("clash")).unwrap();

        let result = copy_dir_recursive(src_dir.path(), &dst_path);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_recursion_error_propagates() {
        // Exercises the recursive-call error-propagation branch OS-agnostically:
        // the destination already has a *file* where the recursion needs to
        // create a subdirectory, so the nested `create_dir_all` fails on every
        // platform and that `Err` bubbles up through the parent's
        // `copy_dir_recursive(...)?`.
        let src_dir = tempfile::tempdir().unwrap();
        let sub = src_dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("file.txt"), "data").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");
        std::fs::create_dir_all(&dst_path).unwrap();
        // Block the recursion's create_dir_all(dst/sub) with a file at that path.
        std::fs::write(dst_path.join("sub"), "i am a file").unwrap();

        let result = copy_dir_recursive(src_dir.path(), &dst_path);
        assert!(result.is_err());
    }

    #[test]
    fn copy_dir_recursive_forced_mid_iteration_entry_error() {
        // Deterministically exercises `unwrap_dir_entry`'s `Err` arm (a real
        // `ReadDir::next()` failure mid-iteration) without racing the
        // filesystem, via the FORCE_DIR_ENTRY_ERROR test toggle.
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("file.txt"), "data").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("copy");

        FORCE_DIR_ENTRY_ERROR.with(|f| f.set(true));
        let result = copy_dir_recursive(src_dir.path(), &dst_path);
        FORCE_DIR_ENTRY_ERROR.with(|f| f.set(false));

        assert!(result.is_err());
    }

    #[test]
    fn unwrap_dir_entry_propagates_a_real_err_argument() {
        // `unwrap_dir_entry`'s own `Ok(entry?)` `?` still has a real error
        // arm distinct from the `FORCE_DIR_ENTRY_ERROR`-triggered early
        // `bail!` above it (that toggle short-circuits *before* this line
        // is ever reached) - `DirEntry` isn't constructible directly, but
        // its `Result` wrapper doesn't need a real one to test the `Err`
        // case: pass a synthetic `io::Error` straight in.
        let result = unwrap_dir_entry(Err(std::io::Error::other("synthetic entry error")));
        assert!(result.is_err());
    }

    // ─── install_from_dir ──────────────────────────────────────────────────

    #[test]
    fn install_from_dir_no_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = tempfile::tempdir().unwrap();
        let result = install_from_dir(dir.path(), agents_dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("agent.leviath"));
    }

    #[test]
    fn install_from_dir_copies_and_names_from_manifest() {
        let src = tempfile::tempdir().unwrap();
        let agents_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"my-agent\"\n",
        )
        .unwrap();
        std::fs::write(src.path().join("extra.txt"), "data").unwrap();

        install_from_dir(src.path(), agents_dir.path()).unwrap();

        let installed_dir = agents_dir.path().join("my-agent");
        assert!(installed_dir.join("agent.leviath").exists());
        assert!(installed_dir.join("extra.txt").exists());
    }

    #[test]
    fn install_from_dir_falls_back_to_dirname_when_name_missing() {
        let src = tempfile::tempdir().unwrap();
        let agent_dir = src.path().join("my-dir-name");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.leviath"), "version = \"1.0\"\n").unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        install_from_dir(&agent_dir, agents_dir.path()).unwrap();

        assert!(agents_dir.path().join("my-dir-name").exists());
    }

    #[test]
    fn install_from_dir_reinstalls_existing() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"dup-agent\"\n",
        )
        .unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        // Pre-create an existing install with a stale file that should be wiped.
        let existing = agents_dir.path().join("dup-agent");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("stale.txt"), "old").unwrap();

        install_from_dir(src.path(), agents_dir.path()).unwrap();

        assert!(!existing.join("stale.txt").exists());
        assert!(existing.join("agent.leviath").exists());
    }

    #[test]
    fn install_from_dir_invalid_utf8_manifest_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.leviath"), [0xFF, 0xFE, 0xFA]).unwrap();
        let agents_dir = tempfile::tempdir().unwrap();

        let result = install_from_dir(dir.path(), agents_dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn install_from_dir_remove_dir_all_failure_errors() {
        // The existing install target is a *file*, so `exists()` passes the
        // reinstall guard but `remove_dir_all` (which requires a directory)
        // fails on every platform, exercising that `?` arm.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"file-agent\"\n",
        )
        .unwrap();

        let agents_dir = tempfile::tempdir().unwrap();
        std::fs::write(agents_dir.path().join("file-agent"), "not a dir").unwrap();

        let result = install_from_dir(src.path(), agents_dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn install_from_dir_copy_failure_propagates() {
        // `agents_dir` is itself a *file*, so `copy_dir_recursive`'s
        // `create_dir_all` for the install target (a child path of a file)
        // fails on every platform, and that `Err` propagates through
        // `install_from_dir`'s `copy_dir_recursive(...)?`.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(
            src.path().join("agent.leviath"),
            "[agent]\nname = \"broken-copy-agent\"\n",
        )
        .unwrap();
        std::fs::write(src.path().join("extra.txt"), "data").unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let agents_file = tmp.path().join("agents-is-a-file");
        std::fs::write(&agents_file, "not a dir").unwrap();

        let result = install_from_dir(src.path(), &agents_file);
        assert!(result.is_err());
    }

    // ─── execute_with: directory + bundle-file paths ───────────────────────

    #[test]
    fn execute_with_directory_package_installs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let src = tempfile::tempdir().unwrap();
                std::fs::write(
                    src.path().join("agent.leviath"),
                    "[agent]\nname = \"dir-pkg\"\n",
                )
                .unwrap();
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: src.path().to_str().unwrap().to_string(),
                };

                execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap();

                assert!(agents_dir.path().join("dir-pkg").exists());
            })
        });
    }

    #[test]
    fn execute_with_directory_without_manifest_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let src = tempfile::tempdir().unwrap(); // no agent.leviath inside
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: src.path().to_str().unwrap().to_string(),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("agent.leviath"));
            })
        });
    }

    #[test]
    fn execute_with_missing_bundle_file_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "nonexistent.leviath-bundle".to_string(),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Package file not found"));
            })
        });
    }

    #[test]
    fn execute_with_bundle_file_installs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let project_dir = tempfile::tempdir().unwrap();
                std::fs::write(
                    project_dir.path().join("agent.leviath"),
                    "[agent]\nname = \"bundled-pkg\"\nversion = \"1.0.0\"\ndescription = \"d\"\n",
                )
                .unwrap();
                let bundle_bytes = leviath_package::AgentBundler::new()
                    .bundle(project_dir.path())
                    .unwrap();
                let bundle_dir = tempfile::tempdir().unwrap();
                // AgentInstaller::install() derives the agent name from the
                // bundle *filename* (not the manifest content), so name it
                // to match what we assert on below.
                let bundle_path = bundle_dir.path().join("bundled-pkg.leviath-bundle");
                std::fs::write(&bundle_path, bundle_bytes).unwrap();

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: bundle_path.to_str().unwrap().to_string(),
                };

                execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap();

                assert!(agents_dir.path().join("bundled-pkg").exists());
            })
        });
    }

    #[test]
    fn execute_with_corrupt_bundle_file_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let bundle_dir = tempfile::tempdir().unwrap();
                let bundle_path = bundle_dir.path().join("broken.leviath-bundle");
                std::fs::write(&bundle_path, b"not a valid gzip archive").unwrap();

                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: bundle_path.to_str().unwrap().to_string(),
                };

                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Failed to extract package"));
            })
        });
    }

    #[test]
    fn execute_with_unrecognized_package_reports_local_only() {
        // A package that is neither a local directory nor a .leviath-bundle
        // file must fail with a clear message, never a network attempt.
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let agents_dir = tempfile::tempdir().unwrap();
                let installer = leviath_package::AgentInstaller::with_install_dir(
                    agents_dir.path().to_path_buf(),
                );
                let args = AddArgs {
                    package: "some-registry-agent".to_string(),
                };
                let err = execute_with(&args, &installer, agents_dir.path())
                    .await
                    .unwrap_err();
                assert!(
                    err.to_string()
                        .contains("not a local agent directory or a .leviath-bundle file"),
                    "expected the v1-cut message, got: {err}"
                );
            })
        });
    }

    // ─── path detection ────────────────────────────────────────────────────

    #[test]
    fn bundle_extension_detected() {
        let package = "my-agent-1.0.leviath-bundle";
        assert!(package.ends_with(".leviath-bundle"));
    }

    #[test]
    fn directory_path_detected() {
        let dir = tempfile::tempdir().unwrap();
        let package_path = Path::new(dir.path().to_str().unwrap());
        assert!(package_path.is_dir());
    }

    #[test]
    fn registry_name_not_dir_not_bundle() {
        let package = "my-cool-agent";
        let package_path = Path::new(package);
        assert!(!package_path.is_dir());
        assert!(!package.ends_with(".leviath-bundle"));
    }

    // ─── parse_agent_name additional ──────────────────────────────────────

    #[test]
    fn parse_agent_name_in_section() {
        let content = r#"
[agent]
name = "my-agent"
version = "1.0"
"#;
        assert_eq!(parse_agent_name(content), Some("my-agent".to_string()));
    }

    #[test]
    fn parse_agent_name_with_single_quotes() {
        // toml uses double quotes, but our parser uses trim_matches('"')
        let content = r#"name = my-agent-no-quotes"#;
        assert_eq!(
            parse_agent_name(content),
            Some("my-agent-no-quotes".to_string())
        );
    }

    #[test]
    fn parse_agent_name_multiple_name_fields_returns_first() {
        let content = r#"
name = "first"
name = "second"
"#;
        assert_eq!(parse_agent_name(content), Some("first".to_string()));
    }

    // ─── copy_dir_recursive with nested dirs ──────────────────────────────

    #[test]
    fn copy_dir_recursive_deeply_nested() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst_path = dst_dir.path().join("deep-copy");

        std::fs::create_dir_all(src_dir.path().join("a/b/c")).unwrap();
        std::fs::write(src_dir.path().join("a/b/c/deep.txt"), "deep").unwrap();

        copy_dir_recursive(src_dir.path(), &dst_path).unwrap();

        assert!(dst_path.join("a/b/c/deep.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_path.join("a/b/c/deep.txt")).unwrap(),
            "deep"
        );
    }

    // ─── execute(): real entry point wrapper ───────────────────────────────

    #[test]
    fn execute_real_wrapper_fails_fast_without_touching_real_agents_dir() {
        // Drives the real `execute()` (dirs::home_dir() + AgentInstaller::new()
        // + delegation to execute_with) - safe because a nonexistent
        // ".leviath-bundle" path bails out in execute_with's "Package file
        // not found" check before any real file under ~/.leviath/agents is
        // ever touched.
        let rt = tokio::runtime::Runtime::new().unwrap();
        with_tracing(|| {
            rt.block_on(async {
                let args = AddArgs {
                    package: "definitely-not-a-real-bundle-xyz.leviath-bundle".to_string(),
                };
                let err = execute(args).await.unwrap_err();
                assert!(err.to_string().contains("Package file not found"));
            })
        });
    }

    #[test]
    fn execute_returns_err_when_agents_dir_unresolvable() {
        // Drives `execute`'s `resolve_agents_dir()?` error-propagation
        // branch for real via the test-only `FORCE_AGENTS_DIR_ERROR` toggle
        // on `resolve_agents_dir`'s twin (see its doc comment for why the
        // real implementation's failure can't be forced directly).
        let rt = tokio::runtime::Runtime::new().unwrap();
        FORCE_AGENTS_DIR_ERROR.with(|f| f.set(true));
        let result = rt.block_on(async {
            let args = AddArgs {
                package: "whatever.leviath-bundle".to_string(),
            };
            execute(args).await
        });
        FORCE_AGENTS_DIR_ERROR.with(|f| f.set(false));

        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("Could not determine home directory")
        );
    }

    // ─── install_from_dir with valid manifest ─────────────────────────────

    #[test]
    fn install_from_dir_with_manifest_runs() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = r#"
[agent]
name = "test-install-agent-xyz"
version = "0.1.0"
description = "test"
"#;
        write_test_agent(dir.path(), manifest);
        std::fs::write(dir.path().join("readme.txt"), "hello").unwrap();

        let agents_dir = tempfile::tempdir().unwrap();
        install_from_dir(dir.path(), agents_dir.path()).unwrap();

        let install_dir = agents_dir.path().join("test-install-agent-xyz");
        assert!(install_dir.join("agent.leviath").exists());
        assert!(install_dir.join("readme.txt").exists());
    }
}
