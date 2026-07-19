//! Coverage gate — runs `cargo llvm-cov` per workspace package and enforces a
//! hard 100% on regions, lines, and functions using llvm-cov's own
//! `--fail-under-*` thresholds. No custom parsing, merging, or aggregation.
//!
//! **Why per-package (and not `--workspace`).** `-C instrument-coverage` emits a
//! coverage record for *every* function in *every* binary that links it —
//! including binaries that never call it (rustc instruments unused functions).
//! A workspace has many test binaries, and a `pub fn` from one crate is linked
//! into every other crate's test binary. When `cargo llvm-cov --workspace`
//! merges them all, a never-run "unused function" record can shadow the covered
//! one, so genuinely-tested code reads below 100% (the phantom is codegen- and
//! OS-dependent, so it can't be closed with a test). Measuring one package at a
//! time keeps few binaries in each profile, where llvm-cov counts accurately.
//! This is the standard granularity for reliable Rust coverage, not a
//! workaround — see <https://github.com/llvm/llvm-project/issues/119558>.
//!
//! Each package is gated by a single `cargo llvm-cov --package <pkg>
//! --fail-under-{lines,functions,regions} 100` invocation, which also writes a
//! browsable HTML report to `coverage/html/<pkg>`. `--fail-under-* 100` on the
//! package total is equivalent to a per-file 100% gate (a total can only be
//! 100% if every file is). `src/main.rs` — the un-unit-tested bin composition
//! root — is excluded via `--ignore-filename-regex`.
//!
//! Branch coverage is intentionally not collected: `cargo llvm-cov --branch`
//! reliably SIGSEGVs on the same open upstream LLVM bug linked above.

use anyhow::{Context, Result};

// ── Runner trait (injectable for testing) ────────────────────────────────────

/// Abstraction over subprocess execution — inject a mock in tests.
pub trait Runner {
    /// Run `cargo <args>` and return whether it exited successfully.
    fn cargo(&self, args: &[&str]) -> Result<bool>;

    /// Run `cargo metadata --no-deps --format-version 1` and return the JSON.
    fn cargo_metadata(&self) -> Result<serde_json::Value>;
}

// ── Production runner ─────────────────────────────────────────────────────────

pub struct RealRunner;

impl Runner for RealRunner {
    fn cargo(&self, args: &[&str]) -> Result<bool> {
        Ok(std::process::Command::new("cargo")
            .args(args)
            .status()
            .expect("failed to spawn cargo — is cargo installed in PATH?")
            .success())
    }

    fn cargo_metadata(&self) -> Result<serde_json::Value> {
        let out = std::process::Command::new("cargo")
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .output()
            .expect("failed to spawn cargo metadata — is cargo installed in PATH?");
        if !out.status.success() {
            anyhow::bail!("cargo metadata exited non-zero");
        }
        serde_json::from_slice(&out.stdout).context("parsing cargo metadata JSON")
    }
}

// ── Coverage mode (CLI argument parsing) ──────────────────────────────────────

/// Which packages to gate, parsed from the arguments following
/// `cargo xtask coverage`.
#[derive(Debug, PartialEq, Eq)]
pub enum CoverageMode {
    /// No arguments: gate every workspace package (local-dev full check).
    All,
    /// `--package <pkg>`: gate exactly one package (the CI per-package fan-out —
    /// each package runs on its own runner, in parallel).
    Package(String),
}

impl CoverageMode {
    /// Parse the arguments that follow the `coverage` subcommand.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            None => Ok(Self::All),
            Some("--package") => {
                let pkg = args.get(1).context(
                    "`--package` requires a package name, e.g. \
                     `cargo xtask coverage --package leviath-core`",
                )?;
                Ok(Self::Package(pkg.clone()))
            }
            Some(other) => anyhow::bail!(
                "unknown `coverage` argument `{other}` (expected no arguments or \
                 `--package <pkg>`)"
            ),
        }
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

pub fn run(mode: CoverageMode) -> Result<()> {
    run_with(&RealRunner, mode)
}

pub fn run_with(runner: &dyn Runner, mode: CoverageMode) -> Result<()> {
    match mode {
        CoverageMode::All => run_all(runner),
        CoverageMode::Package(pkg) => gate_package(runner, &pkg),
    }
}

/// Gate every workspace package in turn (local-dev convenience). CI gates each
/// package on its own runner via `--package` instead.
fn run_all(runner: &dyn Runner) -> Result<()> {
    let meta = runner.cargo_metadata()?;
    let packages = parse_workspace_packages(&meta);
    for pkg in &packages {
        gate_package(runner, pkg)?;
    }
    println!("[coverage] All {} package(s) at 100%. ✓", packages.len());
    Ok(())
}

/// Gate a single package at a hard 100% (regions/lines/functions) via llvm-cov's
/// own `--fail-under-*` thresholds, writing its browsable HTML to
/// `coverage/html/<pkg>`. Returns an error if the package is below 100% (or its
/// build/tests fail).
fn gate_package(runner: &dyn Runner, pkg: &str) -> Result<()> {
    println!("[coverage] Gating {pkg} at 100%…");
    let html_dir = format!("coverage/html/{pkg}");
    let args = [
        "llvm-cov",
        "--package",
        pkg,
        "--all-features",
        // The bin (src/main.rs) is the un-unit-tested composition root; exclude
        // it on every OS (llvm-cov reports `...\src\main.rs` on Windows).
        "--ignore-filename-regex",
        r"[\\/]main\.rs$",
        "--fail-under-lines",
        "100",
        "--fail-under-functions",
        "100",
        "--fail-under-regions",
        "100",
        "--html",
        "--output-dir",
        &html_dir,
    ];
    if !runner.cargo(&args)? {
        anyhow::bail!(
            "[coverage] {pkg} is below 100% (or its build/tests failed). Open \
             {html_dir}/html/index.html to see exactly which lines are uncovered."
        );
    }
    Ok(())
}

/// Parse workspace package names from `cargo metadata` JSON. `xtask` itself is
/// excluded — it is the coverage tool, not measured by the gate.
pub fn parse_workspace_packages(meta: &serde_json::Value) -> Vec<String> {
    let members: std::collections::HashSet<String> = meta["workspace_members"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::to_owned)
        .collect();

    meta["packages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter(|p| p["id"].as_str().is_some_and(|id| members.contains(id)))
        .filter_map(|p| p["name"].as_str())
        .filter(|n| *n != "xtask")
        .map(str::to_owned)
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every `cargo` invocation and returns a configurable success flag.
    struct MockRunner {
        succeed: bool,
        calls: RefCell<Vec<Vec<String>>>,
        metadata: serde_json::Value,
    }

    impl MockRunner {
        fn new(succeed: bool, metadata: serde_json::Value) -> Self {
            Self {
                succeed,
                calls: RefCell::new(Vec::new()),
                metadata,
            }
        }
    }

    impl Runner for MockRunner {
        fn cargo(&self, args: &[&str]) -> Result<bool> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| (*s).to_owned()).collect());
            Ok(self.succeed)
        }
        fn cargo_metadata(&self) -> Result<serde_json::Value> {
            Ok(self.metadata.clone())
        }
    }

    /// Build a minimal `cargo metadata` JSON containing the given member names.
    fn meta_with(pkgs: &[&str]) -> serde_json::Value {
        let members: Vec<String> = pkgs
            .iter()
            .map(|p| format!("{p} 0.1.0 (path+file:///w/{p})"))
            .collect();
        let packages: Vec<serde_json::Value> = pkgs
            .iter()
            .zip(&members)
            .map(|(name, id)| serde_json::json!({ "id": id, "name": name }))
            .collect();
        serde_json::json!({ "workspace_members": members, "packages": packages })
    }

    /// True if `args` contains the consecutive pair `[a, b]`.
    fn has_pair(args: &[String], a: &str, b: &str) -> bool {
        args.windows(2).any(|w| w[0] == a && w[1] == b)
    }

    // ── CoverageMode::parse ───────────────────────────────────────────────────

    #[test]
    fn parse_no_args_is_all() {
        assert_eq!(CoverageMode::parse(&[]).unwrap(), CoverageMode::All);
    }

    #[test]
    fn parse_package_captures_name() {
        assert_eq!(
            CoverageMode::parse(&["--package".to_owned(), "leviath-core".to_owned()]).unwrap(),
            CoverageMode::Package("leviath-core".to_owned())
        );
    }

    #[test]
    fn parse_package_without_name_errs() {
        assert!(CoverageMode::parse(&["--package".to_owned()]).is_err());
    }

    #[test]
    fn parse_unknown_arg_errs() {
        assert!(CoverageMode::parse(&["--gate".to_owned()]).is_err());
    }

    // ── gate_package invocation ────────────────────────────────────────────────

    #[test]
    fn gate_package_builds_native_fail_under_invocation() {
        let m = MockRunner::new(true, meta_with(&[]));
        gate_package(&m, "leviath-core").unwrap();
        let calls = m.calls.borrow();
        assert_eq!(calls.len(), 1);
        let a = &calls[0];
        assert_eq!(a[0], "llvm-cov");
        assert!(has_pair(a, "--package", "leviath-core"));
        assert!(has_pair(a, "--fail-under-lines", "100"));
        assert!(has_pair(a, "--fail-under-functions", "100"));
        assert!(has_pair(a, "--fail-under-regions", "100"));
        assert!(a.iter().any(|s| s == r"[\\/]main\.rs$"));
        assert!(has_pair(a, "--output-dir", "coverage/html/leviath-core"));
    }

    #[test]
    fn gate_package_bails_when_below_100() {
        let m = MockRunner::new(false, meta_with(&[]));
        let err = gate_package(&m, "leviath-core").unwrap_err();
        assert!(err.to_string().contains("leviath-core"));
    }

    // ── run_all ────────────────────────────────────────────────────────────────

    #[test]
    fn run_all_gates_every_package_except_xtask() {
        let m = MockRunner::new(true, meta_with(&["leviath-core", "leviath-cli", "xtask"]));
        run_with(&m, CoverageMode::All).unwrap();
        let calls = m.calls.borrow();
        assert_eq!(calls.len(), 2, "one gate call per non-xtask package");
        assert!(
            calls
                .iter()
                .any(|a| has_pair(a, "--package", "leviath-core"))
        );
        assert!(
            calls
                .iter()
                .any(|a| has_pair(a, "--package", "leviath-cli"))
        );
        assert!(!calls.iter().any(|a| has_pair(a, "--package", "xtask")));
    }

    #[test]
    fn run_all_propagates_a_failing_package() {
        let m = MockRunner::new(false, meta_with(&["leviath-core"]));
        assert!(run_with(&m, CoverageMode::All).is_err());
    }

    // ── parse_workspace_packages ───────────────────────────────────────────────

    #[test]
    fn parse_workspace_packages_excludes_xtask() {
        let pkgs = parse_workspace_packages(&meta_with(&["leviath-core", "xtask", "leviath-cli"]));
        assert_eq!(
            pkgs,
            vec!["leviath-core".to_owned(), "leviath-cli".to_owned()]
        );
    }

    #[test]
    fn parse_workspace_packages_empty_metadata_is_empty() {
        assert!(parse_workspace_packages(&serde_json::json!({})).is_empty());
    }
}
