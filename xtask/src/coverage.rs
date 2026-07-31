//! Coverage gate - runs `cargo llvm-cov` per workspace package and enforces a
//! hard 100% on regions, lines, and functions using llvm-cov's own
//! `--fail-under-*` thresholds. No custom parsing, merging, or aggregation.
//!
//! **Why per-package (and not `--workspace`).** `-C instrument-coverage` emits a
//! coverage record for *every* function in *every* binary that links it -
//! including binaries that never call it (rustc instruments unused functions).
//! A workspace has many test binaries, and a `pub fn` from one crate is linked
//! into every other crate's test binary. When `cargo llvm-cov --workspace`
//! merges them all, a never-run "unused function" record can shadow the covered
//! one, so genuinely-tested code reads below 100% (the phantom is codegen- and
//! OS-dependent, so it can't be closed with a test). Measuring one package at a
//! time keeps few binaries in each profile, where llvm-cov counts accurately.
//! This is the standard granularity for reliable Rust coverage, not a
//! workaround - see <https://github.com/llvm/llvm-project/issues/119558>.
//!
//! Each package is gated by a single `cargo llvm-cov --package <pkg>
//! --fail-under-{lines,functions,regions} 100` invocation, which also writes a
//! browsable HTML report to `coverage/html/<pkg>`. `--fail-under-* 100` on the
//! package total is equivalent to a per-file 100% gate (a total can only be
//! 100% if every file is). `src/main.rs` - the un-unit-tested bin composition
//! root - is excluded via `--ignore-filename-regex`.
//!
//! Branch coverage is intentionally not collected: `cargo llvm-cov --branch`
//! reliably SIGSEGVs on the same open upstream LLVM bug linked above.

use anyhow::{Context, Result};

// ── Runner trait (injectable for testing) ────────────────────────────────────

/// Abstraction over subprocess execution - inject a mock in tests.
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
            .expect("failed to spawn cargo - is cargo installed in PATH?")
            .success())
    }

    fn cargo_metadata(&self) -> Result<serde_json::Value> {
        let out = std::process::Command::new("cargo")
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .output()
            .expect("failed to spawn cargo metadata - is cargo installed in PATH?");
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
    /// `--package <pkg>`: gate exactly one package (the CI per-package fan-out -
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
        // Say *what* is uncovered, not just that something is. "Open the HTML
        // report" is useless on a CI runner, whose filesystem nobody will ever
        // see - and a gap that only appears on one OS (a `#[cfg(target_os)]`
        // block, or a test the other platform skips) can only be diagnosed
        // from that machine's own run.
        let uncovered = uncovered_regions(runner, pkg).unwrap_or_default();
        if !uncovered.is_empty() {
            println!("[coverage] {pkg}: uncovered region entries (file:line:col):");
            for entry in &uncovered {
                println!("  {entry}");
            }
        }
        anyhow::bail!(
            "[coverage] {pkg} is below 100% (or its build/tests failed). \
             {html_dir}/html/index.html has the annotated source."
        );
    }
    Ok(())
}

/// Where the JSON export used by [`uncovered_regions`] is written.
const REPORT_JSON: &str = "coverage/uncovered.json";

/// The `file:line:col` of every region entry llvm-cov counted zero times.
///
/// Re-exports the *same* profile data the gate just measured (no rebuild, no
/// re-run) as JSON and reads the region entries out of it. Best effort: if the
/// export or the parse fails, the caller still reports the failure, just without
/// the detail.
fn uncovered_regions(runner: &dyn Runner, pkg: &str) -> Option<Vec<String>> {
    let ok = runner
        .cargo(&[
            "llvm-cov",
            "report",
            "--package",
            pkg,
            // No `--all-features` here: `report` re-reads the profile data the
            // gate just produced and rejects the flag outright.
            "--ignore-filename-regex",
            r"[\\/]main\.rs$",
            "--json",
            "--output-path",
            REPORT_JSON,
        ])
        .ok()?;
    if !ok {
        return None;
    }
    let text = std::fs::read_to_string(REPORT_JSON).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(parse_uncovered(&json))
}

/// Pull zero-count region entries out of an llvm-cov JSON export.
///
/// A `segments` entry is `[line, col, count, has_count, is_region_entry,
/// is_gap]`. A region *entry* with a count of zero is the start of code that
/// never ran - which is exactly what the gate is complaining about. Non-entry
/// segments and gap regions are noise here.
pub fn parse_uncovered(json: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let files = json["data"][0]["files"].as_array();
    for file in files.into_iter().flatten() {
        let Some(name) = file["filename"].as_str() else {
            continue;
        };
        // Trim to something readable: the workspace-relative tail. Both
        // separators, because llvm-cov reports `...\src\lib.rs` on Windows and
        // that is exactly where these one-platform gaps show up.
        let short = name
            .rsplit_once("/src/")
            .or_else(|| name.rsplit_once("\\src\\"))
            .map_or(name, |(_, tail)| tail);
        let mut seen = std::collections::BTreeSet::new();
        for seg in file["segments"].as_array().into_iter().flatten() {
            let s = seg.as_array().filter(|s| s.len() >= 6);
            let Some(s) = s else { continue };
            let is_entry = s[4].as_bool().unwrap_or(false);
            let has_count = s[3].as_bool().unwrap_or(false);
            let count = s[2].as_u64().unwrap_or(1);
            if is_entry && has_count && count == 0 {
                seen.insert((s[0].as_u64().unwrap_or(0), s[1].as_u64().unwrap_or(0)));
            }
        }
        for (line, col) in seen {
            out.push(format!("{short}:{line}:{col}"));
        }
    }
    out
}

/// Parse workspace package names from `cargo metadata` JSON. Three members
/// are excluded from the gate: `xtask` (the coverage tool itself),
/// `leviath-testkit` (dev-dependency-only test scaffolding whose every line
/// executes inside other packages' gated suites - self-gating it at 100%
/// would force tests-of-test-helpers with no defect-finding power), and
/// `leviath` (the re-export-only crates.io facade: it has zero executable
/// regions, so llvm-cov has nothing to count; CI's `guard-facade` job is what
/// keeps executable code from ever landing there).
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
        .filter(|n| *n != "xtask" && *n != "leviath-testkit" && *n != "leviath")
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

    /// The gate's failure message is only useful if it names real regions.
    /// A segment is `[line, col, count, has_count, is_region_entry, is_gap]`;
    /// only a *region entry* with a recorded count of zero is code that never
    /// ran.
    #[test]
    fn parse_uncovered_reports_only_zero_count_region_entries() {
        let json = serde_json::json!({
            "data": [{
                "files": [{
                    "filename": "/w/crates/leviath-sys/src/perms.rs",
                    "segments": [
                        [191, 1, 0, true, true, false],   // uncovered entry
                        [192, 8, 0, true, true, false],   // uncovered entry
                        [191, 1, 0, true, true, false],   // duplicate, collapsed
                        [200, 4, 7, true, true, false],   // covered
                        [210, 2, 0, true, false, false],  // not a region entry
                        [220, 2, 0, false, true, false],  // no recorded count
                        [230, 1],                          // malformed, skipped
                    ]
                }]
            }]
        });
        assert_eq!(
            parse_uncovered(&json),
            vec!["perms.rs:191:1".to_string(), "perms.rs:192:8".to_string()]
        );
    }

    /// A report with nothing uncovered says nothing, and a shape that is not an
    /// llvm-cov export does not panic.
    #[test]
    fn parse_uncovered_is_quiet_when_there_is_nothing_to_report() {
        let covered = serde_json::json!({
            "data": [{"files": [{
                "filename": "/w/crates/x/src/a.rs",
                "segments": [[1, 1, 3, true, true, false]]
            }]}]
        });
        assert!(parse_uncovered(&covered).is_empty());
        assert!(parse_uncovered(&serde_json::json!({})).is_empty());
        assert!(parse_uncovered(&serde_json::json!({"data": []})).is_empty());
    }

    /// A filename with no `/src/` segment is reported whole rather than
    /// silently dropped.
    /// llvm-cov reports Windows paths with backslashes, and a one-platform gap
    /// is exactly the case this output exists for - so the shortening has to
    /// handle both separators.
    #[test]
    fn parse_uncovered_shortens_a_windows_path() {
        let json = serde_json::json!({
            "data": [{"files": [{
                "filename": "D:\\a\\leviath\\crates\\leviath-tools\\src\\lib.rs",
                "segments": [[803, 13, 0, true, true, false]]
            }]}]
        });
        assert_eq!(parse_uncovered(&json), vec!["lib.rs:803:13".to_string()]);
    }

    #[test]
    fn parse_uncovered_keeps_a_path_it_cannot_shorten() {
        let json = serde_json::json!({
            "data": [{"files": [{
                "filename": "build.rs",
                "segments": [[4, 2, 0, true, true, false]]
            }]}]
        });
        assert_eq!(parse_uncovered(&json), vec!["build.rs:4:2".to_string()]);
    }

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
    fn run_all_gates_every_package_except_the_ungated_members() {
        let m = MockRunner::new(
            true,
            meta_with(&[
                "leviath-core",
                "leviath-cli",
                "xtask",
                "leviath-testkit",
                "leviath",
            ]),
        );
        run_with(&m, CoverageMode::All).unwrap();
        let calls = m.calls.borrow();
        assert_eq!(calls.len(), 2, "one gate call per gated package");
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
        assert!(
            !calls
                .iter()
                .any(|a| has_pair(a, "--package", "leviath-testkit"))
        );
        assert!(!calls.iter().any(|a| has_pair(a, "--package", "leviath")));
    }

    #[test]
    fn run_all_propagates_a_failing_package() {
        let m = MockRunner::new(false, meta_with(&["leviath-core"]));
        assert!(run_with(&m, CoverageMode::All).is_err());
    }

    // ── parse_workspace_packages ───────────────────────────────────────────────

    #[test]
    fn parse_workspace_packages_excludes_the_ungated_members() {
        let pkgs = parse_workspace_packages(&meta_with(&[
            "leviath-core",
            "xtask",
            "leviath-cli",
            "leviath-testkit",
            "leviath",
        ]));
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
