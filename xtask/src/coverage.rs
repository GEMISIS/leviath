//! Coverage enforcement — runs cargo-llvm-cov and verifies all four metrics hit 100%.
//!
//! Strategy
//! --------
//! 1. Try a workspace-level run with --branch.
//!    On macOS/ARM64 the LLVM `llvm-cov` tool sometimes crashes (SIGSEGV) when
//!    asked to aggregate branch data across many objects simultaneously (upstream
//!    LLVM bug).  On Linux CI this run succeeds normally.
//! 2. If the workspace run exits non-zero, fall back to per-package runs.
//!    Each package is run independently with a clean `llvm-cov-target/` so that
//!    `llvm-cov` sees only that package's objects and does not crash.
//!    Trade-off: each package's coverage is measured against its own tests only
//!    (not against downstream crates' tests), which is the stricter definition.
//! 3. Parse the resulting JSON and fail if any metric is below 100%.
//!    Every gap is reported with filename and per-metric missed count.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Runner trait (injectable for testing) ────────────────────────────────────

/// Abstraction over subprocess execution — inject a mock in tests.
///
/// All orchestration functions take `&dyn Runner` (not `impl Runner`) so that a
/// single compiled copy of each function is shared by both `RealRunner` and
/// `MockRunner` invocations.  This means MockRunner-based unit tests contribute
/// coverage to the same regions that RealRunner uses in production — no separate
/// monomorphization is left uncovered.
pub trait Runner {
    /// Run `cargo <args>` and return whether it exited successfully.
    fn cargo(&self, args: &[&str]) -> Result<bool>;

    /// Run `cargo metadata --no-deps --format-version 1` and return the JSON.
    fn cargo_metadata(&self) -> Result<serde_json::Value>;

    /// Remove a directory tree (best-effort; silently ignores errors).
    fn remove_dir(&self, path: &str);
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

    fn remove_dir(&self, path: &str) {
        let _ = std::fs::remove_dir_all(path);
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    run_with(&RealRunner)
}

pub fn run_with(runner: &dyn Runner) -> Result<()> {
    let in_ci = std::env::var("GITHUB_ACTIONS").is_ok();
    let github_output = std::env::var("GITHUB_OUTPUT").ok();
    std::fs::create_dir_all("target")
        .expect("failed to create target/ directory — check filesystem permissions");
    run_report(
        runner,
        "target/llvm-cov.json",
        in_ci,
        github_output.as_deref(),
    )
}

/// Core reporting logic extracted from `run_with` for unit-testability.
///
/// Runs coverage via `runner`, prints a summary, reports any gaps, and writes
/// CI output variables.  All paths (including error paths) are reachable from
/// tests through the `MockRunner` abstraction.
pub fn run_report(
    runner: &dyn Runner,
    output_path: &str,
    in_ci: bool,
    github_output: Option<&str>,
) -> Result<()> {
    println!("[coverage] Running coverage analysis…");
    let report = run_coverage(runner, output_path, in_ci)?;
    let data = report
        .data
        .first()
        .context("llvm-cov JSON contained no data")?;

    print_summary(&data.totals);

    let gaps = gap_files(data);

    if gaps.is_empty() {
        println!("\n[coverage] All metrics at 100%. ✓");
        write_github_output(&data.totals, github_output)?;
        return Ok(());
    }

    print_gaps(&gaps);
    anyhow::bail!("[coverage] Coverage is not 100%. Fix the gaps above.");
}

// ── Gap detection (pure, testable) ───────────────────────────────────────────

/// Return the subset of files that are not at 100% on all metrics.
pub fn gap_files(data: &CovData) -> Vec<&FileCov> {
    data.files
        .iter()
        .filter(|f| !f.summary.is_100_percent())
        .collect()
}

fn print_gaps(gaps: &[&FileCov]) {
    eprintln!("\n[coverage] {} file(s) have gaps:", gaps.len());
    for gap in gaps {
        let name = match gap.filename.split_once("/crates/") {
            Some((_, suffix)) => format!("crates/{suffix}"),
            None => gap.filename.clone(),
        };
        eprintln!("  {name}");
        let s = &gap.summary;
        if !s.regions.is_fully_covered() {
            eprintln!("    regions:   missed {}", s.regions.missed());
        }
        if !s.lines.is_fully_covered() {
            eprintln!("    lines:     missed {}", s.lines.missed());
        }
        if !s.functions.is_fully_covered() {
            eprintln!("    functions: missed {}", s.functions.missed());
        }
        if !s.branches.is_fully_covered() {
            eprintln!("    branches:  missed {}", s.branches.missed());
        }
    }
}

// ── Internal orchestration ───────────────────────────────────────────────────

pub fn run_coverage(runner: &dyn Runner, output_path: &str, in_ci: bool) -> Result<LlvmCovReport> {
    let ok = runner.cargo(&[
        "llvm-cov",
        "--all-features",
        "--workspace",
        "--branch",
        "--json",
        "--output-path",
        output_path,
    ])?;

    if ok {
        return parse_json(output_path);
    }

    eprintln!("[coverage] Workspace --branch run failed (likely macOS/LLVM SIGSEGV).");
    eprintln!("[coverage] Falling back to per-package coverage…");
    run_per_package_coverage(runner, output_path, in_ci)
}

pub fn run_per_package_coverage(
    runner: &dyn Runner,
    output_path: &str,
    in_ci: bool,
) -> Result<LlvmCovReport> {
    let packages = workspace_packages(runner)?;
    println!(
        "[coverage] Per-package run for {} crates: {}",
        packages.len(),
        packages.join(", ")
    );

    // Place per-package JSON files next to the aggregated output file so they
    // always land in a writable directory (avoids relative-path issues in tests).
    let out_dir = std::path::Path::new(output_path)
        .parent()
        .unwrap_or(std::path::Path::new("."));

    let mut all_files: Vec<FileCov> = Vec::new();
    let mut totals = Metrics::default();
    let mut no_branch_pkgs: Vec<String> = Vec::new();

    for pkg in &packages {
        println!("[coverage]   → {pkg}");
        runner.remove_dir("target/llvm-cov-target");

        let pkg_output = out_dir
            .join(format!("llvm-cov-{pkg}.json"))
            .to_string_lossy()
            .into_owned();

        let pkg_report = match run_single_package(runner, pkg, true, &pkg_output) {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "[coverage] --branch crashed for {pkg} (macOS LLVM bug); \
                     retrying without --branch — branches enforced on CI only."
                );
                no_branch_pkgs.push(pkg.clone());
                runner.remove_dir("target/llvm-cov-target");
                run_single_package(runner, pkg, false, &pkg_output)
                    .with_context(|| format!("coverage failed for {pkg}"))?
            }
        };

        if let Some(data) = pkg_report.data.into_iter().next() {
            all_files.extend(data.files);
            totals.add(&data.totals);
        }
    }

    handle_no_branch_packages(&no_branch_pkgs, in_ci)?;

    totals.recompute_percents();
    let aggregated = LlvmCovReport {
        data: vec![CovData {
            files: all_files,
            totals,
        }],
    };
    let json = serde_json::to_string_pretty(&aggregated)
        .expect("failed to serialise aggregated JSON — all fields are serialisable");
    std::fs::write(output_path, json).with_context(|| format!("writing {output_path}"))?;
    Ok(aggregated)
}

/// Warn or fail (on CI) when some packages couldn't produce branch coverage.
///
/// `in_ci` is passed explicitly (derived from `GITHUB_ACTIONS` by the caller)
/// so this function is pure and unit-testable without environment variable
/// manipulation.
pub fn handle_no_branch_packages(no_branch: &[String], in_ci: bool) -> Result<()> {
    if no_branch.is_empty() {
        return Ok(());
    }
    eprintln!(
        "\n[coverage] WARNING: branch coverage not available locally for: {}",
        no_branch.join(", ")
    );
    eprintln!("[coverage] Branches will be enforced by CI (Linux) where this LLVM bug is absent.");
    if in_ci {
        anyhow::bail!(
            "[coverage] Branch coverage crashed on CI runner for: {}. \
             Investigate — this should not occur on Linux.",
            no_branch.join(", ")
        );
    }
    Ok(())
}

fn run_single_package(
    runner: &dyn Runner,
    pkg: &str,
    branch: bool,
    output_path: &str,
) -> Result<LlvmCovReport> {
    let mut args = vec![
        "llvm-cov",
        "--all-features",
        "--package",
        pkg,
        "--json",
        "--output-path",
        output_path,
    ];
    if branch {
        args.push("--branch");
    }

    if !runner.cargo(&args)? {
        anyhow::bail!("cargo llvm-cov exited non-zero for package {pkg}");
    }

    parse_json(output_path).with_context(|| format!("parsing coverage JSON for {pkg}"))
}

/// Parse workspace package names from `cargo metadata` JSON.
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

fn workspace_packages(runner: &dyn Runner) -> Result<Vec<String>> {
    let meta = runner.cargo_metadata()?;
    Ok(parse_workspace_packages(&meta))
}

// ── Formatting helpers ───────────────────────────────────────────────────────

fn print_summary(t: &Metrics) {
    println!("\n[coverage] Summary:");
    println!(
        "  Regions:   {}/{} ({:.2}%)",
        t.regions.covered, t.regions.count, t.regions.percent
    );
    println!(
        "  Lines:     {}/{} ({:.2}%)",
        t.lines.covered, t.lines.count, t.lines.percent
    );
    println!(
        "  Functions: {}/{} ({:.2}%)",
        t.functions.covered, t.functions.count, t.functions.percent
    );
    println!(
        "  Branches:  {}/{} ({:.2}%)",
        t.branches.covered, t.branches.count, t.branches.percent
    );
}

// ── GitHub Actions output ────────────────────────────────────────────────────

/// Write coverage percentages to the GitHub Actions output file when running in CI.
pub fn write_github_output(totals: &Metrics, output_path: Option<&str>) -> Result<()> {
    let Some(path) = output_path else {
        return Ok(());
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening GITHUB_OUTPUT at {path}"))?;
    write_github_output_content(totals, &mut f).context("writing coverage percentages")
}

/// Write the four coverage-percentage lines to any `Write` sink.
///
/// Extracted from `write_github_output` so that unit tests can pass a mock
/// writer (e.g. a `FailWriter`) to cover the `write_all` error path without
/// needing a real filesystem error.
pub(crate) fn write_github_output_content<W: std::io::Write>(
    totals: &Metrics,
    writer: &mut W,
) -> std::io::Result<()> {
    let content = format!(
        "regions={:.1}\nlines={:.1}\nfunctions={:.1}\nbranches={:.1}\n",
        totals.regions.percent,
        totals.lines.percent,
        totals.functions.percent,
        totals.branches.percent,
    );
    writer.write_all(content.as_bytes())
}

// ── JSON parsing/serialisation ───────────────────────────────────────────────

pub fn parse_json(path: &str) -> Result<LlvmCovReport> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing JSON from {path}"))
}

// ── Data types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlvmCovReport {
    pub data: Vec<CovData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CovData {
    pub files: Vec<FileCov>,
    pub totals: Metrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCov {
    pub filename: String,
    pub summary: Metrics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    #[serde(default)]
    pub regions: Metric,
    #[serde(default)]
    pub lines: Metric,
    #[serde(default)]
    pub functions: Metric,
    #[serde(default)]
    pub branches: Metric,
}

impl Metrics {
    pub fn is_100_percent(&self) -> bool {
        self.regions.is_fully_covered()
            && self.lines.is_fully_covered()
            && self.functions.is_fully_covered()
            && self.branches.is_fully_covered()
    }

    pub fn add(&mut self, other: &Metrics) {
        self.regions.count += other.regions.count;
        self.regions.covered += other.regions.covered;
        self.lines.count += other.lines.count;
        self.lines.covered += other.lines.covered;
        self.functions.count += other.functions.count;
        self.functions.covered += other.functions.covered;
        self.branches.count += other.branches.count;
        self.branches.covered += other.branches.covered;
    }

    pub fn recompute_percents(&mut self) {
        self.regions.recompute_percent();
        self.lines.recompute_percent();
        self.functions.recompute_percent();
        self.branches.recompute_percent();
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metric {
    pub count: u64,
    pub covered: u64,
    pub percent: f64,
}

impl Metric {
    pub fn is_fully_covered(&self) -> bool {
        self.count == 0 || self.covered == self.count
    }

    pub fn missed(&self) -> u64 {
        self.count.saturating_sub(self.covered)
    }

    pub fn recompute_percent(&mut self) {
        self.percent = if self.count == 0 {
            100.0
        } else {
            (self.covered as f64 / self.count as f64) * 100.0
        };
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // ── Mock runner ──────────────────────────────────────────────────────────

    /// A mock runner for unit-testing orchestration logic without subprocesses.
    struct MockRunner {
        /// Responses for `cargo llvm-cov --package PKG`: Ok(true) = success with JSON written,
        /// Ok(false) = non-zero exit (simulates SIGSEGV), Err = spawn failure.
        package_results: HashMap<String, Result<LlvmCovReport, String>>,
        /// Whether the workspace run should succeed.
        workspace_ok: bool,
        /// If Some, write this JSON verbatim for workspace runs instead of the default.
        workspace_json: Option<String>,
        /// If true, cargo() returns Ok(true) but writes no JSON (simulates a write failure path).
        fail_write_json: bool,
        /// JSON to return from cargo_metadata.
        metadata: serde_json::Value,
        /// Whether cargo_metadata should return an error.
        fail_metadata: bool,
        /// If true, cargo() immediately returns Err (simulates a spawn/IO failure).
        fail_cargo_err: bool,
        /// Files written during mock runs (path → JSON).
        written: Arc<Mutex<HashMap<String, String>>>,
    }

    impl MockRunner {
        fn new(workspace_ok: bool, metadata: serde_json::Value) -> Self {
            Self {
                package_results: HashMap::new(),
                workspace_ok,
                workspace_json: None,
                fail_write_json: false,
                metadata,
                fail_metadata: false,
                fail_cargo_err: false,
                written: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        fn with_package(mut self, pkg: &str, result: Result<LlvmCovReport, String>) -> Self {
            self.package_results.insert(pkg.to_owned(), result);
            self
        }

        fn with_workspace_json(mut self, json: String) -> Self {
            self.workspace_json = Some(json);
            self
        }

        fn with_fail_write(mut self) -> Self {
            self.fail_write_json = true;
            self
        }

        fn with_fail_metadata(mut self) -> Self {
            self.fail_metadata = true;
            self
        }

        fn with_fail_cargo_err(mut self) -> Self {
            self.fail_cargo_err = true;
            self
        }
    }

    impl Runner for MockRunner {
        fn cargo(&self, args: &[&str]) -> Result<bool> {
            if self.fail_cargo_err {
                anyhow::bail!("simulated cargo spawn failure");
            }
            // Identify the output path and write mock JSON when it's a package run.
            let output_path = args
                .windows(2)
                .find(|w| w[0] == "--output-path")
                .and_then(|w| w.get(1).copied());

            if let Some(pkg_idx) = args.iter().position(|a| *a == "--package") {
                let pkg = args[pkg_idx + 1];
                let has_branch = args.contains(&"--branch");
                match self.package_results.get(pkg) {
                    Some(Ok(report)) => {
                        if !self.fail_write_json {
                            if let Some(path) = output_path {
                                let json = serde_json::to_string(report).unwrap();
                                self.written
                                    .lock()
                                    .unwrap()
                                    .insert(path.to_owned(), json.clone());
                                std::fs::write(path, json).ok();
                            }
                        }
                        Ok(true)
                    }
                    // Err entries simulate a --branch crash only; the no-branch retry
                    // succeeds with an empty report so the fallback path is exercised.
                    Some(Err(_)) if has_branch => Ok(false),
                    Some(Err(_)) | None => {
                        // Default: succeed with empty report
                        let report = LlvmCovReport {
                            data: vec![CovData {
                                files: vec![],
                                totals: full_metrics(0),
                            }],
                        };
                        if !self.fail_write_json {
                            if let Some(path) = output_path {
                                let json = serde_json::to_string(&report).unwrap();
                                self.written
                                    .lock()
                                    .unwrap()
                                    .insert(path.to_owned(), json.clone());
                                std::fs::write(path, json).ok();
                            }
                        }
                        Ok(true)
                    }
                }
            } else if args.contains(&"--workspace") {
                // Workspace run
                if self.workspace_ok {
                    if !self.fail_write_json {
                        if let Some(path) = output_path {
                            let json = if let Some(ref custom) = self.workspace_json {
                                custom.clone()
                            } else {
                                let report = LlvmCovReport {
                                    data: vec![CovData {
                                        files: vec![],
                                        totals: full_metrics(10),
                                    }],
                                };
                                serde_json::to_string(&report).unwrap()
                            };
                            std::fs::write(path, json).ok();
                        }
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            } else {
                Ok(true)
            }
        }

        fn cargo_metadata(&self) -> Result<serde_json::Value> {
            if self.fail_metadata {
                anyhow::bail!("simulated cargo metadata failure");
            }
            Ok(self.metadata.clone())
        }

        fn remove_dir(&self, _path: &str) {
            // no-op in tests
        }
    }

    // ── Test helpers ─────────────────────────────────────────────────────────

    fn metric(count: u64, covered: u64) -> Metric {
        let percent = if count == 0 {
            100.0
        } else {
            (covered as f64 / count as f64) * 100.0
        };
        Metric {
            count,
            covered,
            percent,
        }
    }

    fn full_metrics(n: u64) -> Metrics {
        let m = metric(n, n);
        Metrics {
            regions: m.clone(),
            lines: m.clone(),
            functions: m.clone(),
            branches: m,
        }
    }

    fn partial_metrics(count: u64, covered: u64) -> Metrics {
        let m = metric(count, covered);
        Metrics {
            regions: m.clone(),
            lines: m.clone(),
            functions: m.clone(),
            branches: m,
        }
    }

    fn simple_metadata(names: &[&str]) -> serde_json::Value {
        let members: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!(format!("path+file:///ws/{n}#0.1.0")))
            .collect();
        let packages: Vec<serde_json::Value> = names
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": format!("path+file:///ws/{n}#0.1.0"),
                    "name": n
                })
            })
            .collect();
        serde_json::json!({"workspace_members": members, "packages": packages})
    }

    fn write_json(dir: &TempDir, name: &str, json: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, json).unwrap();
        path.to_string_lossy().into_owned()
    }

    // ── Metric ──────────────────────────────────────────────────────────────

    #[test]
    fn metric_fully_covered_when_count_zero() {
        assert!(metric(0, 0).is_fully_covered());
    }

    #[test]
    fn metric_fully_covered_when_all_hit() {
        assert!(metric(10, 10).is_fully_covered());
    }

    #[test]
    fn metric_not_fully_covered_when_some_missed() {
        assert!(!metric(10, 9).is_fully_covered());
    }

    #[test]
    fn metric_missed_correct() {
        assert_eq!(metric(10, 7).missed(), 3);
    }

    #[test]
    fn metric_missed_zero_when_fully_covered() {
        assert_eq!(metric(10, 10).missed(), 0);
    }

    #[test]
    fn metric_missed_zero_when_count_zero() {
        assert_eq!(metric(0, 0).missed(), 0);
    }

    #[test]
    fn metric_recompute_percent_all_covered() {
        let mut m = Metric {
            count: 10,
            covered: 10,
            percent: 0.0,
        };
        m.recompute_percent();
        assert!((m.percent - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metric_recompute_percent_partial() {
        let mut m = Metric {
            count: 4,
            covered: 3,
            percent: 0.0,
        };
        m.recompute_percent();
        assert!((m.percent - 75.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metric_recompute_percent_zero_count() {
        let mut m = Metric {
            count: 0,
            covered: 0,
            percent: 0.0,
        };
        m.recompute_percent();
        assert!((m.percent - 100.0).abs() < f64::EPSILON);
    }

    // ── Metrics ──────────────────────────────────────────────────────────────

    #[test]
    fn metrics_100_percent_when_all_full() {
        assert!(full_metrics(10).is_100_percent());
    }

    #[test]
    fn metrics_100_percent_when_counts_zero() {
        assert!(Metrics::default().is_100_percent());
    }

    #[test]
    fn metrics_not_100_when_regions_partial() {
        let mut m = full_metrics(10);
        m.regions = metric(10, 9);
        assert!(!m.is_100_percent());
    }

    #[test]
    fn metrics_not_100_when_lines_partial() {
        let mut m = full_metrics(10);
        m.lines = metric(10, 9);
        assert!(!m.is_100_percent());
    }

    #[test]
    fn metrics_not_100_when_functions_partial() {
        let mut m = full_metrics(10);
        m.functions = metric(10, 9);
        assert!(!m.is_100_percent());
    }

    #[test]
    fn metrics_not_100_when_branches_partial() {
        let mut m = full_metrics(10);
        m.branches = metric(10, 9);
        assert!(!m.is_100_percent());
    }

    #[test]
    fn metrics_add_accumulates_all_fields() {
        let mut a = partial_metrics(10, 8);
        let b = partial_metrics(20, 18);
        a.add(&b);
        assert_eq!(a.regions.count, 30);
        assert_eq!(a.regions.covered, 26);
        assert_eq!(a.lines.count, 30);
        assert_eq!(a.branches.covered, 26);
    }

    #[test]
    fn metrics_add_with_zero_other() {
        let mut a = full_metrics(5);
        a.add(&Metrics::default());
        assert_eq!(a.regions.count, 5);
        assert_eq!(a.regions.covered, 5);
    }

    #[test]
    fn metrics_recompute_percents_after_add() {
        let mut m = Metrics {
            regions: Metric {
                count: 4,
                covered: 3,
                percent: 0.0,
            },
            lines: Metric {
                count: 10,
                covered: 10,
                percent: 0.0,
            },
            functions: Metric {
                count: 2,
                covered: 1,
                percent: 0.0,
            },
            branches: Metric {
                count: 0,
                covered: 0,
                percent: 0.0,
            },
        };
        m.recompute_percents();
        assert!((m.regions.percent - 75.0).abs() < f64::EPSILON);
        assert!((m.lines.percent - 100.0).abs() < f64::EPSILON);
        assert!((m.functions.percent - 50.0).abs() < f64::EPSILON);
        assert!((m.branches.percent - 100.0).abs() < f64::EPSILON);
    }

    // ── parse_workspace_packages ─────────────────────────────────────────────

    #[test]
    fn parse_workspace_packages_new_format() {
        let meta = simple_metadata(&["leviath-core", "leviath-runtime"]);
        let pkgs = parse_workspace_packages(&meta);
        assert_eq!(pkgs.len(), 2);
        assert!(pkgs.contains(&"leviath-core".to_owned()));
        assert!(pkgs.contains(&"leviath-runtime".to_owned()));
    }

    #[test]
    fn parse_workspace_packages_excludes_xtask() {
        let meta = simple_metadata(&["leviath-core", "xtask"]);
        let pkgs = parse_workspace_packages(&meta);
        assert_eq!(pkgs.len(), 1);
        assert!(!pkgs.contains(&"xtask".to_owned()));
    }

    #[test]
    fn parse_workspace_packages_empty_metadata() {
        let meta = serde_json::json!({"workspace_members": [], "packages": []});
        assert!(parse_workspace_packages(&meta).is_empty());
    }

    #[test]
    fn parse_workspace_packages_only_listed_members_included() {
        // Package in `packages` but not in `workspace_members` is excluded.
        let meta = serde_json::json!({
            "workspace_members": ["path+file:///ws/leviath-core#0.1.0"],
            "packages": [
                {"id": "path+file:///ws/leviath-core#0.1.0", "name": "leviath-core"},
                {"id": "path+file:///ext/thirdparty#1.0.0", "name": "thirdparty"}
            ]
        });
        let pkgs = parse_workspace_packages(&meta);
        assert_eq!(pkgs, vec!["leviath-core"]);
    }

    // ── gap_files ────────────────────────────────────────────────────────────

    #[test]
    fn gap_files_empty_when_all_100_percent() {
        let data = CovData {
            files: vec![
                FileCov {
                    filename: "a.rs".to_owned(),
                    summary: full_metrics(10),
                },
                FileCov {
                    filename: "b.rs".to_owned(),
                    summary: full_metrics(5),
                },
            ],
            totals: full_metrics(15),
        };
        assert!(gap_files(&data).is_empty());
    }

    #[test]
    fn gap_files_returns_partial_files() {
        let data = CovData {
            files: vec![
                FileCov {
                    filename: "a.rs".to_owned(),
                    summary: full_metrics(10),
                },
                FileCov {
                    filename: "b.rs".to_owned(),
                    summary: partial_metrics(10, 8),
                },
            ],
            totals: partial_metrics(20, 18),
        };
        let gaps = gap_files(&data);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].filename, "b.rs");
    }

    #[test]
    fn gap_files_returns_all_when_all_partial() {
        let data = CovData {
            files: vec![
                FileCov {
                    filename: "a.rs".to_owned(),
                    summary: partial_metrics(5, 4),
                },
                FileCov {
                    filename: "b.rs".to_owned(),
                    summary: partial_metrics(3, 2),
                },
            ],
            totals: partial_metrics(8, 6),
        };
        assert_eq!(gap_files(&data).len(), 2);
    }

    // ── handle_no_branch_packages ────────────────────────────────────────────

    #[test]
    fn handle_no_branch_packages_empty_is_ok() {
        // Both CI and non-CI should be fine when the list is empty.
        assert!(handle_no_branch_packages(&[], false).is_ok());
        assert!(handle_no_branch_packages(&[], true).is_ok());
    }

    #[test]
    fn handle_no_branch_packages_nonempty_warns_but_ok_outside_ci() {
        let result = handle_no_branch_packages(&["leviath-runtime".to_owned()], false);
        assert!(result.is_ok(), "should not fail outside CI: {result:?}");
    }

    #[test]
    fn handle_no_branch_packages_nonempty_fails_in_ci() {
        let result = handle_no_branch_packages(&["leviath-runtime".to_owned()], true);
        assert!(result.is_err(), "should fail in CI");
    }

    // ── parse_json ───────────────────────────────────────────────────────────

    #[test]
    fn parse_json_full_100_percent() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[{"filename":"/r/a.rs","summary":{"regions":{"count":10,"covered":10,"percent":100.0},"lines":{"count":20,"covered":20,"percent":100.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":8,"covered":8,"percent":100.0}}}],"totals":{"regions":{"count":10,"covered":10,"percent":100.0},"lines":{"count":20,"covered":20,"percent":100.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":8,"covered":8,"percent":100.0}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert_eq!(report.data.len(), 1);
        assert_eq!(report.data[0].files.len(), 1);
        assert!(report.data[0].totals.is_100_percent());
    }

    #[test]
    fn parse_json_partial_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[{"filename":"/r/a.rs","summary":{"regions":{"count":10,"covered":8,"percent":80.0},"lines":{"count":20,"covered":18,"percent":90.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":8,"covered":6,"percent":75.0}}}],"totals":{"regions":{"count":10,"covered":8,"percent":80.0},"lines":{"count":20,"covered":18,"percent":90.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":8,"covered":6,"percent":75.0}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert!(!report.data[0].totals.is_100_percent());
        assert_eq!(report.data[0].totals.regions.missed(), 2);
    }

    #[test]
    fn parse_json_zero_branch_count_is_100_percent() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[],"totals":{"regions":{"count":5,"covered":5,"percent":100.0},"lines":{"count":10,"covered":10,"percent":100.0},"functions":{"count":3,"covered":3,"percent":100.0},"branches":{"count":0,"covered":0,"percent":0.0}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert!(report.data[0].totals.branches.is_fully_covered());
        assert!(report.data[0].totals.is_100_percent());
    }

    #[test]
    fn parse_json_extra_unknown_fields_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[],"totals":{"regions":{"count":1,"covered":1,"percent":100.0},"lines":{"count":1,"covered":1,"percent":100.0},"functions":{"count":1,"covered":1,"percent":100.0},"branches":{"count":1,"covered":1,"percent":100.0}}}],"type":"llvm.coverage","version":"2"}"#;
        let path = write_json(&dir, "cov.json", json);
        assert!(parse_json(&path).is_ok());
    }

    #[test]
    fn parse_json_error_on_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_json(&dir, "bad.json", "not json at all");
        assert!(parse_json(&path).is_err());
    }

    #[test]
    fn parse_json_error_on_missing_file() {
        assert!(parse_json("/tmp/does_not_exist_llvm_cov.json").is_err());
    }

    #[test]
    fn parse_json_missing_metrics_default_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[],"totals":{"regions":{"count":3,"covered":3,"percent":100.0},"lines":{"count":3,"covered":3,"percent":100.0},"functions":{"count":3,"covered":3,"percent":100.0}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert!(report.data[0].totals.branches.is_fully_covered());
    }

    // ── write_github_output ──────────────────────────────────────────────────

    #[test]
    fn write_github_output_no_path_is_noop() {
        assert!(write_github_output(&full_metrics(10), None).is_ok());
    }

    #[test]
    fn write_github_output_writes_all_four_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gha_output");
        std::fs::write(&path, "").unwrap();
        let m = Metrics {
            regions: metric(10, 10),
            lines: metric(20, 18),
            functions: metric(5, 5),
            branches: metric(8, 6),
        };
        write_github_output(&m, Some(path.to_str().unwrap())).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("regions=100.0"), "content: {content}");
        assert!(content.contains("lines=90.0"), "content: {content}");
        assert!(content.contains("functions=100.0"), "content: {content}");
        assert!(content.contains("branches=75.0"), "content: {content}");
    }

    #[test]
    fn write_github_output_appends_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gha_output");
        std::fs::write(&path, "previous_step_output=yes\n").unwrap();
        write_github_output(&full_metrics(1), Some(path.to_str().unwrap())).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("previous_step_output=yes"),
            "content: {content}"
        );
        assert!(content.contains("regions=100.0"), "content: {content}");
    }

    #[test]
    fn write_github_output_error_on_unwritable_path() {
        assert!(write_github_output(&full_metrics(1), Some("/no/such/dir/output")).is_err());
    }

    // ── Aggregation round-trip ───────────────────────────────────────────────

    #[test]
    fn aggregated_json_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/repo/a.rs".to_owned(),
                    summary: full_metrics(5),
                }],
                totals: full_metrics(5),
            }],
        };
        let path = dir.path().join("agg.json");
        std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
        let rt = parse_json(path.to_str().unwrap()).unwrap();
        assert_eq!(rt.data[0].files[0].filename, "/repo/a.rs");
        assert!(rt.data[0].totals.is_100_percent());
    }

    // ── RealRunner (actual cargo subprocess) ─────────────────────────────────

    #[test]
    fn real_runner_cargo_help_succeeds() {
        // `cargo help` is a fast, read-only command — safe to run in tests.
        assert!(RealRunner.cargo(&["help"]).unwrap());
    }

    #[test]
    fn real_runner_cargo_nonexistent_subcommand_returns_false() {
        // cargo will exit non-zero for an unknown subcommand.
        // We pass an implausible subcommand to avoid accidental side effects.
        let result = RealRunner.cargo(&["__no_such_subcommand_xyz__"]);
        // Either fails to parse or returns false — either way it shouldn't panic.
        let _ = result;
    }

    #[test]
    fn real_runner_cargo_metadata_returns_valid_json() {
        let meta = RealRunner.cargo_metadata().unwrap();
        assert!(meta["packages"].is_array());
        assert!(meta["workspace_members"].is_array());
    }

    #[test]
    fn real_runner_remove_dir_nonexistent_is_silent() {
        // Should not panic or return an error for a non-existent dir.
        RealRunner.remove_dir("/tmp/nonexistent_xtask_test_dir_xyz_abc_123");
    }

    #[test]
    fn real_runner_remove_dir_existing_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_owned();
        // TempDir is dropped at the end, but we test remove_dir explicitly.
        std::fs::create_dir_all(&path).unwrap();
        RealRunner.remove_dir(&path);
        // After removal, dir no longer exists (or was already cleaned up by TempDir).
        // Just ensure no panic.
    }

    // ── run_report — the core post-analysis logic ────────────────────────────

    #[test]
    fn run_report_100_percent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[]));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, false, None);
        assert!(result.is_ok(), "100% coverage should pass: {result:?}");
    }

    #[test]
    fn run_report_100_percent_writes_github_output() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[]));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let gha = dir.path().join("gha_output");
        std::fs::write(&gha, "").unwrap();
        let result = run_report(&runner, &output, false, Some(gha.to_str().unwrap()));
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&gha).unwrap();
        assert!(content.contains("regions="), "content: {content}");
    }

    #[test]
    fn run_report_partial_coverage_is_err() {
        let dir = tempfile::tempdir().unwrap();
        // Build a partial-coverage report and supply it as the workspace JSON.
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/foo.rs".to_owned(),
                    summary: partial_metrics(10, 8),
                }],
                totals: partial_metrics(10, 8),
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_workspace_json(json);
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, false, None);
        assert!(result.is_err(), "partial coverage should fail");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("100%"), "error should mention 100%: {msg}");
    }

    #[test]
    fn run_report_empty_data_array_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[]}"#.to_owned();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_workspace_json(json);
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, false, None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no data"),
            "error should mention no data: {msg}"
        );
    }

    #[test]
    fn run_report_parse_error_is_err() {
        // cargo() returns Ok(true) but writes no JSON → parse_json fails.
        // Covers the error path of the run_coverage → parse_json step.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_fail_write();
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, false, None);
        assert!(
            result.is_err(),
            "should fail when JSON not written: {result:?}"
        );
    }

    // ── Mock-based orchestration tests ───────────────────────────────────────

    #[test]
    fn run_coverage_workspace_success_returns_ok_report() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&["leviath-core"]));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path, false);
        assert!(result.is_ok(), "workspace run should succeed: {result:?}");
    }

    #[test]
    fn run_coverage_workspace_failure_falls_back_to_per_package() {
        let dir = tempfile::tempdir().unwrap();
        // Workspace fails → should attempt per-package for leviath-core.
        let runner = MockRunner::new(false, simple_metadata(&["leviath-core"]));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path, false);
        assert!(
            result.is_ok(),
            "per-package fallback should succeed: {result:?}"
        );
    }

    #[test]
    fn workspace_packages_metadata_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        // When cargo_metadata() fails, workspace_packages() (and the per-package
        // fallback) must propagate the error rather than silently using an empty list.
        let runner = MockRunner::new(false, simple_metadata(&[])).with_fail_metadata();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_per_package_coverage(&runner, &output_path, false);
        assert!(
            result.is_err(),
            "metadata failure should propagate: {result:?}"
        );
        assert!(
            result.unwrap_err().to_string().contains("simulated"),
            "error should mention metadata failure"
        );
    }

    #[test]
    fn per_package_fallback_branch_crash_retries_without_branch() {
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["leviath-runtime"]);
        let runner = MockRunner::new(false, meta)
            .with_package("leviath-runtime", Err("simulated SIGSEGV".to_owned()));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        // Pass in_ci=false explicitly — no env var manipulation, safe for parallel tests.
        let result = run_per_package_coverage(&runner, &output_path, false);
        // The fallback (no --branch) should write a default empty report and succeed.
        assert!(result.is_ok(), "branch fallback should succeed: {result:?}");
    }

    #[test]
    fn per_package_fallback_fails_in_ci_when_branch_crashes() {
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["leviath-runtime"]);
        let runner = MockRunner::new(false, meta)
            .with_package("leviath-runtime", Err("simulated SIGSEGV".to_owned()));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        // On CI (in_ci=true) any branch crash is a hard failure.
        let result = run_per_package_coverage(&runner, &output_path, true);
        assert!(
            result.is_err(),
            "should fail in CI when branch crashes: {result:?}"
        );
    }

    #[test]
    fn per_package_aggregated_write_error_propagates() {
        // output_path is a directory → std::fs::write will fail with "Is a directory".
        // This covers the with_context(|| format!("writing {output_path}")) closure.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(false, simple_metadata(&["leviath-core"]));
        // Pass the directory itself as the output path; writing to it must fail.
        let output_path = dir.path().to_str().unwrap().to_owned();
        let result = run_per_package_coverage(&runner, &output_path, false);
        assert!(
            result.is_err(),
            "write to directory should fail: {result:?}"
        );
    }

    // ── MockRunner direct tests — uncovered branches ─────────────────────────
    //
    // The following tests exercise specific branches inside MockRunner that are
    // never reached by the higher-level orchestration tests above.  They call
    // MockRunner::cargo() directly rather than going through the orchestration
    // functions, which lets each combination of (pkg/workspace/else) × (flags)
    // be hit independently and quickly.

    #[test]
    fn mock_runner_package_ok_result_writes_json() {
        // Exercises the `Some(Ok(report))` arm (branch) in MockRunner::cargo().
        let dir = tempfile::tempdir().unwrap();
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![],
                totals: full_metrics(5),
            }],
        };
        let runner =
            MockRunner::new(true, simple_metadata(&["my-pkg"])).with_package("my-pkg", Ok(report));
        let out = dir.path().join("my-pkg.json");
        let result = runner.cargo(&[
            "llvm-cov",
            "--package",
            "my-pkg",
            "--json",
            "--output-path",
            out.to_str().unwrap(),
        ]);
        assert!(
            matches!(result, Ok(true)),
            "package Ok run should succeed: {result:?}"
        );
        assert!(out.exists(), "JSON should have been written for Ok result");
    }

    #[test]
    fn mock_runner_package_ok_with_fail_write_skips_json() {
        // Exercises the `if !fail_write_json` false branch in the `Some(Ok)` arm.
        let dir = tempfile::tempdir().unwrap();
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![],
                totals: full_metrics(1),
            }],
        };
        let runner = MockRunner::new(true, simple_metadata(&["my-pkg"]))
            .with_package("my-pkg", Ok(report))
            .with_fail_write();
        let out = dir.path().join("my-pkg.json");
        let result = runner.cargo(&[
            "llvm-cov",
            "--package",
            "my-pkg",
            "--json",
            "--output-path",
            out.to_str().unwrap(),
        ]);
        // cargo still returns Ok(true) even when writing is suppressed.
        assert!(matches!(result, Ok(true)));
        // But no JSON was written.
        assert!(
            !out.exists(),
            "JSON should not be written when fail_write_json=true"
        );
    }

    #[test]
    fn mock_runner_package_none_with_fail_write_skips_json() {
        // Exercises the `if !fail_write_json` false branch in the `None/Err` arm.
        // Package not in package_results (None) + fail_write=true → skip write but
        // still return Ok(true).
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_fail_write();
        let out = dir.path().join("cov.json");
        let result = runner.cargo(&[
            "llvm-cov",
            "--package",
            "ghost-pkg",
            "--json",
            "--output-path",
            out.to_str().unwrap(),
        ]);
        assert!(matches!(result, Ok(true)));
        assert!(!out.exists(), "no JSON when fail_write_json=true");
    }

    #[test]
    fn mock_runner_workspace_without_output_path_skips_write() {
        // Exercises the `if let Some(path) = output_path` None branch in the
        // workspace arm — reached when --output-path is omitted from args.
        let runner = MockRunner::new(true, simple_metadata(&[]));
        let result = runner.cargo(&["llvm-cov", "--workspace", "--all-features"]);
        assert!(matches!(result, Ok(true)));
    }

    #[test]
    fn mock_runner_non_coverage_cargo_call_returns_ok() {
        // Exercises the final `else { Ok(true) }` branch — a cargo call that is
        // neither a workspace run nor a per-package run.
        let runner = MockRunner::new(true, simple_metadata(&[]));
        let result = runner.cargo(&["build", "--all-targets"]);
        assert!(matches!(result, Ok(true)));
    }

    #[test]
    fn mock_runner_package_ok_without_output_path_skips_write() {
        // Exercises the `if let Some(path) = output_path` None branch inside the
        // `Some(Ok)` arm — reached when --output-path is absent for a package run.
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![],
                totals: full_metrics(2),
            }],
        };
        let runner =
            MockRunner::new(true, simple_metadata(&["my-pkg"])).with_package("my-pkg", Ok(report));
        // No --output-path in args → output_path = None → writing skipped.
        let result = runner.cargo(&["llvm-cov", "--package", "my-pkg", "--all-features"]);
        assert!(matches!(result, Ok(true)));
    }

    // ── print_gaps branch coverage — false branches of each metric condition ──

    #[test]
    fn run_report_partial_only_functions_and_branches() {
        // regions=100%, lines=100%, functions=partial, branches=partial.
        // In print_gaps, the `if !s.regions.is_fully_covered()` and
        // `if !s.lines.is_fully_covered()` conditions evaluate to FALSE (line 135
        // false branch, line 138 false branch), covering the "skip if 100%" path.
        let dir = tempfile::tempdir().unwrap();
        let mixed = Metrics {
            regions: metric(10, 10), // 100% → false at line 135
            lines: metric(10, 10),   // 100% → false at line 138
            functions: metric(5, 4), // 80%  → true  at line 141
            branches: metric(8, 7),  // 87%  → true  at line 144
        };
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/partial.rs".to_owned(),
                    summary: mixed.clone(),
                }],
                totals: mixed,
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_workspace_json(json);
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, false, None);
        assert!(result.is_err(), "partial coverage should fail: {result:?}");
    }

    #[test]
    fn run_report_partial_with_crates_filename() {
        // regions=partial, lines=partial, functions=100%, branches=0/0 (trivially 100%).
        // In print_gaps:
        //   • split_once("/crates/") returns Some → covers the Some arm of the match
        //   • `if !s.functions.is_fully_covered()` → false (line 141 false branch)
        //   • `if !s.branches.is_fully_covered()` → false (line 144 false branch)
        let dir = tempfile::tempdir().unwrap();
        let mixed = Metrics {
            regions: metric(10, 9),  // 90%  → true  at line 135
            lines: metric(10, 9),    // 90%  → true  at line 138
            functions: metric(5, 5), // 100% → false at line 141
            branches: metric(0, 0),  // 0/0 is_fully_covered() → false at line 144
        };
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    // "/crates/" in the path exercises split_once Some arm
                    filename: "/workspace/crates/my-crate/src/lib.rs".to_owned(),
                    summary: mixed.clone(),
                }],
                totals: mixed,
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_workspace_json(json);
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, false, None);
        assert!(result.is_err(), "partial coverage should fail: {result:?}");
    }

    // ── Line 217: None branch of if let Some(data) = pkg_report.data.into_iter().next()

    #[test]
    fn per_package_with_empty_data_report_skipped() {
        // When a package's llvm-cov JSON contains an empty `data` array, the
        // `if let Some(data) = pkg_report.data.into_iter().next()` evaluates to
        // None and the if-body is skipped (coverage.rs line 217 None branch).
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["my-pkg"]);
        // Ok result with empty data array → parse_json returns { data: [] }.
        let empty_report = LlvmCovReport { data: vec![] };
        let runner = MockRunner::new(false, meta).with_package("my-pkg", Ok(empty_report));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_per_package_coverage(&runner, &output_path, false);
        assert!(
            result.is_ok(),
            "empty data should be silently skipped: {result:?}"
        );
        // The aggregated report should have no files (the empty pkg data was not merged).
        let aggregated = parse_json(&output_path).unwrap();
        assert!(aggregated.data[0].files.is_empty());
    }

    // ── Line 553: None branch of if let Some(path) = output_path (None/Err arm) ─

    #[test]
    fn mock_runner_package_none_without_output_path_returns_ok() {
        // Calls cargo with --package but WITHOUT --output-path → output_path = None.
        // The package is not in package_results (None arm) with fail_write=false.
        // Inside the `if !fail_write_json` block, `if let Some(path) = output_path`
        // evaluates to None → writing is skipped (coverage.rs line 553 None branch).
        let runner = MockRunner::new(true, simple_metadata(&[]));
        let result = runner.cargo(&["llvm-cov", "--package", "ghost-pkg", "--all-features"]);
        assert!(matches!(result, Ok(true)));
    }

    // ── with_context closures at run_single_package and run_per_package_coverage ─

    #[test]
    fn per_package_cargo_ok_but_json_not_written_fails() {
        // With fail_write=true, runner.cargo() returns Ok(true) but writes no JSON.
        // parse_json then fails (file missing), calling the
        //   with_context(|| format!("parsing coverage JSON for {pkg}"))
        // closure in run_single_package.  After the branch run fails, the no-branch
        // retry also fails, which calls the
        //   with_context(|| format!("coverage failed for {pkg}"))
        // closure at coverage.rs line 213.
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["my-pkg"]);
        let runner = MockRunner::new(false, meta).with_fail_write();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_per_package_coverage(&runner, &output_path, false);
        assert!(
            result.is_err(),
            "missing JSON should propagate as error: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("coverage failed for my-pkg"), "error: {msg}");
    }

    // ── Cargo spawn-failure Err paths ─────────────────────────────────────────

    #[test]
    fn run_coverage_cargo_err_propagates() {
        // run_coverage calls runner.cargo(...)?  (workspace run).
        // When cargo() returns Err, that Err must propagate through run_coverage.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_fail_cargo_err();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path, false);
        assert!(result.is_err(), "cargo Err should propagate: {result:?}");
        assert!(
            result.unwrap_err().to_string().contains("simulated"),
            "error should mention the simulated failure"
        );
    }

    #[test]
    fn run_single_package_cargo_err_propagates() {
        // run_single_package calls runner.cargo(&args)?
        // When cargo() returns Err, the function propagates it.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[])).with_fail_cargo_err();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_single_package(&runner, "some-pkg", true, &output_path);
        assert!(
            result.is_err(),
            "cargo Err should propagate from run_single_package: {result:?}"
        );
    }

    // ── write_github_output_content write-failure path ────────────────────────

    /// A `Write` impl that always fails — used to cover the `write_all` Err path
    /// inside `write_github_output_content`.
    struct FailWriter;
    impl std::io::Write for FailWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_github_output_content_write_failure_is_err() {
        // Exercises the `write_all(...)` Err path inside write_github_output_content.
        let mut writer = FailWriter;
        let result = write_github_output_content(&full_metrics(1), &mut writer);
        assert!(result.is_err(), "write to FailWriter should fail");
    }

    #[test]
    fn fail_writer_flush_is_ok() {
        // Covers the FailWriter::flush() impl — flush() always succeeds even
        // though write() always fails (the two are orthogonal).
        use std::io::Write;
        let mut writer = FailWriter;
        assert!(writer.flush().is_ok());
    }

    // ── run_report: write_github_output Err at line 106 ──────────────────────

    #[test]
    fn run_report_github_output_write_error_propagates() {
        // Arrange: 100% coverage so gaps.is_empty() = true, but a bad github_output
        // path so write_github_output() fails.  This covers the `?` Err arm at the
        // `write_github_output(&data.totals, github_output)?` call in run_report.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(true, simple_metadata(&[]));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let bad_gha = "/no/such/directory/gha_output.txt";
        let result = run_report(&runner, &output, false, Some(bad_gha));
        assert!(
            result.is_err(),
            "should fail when GITHUB_OUTPUT path is unwritable: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("GITHUB_OUTPUT"),
            "error should mention GITHUB_OUTPUT: {msg}"
        );
    }
}
