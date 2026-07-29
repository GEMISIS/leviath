//! Integration tests for the `xtask` binary.
//!
//! These tests spawn the compiled `xtask` binary and verify its exit codes and
//! output - end-to-end coverage of the `main()` entry point and the
//! `dispatch()` router, which cannot be reached by unit tests alone.
//!
//! `env!("CARGO_BIN_EXE_xtask")` resolves to the path of the compiled binary at
//! compile time; cargo-llvm-cov instruments the binary and captures its profile
//! data, so coverage from these spawned child processes is attributed back to
//! the source files.

use std::process::Command;

/// Path to the compiled `xtask` binary - set by Cargo at build time.
fn xtask_bin() -> &'static str {
    env!("CARGO_BIN_EXE_xtask")
}

/// Run `xtask` with the given args and return (exit_success, stdout, stderr).
fn run_xtask(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(xtask_bin())
        .args(args)
        .output()
        .expect("failed to spawn xtask binary");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), stdout, stderr)
}

// ── help subcommand ──────────────────────────────────────────────────────────

#[test]
fn integration_help_exits_zero() {
    let (ok, stdout, _) = run_xtask(&["help"]);
    assert!(ok, "xtask help should exit 0");
    assert!(
        stdout.contains("cargo xtask"),
        "help output should mention 'cargo xtask', got: {stdout}"
    );
}

#[test]
fn integration_help_flag_exits_zero() {
    let (ok, stdout, _) = run_xtask(&["--help"]);
    assert!(ok, "xtask --help should exit 0");
    assert!(
        stdout.contains("coverage"),
        "help should list coverage subcommand"
    );
}

#[test]
fn integration_short_help_flag_exits_zero() {
    let (ok, _, _) = run_xtask(&["-h"]);
    assert!(ok, "xtask -h should exit 0");
}

#[test]
fn integration_no_args_defaults_to_help() {
    // When invoked with no arguments `dispatch` defaults to "help".
    let (ok, stdout, _) = run_xtask(&[]);
    assert!(ok, "xtask with no args should exit 0 (help)");
    assert!(
        stdout.contains("Subcommands:"),
        "output should show subcommands, got: {stdout}"
    );
}

// ── Unknown subcommands ──────────────────────────────────────────────────────

#[test]
fn integration_unknown_subcommand_exits_nonzero() {
    let (ok, _, stderr) = run_xtask(&["unknown-subcommand-xyz"]);
    assert!(!ok, "unknown subcommand should exit non-zero");
    assert!(
        stderr.contains("unknown-subcommand-xyz") || stderr.contains("Unknown"),
        "stderr should mention the bad subcommand, got: {stderr}"
    );
}

#[test]
fn integration_misspelled_subcommand_exits_nonzero() {
    let (ok, _, _) = run_xtask(&["coverages"]);
    assert!(!ok, "misspelled subcommand should exit non-zero");
}

// ── coverage fast-fail path (workspace has no tests dir) ────────────────────

#[test]
fn integration_coverage_from_empty_dir_exits_nonzero() {
    // Run `xtask coverage` from a fresh temp directory that has no Cargo.toml.
    // cargo-llvm-cov will fail quickly because there is nothing to compile.
    // This exercises the error path of `coverage::run()` without actually
    // running the full suite.
    let dir = tempfile::tempdir().expect("creating tempdir");
    let output = Command::new(xtask_bin())
        .arg("coverage")
        .current_dir(dir.path())
        .output()
        .expect("spawning xtask coverage");
    assert!(
        !output.status.success(),
        "coverage should fail in an empty directory (no Cargo.toml)"
    );
}
