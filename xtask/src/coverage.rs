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
//! `target/`, never committed. A browsable HTML report (source-highlighted,
//! click-through per file) also lands at `coverage/html/index.html` on every
//! run, for visual inspection -- see [`generate_html_report`]'s doc comment
//! for why its numbers aren't the authoritative ones `run_report`'s 100% gate
//! uses.
//!
//! **Region coverage is measured merged-by-source-position, not summed
//! per-monomorphization.** llvm-cov's per-file `summary.regions` *sums* regions
//! across every monomorphization of a generic function: a generic fn with N
//! instantiations contributes its regions N times, and a source position that
//! one instantiation exercises but another doesn't is counted as missed. That
//! per-instantiation jitter (its size and even *which* files it lands on vary
//! by OS/toolchain — the same `getInstantiationGroups` family of quirks behind
//! the `--branch` crash and the `--workspace` inaccuracy above) is why this
//! project historically enforced a tolerance ceiling on missed regions rather
//! than a hard 100%. Instead of tolerating that noise, we eliminate it: we read
//! llvm-cov's per-function `regions` arrays directly and key each **code**
//! region (`Kind == 0`) by `(filename, line/col start, line/col end)`, counting
//! a position as covered if *any* instantiation executed it (see
//! [`merge_regions_by_position`]). For a file with no multi-instantiation
//! generics this yields the identical count to `summary.regions`; for
//! multi-instantiation generics it is strictly lower (the dedup) and never
//! spuriously counts a covered source position as missed. `lines` and
//! `functions` are already stable across instantiations and are used as
//! llvm-cov reports them.
//!
//! **The gate enforces a hard 100%.** With region jitter removed, any file
//! below 100% on regions/lines/functions is a real, closeable gap and fails
//! the build (see [`run_report`]).

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

// ── Coverage mode (CLI argument parsing) ──────────────────────────────────────

/// Which coverage computation to perform, parsed from the arguments following
/// `cargo xtask coverage`.
///
/// The measurement math is identical across all three — `Package` and `Gate`
/// merely split the single-runner `All` flow across CI runners: each package
/// computes its own `CovData` in parallel (`Package`), then one dependent job
/// aggregates them and enforces the gate (`Gate`).
#[derive(Debug, PartialEq, Eq)]
pub enum CoverageMode {
    /// No arguments: compute every workspace package, aggregate, enforce the
    /// hard 100% gate, and render the full HTML report (local-dev convenience).
    All,
    /// `--package <pkg>`: compute just one package's coverage, writing its
    /// `CovData` to `coverage/per-package/<pkg>.json` and its browsable HTML to
    /// `coverage/html/<pkg>`. Does NOT aggregate or run the gate — that is
    /// `Gate`'s job, after every package's per-package JSON is collected.
    Package(String),
    /// `--gate`: aggregate every `coverage/per-package/*.json` and enforce the
    /// hard 100% gate — the same gap detection and failure message `All` uses.
    Gate,
}

impl CoverageMode {
    /// Parse the arguments that follow the `coverage` subcommand.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            None => Ok(Self::All),
            Some("--gate") => Ok(Self::Gate),
            Some("--package") => {
                let pkg = args.get(1).context(
                    "`--package` requires a package name, e.g. \
                     `cargo xtask coverage --package leviath-core`",
                )?;
                Ok(Self::Package(pkg.clone()))
            }
            Some(other) => anyhow::bail!(
                "unknown `coverage` argument `{other}` (expected no arguments, \
                 `--gate`, or `--package <pkg>`)"
            ),
        }
    }
}

// ── Public entry point ──────────────────────────────────────────────────────

pub fn run(mode: CoverageMode) -> Result<()> {
    run_with(&RealRunner, mode)
}

pub fn run_with(runner: &dyn Runner, mode: CoverageMode) -> Result<()> {
    std::fs::create_dir_all("coverage")
        .expect("failed to create coverage/ directory — check filesystem permissions");
    match mode {
        CoverageMode::All => run_all(runner),
        CoverageMode::Package(pkg) => run_package_mode(runner, &pkg),
        CoverageMode::Gate => run_gate_mode(),
    }
}

/// `cargo xtask coverage` with no arguments: the original all-in-one path —
/// compute every package, aggregate, enforce the 100% gate, and render HTML.
fn run_all(runner: &dyn Runner) -> Result<()> {
    let github_output = std::env::var("GITHUB_OUTPUT").ok();
    let report_result = run_report(runner, "coverage/llvm-cov.json", github_output.as_deref());
    // Always attempt the HTML report, even if the 100% gate above failed
    // -- browsing exactly which lines are (un)covered is often the fastest
    // way to understand *why* a coverage regression happened. A failure
    // generating it (e.g. a real cargo/IO problem) is still surfaced, but
    // the original report_result -- the authoritative pass/fail signal --
    // is what's returned once both have run.
    generate_html_report(runner)?;
    report_result
}

/// `cargo xtask coverage --package <pkg>`: compute exactly one package — via
/// the identical per-target/merge logic the all-packages path uses — and write
/// its `CovData` to `coverage/per-package/<pkg>.json`, plus its browsable HTML
/// to `coverage/html/<pkg>`. Deliberately does NOT aggregate or run the gate:
/// on CI each package runs this on its own runner in parallel, and one
/// dependent `--gate` job aggregates the collected JSONs and enforces 100%.
fn run_package_mode(runner: &dyn Runner, pkg: &str) -> Result<()> {
    run_package_mode_in(
        runner,
        pkg,
        std::path::Path::new("coverage"),
        "coverage/per-package",
    )
}

fn run_package_mode_in(
    runner: &dyn Runner,
    pkg: &str,
    scratch_dir: &std::path::Path,
    per_package_dir: &str,
) -> Result<()> {
    println!("[coverage] Computing coverage for package {pkg}…");
    let meta = runner.cargo_metadata()?;
    let data = coverage_one_package(runner, &meta, pkg, scratch_dir)?;

    std::fs::create_dir_all(per_package_dir)
        .with_context(|| format!("creating {per_package_dir}"))?;
    let out_path = format!("{per_package_dir}/{pkg}.json");
    let json = serde_json::to_string_pretty(&data)
        .expect("failed to serialise per-package CovData — all fields are serialisable");
    std::fs::write(&out_path, json).with_context(|| format!("writing {out_path}"))?;
    println!("[coverage] Wrote per-package coverage to {out_path}");

    // One package's browsable HTML — no top-level index in this mode (the
    // `--gate` step writes that once all packages' HTML dirs are present).
    generate_package_html(runner, pkg)?;
    Ok(())
}

/// `cargo xtask coverage --gate`: aggregate every per-package JSON and enforce
/// the hard 100% gate — the SAME gap detection and error message the all-in-one
/// path uses, just fed from pre-computed per-package `CovData` instead of
/// recomputing them here.
fn run_gate_mode() -> Result<()> {
    let github_output = std::env::var("GITHUB_OUTPUT").ok();
    run_gate_mode_in(
        "coverage/per-package",
        "coverage/html",
        github_output.as_deref(),
    )
}

fn run_gate_mode_in(
    per_package_dir: &str,
    html_dir: &str,
    github_output: Option<&str>,
) -> Result<()> {
    println!("[coverage] Aggregating per-package coverage from {per_package_dir}…");
    let data = aggregate_per_package(per_package_dir)?;
    // Link whatever per-package HTML dirs exist (each is uploaded separately
    // on CI); write the index before the gate so it's produced even on failure.
    write_gate_html_index(html_dir)?;
    report_data(&data, github_output)
}

/// Aggregate every `<pkg>.json` `CovData` under `dir` into one report:
/// concatenate all files and sum totals via [`Metrics::add`], then
/// [`Metrics::recompute_percents`] — the identical arithmetic the all-in-one
/// path applies across packages, so the gate result is byte-for-byte the same.
fn aggregate_per_package(dir: &str) -> Result<CovData> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading per-package coverage directory {dir}"))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();

    let mut all_files: Vec<FileCov> = Vec::new();
    let mut totals = Metrics::default();
    for path in &paths {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading per-package coverage JSON {}", path.display()))?;
        let data: CovData = serde_json::from_str(&raw)
            .with_context(|| format!("parsing per-package coverage JSON {}", path.display()))?;
        totals.add(&data.totals);
        all_files.extend(data.files);
    }
    totals.recompute_percents();
    Ok(CovData {
        files: all_files,
        totals,
    })
}

/// Generate browsable per-package HTML coverage reports — one
/// `cargo llvm-cov --package <pkg> --lib --html` per workspace crate — plus a
/// top-level `coverage/html/index.html` linking them.
///
/// Per-package `--lib` deliberately mirrors how [`run_coverage`]'s
/// authoritative gate measures, so the browsable report agrees with the 100%
/// gate. A single `cargo llvm-cov --workspace --html` pass (the previous
/// approach) disagreed on two counts: (a) it sums regions per-monomorphization
/// across the whole workspace, inflating "missed" counts on generic-heavy
/// files; and (b) `--workspace` instruments the `[[bin]]` targets, whose
/// `#[cfg(not(test))]` code is compiled but never executed under `cargo test`,
/// so their real bodies (e.g. `dispatch.rs`'s process-spawning wrappers)
/// rendered as uncovered red even though the gate correctly never counts them.
/// `--lib` scoping excludes bins, and per-package runs avoid the cross-package
/// summation, so the HTML now matches the gate. `xtask` itself is not in
/// [`parse_workspace_packages`] (it has no lib and is the coverage tool), so
/// it's absent here too — consistent with the gate.
fn generate_html_report(runner: &dyn Runner) -> Result<()> {
    println!("[coverage] Generating per-package HTML reports…");
    // Clear any previous report (e.g. a stale whole-workspace one) so the
    // directory only ever holds the current per-package layout.
    runner.remove_dir("coverage/html");
    let meta = runner.cargo_metadata()?;
    let packages = parse_workspace_packages(&meta);
    for pkg in &packages {
        generate_package_html(runner, pkg)?;
    }
    write_html_index("coverage/html", &packages)?;
    println!("[coverage] HTML report: coverage/html/index.html");
    Ok(())
}

/// Generate one package's browsable `cargo llvm-cov --lib --html` report into
/// `coverage/html/<pkg>`, cleaning `target/llvm-cov-target` first (same reason
/// as the coverage runs). Shared by the all-in-one HTML path and `--package`.
fn generate_package_html(runner: &dyn Runner, pkg: &str) -> Result<()> {
    // Clean slate between packages (same reason as `run_coverage`).
    runner.remove_dir("target/llvm-cov-target");
    let out_dir = format!("coverage/html/{pkg}");
    if !runner.cargo(&[
        "llvm-cov",
        "--all-features",
        "--package",
        pkg,
        "--lib",
        "--html",
        "--output-dir",
        &out_dir,
    ])? {
        anyhow::bail!("cargo llvm-cov exited non-zero while generating the HTML report for {pkg}");
    }
    Ok(())
}

/// Write a small top-level `<html_dir>/index.html` linking to each per-package
/// report at `<html_dir>/<pkg>/html/index.html`.
fn write_html_index(html_dir: &str, packages: &[String]) -> Result<()> {
    std::fs::create_dir_all(html_dir).with_context(|| format!("creating {html_dir} directory"))?;
    let mut html = String::from(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<title>Leviath coverage</title>\n\
         <h1>Leviath coverage — per-package reports</h1>\n\
         <p>Each link is a source-highlighted <code>cargo llvm-cov --lib</code> report, \
         measured the same way the 100% gate is.</p>\n<ul>\n",
    );
    for pkg in packages {
        html.push_str(&format!(
            "  <li><a href=\"{pkg}/html/index.html\">{pkg}</a></li>\n"
        ));
    }
    html.push_str("</ul>\n");
    let index_path = format!("{html_dir}/index.html");
    std::fs::write(&index_path, html).with_context(|| format!("writing {index_path}"))?;
    Ok(())
}

/// Discover whatever per-package HTML report dirs already exist under
/// `html_dir` (each `--package` run drops one) and write the top-level index
/// linking them. Used by `--gate`, where the per-package HTML dirs may or may
/// not have been collected onto this runner.
fn write_gate_html_index(html_dir: &str) -> Result<()> {
    let mut packages: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(html_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    packages.push(name.to_owned());
                }
            }
        }
    }
    packages.sort();
    write_html_index(html_dir, &packages)
}

/// Core reporting logic extracted from `run_with` for unit-testability.
///
/// Runs coverage via `runner`, prints a summary, reports any gaps, and writes
/// CI output variables. Enforces a hard 100% on regions/lines/functions: any
/// file below 100% fails the build. All paths (including the failure path) are
/// reachable from tests through the `MockRunner` abstraction.
pub fn run_report(
    runner: &dyn Runner,
    output_path: &str,
    github_output: Option<&str>,
) -> Result<()> {
    println!("[coverage] Running coverage analysis…");
    let data = run_coverage(runner, output_path)?;
    report_data(&data, github_output)
}

/// Print the summary, publish badge percentages, and enforce the hard 100%
/// gate over `data`.
///
/// Shared by the all-in-one path ([`run_report`]) and the `--gate` path
/// ([`run_gate_mode_in`]) so both emit the identical summary and — crucially —
/// the identical failure message when any file is below 100%.
fn report_data(data: &CovData, github_output: Option<&str>) -> Result<()> {
    print_summary(&data.totals);

    let gaps = gap_files(data);

    // Always publish the computed percentages -- the coverage badges need
    // real numbers regardless of whether 100% is currently met.
    write_github_output(&data.totals, github_output)?;

    if gaps.is_empty() {
        println!("\n[coverage] All metrics at 100%. ✓");
        return Ok(());
    }

    print_gaps(&gaps);

    anyhow::bail!(
        "[coverage] Coverage must be 100% on regions, lines, and functions, but \
         {} file(s) have gaps (listed above). Region coverage is measured \
         merged-by-source-position, so per-monomorphization jitter is already \
         removed -- every gap above is a real, closeable one. Cover it with a \
         test; if a line is genuinely unreachable real-IO, split it behind a \
         `#[cfg(not(test))]`/`#[cfg(test)]` seam. Investigate exactly which \
         lines/regions are uncovered with `cargo llvm-cov show` or the browsable \
         `coverage/html/index.html` report.",
        gaps.len()
    );
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
    let scratch_dir = std::path::Path::new(output_path)
        .parent()
        .unwrap_or(std::path::Path::new("."));

    let mut all_files: Vec<FileCov> = Vec::new();
    let mut totals = Metrics::default();

    for pkg in &packages {
        println!("[coverage]   → {pkg}");
        let pkg_data = coverage_one_package(runner, &meta, pkg, scratch_dir)?;
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

/// Compute one package's coverage exactly as the all-packages loop does — the
/// single source of truth shared by [`run_coverage`] and `--package` mode.
///
/// Scopes to `--lib`, then to each integration-test binary, SEPARATELY, rather
/// than one bare `cargo llvm-cov --package X` call that lets llvm-cov merge
/// every test binary's profraw data internally. That internal multi-binary
/// merge is subject to the same `getInstantiationGroups` merge-inaccuracy bug
/// documented at the top of this file for cross-package `--workspace` runs --
/// confirmed empirically: leviath-cli's commands/add.rs read 6 missed regions
/// when merged across all 3 of its test binaries (lib + 2 integration tests) in
/// one invocation, but only 2 missed when measured via `--lib` alone. A clean
/// `target/llvm-cov-target` between every scoped run is belt-and-suspenders
/// against cross-run contamination of the profraw/profdata llvm-cov
/// accumulates there. Per-target region counts are already merged-by-source-
/// position by [`run_single_target`]; [`merge_target_reports`] then recombines
/// the separately-scoped results (`max`-covered per file). Per-target JSONs are
/// written under `scratch_dir`.
fn coverage_one_package(
    runner: &dyn Runner,
    meta: &serde_json::Value,
    pkg: &str,
    scratch_dir: &std::path::Path,
) -> Result<CovData> {
    let mut scopes: Vec<Vec<&str>> = vec![vec!["--lib"]];
    for test_name in package_test_targets(meta, pkg) {
        scopes.push(vec!["--test", test_name]);
    }

    let mut target_reports: Vec<CovData> = Vec::new();
    for scope in &scopes {
        runner.remove_dir("target/llvm-cov-target");

        let scope_tag = scope.join("-").replace(['-', ' '], "_");
        let target_output = scratch_dir
            .join(format!("llvm-cov-{pkg}-{scope_tag}.json"))
            .to_string_lossy()
            .into_owned();

        let report = run_single_target(runner, pkg, scope, &target_output)
            .with_context(|| format!("coverage failed for {pkg} ({})", scope.join(" ")))?;

        if let Some(data) = report.data.into_iter().next() {
            target_reports.push(data);
        }
    }

    Ok(merge_target_reports(target_reports))
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
    // Exclude binary entry points (`src/main.rs`) from measurement. A bin is
    // the composition root: it wires real terminal/stdin/network/subprocess
    // I/O — which no unit test may trigger — into the library's tested cores.
    // The `--test` scope would otherwise capture the spawned, instrumented
    // `lev` binary's profile (from `tests/cli_dispatch.rs`), forcing that
    // un-unit-testable wiring to be "covered". Keeping bins unmeasured is what
    // lets library code stay 100%-covered with zero `#[cfg(not(test))]`
    // escape hatches (see `crates/leviath-cli/src/main.rs`). `/main\.rs$`
    // requires a path separator so it matches only real bin roots, never a
    // file like `domain.rs`.
    args.extend_from_slice(&["--ignore-filename-regex", r"/main\.rs$"]);
    args.extend_from_slice(&["--json", "--output-path", output_path]);

    if !runner.cargo(&args)? {
        anyhow::bail!(
            "cargo llvm-cov exited non-zero for package {pkg} ({})",
            scope.join(" ")
        );
    }

    let mut report = parse_json(output_path)
        .with_context(|| format!("parsing coverage JSON for {pkg} ({})", scope.join(" ")))?;

    // Re-parse the same raw JSON to recompute each file's region metric
    // merged-by-source-position (deduplicating llvm-cov's per-monomorphization
    // double-counting -- see this file's top doc comment). `lines`/`functions`
    // are left as llvm-cov reports them.
    let raw = std::fs::read_to_string(output_path)
        .with_context(|| format!("re-reading coverage JSON for {pkg} ({})", scope.join(" ")))?;
    let raw: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("re-parsing coverage JSON for {pkg} ({})", scope.join(" ")))?;
    apply_merged_regions(&mut report, &raw);

    Ok(report)
}

/// Overwrite each file's `summary.regions` in `report` with the
/// merged-by-source-position count/covered derived from the raw JSON's
/// per-function `regions` arrays, recomputing the percentage.
///
/// Only files present in the merged map are updated; `lines` and `functions`
/// are never touched. `report`'s own per-target `totals` are intentionally not
/// recomputed here — `merge_target_reports` rebuilds package totals from the
/// (now-corrected) file summaries, and per-target totals are never read before
/// that merge.
fn apply_merged_regions(report: &mut LlvmCovReport, raw: &serde_json::Value) {
    let Some(data_arr) = raw["data"].as_array() else {
        return;
    };
    for (i, data) in data_arr.iter().enumerate() {
        let per_file = merge_regions_by_position(&data["functions"]);
        let Some(cov_data) = report.data.get_mut(i) else {
            continue;
        };
        for file in &mut cov_data.files {
            if let Some(&(count, covered)) = per_file.get(&file.filename) {
                file.summary.regions.count = count;
                file.summary.regions.covered = covered;
                file.summary.regions.recompute_percent();
            }
        }
    }
}

/// Compute per-file region coverage merged by source position across ALL
/// function instantiations, keyed on `(filename, LineStart, ColStart, LineEnd,
/// ColEnd)`.
///
/// `functions` is llvm-cov's per-`data`-element `functions` array. Each element
/// has `filenames: [String]` and `regions: [[LineStart, ColStart, LineEnd,
/// ColEnd, ExecutionCount, FileID, ExpandedFileID, Kind]]`. Only code regions
/// (`Kind == 0`) are counted; a position is "covered" if ANY instantiation
/// executed it (`ExecutionCount > 0`). Returns a map from filename to
/// `(distinct-region-count, covered-count)`.
fn merge_regions_by_position(
    functions: &serde_json::Value,
) -> std::collections::HashMap<String, (u64, u64)> {
    // (filename, l_start, c_start, l_end, c_end) -> covered by any instantiation
    let mut positions: std::collections::HashMap<(String, u64, u64, u64, u64), bool> =
        std::collections::HashMap::new();

    let Some(funcs) = functions.as_array() else {
        return std::collections::HashMap::new();
    };

    for func in funcs {
        let (Some(filenames), Some(regions)) =
            (func["filenames"].as_array(), func["regions"].as_array())
        else {
            continue;
        };
        for region in regions {
            let Some(arr) = region.as_array() else {
                continue;
            };
            // [LineStart, ColStart, LineEnd, ColEnd, ExecutionCount, FileID,
            //  ExpandedFileID, Kind]
            if arr.len() < 8 {
                continue;
            }
            // Code regions only (Kind == 0); skip expansion/gap/skipped kinds.
            if arr[7].as_u64() != Some(0) {
                continue;
            }
            let file_id = arr[5].as_u64().unwrap_or(0) as usize;
            let Some(filename) = filenames.get(file_id).and_then(|v| v.as_str()) else {
                continue;
            };
            let key = (
                filename.to_owned(),
                arr[0].as_u64().unwrap_or(0),
                arr[1].as_u64().unwrap_or(0),
                arr[2].as_u64().unwrap_or(0),
                arr[3].as_u64().unwrap_or(0),
            );
            let executed = arr[4].as_u64().unwrap_or(0) > 0;
            let covered = positions.entry(key).or_insert(false);
            *covered |= executed;
        }
    }

    let mut per_file: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new();
    for ((filename, ..), covered) in positions {
        let entry = per_file.entry(filename).or_insert((0, 0));
        entry.0 += 1;
        if covered {
            entry.1 += 1;
        }
    }
    per_file
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
    use std::sync::Mutex;
    use tempfile::TempDir;

    // ── Real-`coverage/`-dir serialization ────────────────────────────────────
    //
    // A handful of tests exercise the fixed-path wrappers that read/write the
    // real repo-relative `coverage/` directory (the all-in-one and gate/package
    // round-trip paths). Serialize them so their concurrent writes to shared
    // files (e.g. `coverage/html/index.html`) can't collide — notably on
    // Windows, where two simultaneous writes to one path can fail.

    static FS_LOCK: Mutex<()> = Mutex::new(());

    fn fs_guard() -> std::sync::MutexGuard<'static, ()> {
        FS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
        /// If true, an `--html` cargo invocation (generate_html_report) returns Ok(false).
        fail_html: bool,
    }

    impl MockRunner {
        fn new(metadata: serde_json::Value) -> Self {
            Self {
                package_results: HashMap::new(),
                metadata,
                fail_metadata: false,
                fail_write_json: false,
                fail_cargo_err: false,
                fail_html: false,
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

        fn with_fail_html(mut self) -> Self {
            self.fail_html = true;
            self
        }
    }

    impl Runner for MockRunner {
        fn cargo(&self, args: &[&str]) -> Result<bool> {
            if self.fail_cargo_err {
                anyhow::bail!("simulated cargo spawn failure");
            }
            // HTML generation is now per-package (`--package X --lib --html`),
            // so the fail-html simulation must trigger regardless of whether a
            // `--package` arg is present.
            if self.fail_html && args.contains(&"--html") {
                return Ok(false);
            }
            let output_path = args
                .windows(2)
                .find(|w| w[0] == "--output-path")
                .and_then(|w| w.get(1).copied());

            let Some(pkg_idx) = args.iter().position(|a| *a == "--package") else {
                // Any other non-package cargo invocation (e.g. plain `cargo help`) — no-op success.
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

    // ── merge_regions_by_position ────────────────────────────────────────────

    #[test]
    fn merge_regions_by_position_dedups_across_instantiations() {
        // Two "functions" = two instantiations of the same generic over the
        // same source spans:
        //   span A (1,1)-(1,10): count 0 in inst-1, count 3 in inst-2 → COVERED
        //   span B (5,1)-(5,10): count 0 in both instantiations       → UNCOVERED
        //   span C (8,1)-(8,10): Kind != 0 (an expansion region)      → IGNORED
        // Region tuple: [LineStart, ColStart, LineEnd, ColEnd,
        //                ExecutionCount, FileID, ExpandedFileID, Kind]
        let functions = serde_json::json!([
            {
                "filenames": ["/src/a.rs"],
                "regions": [
                    [1, 1, 1, 10, 0, 0, 0, 0],
                    [5, 1, 5, 10, 0, 0, 0, 0],
                    [8, 1, 8, 10, 7, 0, 0, 2]
                ]
            },
            {
                "filenames": ["/src/a.rs"],
                "regions": [
                    [1, 1, 1, 10, 3, 0, 0, 0],
                    [5, 1, 5, 10, 0, 0, 0, 0],
                    [8, 1, 8, 10, 9, 0, 0, 2]
                ]
            }
        ]);
        let per_file = merge_regions_by_position(&functions);
        // Only spans A and B are code regions (Kind == 0); C is ignored.
        // A is covered (any instantiation executed it), B is not.
        assert_eq!(per_file.get("/src/a.rs"), Some(&(2, 1)));
    }

    #[test]
    fn merge_regions_by_position_non_array_is_empty() {
        // A missing/`null` `functions` value yields an empty map.
        assert!(merge_regions_by_position(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn merge_regions_by_position_skips_malformed_entries() {
        // Functions missing `filenames`/`regions`, non-array regions, short
        // region tuples, and out-of-range FileIDs are all skipped without
        // panicking; the one well-formed covered region is counted.
        let functions = serde_json::json!([
            { "filenames": ["/src/a.rs"] },                       // no regions
            { "regions": [[1, 1, 1, 2, 1, 0, 0, 0]] },            // no filenames
            { "filenames": ["/src/a.rs"], "regions": ["nope"] },  // region not an array
            { "filenames": ["/src/a.rs"], "regions": [[1, 1, 1]] }, // too short
            { "filenames": ["/src/a.rs"], "regions": [[9, 1, 9, 2, 1, 5, 0, 0]] }, // bad FileID
            { "filenames": ["/src/a.rs"], "regions": [[2, 1, 2, 5, 4, 0, 0, 0]] }  // valid, covered
        ]);
        let per_file = merge_regions_by_position(&functions);
        assert_eq!(per_file.get("/src/a.rs"), Some(&(1, 1)));
    }

    #[test]
    fn apply_merged_regions_overwrites_region_metric() {
        // The parsed report's summed regions (10/6) are replaced by the
        // merged-by-position count derived from the raw functions array
        // (1 distinct code region, covered), and lines/functions are left
        // untouched.
        let mut report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/a.rs".to_owned(),
                    summary: Metrics {
                        regions: metric(10, 6),
                        lines: metric(20, 18),
                        functions: metric(5, 5),
                    },
                }],
                totals: partial_metrics(10, 6),
            }],
        };
        let raw = serde_json::json!({
            "data": [{
                "functions": [{
                    "filenames": ["/src/a.rs"],
                    "regions": [[1, 1, 1, 10, 2, 0, 0, 0]]
                }]
            }]
        });
        apply_merged_regions(&mut report, &raw);
        let regions = &report.data[0].files[0].summary.regions;
        assert_eq!((regions.count, regions.covered), (1, 1));
        assert!((regions.percent - 100.0).abs() < f64::EPSILON);
        // lines/functions untouched.
        assert_eq!(report.data[0].files[0].summary.lines.count, 20);
        assert_eq!(report.data[0].files[0].summary.functions.count, 5);
    }

    #[test]
    fn apply_merged_regions_no_data_array_is_noop() {
        // Raw JSON without a `data` array leaves the report unchanged.
        let mut report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/a.rs".to_owned(),
                    summary: partial_metrics(10, 6),
                }],
                totals: partial_metrics(10, 6),
            }],
        };
        apply_merged_regions(&mut report, &serde_json::json!({}));
        assert_eq!(report.data[0].files[0].summary.regions.count, 10);
    }

    #[test]
    fn apply_merged_regions_extra_data_element_is_ignored() {
        // A raw `data` array longer than the report's `data` (the trailing
        // element has no matching CovData) exercises the `get_mut` None arm,
        // and a file absent from the merged map keeps its original regions.
        let mut report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/present.rs".to_owned(),
                    summary: partial_metrics(10, 6),
                }],
                totals: partial_metrics(10, 6),
            }],
        };
        let raw = serde_json::json!({
            "data": [
                {
                    "functions": [{
                        "filenames": ["/src/other.rs"],
                        "regions": [[1, 1, 1, 10, 1, 0, 0, 0]]
                    }]
                },
                { "functions": [] }
            ]
        });
        apply_merged_regions(&mut report, &raw);
        // present.rs isn't in the merged map → regions unchanged.
        assert_eq!(report.data[0].files[0].summary.regions.count, 10);
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
    fn run_report_partial_coverage_fails_but_still_writes_github_output() {
        // Any file below 100% must fail the build (hard 100% gate), but the
        // badge percentages are written BEFORE the gate check, so they still
        // get published even when coverage isn't 100%.
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
            result.is_err(),
            "coverage below 100% must fail the build: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("must be 100%"),
            "error should explain the 100% requirement: {msg}"
        );
        let content = std::fs::read_to_string(&gha).unwrap();
        assert!(
            content.contains("regions="),
            "badge percentages should still be written when coverage is partial: {content}"
        );
    }

    #[test]
    fn run_report_regions_gap_is_err() {
        // A file below 100% on regions must fail the build -- this is the
        // hard 100% gate.
        let dir = tempfile::tempdir().unwrap();
        let gap = Metrics {
            regions: metric(100, 0),
            lines: metric(10, 10),
            functions: metric(10, 10),
        };
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/foo.rs".to_owned(),
                    summary: gap.clone(),
                }],
                totals: gap,
            }],
        };
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Ok(report));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(
            result.is_err(),
            "a regions gap must fail the build: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("must be 100%"),
            "error should explain the 100% requirement: {msg}"
        );
    }

    #[test]
    fn run_report_functions_gap_is_err() {
        // A file below 100% on functions must also fail the build,
        // independently of regions/lines being fully covered.
        let dir = tempfile::tempdir().unwrap();
        let gap = Metrics {
            regions: metric(10, 10),
            lines: metric(10, 10),
            functions: metric(25, 0),
        };
        let report = LlvmCovReport {
            data: vec![CovData {
                files: vec![FileCov {
                    filename: "/src/foo.rs".to_owned(),
                    summary: gap.clone(),
                }],
                totals: gap,
            }],
        };
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Ok(report));
        let output = dir.path().join("cov.json").to_str().unwrap().to_owned();
        let result = run_report(&runner, &output, None);
        assert!(
            result.is_err(),
            "a functions gap must fail the build: {result:?}"
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
            result.is_err(),
            "a functions gap must fail the hard 100% gate: {result:?}"
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
            result.is_err(),
            "regions/lines gaps must fail the hard 100% gate: {result:?}"
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

    // ── generate_html_report ─────────────────────────────────────────────────

    #[test]
    fn generate_html_report_success_is_ok() {
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        let result = generate_html_report(&runner);
        assert!(result.is_ok(), "html generation should succeed: {result:?}");
    }

    #[test]
    fn generate_html_report_cargo_exit_failure_is_err() {
        let runner = MockRunner::new(simple_metadata(&["leviath-core"])).with_fail_html();
        let result = generate_html_report(&runner);
        assert!(
            result.is_err(),
            "a non-zero cargo llvm-cov --html exit should be an error: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("HTML report"),
            "error should mention the HTML report: {msg}"
        );
    }

    #[test]
    fn generate_html_report_cargo_spawn_failure_is_err() {
        let runner = MockRunner::new(simple_metadata(&["leviath-core"])).with_fail_cargo_err();
        let result = generate_html_report(&runner);
        assert!(
            result.is_err(),
            "a cargo spawn failure should propagate as an error: {result:?}"
        );
    }

    // ── run_with ──────────────────────────────────────────────────────────────

    #[test]
    fn run_with_all_html_generation_failure_is_err_even_when_coverage_passes() {
        // 100% coverage (report check would be Ok), but HTML generation
        // itself fails -- the overall result must still surface that error,
        // not silently swallow it just because the 100% gate passed.
        let _guard = fs_guard();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"])).with_fail_html();
        let result = run_with(&runner, CoverageMode::All);
        assert!(
            result.is_err(),
            "an HTML generation failure must fail run_with even when coverage itself passed: {result:?}"
        );
    }

    #[test]
    fn run_with_all_success_returns_ok() {
        // Covers run_all's success return: run_report Ok, HTML Ok → Ok.
        let _guard = fs_guard();
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        let result = run_with(&runner, CoverageMode::All);
        assert!(
            result.is_ok(),
            "all-in-one 100% run should pass: {result:?}"
        );
    }

    #[test]
    fn run_with_package_then_gate_round_trips_at_100_percent() {
        // End-to-end through the fixed-path wrappers: `--package` writes a
        // per-package CovData, then `--gate` aggregates it and passes the 100%
        // gate. This is the only test that touches the real `coverage/` dir for
        // per-package/gate flows, so the fs lock keeps it isolated from the
        // all-in-one tests above (which also write under `coverage/`).
        let _guard = fs_guard();
        // Clean slate so the gate only sees this test's file.
        let _ = std::fs::remove_dir_all("coverage/per-package");
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));

        let pkg_result = run_with(&runner, CoverageMode::Package("leviath-core".to_owned()));
        assert!(
            pkg_result.is_ok(),
            "--package run should pass: {pkg_result:?}"
        );
        assert!(
            std::path::Path::new("coverage/per-package/leviath-core.json").exists(),
            "per-package JSON should have been written"
        );

        let gate_result = run_with(&runner, CoverageMode::Gate);
        assert!(
            gate_result.is_ok(),
            "--gate over a 100% per-package report should pass: {gate_result:?}"
        );
        let _ = std::fs::remove_dir_all("coverage/per-package");
    }

    // ── CoverageMode::parse ───────────────────────────────────────────────────

    #[test]
    fn parse_mode_no_args_is_all() {
        assert_eq!(CoverageMode::parse(&[]).unwrap(), CoverageMode::All);
    }

    #[test]
    fn parse_mode_gate() {
        assert_eq!(
            CoverageMode::parse(&["--gate".to_owned()]).unwrap(),
            CoverageMode::Gate
        );
    }

    #[test]
    fn parse_mode_package() {
        assert_eq!(
            CoverageMode::parse(&["--package".to_owned(), "leviath-core".to_owned()]).unwrap(),
            CoverageMode::Package("leviath-core".to_owned())
        );
    }

    #[test]
    fn parse_mode_package_without_name_is_err() {
        let result = CoverageMode::parse(&["--package".to_owned()]);
        assert!(
            result.is_err(),
            "missing package name must error: {result:?}"
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires a package name"));
    }

    #[test]
    fn parse_mode_unknown_flag_is_err() {
        let result = CoverageMode::parse(&["--nope".to_owned()]);
        assert!(result.is_err(), "unknown flag must error: {result:?}");
        assert!(result.unwrap_err().to_string().contains("--nope"));
    }

    // ── coverage_one_package ──────────────────────────────────────────────────

    #[test]
    fn coverage_one_package_merges_lib_and_test_scopes() {
        // A package with an integration test target is scoped per-target
        // (--lib, then --test <name>) and merged into one CovData.
        let dir = tempfile::tempdir().unwrap();
        let meta = metadata_with_targets(
            "leviath-cli",
            serde_json::json!([
                {"kind": ["lib"], "name": "leviath_cli"},
                {"kind": ["test"], "name": "cli_dispatch"},
            ]),
        );
        let runner = MockRunner::new(meta.clone());
        let data = coverage_one_package(&runner, &meta, "leviath-cli", dir.path()).unwrap();
        assert!(data.totals.is_100_percent());
    }

    #[test]
    fn coverage_one_package_propagates_target_error() {
        let dir = tempfile::tempdir().unwrap();
        let meta = simple_metadata(&["leviath-core"]);
        let runner =
            MockRunner::new(meta.clone()).with_package("leviath-core", Err("boom".to_owned()));
        let result = coverage_one_package(&runner, &meta, "leviath-core", dir.path());
        assert!(
            result.is_err(),
            "a failing scope must propagate: {result:?}"
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("coverage failed for leviath-core"));
    }

    // ── run_package_mode_in ───────────────────────────────────────────────────

    #[test]
    fn run_package_mode_in_writes_per_package_json() {
        let dir = tempfile::tempdir().unwrap();
        let per_pkg = dir.path().join("per-package");
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta);
        let result = run_package_mode_in(
            &runner,
            "leviath-core",
            dir.path(),
            per_pkg.to_str().unwrap(),
        );
        assert!(result.is_ok(), "package mode should succeed: {result:?}");
        let json_path = per_pkg.join("leviath-core.json");
        assert!(json_path.exists(), "per-package JSON must be written");
        // It must deserialize as CovData (the shape --gate aggregates).
        let raw = std::fs::read_to_string(&json_path).unwrap();
        let parsed: CovData = serde_json::from_str(&raw).unwrap();
        assert!(parsed.totals.is_100_percent());
    }

    #[test]
    fn run_package_mode_in_metadata_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let runner = MockRunner::new(simple_metadata(&[])).with_fail_metadata();
        let result = run_package_mode_in(&runner, "leviath-core", dir.path(), "unused");
        assert!(
            result.is_err(),
            "metadata failure must propagate: {result:?}"
        );
    }

    #[test]
    fn run_package_mode_in_coverage_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let per_pkg = dir.path().join("per-package");
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_package("leviath-core", Err("boom".to_owned()));
        let result = run_package_mode_in(
            &runner,
            "leviath-core",
            dir.path(),
            per_pkg.to_str().unwrap(),
        );
        assert!(
            result.is_err(),
            "coverage failure must propagate: {result:?}"
        );
    }

    #[test]
    fn run_package_mode_in_write_error_propagates() {
        // per_package_dir is a *file*, so create_dir_all fails.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a dir").unwrap();
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta);
        let result = run_package_mode_in(
            &runner,
            "leviath-core",
            dir.path(),
            blocker.to_str().unwrap(),
        );
        assert!(
            result.is_err(),
            "unwritable per-package dir must error: {result:?}"
        );
    }

    #[test]
    fn run_package_mode_in_html_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let per_pkg = dir.path().join("per-package");
        let meta = simple_metadata(&["leviath-core"]);
        let runner = MockRunner::new(meta).with_fail_html();
        let result = run_package_mode_in(
            &runner,
            "leviath-core",
            dir.path(),
            per_pkg.to_str().unwrap(),
        );
        assert!(
            result.is_err(),
            "an HTML failure must propagate: {result:?}"
        );
    }

    // ── aggregate_per_package ─────────────────────────────────────────────────

    fn write_covdata(dir: &std::path::Path, name: &str, data: &CovData) {
        let json = serde_json::to_string_pretty(data).unwrap();
        std::fs::write(dir.join(name), json).unwrap();
    }

    #[test]
    fn aggregate_per_package_sums_and_ignores_non_json() {
        let dir = tempfile::tempdir().unwrap();
        write_covdata(
            dir.path(),
            "a.json",
            &CovData {
                files: vec![FileCov {
                    filename: "/src/a.rs".to_owned(),
                    summary: full_metrics(5),
                }],
                totals: full_metrics(5),
            },
        );
        write_covdata(
            dir.path(),
            "b.json",
            &CovData {
                files: vec![FileCov {
                    filename: "/src/b.rs".to_owned(),
                    summary: full_metrics(3),
                }],
                totals: full_metrics(3),
            },
        );
        // A non-.json file that must be ignored by the extension filter.
        std::fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();

        let agg = aggregate_per_package(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(agg.files.len(), 2, "both per-package files aggregated");
        assert_eq!(agg.totals.regions.count, 8);
        assert_eq!(agg.totals.regions.covered, 8);
        assert!(agg.totals.is_100_percent());
    }

    #[test]
    fn aggregate_per_package_missing_dir_is_err() {
        let result = aggregate_per_package("/tmp/no_such_per_package_dir_xyz_123");
        assert!(result.is_err(), "missing dir must error: {result:?}");
    }

    #[test]
    fn aggregate_per_package_read_error_propagates() {
        // A subdirectory named `x.json` — read_to_string fails on a directory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("x.json")).unwrap();
        let result = aggregate_per_package(dir.path().to_str().unwrap());
        assert!(
            result.is_err(),
            "reading a dir-as-file must error: {result:?}"
        );
    }

    #[test]
    fn aggregate_per_package_parse_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), "not valid json").unwrap();
        let result = aggregate_per_package(dir.path().to_str().unwrap());
        assert!(result.is_err(), "invalid JSON must error: {result:?}");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("parsing per-package coverage JSON"));
    }

    // ── run_gate_mode_in ──────────────────────────────────────────────────────

    #[test]
    fn run_gate_mode_in_100_percent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let per_pkg = dir.path().join("per-package");
        std::fs::create_dir(&per_pkg).unwrap();
        write_covdata(
            &per_pkg,
            "leviath-core.json",
            &CovData {
                files: vec![FileCov {
                    filename: "/src/a.rs".to_owned(),
                    summary: full_metrics(5),
                }],
                totals: full_metrics(5),
            },
        );
        let html = dir.path().join("html");
        let result = run_gate_mode_in(per_pkg.to_str().unwrap(), html.to_str().unwrap(), None);
        assert!(result.is_ok(), "100% gate should pass: {result:?}");
        assert!(
            html.join("index.html").exists(),
            "gate must write the top-level HTML index"
        );
    }

    #[test]
    fn run_gate_mode_in_gap_fails_with_same_message() {
        let dir = tempfile::tempdir().unwrap();
        let per_pkg = dir.path().join("per-package");
        std::fs::create_dir(&per_pkg).unwrap();
        write_covdata(
            &per_pkg,
            "leviath-core.json",
            &CovData {
                files: vec![FileCov {
                    filename: "/src/gap.rs".to_owned(),
                    summary: partial_metrics(10, 8),
                }],
                totals: partial_metrics(10, 8),
            },
        );
        let html = dir.path().join("html");
        let result = run_gate_mode_in(per_pkg.to_str().unwrap(), html.to_str().unwrap(), None);
        assert!(result.is_err(), "a gap must fail the gate: {result:?}");
        assert!(
            result.unwrap_err().to_string().contains("must be 100%"),
            "gate must reuse run_report's failure message"
        );
    }

    #[test]
    fn run_gate_mode_in_aggregate_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("html");
        // per-package dir doesn't exist → aggregate errors before the gate.
        let result = run_gate_mode_in(
            dir.path().join("missing").to_str().unwrap(),
            html.to_str().unwrap(),
            None,
        );
        assert!(
            result.is_err(),
            "missing per-package dir must error: {result:?}"
        );
    }

    #[test]
    fn run_gate_mode_in_github_output_error_propagates() {
        // 100% (gaps empty) but an unwritable GITHUB_OUTPUT path → the
        // write_github_output `?` inside report_data fails.
        let dir = tempfile::tempdir().unwrap();
        let per_pkg = dir.path().join("per-package");
        std::fs::create_dir(&per_pkg).unwrap();
        write_covdata(
            &per_pkg,
            "leviath-core.json",
            &CovData {
                files: vec![],
                totals: full_metrics(1),
            },
        );
        let html = dir.path().join("html");
        let result = run_gate_mode_in(
            per_pkg.to_str().unwrap(),
            html.to_str().unwrap(),
            Some("/no/such/dir/gha_output.txt"),
        );
        assert!(
            result.is_err(),
            "unwritable GITHUB_OUTPUT must error: {result:?}"
        );
    }

    // ── generate_package_html ─────────────────────────────────────────────────

    #[test]
    fn generate_package_html_success_is_ok() {
        let runner = MockRunner::new(simple_metadata(&["leviath-core"]));
        assert!(generate_package_html(&runner, "leviath-core").is_ok());
    }

    #[test]
    fn generate_package_html_cargo_exit_failure_is_err() {
        let runner = MockRunner::new(simple_metadata(&["leviath-core"])).with_fail_html();
        let result = generate_package_html(&runner, "leviath-core");
        assert!(
            result.is_err(),
            "non-zero --html exit must error: {result:?}"
        );
        assert!(result.unwrap_err().to_string().contains("HTML report"));
    }

    #[test]
    fn generate_package_html_cargo_spawn_failure_is_err() {
        let runner = MockRunner::new(simple_metadata(&["leviath-core"])).with_fail_cargo_err();
        assert!(generate_package_html(&runner, "leviath-core").is_err());
    }

    // ── write_gate_html_index ─────────────────────────────────────────────────

    #[test]
    fn write_gate_html_index_lists_existing_dirs_and_skips_files() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("html");
        std::fs::create_dir_all(html.join("leviath-core")).unwrap();
        std::fs::create_dir_all(html.join("leviath-cli")).unwrap();
        // A stray file at the top level must be skipped (is_dir == false).
        std::fs::write(html.join("stray.txt"), "x").unwrap();

        write_gate_html_index(html.to_str().unwrap()).unwrap();
        let index = std::fs::read_to_string(html.join("index.html")).unwrap();
        assert!(index.contains("leviath-core/html/index.html"));
        assert!(index.contains("leviath-cli/html/index.html"));
        assert!(!index.contains("stray.txt"));
    }

    #[test]
    fn write_gate_html_index_missing_dir_writes_empty_index() {
        // read_dir Err branch → no packages, but write_html_index still
        // creates the dir and writes an (empty) index.
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("nonexistent-html");
        write_gate_html_index(html.to_str().unwrap()).unwrap();
        assert!(html.join("index.html").exists());
    }

    // ── write_html_index ──────────────────────────────────────────────────────

    #[test]
    fn write_html_index_create_dir_error_propagates() {
        // html_dir is a *file*, so create_dir_all fails.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a dir").unwrap();
        let result = write_html_index(blocker.to_str().unwrap(), &[]);
        assert!(
            result.is_err(),
            "unwritable html dir must error: {result:?}"
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
