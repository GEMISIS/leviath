//! `lev list` - List available agents and blueprints

use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use leviath_core::manifest::parse_manifest;

#[derive(Args)]
pub struct ListArgs {
    /// Filter by type (agents, blueprints, all)
    #[arg(short, long, default_value = "all")]
    pub filter: String,
}

/// Info parsed from an agent manifest for display.
struct AgentInfo {
    name: String,
    version: String,
    description: String,
}

fn read_agent_info(manifest_path: &Path) -> Option<AgentInfo> {
    let content = fs::read_to_string(manifest_path).ok()?;
    let blueprint = parse_manifest(&content).ok()?;
    Some(AgentInfo {
        name: blueprint.name,
        version: blueprint.version,
        description: blueprint.description,
    })
}

fn scan_directory_for_agents(dir: &Path) -> Vec<(PathBuf, AgentInfo)> {
    let mut agents = Vec::new();
    if !dir.exists() {
        return agents;
    }

    // Check if this directory itself has an agent.leviath
    let direct_manifest = dir.join("agent.leviath");
    if direct_manifest.exists()
        && let Some(info) = read_agent_info(&direct_manifest)
    {
        agents.push((dir.to_path_buf(), info));
    }

    // Check subdirectories
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let manifest_path = path.join("agent.leviath");
                if manifest_path.exists()
                    && let Some(info) = read_agent_info(&manifest_path)
                {
                    agents.push((path, info));
                }
            }
        }
    }

    agents
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting `execute_falls_back_to_default_cwd_via_forced_error`
    /// force [`resolve_cwd`]'s `Err` arm deterministically on every platform,
    /// as a companion to
    /// `execute_falls_back_to_default_cwd_when_current_dir_is_gone`'s genuine
    /// Unix-only filesystem reproduction (real `remove_dir_all` of the live
    /// CWD is a sharing violation on Windows, not a success, so that same
    /// trick isn't available there).
    static FORCE_CWD_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Real CWD lookup, with a test-only failure-injection toggle so its `Err`
/// arm can be forced deterministically (see [`FORCE_CWD_ERROR`]) without
/// changing what production actually calls.
fn resolve_cwd() -> std::io::Result<PathBuf> {
    #[cfg(test)]
    if FORCE_CWD_ERROR.with(|f| f.get()) {
        return Err(std::io::Error::other("forced CWD error for testing"));
    }
    std::env::current_dir()
}

pub async fn execute(_args: ListArgs) -> anyhow::Result<()> {
    let config = Config::load().unwrap_or_default();
    let agents_dir = get_agents_dir()?;
    let cwd = resolve_cwd().unwrap_or_default();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    print_agent_listing(&agents_dir, &cwd, exe_dir.as_deref(), &config)
}

/// Core `lev list` logic, parameterized by every real-environment source it
/// reads from so it can be tested against tempdirs instead of the real
/// home directory / CWD / executable location / config.
fn print_agent_listing(
    agents_dir: &Path,
    cwd: &Path,
    exe_dir: Option<&Path>,
    config: &Config,
) -> anyhow::Result<()> {
    // Tracks whether the user has any agent they can actually *run*. The
    // bundled catalog deliberately does not count: it is always non-empty, and
    // treating it as "you have agents" would suppress the get-started guidance
    // for exactly the person who needs it — someone with a fresh install and
    // nothing installed yet.
    let mut found_runnable = false;

    // 1. Installed agents (~/.leviath/agents/)
    let installed = scan_directory_for_agents(agents_dir);
    if !installed.is_empty() {
        found_runnable = true;
        println!("Installed agents (~/.leviath/agents/):");
        for (_path, info) in &installed {
            let desc = if info.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", info.description)
            };
            println!("  {} (v{}){}", info.name, info.version, desc);
        }
        println!();
    }

    // 2. Local (current directory)
    let local_manifest = cwd.join("agent.leviath");
    if local_manifest.exists()
        && let Some(info) = read_agent_info(&local_manifest)
    {
        found_runnable = true;
        let desc = if info.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", info.description)
        };
        println!("Local (current directory):");
        println!("  {} (v{}){}", info.name, info.version, desc);
        println!();
    }

    // 3. Config's agent_paths directories
    let mut config_agents = Vec::new();
    for agent_path in &config.agent_paths {
        let found = scan_directory_for_agents(agent_path);
        config_agents.extend(found);
    }
    if !config_agents.is_empty() {
        found_runnable = true;
        println!("From configured paths:");
        for (_path, info) in &config_agents {
            let desc = if info.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", info.description)
            };
            println!("  {} (v{}){}", info.name, info.version, desc);
        }
        println!();
    }

    // 4. Bundled agents — the blueprints embedded in this binary.
    //
    // This used to scan `<exe_dir>/agents`, a directory no real install has, so
    // the section never printed outside a git checkout. It now reports the
    // embedded catalog, which is what `lev setup` installs from; the on-disk
    // scan stays as a second source so a checkout or a packaging layout that
    // *does* ship an `agents/` dir next to the binary still shows up.
    let mut builtin_names: Vec<String> = crate::bundled::BUNDLED_AGENTS
        .iter()
        .map(|a| format!("{} (v{})", a.name, a.version))
        .collect();
    if let Some(exe_dir) = exe_dir {
        for (_path, info) in scan_directory_for_agents(&exe_dir.join("agents")) {
            let entry = format!("{} (v{})", info.name, info.version);
            if !builtin_names.contains(&entry) {
                builtin_names.push(entry);
            }
        }
    }
    // No emptiness guard: the embedded catalog is always populated (a build
    // that found no blueprints fails `bundled`'s own invariant test), so an
    // `if !builtin_names.is_empty()` here would be a branch that can never be
    // false — unreachable code dressed up as a handled case.
    println!("Bundled agents (install with `lev setup`):");
    println!("  {}", builtin_names.join(", "));
    println!();

    if !found_runnable {
        println!("No agents installed yet.");
        println!();
        println!("To install the bundled agents:");
        println!("  lev setup");
        println!();
        println!("To create your own:");
        println!("  lev create my-agent");
    }

    Ok(())
}

/// Core `get_agents_dir` logic, parameterized by the home directory so the
/// "could not determine home directory" error path can be unit tested
/// without depending on the real environment.
fn get_agents_dir_or_error(dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    dir.ok_or(anyhow::anyhow!("Could not determine home directory"))
}

/// Resolve `~/.leviath/agents`, the directory `lev list` scans for installed
/// agents.
///
/// A thin wrapper over [`get_agents_dir_or_error`] supplying the real
/// resolved directory. The `#[cfg(test)]` guard below only lets tests force the
/// "no home directory" error arm of `execute()` deterministically — the real
/// the shared resolver can't be made to return `None` in any environment a
/// test may safely create (on macOS `dirs::home_dir()` falls back to a
/// passwd-database lookup independent of `$HOME`). It does NOT hide the real
/// body from coverage: with the toggle off, `get_agents_dir_or_error(
/// leviath_core::paths::agents_dir())` runs (and is measured) in every ordinary test, and
/// only computes a `PathBuf` (no filesystem writes). The `None` arm of
/// `get_agents_dir_or_error` is covered directly by
/// `get_agents_dir_or_error_none_returns_error`.
fn get_agents_dir() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    if FORCE_AGENTS_DIR_ERROR.with(|f| f.get()) {
        anyhow::bail!("Could not determine home directory");
    }
    get_agents_dir_or_error(leviath_core::paths::agents_dir())
}

#[cfg(test)]
thread_local! {
    /// Test-only toggle letting `execute_returns_err_when_agents_dir_unresolvable`
    /// force `get_agents_dir`'s `Err` arm deterministically.
    static FORCE_AGENTS_DIR_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;

    fn write_manifest(dir: &Path, name: &str) {
        write_manifest_with_description(dir, name, "Test agent");
    }

    fn write_manifest_with_description(dir: &Path, name: &str, description: &str) {
        let content = format!(
            r#"[agent]
name = "{}"
version = "1.0.0"
description = "{}"

[stages.main]
mode = "autonomous"
model = {{ provider = "anthropic", model = "claude-sonnet-4-6" }}
description = "Main"
max_iterations = 5

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
"#,
            name, description
        );
        write_test_agent(dir, content);
    }

    #[test]
    fn read_agent_info_valid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "my-agent");
        let info = read_agent_info(&dir.path().join("agent.leviath")).unwrap();
        assert_eq!(info.name, "my-agent");
        assert_eq!(info.version, "1.0.0");
        assert_eq!(info.description, "Test agent");
    }

    #[test]
    fn read_agent_info_missing_file_returns_none() {
        let result = read_agent_info(Path::new("/nonexistent/agent.leviath"));
        assert!(result.is_none());
    }

    #[test]
    fn read_agent_info_invalid_toml_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("agent.leviath"), "not valid toml {{{{").unwrap();
        let result = read_agent_info(&dir.path().join("agent.leviath"));
        assert!(result.is_none());
    }

    #[test]
    fn scan_directory_nonexistent_returns_empty() {
        let agents = scan_directory_for_agents(Path::new("/nonexistent/path"));
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_directory_path_is_a_file_returns_empty() {
        // `dir.exists()` is true for a plain file too, so this reaches
        // `fs::read_dir(dir)` -- which fails with "not a directory",
        // exercising the `if let Ok(entries) = ...` construct's implicit
        // (no-`else`) false arm that no other test hits.
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-directory.txt");
        fs::write(&file_path, "hello").unwrap();
        let agents = scan_directory_for_agents(&file_path);
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_directory_direct_manifest_invalid_is_skipped() {
        // The direct-manifest branch (as opposed to the subdirectory-scan
        // branch, covered separately by `scan_directory_subdir_with_invalid_manifest`)
        // has its own `if let Some(info) = read_agent_info(...)` — this
        // exercises that branch's `None` arm when the manifest at the
        // directory's own root is present but unparseable.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("agent.leviath"), "not valid toml {{{{").unwrap();
        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_directory_with_direct_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "direct-agent");
        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.name, "direct-agent");
    }

    #[test]
    fn scan_directory_with_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let sub1 = dir.path().join("agent-a");
        let sub2 = dir.path().join("agent-b");
        fs::create_dir_all(&sub1).unwrap();
        fs::create_dir_all(&sub2).unwrap();
        write_manifest(&sub1, "agent-a");
        write_manifest(&sub2, "agent-b");

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 2);
        let names: Vec<&str> = agents.iter().map(|a| a.1.name.as_str()).collect();
        assert!(names.contains(&"agent-a"));
        assert!(names.contains(&"agent-b"));
    }

    #[test]
    fn scan_directory_ignores_subdirs_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("no-manifest");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("readme.txt"), "not a manifest").unwrap();

        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    #[test]
    fn list_args_default_filter() {
        let args = ListArgs {
            filter: "all".to_string(),
        };
        assert_eq!(args.filter, "all");
    }

    // ─── read_agent_info: description and version ───────────────────────

    #[test]
    fn read_agent_info_extracts_description() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "my-agent");
        let info = read_agent_info(&dir.path().join("agent.leviath")).unwrap();
        assert_eq!(info.description, "Test agent");
        assert_eq!(info.version, "1.0.0");
    }

    // ─── scan_directory: nested but not deep ────────────────────────────

    #[test]
    fn scan_directory_with_both_direct_and_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        // Direct manifest
        write_manifest(dir.path(), "root-agent");
        // Subdirectory with manifest
        let sub = dir.path().join("child");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "child-agent");

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 2);
        let names: Vec<&str> = agents.iter().map(|a| a.1.name.as_str()).collect();
        assert!(names.contains(&"root-agent"));
        assert!(names.contains(&"child-agent"));
    }

    // ─── scan_directory: empty directory ────────────────────────────────

    #[test]
    fn scan_directory_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    // ─── scan_directory: subdirectory with invalid manifest ─────────────

    #[test]
    fn scan_directory_subdir_with_invalid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("bad-agent");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("agent.leviath"), "invalid toml {{{{").unwrap();

        let agents = scan_directory_for_agents(dir.path());
        assert!(agents.is_empty());
    }

    // ─── get_agents_dir ────────────────────────────────────────────────

    #[test]
    fn get_agents_dir_returns_path_with_agents() {
        let dir = get_agents_dir().unwrap();
        assert!(dir.to_str().unwrap().contains(".leviath"));
        assert!(dir.to_str().unwrap().ends_with("agents"));
    }

    #[test]
    fn get_agents_dir_or_error_some_returns_path() {
        let dir = PathBuf::from("/home/testuser/.leviath/agents");
        assert_eq!(get_agents_dir_or_error(Some(dir.clone())).unwrap(), dir);
    }

    #[test]
    fn get_agents_dir_or_error_none_returns_error() {
        let err = get_agents_dir_or_error(None).unwrap_err();
        assert!(
            err.to_string()
                .contains("Could not determine home directory")
        );
    }

    // ─── read_agent_info: minimal manifest ──────────────────────────────

    #[test]
    fn read_agent_info_minimal_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"[agent]
name = "minimal"
version = "0.0.1"
description = ""

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_test_agent(dir.path(), content);
        let info = read_agent_info(&dir.path().join("agent.leviath")).unwrap();
        assert_eq!(info.name, "minimal");
        assert_eq!(info.description, "");
    }

    // ─── execute() smoke test (real environment) ────────────────────────

    #[tokio::test]
    async fn execute_runs_without_error() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-runs-ok", |_fake_dir| async move {
            // Touches the real environment (home dir / CWD / exe location /
            // config) but must always succeed regardless of what it finds.
            let args = ListArgs {
                filter: "all".to_string(),
            };
            let result = execute(args).await;
            assert!(result.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn execute_falls_back_to_default_config_when_config_file_is_malformed() {
        // `execute`'s `Config::load().unwrap_or_default()` can only take
        // its fallback arm when `Config::load()` errors -- every other
        // `execute()` test sees either no config file (defaults) or a
        // well-formed one. Redirecting `LEVIATH_CONFIG_PATH` to malformed
        // TOML (the same technique as `config.rs`'s
        // `load_propagates_error_when_real_config_file_is_malformed`)
        // forces that for real, and `execute` must still succeed by
        // falling back to `Config::default()`.
        crate::config::with_isolated_config_path_async(
            "list-execute-malformed-config",
            |fake_dir| async move {
                std::fs::write(fake_dir.join("config.toml"), "not valid toml [[[").unwrap();

                let args = ListArgs {
                    filter: "all".to_string(),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_returns_err_when_agents_dir_unresolvable() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-dir-err", |_fake_dir| async move {
            // Drives `execute`'s `get_agents_dir()?` error-propagation branch
            // for real via the test-only `FORCE_AGENTS_DIR_ERROR` toggle on
            // `get_agents_dir`'s twin (see its doc comment for why the real
            // implementation's failure can't be forced directly).
            FORCE_AGENTS_DIR_ERROR.with(|f| f.set(true));
            let args = ListArgs {
                filter: "all".to_string(),
            };
            let result = execute(args).await;
            FORCE_AGENTS_DIR_ERROR.with(|f| f.set(false));

            let err = result.unwrap_err();
            assert!(
                err.to_string()
                    .contains("Could not determine home directory")
            );
        })
        .await;
    }

    // `execute`'s `std::env::current_dir().unwrap_or_default()` can only take
    // its `Err` arm in a real (if rare) TOCTOU scenario: the process's CWD is
    // removed out from under it. That's genuinely reproducible on Unix (not a
    // fake): create a directory, `chdir` into it, then delete it --
    // `current_dir()` then reliably returns an error. On Windows this same
    // sequence isn't reproducible: NTFS/Win32 refuse to remove a directory
    // that's a live process's current working directory (a sharing
    // violation), so `remove_dir_all` itself fails there instead of
    // succeeding -- confirmed via real Windows CI. Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn execute_falls_back_to_default_cwd_when_current_dir_is_gone() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-cwd-gone", |_fake_dir| async move {
            // `isolate_cwd_for_test` serializes against every other CWD-mutating
            // test in the crate and restores CWD automatically on drop, so it's
            // safe to hold across the `.await` below.
            let _guard = crate::config::isolate_cwd_for_test();
            let dir = std::env::temp_dir().join("lev-test-list-cwd-gone");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_current_dir(&dir).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();

            let args = ListArgs {
                filter: "all".to_string(),
            };
            let result = execute(args).await;

            assert!(result.is_ok());
        })
        .await;
    }

    /// Cross-platform companion to the Unix-only real-filesystem test above:
    /// forces [`resolve_cwd`]'s `Err` arm deterministically via
    /// [`FORCE_CWD_ERROR`] so `execute`'s `unwrap_or_default()` fallback is
    /// also exercised on Windows, where the real filesystem race isn't
    /// reproducible.
    #[tokio::test]
    async fn execute_falls_back_to_default_cwd_via_forced_error() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("list-cwd-forced", |_fake_dir| async move {
            FORCE_CWD_ERROR.with(|f| f.set(true));
            let args = ListArgs {
                filter: "all".to_string(),
            };
            let result = execute(args).await;
            FORCE_CWD_ERROR.with(|f| f.set(false));

            assert!(result.is_ok());
        })
        .await;
    }

    // ─── print_agent_listing (fully injectable) ─────────────────────────

    #[test]
    fn print_agent_listing_nothing_installed() {
        // The bundled catalog is always non-empty, so it must not count as
        // "you have agents" -- otherwise the get-started guidance would be
        // suppressed for exactly the fresh install that needs it.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let config = Config::default();

        let result = print_agent_listing(agents_dir.path(), cwd.path(), None, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_finds_installed_agent() {
        let agents_dir = tempfile::tempdir().unwrap();
        let sub = agents_dir.path().join("installed-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "installed-agent");

        let cwd = tempfile::tempdir().unwrap();
        let config = Config::default();

        let result = print_agent_listing(agents_dir.path(), cwd.path(), None, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_finds_local_manifest() {
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_manifest(cwd.path(), "local-agent");
        let config = Config::default();

        let result = print_agent_listing(agents_dir.path(), cwd.path(), None, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_local_manifest_invalid_is_skipped() {
        // The local-manifest section has its own `if let Some(info) = ...`
        // construct with no `else`; this exercises its false arm (an
        // existing but unparseable `agent.leviath` in the cwd), which
        // `print_agent_listing_finds_local_manifest` (valid manifest) never
        // reaches.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        fs::write(cwd.path().join("agent.leviath"), "not valid toml {{{{").unwrap();
        let config = Config::default();

        let result = print_agent_listing(agents_dir.path(), cwd.path(), None, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_finds_configured_path_agent() {
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let configured = tempfile::tempdir().unwrap();
        let sub = configured.path().join("configured-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "configured-agent");

        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };

        let result = print_agent_listing(agents_dir.path(), cwd.path(), None, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_finds_builtin_agents() {
        // An `agents/` directory beside the executable contributes a blueprint
        // the embedded catalog doesn't have, so it is appended to the list.
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let exe_dir = tempfile::tempdir().unwrap();
        let builtin_dir = exe_dir.path().join("agents");
        let sub = builtin_dir.join("builtin-agent");
        fs::create_dir_all(&sub).unwrap();
        write_manifest(&sub, "builtin-agent");
        let config = Config::default();

        let result =
            print_agent_listing(agents_dir.path(), cwd.path(), Some(exe_dir.path()), &config);
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_does_not_list_a_bundled_agent_twice() {
        // Running from a git checkout puts the *same* blueprints both in the
        // embedded catalog and in `<exe_dir>/agents`. Listing each one twice
        // would be pure noise, so the on-disk scan only appends what the
        // catalog doesn't already carry.
        let bundled = &crate::bundled::BUNDLED_AGENTS[0];
        let agents_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let exe_dir = tempfile::tempdir().unwrap();
        let sub = exe_dir.path().join("agents").join(bundled.name);
        fs::create_dir_all(&sub).unwrap();
        crate::bundled::install_bundled(bundled, &exe_dir.path().join("agents")).unwrap();
        let config = Config::default();

        let result =
            print_agent_listing(agents_dir.path(), cwd.path(), Some(exe_dir.path()), &config);

        assert!(result.is_ok());
        // The same name+version pair the catalog already holds resolves to one
        // entry, not two.
        let entry = format!("{} (v{})", bundled.name, bundled.version);
        let names: Vec<String> = crate::bundled::BUNDLED_AGENTS
            .iter()
            .map(|a| format!("{} (v{})", a.name, a.version))
            .collect();
        assert_eq!(names.iter().filter(|n| **n == entry).count(), 1);
    }

    #[test]
    fn print_agent_listing_all_sources_populated() {
        let agents_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(agents_dir.path().join("installed")).unwrap();
        write_manifest(&agents_dir.path().join("installed"), "installed");

        let cwd = tempfile::tempdir().unwrap();
        write_manifest(cwd.path(), "local");

        let configured = tempfile::tempdir().unwrap();
        fs::create_dir_all(configured.path().join("configured")).unwrap();
        write_manifest(&configured.path().join("configured"), "configured");

        let exe_dir = tempfile::tempdir().unwrap();
        let builtin_sub = exe_dir.path().join("agents").join("builtin");
        fs::create_dir_all(&builtin_sub).unwrap();
        write_manifest(&builtin_sub, "builtin");

        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };

        let result =
            print_agent_listing(agents_dir.path(), cwd.path(), Some(exe_dir.path()), &config);
        assert!(result.is_ok());
    }

    #[test]
    fn print_agent_listing_empty_descriptions_across_all_sources() {
        // Every section (installed / local / configured-path) has its own
        // "empty description -> no dash suffix" branch; the tests above only
        // ever exercise the non-empty path for all three, since
        // `write_manifest` hardcodes a non-empty description.
        let agents_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(agents_dir.path().join("installed")).unwrap();
        write_manifest_with_description(&agents_dir.path().join("installed"), "installed", "");

        let cwd = tempfile::tempdir().unwrap();
        write_manifest_with_description(cwd.path(), "local", "");

        let configured = tempfile::tempdir().unwrap();
        fs::create_dir_all(configured.path().join("configured")).unwrap();
        write_manifest_with_description(&configured.path().join("configured"), "configured", "");

        let config = Config {
            agent_paths: vec![configured.path().to_path_buf()],
            ..Config::default()
        };

        let result = print_agent_listing(agents_dir.path(), cwd.path(), None, &config);
        assert!(result.is_ok());
    }

    // ─── scan_directory: agent with empty description ────────────────────

    #[test]
    fn scan_directory_agent_with_empty_description() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("my-agent");
        fs::create_dir_all(&sub).unwrap();
        let content = r#"[agent]
name = "my-agent"
version = "2.0.0"
description = ""

[stages.main]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
description = "Main"
max_iterations = 5

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
"#;
        write_test_agent(sub, content);

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.description, "");
    }

    // ─── scan_directory: multiple subdirs with mixed manifests ──────────

    #[test]
    fn scan_directory_mixed_valid_and_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good");
        let bad = dir.path().join("bad");
        let empty = dir.path().join("empty");
        fs::create_dir_all(&good).unwrap();
        fs::create_dir_all(&bad).unwrap();
        fs::create_dir_all(&empty).unwrap();

        write_manifest(&good, "good-agent");
        fs::write(bad.join("agent.leviath"), "bad {{ toml").unwrap();

        let agents = scan_directory_for_agents(dir.path());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].1.name, "good-agent");
    }
}
