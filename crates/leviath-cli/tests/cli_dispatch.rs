//! Integration tests that spawn the built `lev` binary to exercise
//! `main.rs`'s top-level argument parsing / tracing-init / command-dispatch
//! path end-to-end, without ever touching the real `~/.leviath`, real user
//! config, or making a billed inference call.
//!
//! Only genuinely safe, side-effect-free, non-interactive subcommands are
//! invoked here: `--version`, `list`, `models list`, `validate <fixture>`,
//! `create <name>`, `setup --non-interactive`, `add <local-dir>`,
//! `remove <name>`, `test <path> --dry-run`, and `pack <local-dir>`.
//!
//! This deliberately never spawns `dash`/`dashboard`, `run` (foreground),
//! `serve`, or `__run-worker` — those touch real terminal/TTY/stdin/
//! subprocess state and are intentionally left untested via real process
//! spawns elsewhere in this codebase for safety reasons. It also never
//! spawns `setup` without `--non-interactive` (blocks on real stdin), nor
//! `add`/`test` in a way that would hit a package registry or a real
//! provider API (both would require network access / billed calls).

use std::path::PathBuf;
use std::process::Command;

/// Get the workspace root (assumes tests run from the leviath-cli crate).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Build a `Command` for the real `lev` binary with a fully isolated
/// environment: a fake `HOME` (so `list`'s `~/.leviath/agents` scan and any
/// `dirs::home_dir()` lookup never touch the real home directory), an
/// isolated `LEVIATH_CONFIG_PATH`/`LEVIATH_SKIP_DOTENV` (so `Config::load()`
/// never reads a real config file or repo-root `.env`), an isolated
/// `LEVIATH_RUNS_DIR`/`LEVIATH_DASHBOARD_LOG_PATH`, and no provider API keys
/// (so nothing here can ever make a real, billed inference call), all
/// mirroring the isolation helpers used by in-process tests
/// (`isolate_config_path_for_test` / `isolate_runs_dir_for_test`) but via
/// `Command::env` so it can't race other tests over process-global env vars.
///
/// Sets `HOME`, `USERPROFILE`, AND `LEVIATH_HOME`: `dirs::home_dir()` does
/// not read *any* environment variable on macOS (`NSHomeDirectory()`) or
/// Windows (`SHGetKnownFolderPath`) -- confirmed via real Windows CI
/// failures in `add`/`remove` even after overriding `HOME`+`USERPROFILE`.
/// `LEVIATH_HOME` (`crate::config::leviath_home_dir()`, and a matching
/// local override inside `leviath-package`'s `AgentInstaller::new()`) is
/// the actual mechanism that redirects every `~/.leviath/...`-rooted path
/// this binary uses; `HOME`/`USERPROFILE` are kept too since some
/// lower-level dependencies may still consult them directly.
fn lev_command(tmp_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lev"));
    cmd.env("HOME", tmp_home)
        .env("USERPROFILE", tmp_home)
        .env("LEVIATH_HOME", tmp_home)
        .env("LEVIATH_CONFIG_PATH", tmp_home.join("config.toml"))
        .env("LEVIATH_SKIP_DOTENV", "1")
        .env("LEVIATH_RUNS_DIR", tmp_home.join("runs"))
        .env("LEVIATH_DASHBOARD_LOG_PATH", tmp_home.join("dashboard.log"))
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GOOGLE_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .current_dir(tmp_home);
    cmd
}

#[test]
fn version_flag_prints_version_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let output = lev_command(tmp.path())
        .arg("--version")
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lev"),
        "unexpected --version output: {stdout}"
    );
}

#[test]
fn list_subcommand_dispatches_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let output = lev_command(tmp.path())
        .arg("list")
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn verbose_flag_selects_debug_log_level_and_still_dispatches() {
    // Drives `main`'s `let level = if cli.verbose { "debug" } else { "info" };`
    // `true` arm for real -- every other test here omits `--verbose`, so
    // that arm was otherwise never exercised. `list` is a safe, ordinary
    // subcommand to pair it with.
    let tmp = tempfile::tempdir().unwrap();
    let output = lev_command(tmp.path())
        .arg("--verbose")
        .arg("list")
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn models_list_subcommand_dispatches_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let output = lev_command(tmp.path())
        .args(["models", "list"])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty());
}

#[test]
fn validate_subcommand_dispatches_and_exits_zero_for_valid_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let manifest = workspace_root()
        .join("agents")
        .join("coder")
        .join("agent.leviath");
    assert!(manifest.exists(), "fixture manifest missing: {manifest:?}");

    let output = lev_command(tmp.path())
        .args(["validate", manifest.to_str().unwrap()])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("valid"),
        "unexpected validate output: {stdout}"
    );
}

// ─── create ────────────────────────────────────────────────────────────
//
// `lev create <name>` only does local filesystem writes (creates a
// directory + a few files under the current directory) -- no network, no
// interactivity. Safe to spawn for real with `current_dir` pinned to the
// isolated tmpdir (see `lev_command`).

#[test]
fn create_subcommand_dispatches_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let output = lev_command(tmp.path())
        .args(["create", "my-new-agent"])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(tmp.path().join("my-new-agent/agent.leviath").exists());
}

// ─── setup --non-interactive ────────────────────────────────────────────
//
// `lev setup --non-interactive` never touches stdin -- it only applies the
// given flag values and saves to `Config::config_path()`, which is
// redirected to the isolated `LEVIATH_CONFIG_PATH` by `lev_command`. The
// plain interactive path (real stdin prompts) is deliberately never
// exercised here.

#[test]
fn setup_non_interactive_subcommand_dispatches_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let output = lev_command(tmp.path())
        .args([
            "setup",
            "--non-interactive",
            "--anthropic-key",
            "sk-ant-fake-test-key",
        ])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();
    assert!(saved.contains("sk-ant-fake-test-key"));
}

// ─── add (local directory) ──────────────────────────────────────────────
//
// `lev add <local-dir>` copies a plain agent directory into
// `<HOME>/.leviath/agents/<name>` -- no network call. `HOME` is redirected
// to the isolated tmpdir by `lev_command`, so this never touches the real
// `~/.leviath`. Only the local-path variant is exercised; a bare package
// name would fall through to the registry-installation branch, which makes
// a real network call and is deliberately never spawned here.

#[test]
fn add_subcommand_installs_from_local_directory_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let coder_agent = workspace_root().join("agents").join("coder");
    assert!(
        coder_agent.exists(),
        "fixture agent missing: {coder_agent:?}"
    );

    let output = lev_command(tmp.path())
        .args(["add", coder_agent.to_str().unwrap()])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        tmp.path()
            .join(".leviath")
            .join("agents")
            .join("coder")
            .join("agent.leviath")
            .exists()
    );
}

// ─── remove ──────────────────────────────────────────────────────────────
//
// `remove_agent` has no interactive confirmation prompt (see
// `commands/remove.rs`) -- it just verifies the agent is installed and
// deletes its directory. Installs a fixture agent first (via `add`) into
// the isolated `HOME`, then removes it with the same isolated environment.

#[test]
fn remove_subcommand_removes_installed_agent_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let coder_agent = workspace_root().join("agents").join("coder");

    let add_output = lev_command(tmp.path())
        .args(["add", coder_agent.to_str().unwrap()])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        add_output.status.success(),
        "add stderr: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let installed_dir = tmp.path().join(".leviath").join("agents").join("coder");
    assert!(installed_dir.exists());

    let remove_output = lev_command(tmp.path())
        .args(["remove", "coder"])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        remove_output.status.success(),
        "remove stderr: {}",
        String::from_utf8_lossy(&remove_output.stderr)
    );
    assert!(!installed_dir.exists());
}

// ─── test (dry-run / no-tests-dir) ──────────────────────────────────────
//
// The `agents/coder` fixture has no `tests/` directory, so `execute()`
// returns early (prints "No tests directory found") before ever calling
// `Config::load()` or building a provider registry -- no network calls are
// possible on this path regardless of `--dry-run`. Pass `--dry-run` anyway
// for defense in depth in case the fixture ever grows a `tests/` dir.

#[test]
fn test_subcommand_dry_run_dispatches_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let coder_agent = workspace_root().join("agents").join("coder");

    let output = lev_command(tmp.path())
        .args(["test", coder_agent.to_str().unwrap(), "--dry-run"])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ─── pack ────────────────────────────────────────────────────────────────
//
// `lev pack <local-dir>` bundles a local agent directory into a
// `.leviath-bundle` archive on disk -- no network, no interactivity.

#[test]
fn pack_subcommand_dispatches_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let coder_agent = workspace_root().join("agents").join("coder");
    let output_bundle = tmp.path().join("out.leviath-bundle");

    let output = lev_command(tmp.path())
        .args([
            "pack",
            coder_agent.to_str().unwrap(),
            "--output",
            output_bundle.to_str().unwrap(),
        ])
        .output()
        .expect("failed to spawn lev binary");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_bundle.exists());
}
