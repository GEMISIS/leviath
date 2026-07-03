//! Integration tests that spawn the built `lev` binary to exercise
//! `main.rs`'s top-level argument parsing / tracing-init / command-dispatch
//! path end-to-end, without ever touching the real `~/.leviath`, real user
//! config, or making a billed inference call.
//!
//! Only genuinely safe, side-effect-free, non-interactive subcommands are
//! invoked here: `--version`, `list`, `models list`, and `validate <fixture>`.
//!
//! This deliberately never spawns `dash`/`dashboard`, `run` (foreground),
//! `serve`, or `__run-worker` — those touch real terminal/TTY/stdin/
//! subprocess state and are intentionally left untested via real process
//! spawns elsewhere in this codebase for safety reasons.

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
fn lev_command(tmp_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lev"));
    cmd.env("HOME", tmp_home)
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
