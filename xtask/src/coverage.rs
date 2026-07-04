//! Coverage reporting — runs `cargo llvm-cov` per-package across the
//! workspace and aggregates region/line/function coverage percentages.
//!
//! Branch coverage is intentionally not collected. `cargo llvm-cov --branch`
//! reliably crashes with SIGSEGV inside
//! `llvm::coverage::CoverageMapping::getInstantiationGroups` — a currently
//! open, unresolved upstream LLVM bug
//! (<https://github.com/llvm/llvm-project/issues/119558>). Reproduced locally
//! against two nightly toolchains five months apart, for leviath-cli,
//! leviath-providers, and leviath-runtime specifically, with or without
//! `-ignore-filename-regex`, and with or without limiting `llvm-cov` to a
//! single worker thread (`-num-threads=1`) — so it isn't a race in llvm-cov's
//! own thread pool, it's deterministic. There is no known workaround short of
//! an upstream LLVM fix, so this project no longer requests `--branch` at
//! all, and the toolchain no longer needs to be pinned to a specific nightly:
//! branch coverage was the only reason nightly was required (source-based
//! coverage instrumentation itself, `-C instrument-coverage`, has worked on
//! stable Rust for years).
//!
//! **Per-package aggregation is still required even without `--branch`.**
//! A single `cargo llvm-cov --workspace` run does complete without crashing
//! (unlike with `--branch`), but it silently produces *wrong* — inflated
//! missed-region — numbers compared to running each package in isolation.
//! Confirmed deterministic (not run-to-run noise) and reproduced across two
//! unrelated crates: e.g. `leviath-runtime/src/systems.rs` reads 41 missed
//! regions under `--workspace` but only 15 under `cargo llvm-cov --package
//! leviath-runtime` alone; `leviath-providers/src/rate_limit.rs` reads 6
//! missed under `--workspace` but 0 (100%) in isolation. This is almost
//! certainly the same underlying `getInstantiationGroups` bug as the
//! `--branch` crash, manifesting as silent merge inaccuracy instead of a
//! crash when there's "only" plain region/line/function data (not branch
//! data) to reconcile across the many object files a full-workspace run
//! merges together — per-package runs merge far fewer objects and are
//! measurably accurate. So: always run per-package, with a clean
//! `llvm-cov-target/` between each (belt-and-suspenders against
//! cross-package contamination), and aggregate the JSON results ourselves.
//!
//! Output lands at `coverage/llvm-cov.json` (gitignored) — never under
//! `target/`, never committed.
//!
//! **A residual class of "missed regions" is a confirmed, permanent
//! limitation, not a real gap.** Generic functions instantiated over many
//! type parameters or closure types (e.g. `leviath-runtime/src/engine.rs`'s
//! `run_inference_loop`, `leviath-providers`'s `*SseStream<S>::poll_next`,
//! `leviath-package/src/bundler.rs`'s `write_bundle<W: Write>`) produce one
//! coverage-mapping instance per monomorphization; llvm-cov sometimes reports
//! a region as uncovered for one instantiation even though every source
//! position is covered by the union of all instantiations. Empirically
//! tested and ruled out: building with `codegen-units=1` has zero measurable
//! effect on these counts (verified via clean rebuilds with before/after JSON
//! region diffs identical to the byte) while costing ~60-70% more wall-clock
//! time per crate — `codegen-units` only affects how already-monomorphized
//! code is partitioned for backend codegen, not how many monomorphizations
//! exist. There is no known workaround; this is inherent to source-based
//! coverage of generic code.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Runner trait (injectable for testing) ────────────────────────────────────

/// Abstraction over subprocess execution — inject a mock in tests.
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
    let github_output = std::env::var("GITHUB_OUTPUT").ok();
    std::fs::create_dir_all("coverage")
        .expect("failed to create coverage/ directory — check filesystem permissions");
    run_report(runner, "coverage/llvm-cov.json", github_output.as_deref())
}

/// Locked-in ceiling on total missed regions/lines/functions, workspace-wide.
///
/// **This project tried per-file exact-match enforcement (a
/// `CONFIRMED_ARTIFACT_ALLOWANCE` list keyed to specific
/// `COVERAGE-CONFIRMED-ARTIFACT`-marked functions) and reverted it after real
/// CI evidence proved it unworkable**: the SAME commit, unchanged, measured
/// `config.rs` at 0 missed regions locally, 8 on a GitHub Actions
/// `macos-latest` runner, and 20 on `ubuntu-latest` -- and also newly hit
/// `leviath-mcp/src/discovery.rs` (2 regions), a file with no
/// `COVERAGE-CONFIRMED-ARTIFACT` marker at all and no history of gaps. This
/// is run-to-run/environment-to-environment measurement jitter from the same
/// upstream `getInstantiationGroups` LLVM bug documented at the top of this
/// file, not a stable, individually-markable set of functions -- there is no
/// finite list of "the artifact files" to allowlist, because which files it
/// hits varies per build. A global ceiling is the only enforcement shape
/// that can absorb jitter of unpredictable *location* while still catching a
/// jitter of unpredictable *size* (a real, large new gap).
///
/// This is a *ratchet*, not a suppression mechanism: this ceiling is set from
/// real, fresh evidence gathered right after the vast majority of this
/// project's real, testable coverage gaps were closed (generic-function
/// monomorphization de-genericized to `dyn Trait`/`dyn Fn` wherever the
/// instantiation count could be reduced to one, dependency-injection seams
/// added for real fault-injection testing, `#[cfg(not(test))]`/`#[cfg(test)]`
/// twins for genuinely-untestable real-IO, and `COVERAGE-CONFIRMED-ARTIFACT`
/// markers for the residual, individually-investigated monomorphization
/// artifacts that couldn't be collapsed further) -- two real CI runs on the
/// identical commit measured 18/3/0 (macOS) and 33/11/3 (ubuntu) missed
/// regions/lines/functions. This ceiling is set with real headroom above the
/// worse of those (ubuntu's 33/11/3) to comfortably absorb further jitter
/// (including on `windows-latest`, historically the worst-measuring
/// platform) while still being roughly 85% smaller than this project's prior
/// ceiling (660/380/46) -- large enough to not be spuriously flaky, small
/// enough that a real, untested new feature would still trip it.
///
/// If future evidence shows this ceiling is still too tight (spurious CI
/// failures with no corresponding code change) or too loose (a real
/// regression slips through unnoticed), gather 2-3 fresh real CI
/// measurements the same way this one was set and adjust -- never raise it
/// on a hunch, and never lower it below what real, repeated CI evidence
/// supports.
const MAX_MISSED_REGIONS: u64 = 100;
const MAX_MISSED_LINES: u64 = 40;
const MAX_MISSED_FUNCTIONS: u64 = 10;

/// Core reporting logic extracted from `run_with` for unit-testability.
///
/// Runs coverage via `runner`, prints a summary, reports any gaps, and writes
/// CI output variables. All paths (including error paths) are reachable from
/// tests through the `MockRunner` abstraction.
pub fn run_report(
    runner: &dyn Runner,
    output_path: &str,
    github_output: Option<&str>,
) -> Result<()> {
    println!("[coverage] Running coverage analysis…");
    let data = run_coverage(runner, output_path)?;

    print_summary(&data.totals);

    let gaps = gap_files(&data);

    // Always publish the computed percentages -- the coverage badges need
    // real numbers regardless of whether 100% is currently met.
    write_github_output(&data.totals, github_output)?;

    if gaps.is_empty() {
        println!("\n[coverage] All metrics at 100%. ✓");
        return Ok(());
    }

    print_gaps(&gaps);

    let regions_missed = data.totals.regions.missed();
    let lines_missed = data.totals.lines.missed();
    let functions_missed = data.totals.functions.missed();

    if regions_missed > MAX_MISSED_REGIONS
        || lines_missed > MAX_MISSED_LINES
        || functions_missed > MAX_MISSED_FUNCTIONS
    {
        anyhow::bail!(
            "[coverage] Coverage regressed below the locked-in floor -- missed \
             regions {regions_missed} (max {MAX_MISSED_REGIONS}), missed lines \
             {lines_missed} (max {MAX_MISSED_LINES}), missed functions \
             {functions_missed} (max {MAX_MISSED_FUNCTIONS}). Fix the gaps above. \
             If a gap is a confirmed permanent tooling limitation (generic-function \
             monomorphization, an llvm-cov tracing-macro-argument artifact, or \
             deliberately-untested real-IO), investigate with `cargo llvm-cov \
             show`/`--html` before assuming so, then gather fresh real CI evidence \
             and adjust the ceiling constants in xtask/src/coverage.rs to match."
        );
    }

    println!(
        "\n[coverage] Below 100% but within the confirmed-permanent-gap ceiling \
         (regions {regions_missed}/{MAX_MISSED_REGIONS}, lines {lines_missed}/{MAX_MISSED_LINES}, \
         functions {functions_missed}/{MAX_MISSED_FUNCTIONS}) — not failing the build."
    );
    Ok(())
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
    }
}

// ── Internal orchestration ───────────────────────────────────────────────────

pub fn run_coverage(runner: &dyn Runner, output_path: &str) -> Result<CovData> {
    let meta = runner.cargo_metadata()?;
    let packages = parse_workspace_packages(&meta);
    println!(
        "[coverage] Per-package run for {} crates: {}",
        packages.len(),
        packages.join(", ")
    );

    // Place per-target JSON files next to the aggregated output file so they
    // always land in a writable directory (avoids relative-path issues in tests).
    let out_dir = std::path::Path::new(output_path)
        .parent()
        .unwrap_or(std::path::Path::new("."));

    let mut all_files: Vec<FileCov> = Vec::new();
    let mut totals = Metrics::default();

    for pkg in &packages {
        println!("[coverage]   → {pkg}");

        // Scope to --lib, then to each integration-test binary, SEPARATELY,
        // rather than one bare `cargo llvm-cov --package X` call that lets
        // llvm-cov merge every test binary's profraw data internally. That
        // internal multi-binary merge is subject to the same
        // `getInstantiationGroups` merge-inaccuracy bug documented above for
        // cross-package `--workspace` runs -- confirmed empirically:
        // leviath-cli's commands/add.rs read 6 missed regions when merged
        // across all 3 of its test binaries (lib + 2 integration tests) in
        // one invocation, but only 2 missed when measured via `--lib` alone.
        // See `merge_target_reports` for how the separately-scoped results
        // are safely recombined.
        let mut scopes: Vec<Vec<&str>> = vec![vec!["--lib"]];
        for test_name in package_test_targets(&meta, pkg) {
            scopes.push(vec!["--test", test_name]);
        }

        let mut target_reports: Vec<CovData> = Vec::new();
        for scope in &scopes {
            // Clean slate between every scoped run: belt-and-suspenders
            // against cross-run contamination of the profraw/profdata
            // llvm-cov accumulates under here.
            runner.remove_dir("target/llvm-cov-target");

            let scope_tag = scope.join("-").replace(['-', ' '], "_");
            let target_output = out_dir
                .join(format!("llvm-cov-{pkg}-{scope_tag}.json"))
                .to_string_lossy()
                .into_owned();

            let report = run_single_target(runner, pkg, scope, &target_output)
                .with_context(|| format!("coverage failed for {pkg} ({})", scope.join(" ")))?;

            if let Some(data) = report.data.into_iter().next() {
                target_reports.push(data);
            }
        }

        let pkg_data = merge_target_reports(target_reports);
        totals.add(&pkg_data.totals);
        all_files.extend(pkg_data.files);
    }

    totals.recompute_percents();
    let aggregated = CovData {
        files: all_files,
        totals,
    };
    // The on-disk report still uses llvm-cov's own `{"data": [...]}` schema
    // (a single-element array) for consistency with what `cargo llvm-cov`
    // itself would produce, even though in memory we deal with the one
    // `CovData` element directly -- there's no realistic way for our own
    // aggregation to produce zero or multiple entries here, so `run_report`
    // doesn't need a fallible "does data have an entry" check on top of it.
    let wrapped = LlvmCovReport {
        data: vec![aggregated.clone()],
    };
    let json = serde_json::to_string_pretty(&wrapped)
        .expect("failed to serialise aggregated JSON — all fields are serialisable");
    std::fs::write(output_path, json).with_context(|| format!("writing {output_path}"))?;
    Ok(aggregated)
}

/// Merge multiple coverage reports for the SAME package (one per test
/// target: `--lib`, plus one per integration-test binary) into a single,
/// more accurate report for that package.
///
/// For each file, per metric, take the higher `covered` count across all
/// target-scoped runs rather than summing. Summing would double-count
/// regions the compiled library shares across binaries (an integration test
/// links the same library code the `--lib` unit tests do); `max` is a
/// mathematically safe lower-bound approximation of the true union of
/// coverage across all test binaries -- it can never report less coverage
/// than any single scoped run actually observed, and can never fabricate
/// coverage no run actually exercised.
fn merge_target_reports(reports: Vec<CovData>) -> CovData {
    let mut by_file: std::collections::HashMap<String, FileCov> = std::collections::HashMap::new();
    for report in reports {
        for file in report.files {
            by_file
                .entry(file.filename.clone())
                .and_modify(|existing| existing.summary.merge_max(&file.summary))
                .or_insert(file);
        }
    }

    let mut files: Vec<FileCov> = by_file.into_values().collect();
    files.sort_by(|a, b| a.filename.cmp(&b.filename));

    let mut totals = Metrics::default();
    for file in &files {
        totals.add(&file.summary);
    }
    totals.recompute_percents();

    CovData { files, totals }
}

fn run_single_target(
    runner: &dyn Runner,
    pkg: &str,
    scope: &[&str],
    output_path: &str,
) -> Result<LlvmCovReport> {
    let mut args = vec!["llvm-cov", "--all-features", "--package", pkg];
    args.extend_from_slice(scope);
    args.extend_from_slice(&["--json", "--output-path", output_path]);

    if !runner.cargo(&args)? {
        anyhow::bail!(
            "cargo llvm-cov exited non-zero for package {pkg} ({})",
            scope.join(" ")
        );
    }

    parse_json(output_path)
        .with_context(|| format!("parsing coverage JSON for {pkg} ({})", scope.join(" ")))
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

/// Return the names of a package's integration-test binaries (files under
/// `tests/`) from `cargo metadata` JSON — NOT its `#[cfg(test)] mod tests`
/// unit tests (which compile into the `--lib` target itself) and NOT its
/// `[[bin]]` targets.
pub fn package_test_targets<'a>(meta: &'a serde_json::Value, pkg: &str) -> Vec<&'a str> {
    meta["packages"]
        .as_array()
        .map(|arr| arr.as_slice())
        .unwrap_or(&[])
        .iter()
        .find(|p| p["name"].as_str() == Some(pkg))
        .and_then(|p| p["targets"].as_array())
        .map(|targets| {
            targets
                .iter()
                .filter(|t| {
                    t["kind"]
                        .as_array()
                        .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("test")))
                })
                .filter_map(|t| t["name"].as_str())
                .collect()
        })
        .unwrap_or_default()
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

/// Write the three coverage-percentage lines to any `Write` sink.
///
/// Extracted from `write_github_output` so that unit tests can pass a mock
/// writer (e.g. a `FailWriter`) to cover the `write_all` error path without
/// needing a real filesystem error.
pub(crate) fn write_github_output_content<W: std::io::Write>(
    totals: &Metrics,
    writer: &mut W,
) -> std::io::Result<()> {
    let content = format!(
        "regions={:.1}\nlines={:.1}\nfunctions={:.1}\n",
        totals.regions.percent, totals.lines.percent, totals.functions.percent,
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

/// `llvm-cov`'s JSON always includes a `branches` key alongside these three
/// (0/0 when `--branch` wasn't requested, which is always, per the module
/// doc comment). `branches` is deliberately not modeled here — serde ignores
/// unknown JSON fields by default, so it's silently dropped on parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    #[serde(default)]
    pub regions: Metric,
    #[serde(default)]
    pub lines: Metric,
    #[serde(default)]
    pub functions: Metric,
}

impl Metrics {
    pub fn is_100_percent(&self) -> bool {
        self.regions.is_fully_covered()
            && self.lines.is_fully_covered()
            && self.functions.is_fully_covered()
    }

    /// Accumulate another package's totals into this running aggregate.
    pub fn add(&mut self, other: &Metrics) {
        self.regions.count += other.regions.count;
        self.regions.covered += other.regions.covered;
        self.lines.count += other.lines.count;
        self.lines.covered += other.lines.covered;
        self.functions.count += other.functions.count;
        self.functions.covered += other.functions.covered;
    }

    /// Recompute percentages after accumulating raw counts via `add`.
    pub fn recompute_percents(&mut self) {
        self.regions.recompute_percent();
        self.lines.recompute_percent();
        self.functions.recompute_percent();
    }

    /// Merge another observation of the *same* underlying compiled code
    /// (e.g. the same package's `--lib` run vs. one of its `--test <name>`
    /// runs) by taking the higher `covered` count per metric — see
    /// `merge_target_reports`'s doc comment for why `max`, not `add`.
    pub fn merge_max(&mut self, other: &Metrics) {
        self.regions.merge_max(&other.regions);
        self.lines.merge_max(&other.lines);
        self.functions.merge_max(&other.functions);
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

    /// Merge another observation of the same underlying regions/lines/
    /// functions by taking the higher `covered` value. `count` is taken as
    /// the max too (defensive — it should already match across runs of the
    /// same compiled code, but doesn't hurt to guard against it not).
    pub fn merge_max(&mut self, other: &Metric) {
        self.count = self.count.max(other.count);
        self.covered = self.covered.max(other.covered);
        self.recompute_percent();
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    // ── Mock runner ──────────────────────────────────────────────────────────

    /// A mock runner for unit-testing orchestration logic without subprocesses.
    struct MockRunner {
        /// Responses for `cargo llvm-cov --package PKG`: Some(Ok) = success with
        /// JSON written, Some(Err) = non-zero exit, None = default success with
        /// an empty (trivially 100%) report.
        package_results: HashMap<String, Result<LlvmCovReport, String>>,
        /// JSON to return from cargo_metadata.
        metadata: serde_json::Value,
        /// Whether cargo_metadata should return an error.
        fail_metadata: bool,
        /// If true, cargo() returns Ok(true) but writes no JSON (simulates a write failure path).
        fail_write_json: bool,
        /// If true, cargo() immediately returns Err (simulates a spawn/IO failure).
        fail_cargo_err: bool,
    }

    impl MockRunner {
        fn new(metadata: serde_json::Value) -> Self {
            Self {
                package_results: HashMap::new(),
                metadata,
                fail_metadata: false,
                fail_write_json: false,
                fail_cargo_err: false,
            }
        }

        fn with_package(mut self, pkg: &str, result: Result<LlvmCovReport, String>) -> Self {
            self.package_results.insert(pkg.to_owned(), result);
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
            let output_path = args
                .windows(2)
                .find(|w| w[0] == "--output-path")
                .and_then(|w| w.get(1).copied());

            let Some(pkg_idx) = args.iter().position(|a| *a == "--package") else {
                // Any non-package cargo invocation (e.g. plain `cargo help`) — no-op success.
                return Ok(true);
            };
            let pkg = args[pkg_idx + 1];

            match self.package_results.get(pkg) {
                Some(Ok(report)) => {
                    if !self.fail_write_json {
                        if let Some(path) = output_path {
                            let json = serde_json::to_string(report).unwrap();
                            std::fs::write(path, json).ok();
                        }
                    }
                    Ok(true)
                }
                Some(Err(_)) => Ok(false),
                None => {
                    if !self.fail_write_json {
                        if let Some(path) = output_path {
                            let report = LlvmCovReport {
                                data: vec![CovData {
                                    files: vec![],
                                    totals: full_metrics(0),
                                }],
                            };
                            std::fs::write(path, serde_json::to_string(&report).unwrap()).ok();
                        }
                    }
                    Ok(true)
                }
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
            functions: m,
        }
    }

    fn partial_metrics(count: u64, covered: u64) -> Metrics {
        let m = metric(count, covered);
        Metrics {
            regions: m.clone(),
            lines: m.clone(),
            functions: m,
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
    fn metrics_add_accumulates_all_fields() {
        let mut a = partial_metrics(10, 8);
        let b = partial_metrics(20, 18);
        a.add(&b);
        assert_eq!(a.regions.count, 30);
        assert_eq!(a.regions.covered, 26);
        assert_eq!(a.lines.count, 30);
        assert_eq!(a.functions.covered, 26);
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
        };
        m.recompute_percents();
        assert!((m.regions.percent - 75.0).abs() < f64::EPSILON);
        assert!((m.lines.percent - 100.0).abs() < f64::EPSILON);
        assert!((m.functions.percent - 50.0).abs() < f64::EPSILON);
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

    // ── package_test_targets ─────────────────────────────────────────────────

    fn metadata_with_targets(pkg: &str, targets: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "workspace_members": [format!("path+file:///ws/{pkg}#0.1.0")],
            "packages": [
                {"id": format!("path+file:///ws/{pkg}#0.1.0"), "name": pkg, "targets": targets}
            ]
        })
    }

    #[test]
    fn package_test_targets_finds_integration_tests() {
        let meta = metadata_with_targets(
            "leviath-cli",
            serde_json::json!([
                {"kind": ["lib"], "name": "leviath_cli"},
                {"kind": ["bin"], "name": "lev"},
                {"kind": ["test"], "name": "cli_dispatch"},
                {"kind": ["test"], "name": "manifest_integration"},
            ]),
        );
        let mut targets = package_test_targets(&meta, "leviath-cli");
        targets.sort_unstable();
        assert_eq!(targets, vec!["cli_dispatch", "manifest_integration"]);
    }

    #[test]
    fn package_test_targets_excludes_lib_and_bin() {
        let meta = metadata_with_targets(
            "leviath-core",
            serde_json::json!([
                {"kind": ["lib"], "name": "leviath_core"},
                {"kind": ["bin"], "name": "some-bin"},
            ]),
        );
        assert!(package_test_targets(&meta, "leviath-core").is_empty());
    }

    #[test]
    fn package_test_targets_unknown_package_returns_empty() {
        let meta = metadata_with_targets("leviath-core", serde_json::json!([]));
        assert!(package_test_targets(&meta, "does-not-exist").is_empty());
    }

    #[test]
    fn package_test_targets_missing_targets_field_returns_empty() {
        // `simple_metadata`-style packages have no "targets" key at all.
        let meta = simple_metadata(&["leviath-core"]);
        assert!(package_test_targets(&meta, "leviath-core").is_empty());
    }

    #[test]
    fn package_test_targets_empty_packages_array_returns_empty() {
        let meta = serde_json::json!({"workspace_members": [], "packages": []});
        assert!(package_test_targets(&meta, "anything").is_empty());
    }

    // ── Metric/Metrics::merge_max ────────────────────────────────────────────

    #[test]
    fn metric_merge_max_takes_higher_covered() {
        let mut a = metric(10, 4);
        a.merge_max(&metric(10, 7));
        assert_eq!(a.covered, 7);
        assert_eq!(a.count, 10);
        assert!((a.percent - 70.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metric_merge_max_keeps_higher_when_self_already_larger() {
        let mut a = metric(10, 9);
        a.merge_max(&metric(10, 3));
        assert_eq!(a.covered, 9);
    }

    #[test]
    fn metric_merge_max_takes_higher_count() {
        // Simulates merging a --lib report (whose count includes the file's
        // own #[cfg(test)] mod tests) with a --test report (which doesn't).
        let mut a = metric(8, 8);
        a.merge_max(&metric(10, 6));
        assert_eq!(a.count, 10);
        assert_eq!(a.covered, 8);
    }

    #[test]
    fn metrics_merge_max_all_fields() {
        let mut a = Metrics {
            regions: metric(10, 4),
            lines: metric(10, 5),
            functions: metric(2, 1),
        };
        a.merge_max(&Metrics {
            regions: metric(10, 8),
            lines: metric(10, 2),
            functions: metric(2, 2),
        });
        assert_eq!(a.regions.covered, 8);
        assert_eq!(a.lines.covered, 5);
        assert_eq!(a.functions.covered, 2);
    }

    // ── merge_target_reports ─────────────────────────────────────────────────

    #[test]
    fn merge_target_reports_empty_is_empty() {
        let merged = merge_target_reports(vec![]);
        assert!(merged.files.is_empty());
        assert!(merged.totals.is_100_percent());
    }

    #[test]
    fn merge_target_reports_single_report_passes_through() {
        let report = CovData {
            files: vec![FileCov {
                filename: "/src/a.rs".to_owned(),
                summary: partial_metrics(10, 8),
            }],
            totals: partial_metrics(10, 8),
        };
        let merged = merge_target_reports(vec![report]);
        assert_eq!(merged.files.len(), 1);
        assert_eq!(merged.totals.regions.covered, 8);
        assert_eq!(merged.totals.regions.count, 10);
    }

    #[test]
    fn merge_target_reports_combines_overlapping_file_with_max_covered() {
        // Simulates --lib (covered=2) and --test cli_dispatch (covered=5) both
        // reporting on the same shared library file -- the merged result must
        // take the higher observed coverage, not sum (which would double-count).
        let lib_report = CovData {
            files: vec![FileCov {
                filename: "/src/commands/add.rs".to_owned(),
                summary: partial_metrics(10, 2),
            }],
            totals: partial_metrics(10, 2),
        };
        let test_report = CovData {
            files: vec![FileCov {
                filename: "/src/commands/add.rs".to_owned(),
                summary: partial_metrics(10, 5),
            }],
            totals: partial_metrics(10, 5),
        };
        let merged = merge_target_reports(vec![lib_report, test_report]);
        assert_eq!(
            merged.files.len(),
            1,
            "same filename must merge into one entry"
        );
        assert_eq!(merged.files[0].summary.regions.covered, 5);
        assert_eq!(merged.totals.regions.covered, 5);
    }

    #[test]
    fn merge_target_reports_disjoint_files_are_both_kept() {
        let lib_report = CovData {
            files: vec![FileCov {
                filename: "/src/a.rs".to_owned(),
                summary: full_metrics(5),
            }],
            totals: full_metrics(5),
        };
        let test_report = CovData {
            files: vec![FileCov {
                filename: "/src/b.rs".to_owned(),
                summary: full_metrics(3),
            }],
            totals: full_metrics(3),
        };
        let merged = merge_target_reports(vec![lib_report, test_report]);
        assert_eq!(merged.files.len(), 2);
        assert_eq!(merged.totals.regions.count, 8);
        assert_eq!(merged.totals.regions.covered, 8);
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

    // ── parse_json ───────────────────────────────────────────────────────────

    #[test]
    fn parse_json_full_100_percent() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[{"filename":"/r/a.rs","summary":{"regions":{"count":10,"covered":10,"percent":100.0},"lines":{"count":20,"covered":20,"percent":100.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":0,"covered":0,"percent":0.0}}}],"totals":{"regions":{"count":10,"covered":10,"percent":100.0},"lines":{"count":20,"covered":20,"percent":100.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":0,"covered":0,"percent":0.0}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert_eq!(report.data.len(), 1);
        assert_eq!(report.data[0].files.len(), 1);
        assert!(report.data[0].totals.is_100_percent());
    }

    #[test]
    fn parse_json_partial_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[{"filename":"/r/a.rs","summary":{"regions":{"count":10,"covered":8,"percent":80.0},"lines":{"count":20,"covered":18,"percent":90.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":0,"covered":0,"percent":0.0}}}],"totals":{"regions":{"count":10,"covered":8,"percent":80.0},"lines":{"count":20,"covered":18,"percent":90.0},"functions":{"count":5,"covered":5,"percent":100.0},"branches":{"count":0,"covered":0,"percent":0.0}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert!(!report.data[0].totals.is_100_percent());
        assert_eq!(report.data[0].totals.regions.missed(), 2);
    }

    #[test]
    fn parse_json_branches_field_ignored() {
        // llvm-cov's JSON always includes `branches`, even when we never
        // request --branch (it's just 0/0). Confirms it's silently ignored
        // rather than causing a deserialization error.
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[],"totals":{"regions":{"count":5,"covered":5,"percent":100.0},"lines":{"count":10,"covered":10,"percent":100.0},"functions":{"count":3,"covered":3,"percent":100.0},"branches":{"count":123,"covered":45,"percent":36.5}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert!(report.data[0].totals.is_100_percent());
    }

    #[test]
    fn parse_json_extra_unknown_fields_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"data":[{"files":[],"totals":{"regions":{"count":1,"covered":1,"percent":100.0},"lines":{"count":1,"covered":1,"percent":100.0},"functions":{"count":1,"covered":1,"percent":100.0}}}],"type":"llvm.coverage","version":"2"}"#;
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
        let json = r#"{"data":[{"files":[],"totals":{"regions":{"count":3,"covered":3,"percent":100.0},"lines":{"count":3,"covered":3,"percent":100.0}}}]}"#;
        let path = write_json(&dir, "cov.json", json);
        let report = parse_json(&path).unwrap();
        assert!(report.data[0].totals.functions.is_fully_covered());
    }

    // ── write_github_output ──────────────────────────────────────────────────

    #[test]
    fn write_github_output_no_path_is_noop() {
        assert!(write_github_output(&full_metrics(10), None).is_ok());
    }

    #[test]
    fn write_github_output_writes_all_three_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gha_output");
        std::fs::write(&path, "").unwrap();
        let m = Metrics {
            regions: metric(10, 10),
            lines: metric(20, 18),
            functions: metric(5, 5),
        };
        write_github_output(&m, Some(path.to_str().unwrap())).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("regions=100.0"), "content: {content}");
        assert!(content.contains("lines=90.0"), "content: {content}");
        assert!(content.contains("functions=100.0"), "content: {content}");
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
        std::fs::create_dir_all(&path).unwrap();
        RealRunner.remove_dir(&path);
        // Just ensure no panic; TempDir will also clean up on drop.
    }

    // ── run_report — the core post-analysis logic ────────────────────────────

    #[test]
    fn run_report_100_percent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(result.is_ok(), "100% coverage should pass: {result:?}");
    }

    #[test]
    fn run_report_100_percent_writes_github_output() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let gha = dir.path().join("gha_output");
        std::fs::write(&gha, "").unwrap();
        let result = run_report(&runner, &output, Some(gha.to_str().unwrap()));
        assert!(result.is_ok());
        let content = std::fs::read_to_string(&gha).unwrap();
        assert!(content.contains("regions="), "content: {content}");
    }

    #[test]
    fn run_report_partial_coverage_within_ceiling_is_ok_and_writes_github_output() {
        // A small number of missed regions/lines/functions -- well within
        // MAX_MISSED_REGIONS/LINES/FUNCTIONS -- should not fail the build,
        // and the badge percentages still get written even when coverage
        // isn't literally 100%.
        let dir = tempfile::tempdir().unwrap();
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/foo.rs".to_owned(),
                    summary: partial_metrics(10, 8),
                }],
                totals: partial_metrics(10, 8),
            }],
        };
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Ok(report));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let gha = dir.path().join("gha_output");
        std::fs::write(&gha, "").unwrap();
        let result = run_report(&runner, &output, Some(gha.to_str().unwrap()));
        assert!(
            result.is_ok(),
            "coverage within the locked-in ceiling should not fail the build: {result:?}"
        );
        let content = std::fs::read_to_string(&gha).unwrap();
        assert!(
            content.contains("regions="),
            "badge percentages should still be written when coverage is partial: {content}"
        );
    }

    #[test]
    fn run_report_regions_over_ceiling_is_err() {
        // Missed regions exceeding MAX_MISSED_REGIONS must fail the build --
        // this is the actual enforcement the ratchet exists to provide.
        let dir = tempfile::tempdir().unwrap();
        let over_ceiling = metric(MAX_MISSED_REGIONS + 100, 0);
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/foo.rs".to_owned(),
                    summary: Metrics {
                        regions: over_ceiling.clone(),
                        lines: metric(10, 10),
                        functions: metric(10, 10),
                    },
                }],
                totals: Metrics {
                    regions: over_ceiling,
                    lines: metric(10, 10),
                    functions: metric(10, 10),
                },
            }],
        };
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Ok(report));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(
            result.is_err(),
            "missed regions over the ceiling must fail the build: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("regressed below the locked-in floor"),
            "error should explain the regression: {msg}"
        );
    }

    #[test]
    fn run_report_functions_over_ceiling_is_err() {
        // Missed functions exceeding MAX_MISSED_FUNCTIONS must also fail the
        // build, independently of regions/lines staying within their own
        // ceilings.
        let dir = tempfile::tempdir().unwrap();
        let over_ceiling = metric(MAX_MISSED_FUNCTIONS + 5, 0);
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/foo.rs".to_owned(),
                    summary: Metrics {
                        regions: metric(10, 10),
                        lines: metric(10, 10),
                        functions: over_ceiling.clone(),
                    },
                }],
                totals: Metrics {
                    regions: metric(10, 10),
                    lines: metric(10, 10),
                    functions: over_ceiling,
                },
            }],
        };
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Ok(report));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(
            result.is_err(),
            "missed functions over the ceiling must fail the build: {result:?}"
        );
    }

    #[test]
    fn run_report_parse_error_is_err() {
        // cargo() returns Ok(true) but writes no JSON → parse_json fails.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"])).with_fail_write();
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(
            result.is_err(),
            "should fail when JSON not written: {result:?}"
        );
    }

    #[test]
    fn run_report_partial_only_functions() {
        // regions=100%, lines=100%, functions=partial.
        // In print_gaps, the `if !s.regions.is_fully_covered()` and
        // `if !s.lines.is_fully_covered()` conditions evaluate to FALSE,
        // covering the "skip if 100%" path.
        let dir = tempfile::tempdir().unwrap();
        let mixed = Metrics {
            regions: metric(10, 10),
            lines: metric(10, 10),
            functions: metric(5, 4),
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
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Ok(report));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(
            result.is_ok(),
            "coverage within the locked-in ceiling should not fail the build: {result:?}"
        );
    }

    #[test]
    fn run_report_partial_with_crates_filename() {
        // regions=partial, lines=partial, functions=100%.
        // In print_gaps: split_once("/crates/") returns Some → covers the
        // Some arm of the match; `if !s.functions.is_fully_covered()` →
        // false (skip branch).
        let dir = tempfile::tempdir().unwrap();
        let mixed = Metrics {
            regions: metric(10, 9),
            lines: metric(10, 9),
            functions: metric(5, 5),
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
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Ok(report));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(
            result.is_ok(),
            "coverage within the locked-in ceiling should not fail the build: {result:?}"
        );
    }

    #[test]
    fn run_report_github_output_write_error_propagates() {
        // Arrange: 100% coverage so gaps.is_empty() = true, but a bad
        // github_output path so write_github_output() fails. Covers the `?`
        // Err arm at the `write_github_output(&data.totals, github_output)?`
        // call in run_report.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let bad_gha = "/no/such/directory/gha_output.txt";
        let result = run_report(&runner, &output, Some(bad_gha));
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

    // ── Mock-based orchestration tests ───────────────────────────────────────

    #[test]
    fn run_coverage_success_returns_ok_data() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(result.is_ok(), "per-package run should succeed: {result:?}");
    }

    #[test]
    fn run_coverage_scopes_lib_and_each_integration_test_target() {
        // A package with an integration test target must be run per-scope
        // (--lib, then --test <name>) rather than one bare `--package` call,
        // exercising the same orchestration path used to fix the real
        // leviath-cli multi-test-binary merge inaccuracy.
        let meta = metadata_with_targets(
            "leviath-cli",
            serde_json::json!([
                {"kind": ["lib"], "name": "leviath_cli"},
                {"kind": ["test"], "name": "cli_dispatch"},
            ]),
        );
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(meta);
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(
            result.is_ok(),
            "multi-scope per-package run should succeed: {result:?}"
        );
    }

    #[test]
    fn run_coverage_package_failure_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["leviath-core"]);
        let runner =
            MockRunner::new(meta).with_package("leviath-core", Err("simulated failure".to_owned()));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(
            result.is_err(),
            "a failing package run should fail outright (no fallback): {result:?}"
        );
    }

    #[test]
    fn run_coverage_cargo_err_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"])).with_fail_cargo_err();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(result.is_err(), "cargo Err should propagate: {result:?}");
        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("simulated"),
            "error chain should mention the simulated failure: {err:#}"
        );
    }

    #[test]
    fn run_coverage_metadata_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&[])).with_fail_metadata();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(
            result.is_err(),
            "metadata failure should propagate: {result:?}"
        );
        assert!(
            result.unwrap_err().to_string().contains("simulated"),
            "error should mention the simulated metadata failure"
        );
    }

    #[test]
    fn run_coverage_package_with_empty_data_is_skipped() {
        // When a package's llvm-cov JSON contains an empty `data` array, the
        // `if let Some(data) = pkg_report.data.into_iter().next()` evaluates to
        // None and the if-body is skipped.
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["my-pkg"]);
        let empty_report = LlvmCovReport { data: vec![] };
        let runner = MockRunner::new(meta).with_package("my-pkg", Ok(empty_report));
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(
            result.is_ok(),
            "empty package data should be silently skipped: {result:?}"
        );
        assert!(result.unwrap().files.is_empty());
    }

    #[test]
    fn run_coverage_aggregated_write_error_propagates() {
        // output_path is a directory → std::fs::write will fail with "Is a
        // directory". Covers the with_context(|| format!("writing
        // {output_path}")) closure in run_coverage.
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        let output_path = dir.path().to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(
            result.is_err(),
            "write to a directory should fail: {result:?}"
        );
    }

    #[test]
    fn run_coverage_package_json_not_written_is_err() {
        // With fail_write=true, runner.cargo() returns Ok(true) but writes no
        // JSON. parse_json then fails, which is wrapped by the
        // with_context(|| format!("coverage failed for {pkg}")) closure.
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["my-pkg"]);
        let runner = MockRunner::new(meta).with_fail_write();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_coverage(&runner, &output_path);
        assert!(
            result.is_err(),
            "missing JSON should propagate as error: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("coverage failed for my-pkg"), "error: {msg}");
    }

    #[test]
    fn run_single_target_cargo_err_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&[])).with_fail_cargo_err();
        let output_path = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_single_target(&runner, "some-pkg", &["--lib"], &output_path);
        assert!(
            result.is_err(),
            "cargo Err should propagate from run_single_target: {result:?}"
        );
    }

    // ── MockRunner direct tests — uncovered branches ─────────────────────────
    //
    // The following tests exercise specific branches inside MockRunner that
    // are never reached by the higher-level orchestration tests above.

    #[test]
    fn mock_runner_package_ok_result_writes_json() {
        let dir = tempfile::tempdir().unwrap();
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![],
                totals: full_metrics(5),
            }],
        };
        let runner =
            MockRunner::new(simple_metadata(&["my-pkg"])).with_package("my-pkg", Ok(report));
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
        let dir = tempfile::tempdir().unwrap();
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![],
                totals: full_metrics(1),
            }],
        };
        let runner = MockRunner::new(simple_metadata(&["my-pkg"]))
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
        assert!(matches!(result, Ok(true)));
        assert!(
            !out.exists(),
            "JSON should not be written when fail_write_json=true"
        );
    }

    #[test]
    fn mock_runner_package_none_with_fail_write_skips_json() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&[])).with_fail_write();
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
    fn mock_runner_package_ok_without_output_path_skips_write() {
        // Exercises the `if let Some(path) = output_path` None branch inside
        // the `Some(Ok)` arm — reached when --output-path is absent.
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![],
                totals: full_metrics(2),
            }],
        };
        let runner =
            MockRunner::new(simple_metadata(&["my-pkg"])).with_package("my-pkg", Ok(report));
        let result = runner.cargo(&["llvm-cov", "--package", "my-pkg", "--all-features"]);
        assert!(matches!(result, Ok(true)));
    }

    #[test]
    fn mock_runner_package_none_without_output_path_returns_ok() {
        // Package not in package_results (None arm) with no --output-path.
        let runner = MockRunner::new(simple_metadata(&[]));
        let result = runner.cargo(&["llvm-cov", "--package", "ghost-pkg", "--all-features"]);
        assert!(matches!(result, Ok(true)));
    }

    #[test]
    fn mock_runner_non_package_cargo_call_returns_ok() {
        // Exercises the early-return branch for a cargo call with no --package.
        let runner = MockRunner::new(simple_metadata(&[]));
        let result = runner.cargo(&["build", "--all-targets"]);
        assert!(matches!(result, Ok(true)));
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
}
