//! Check that no coverage-suppression escape hatches exist in the codebase.
//!
//! The zero-exclusions policy means developers must refactor code until it is
//! testable rather than hiding it from coverage instrumentation.  This scanner
//! enforces that policy mechanically by refusing to let any of the known
//! suppression markers land in source or CI config files.
//!
//! Scanned locations
//! -----------------
//! * `crates/**/*.rs`       — all Rust source files (not `target/`, not `xtask/`)
//! * `.github/workflows/`  — CI YAML files
//! * `.cargo-husky/hooks/`  — commit hooks (auto-installed by `cargo-husky`)
//! * `.cargo/config.toml`  — cargo configuration

use anyhow::Result;
use std::path::{Path, PathBuf};

// ── Public entry point ──────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    run_with_root(Path::new("."))
}

/// Run the scan from an explicit root directory (extracted for testability).
pub fn run_with_root(root: &Path) -> Result<()> {
    let violations = scan_workspace_from(root)?;
    report_violations(violations)
}

/// Report violations and fail if any exist.
pub fn report_violations(violations: Vec<Violation>) -> Result<()> {
    if violations.is_empty() {
        println!("[check-exclusions] No coverage suppression markers found. ✓");
        return Ok(());
    }

    eprintln!(
        "[check-exclusions] {} violation(s) found:",
        violations.len()
    );
    for v in &violations {
        eprintln!("  {}:{}", v.file.display(), v.line_num);
        eprintln!("    pattern : {:?}", v.pattern);
        eprintln!("    reason  : {}", v.reason);
        eprintln!("    content : {}", v.line.trim());
    }

    anyhow::bail!(
        "[check-exclusions] {} coverage suppression violation(s). Remove them and refactor.",
        violations.len()
    );
}

// ── Banned patterns ──────────────────────────────────────────────────────────

/// A string pattern that must not appear in the codebase.
pub struct BannedPattern {
    /// Exact substring to search for (case-sensitive).
    pub pattern: &'static str,
    /// Human-readable explanation for the error message.
    pub reason: &'static str,
    /// Apply this check to Rust `.rs` source files.
    pub check_rs: bool,
    /// Apply this check to CI / config / hook files.
    pub check_config: bool,
}

pub const BANNED: &[BannedPattern] = &[
    // ── Coverage-attribute suppression (nightly feature) ────────────────────
    BannedPattern {
        pattern: "coverage(off)",
        reason: "coverage(off) suppression is forbidden — refactor the code to be testable instead",
        check_rs: true,
        check_config: false,
    },
    // ── Tarpaulin markers ────────────────────────────────────────────────────
    BannedPattern {
        pattern: "tarpaulin_include",
        reason: "tarpaulin coverage annotation is forbidden",
        check_rs: true,
        check_config: true,
    },
    BannedPattern {
        pattern: "cfg(tarpaulin)",
        reason: "tarpaulin cfg gate is forbidden",
        check_rs: true,
        check_config: false,
    },
    BannedPattern {
        pattern: "cfg(not(tarpaulin))",
        reason: "tarpaulin cfg gate is forbidden",
        check_rs: true,
        check_config: false,
    },
    // ── grcov / lcov exclusion comments ─────────────────────────────────────
    BannedPattern {
        pattern: "LCOV_EXCL",
        reason: "LCOV exclusion marker is forbidden",
        check_rs: true,
        check_config: true,
    },
    BannedPattern {
        pattern: "GRCOV_EXCL",
        reason: "grcov exclusion marker is forbidden",
        check_rs: true,
        check_config: true,
    },
    // ── Tarpaulin as a CI tool ───────────────────────────────────────────────
    BannedPattern {
        pattern: "cargo-tarpaulin",
        reason: "tarpaulin is not the designated coverage tool; use `cargo xtask coverage` instead",
        check_rs: false,
        check_config: true,
    },
    BannedPattern {
        pattern: "cargo tarpaulin",
        reason: "tarpaulin is not the designated coverage tool; use `cargo xtask coverage` instead",
        check_rs: false,
        check_config: true,
    },
];

// ── Scanning ─────────────────────────────────────────────────────────────────

/// A single policy violation found during scanning.
#[derive(Debug)]
pub struct Violation {
    /// Path to the file that contains the violation.
    pub file: PathBuf,
    /// 1-indexed line number.
    pub line_num: usize,
    /// The raw line content (untrimmed).
    pub line: String,
    /// The banned pattern that matched (or a short tag identifying which
    /// structural check fired, for the non-banned-pattern checks below).
    pub pattern: &'static str,
    /// Human-readable reason. Owned (rather than `&'static str`) because the
    /// `#[cfg(not(test))]` escape-hatch checks below need to interpolate the
    /// offending function's name into the message.
    pub reason: String,
}

/// Scan the workspace rooted at `root` for banned coverage-suppression markers.
///
/// Takes an explicit root so tests can pass either a temp directory or
/// `CARGO_MANIFEST_DIR/../..` (the real workspace root) without changing the
/// process working directory.
pub fn scan_workspace_from(root: &Path) -> Result<Vec<Violation>> {
    let mut violations = Vec::new();

    // Rust source files under crates/ (skips target/ and hidden dirs)
    let crates_dir = root.join("crates");
    if crates_dir.exists() {
        scan_dir(&crates_dir, true, &mut violations)?;
    }

    // Cap the number of `#[cfg(not(test))]` gates in the one allowed crate
    // (leviath-sys), so real-IO exclusions can't accrete silently. Raising the
    // cap is a deliberate, reviewed change to `MAX_SYS_EXCLUSIONS`.
    let sys_dir = crates_dir.join(ALLOWED_CFG_NOT_TEST_CRATE);
    if sys_dir.exists() {
        let count = count_cfg_not_test_gates(&sys_dir);
        if count > MAX_SYS_EXCLUSIONS {
            violations.push(Violation {
                file: sys_dir.clone(),
                line_num: 0,
                line: String::new(),
                pattern: "cfg(not(test)) cap exceeded in leviath-sys",
                reason: format!(
                    "crates/{ALLOWED_CFG_NOT_TEST_CRATE}/ has {count} #[cfg(not(test))] gates but \
                     the cap (MAX_SYS_EXCLUSIONS) is {MAX_SYS_EXCLUSIONS}. Prefer an injected seam \
                     over a new real-IO exclusion; if one is genuinely unavoidable, raise the cap \
                     deliberately in xtask/src/check_exclusions.rs."
                ),
            });
        }
    }

    // CI and config files
    for rel in &[
        ".github/workflows/ci.yml",
        ".github/workflows/alpha.yml",
        ".github/workflows/beta.yml",
        ".github/workflows/prod.yml",
        ".cargo-husky/hooks/pre-commit",
        ".cargo/config.toml",
    ] {
        let p = root.join(rel);
        if p.exists() {
            scan_file(&p, false, &mut violations);
        }
    }

    Ok(violations)
}

/// Recursively scan a directory, treating each `.rs` file as `is_rs = true`.
pub fn scan_dir(dir: &Path, is_rs: bool, violations: &mut Vec<Violation>) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();

        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Skip target, hidden dirs, and the xtask crate itself (its tests
            // contain the banned strings as string literals).
            if name != "target" && !name.starts_with('.') && name != "xtask" {
                // Silently skip any subdirectory that can't be read — the path
                // just came from read_dir so failures are transient (race on
                // deletion, permission change mid-scan).  Top-level directories
                // propagate errors via the `?` in scan_workspace_from.
                let _ = scan_dir(&path, is_rs, violations);
            }
        } else if is_rs && path.extension().is_some_and(|e| e == "rs") {
            scan_file(&path, true, violations);
        }
    }
    Ok(())
}

/// Scan a single file for banned patterns.
///
/// Unreadable files are silently skipped — the caller never sees an error.
pub fn scan_file(path: &Path, is_rs: bool, violations: &mut Vec<Violation>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return, // skip unreadable files silently
    };

    for (line_idx, line) in content.lines().enumerate() {
        for banned in BANNED {
            let applies = (is_rs && banned.check_rs) || (!is_rs && banned.check_config);
            if applies && line.contains(banned.pattern) {
                violations.push(Violation {
                    file: path.to_owned(),
                    line_num: line_idx + 1,
                    line: line.to_owned(),
                    pattern: banned.pattern,
                    reason: banned.reason.to_owned(),
                });
            }
        }
    }

    if is_rs {
        scan_cfg_not_test_escape_hatches(path, &content, violations);
        scan_coverage_confirmed_artifact_markers(path, &content, violations);
    }
}

// ── `#[cfg(not(test))]` escape-hatch audit ──────────────────────────────────
//
// `#[cfg(not(test))]` hides its real body from coverage measurement, so it is
// the one attribute an agent could use to dodge testing. The policy is a single
// unabusable PATH rule: it is **banned in every crate except `leviath-sys`** —
// the leaf crate that quarantines raw OS syscalls. There is no marker that
// makes it acceptable in ordinary library code, so it can't be smuggled in
// with a plausible-looking justification. Un-unit-testable real-I/O composition
// (real terminal, blocking stdin, port bind, subprocess spawn, real inference)
// belongs in the coverage-excluded `lev` binary (`main.rs`) or behind an
// injected seam (see the dispatch `RiskyExecutors` trait, dashboard
// `execute_with`, and the run `ForegroundIo` bundle).
//
// Inside `leviath-sys` the attribute is permitted for the final syscall leaves,
// but still audited:
// 1. Every gate must be immediately preceded (skipping blanks/attributes) by a
//    `// REAL-IO-EXCLUDED: <non-empty reason>` comment (`//` or `///`) — a
//    reviewable justification.
// 2. Every `#[cfg(not(test))]`-gated `fn NAME` must have a matching
//    `#[cfg(test)]`-gated `fn NAME` twin in the same file, so the surrounding
//    logic stays testable.
// 3. The total number of gates in `leviath-sys` is capped at
//    [`MAX_SYS_EXCLUSIONS`] (enforced in `scan_workspace_from`) — raising it is
//    a deliberate, reviewed diff.
//
// This is a line/regex-style scan (matching the rest of this file), not a full
// parser: it looks for the exact `#[cfg(not(test))]` / `#[cfg(test)]` attribute
// on its own line, and reads a function name off the next non-blank,
// non-attribute line by splitting on the `fn` token. It is deliberately
// tolerant of being fooled by unusual formatting — the existing scanner in this
// file makes the same trade-off.

/// Doc-comment marker required immediately before every `#[cfg(not(test))]`
/// item that lives in `crates/leviath-sys/` (the ONE crate permitted to use the
/// attribute — see the module doc above), explaining why the real body cannot
/// be exercised by a real test.
const REAL_IO_EXCLUDED_MARKER: &str = "REAL-IO-EXCLUDED:";

/// The directory whose `#[cfg(not(test))]` uses are permitted (the leaf crate
/// that quarantines raw OS syscalls). Matched as a path component so it never
/// false-matches a substring.
const ALLOWED_CFG_NOT_TEST_CRATE: &str = "leviath-sys";

/// Hard cap on the number of `#[cfg(not(test))]` gates allowed inside
/// `crates/leviath-sys/`. Raising it is a deliberate, reviewed one-line diff —
/// the ratchet that stops real-IO exclusions accreting silently.
const MAX_SYS_EXCLUSIONS: usize = 2;

/// True if `path` is inside the one crate permitted to use `#[cfg(not(test))]`.
fn is_allowed_cfg_not_test_path(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == ALLOWED_CFG_NOT_TEST_CRATE)
}

/// Recursively count `#[cfg(not(test))]` attribute lines under `dir` (a
/// trimmed exact match, so doc-comment mentions in backticks don't count),
/// skipping `target/` and hidden dirs. Used to enforce [`MAX_SYS_EXCLUSIONS`].
fn count_cfg_not_test_gates(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name != "target" && !name.starts_with('.') {
                count += count_cfg_not_test_gates(&path);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                count += content
                    .lines()
                    .filter(|l| l.trim() == "#[cfg(not(test))]")
                    .count();
            }
        }
    }
    count
}

/// Scan a single file's already-read `content` for `#[cfg(not(test))]`
/// escape-hatch violations (see the module doc above for the two rules).
fn scan_cfg_not_test_escape_hatches(path: &Path, content: &str, violations: &mut Vec<Violation>) {
    let lines: Vec<&str> = content.lines().collect();
    let allowed = is_allowed_cfg_not_test_path(path);

    // Every `fn NAME` immediately gated by `#[cfg(test)]` anywhere in the
    // file — the pool of legitimate "twins" (only relevant inside the allowed
    // crate).
    let mut test_fn_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            if let Some(name) = fn_name_after_attribute(&lines, idx) {
                test_fn_names.insert(name);
            }
        }
    }

    for (idx, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(not(test))]" {
            continue;
        }

        // Rule 1 (path ban): `#[cfg(not(test))]` is forbidden in every crate
        // except `leviath-sys`. There is no marker that can satisfy this in
        // ordinary library code — the escape hatch is a physical location, not
        // a comment, so it can't be forged by dropping in a justification.
        if !allowed {
            violations.push(Violation {
                file: path.to_owned(),
                line_num: idx + 1,
                line: (*line).to_owned(),
                pattern: "cfg(not(test)) outside crates/leviath-sys",
                reason:
                    "#[cfg(not(test))] is banned in library code. Real-I/O composition belongs in \
                     the (coverage-excluded) `lev` binary or behind an injected seam (see the \
                     dispatch `RiskyExecutors` pattern and `main.rs`); only crates/leviath-sys/ \
                     (raw OS syscalls) may use #[cfg(not(test))], and only with a \
                     `REAL-IO-EXCLUDED:` justification + a #[cfg(test)] twin"
                        .to_owned(),
            });
            continue;
        }

        // Inside leviath-sys: require the `REAL-IO-EXCLUDED:` justification on
        // every gate (fn/method, `use`, `impl`, `struct`, or inline block)...
        if !preceded_by_real_io_excluded_marker(&lines, idx) {
            violations.push(Violation {
                file: path.to_owned(),
                line_num: idx + 1,
                line: (*line).to_owned(),
                pattern: "cfg(not(test)) missing REAL-IO-EXCLUDED marker",
                reason: "#[cfg(not(test))] in leviath-sys must be immediately preceded by a \
                     `// REAL-IO-EXCLUDED: <reason>` comment explaining why the real body cannot \
                     be exercised by a test"
                    .to_owned(),
            });
        }

        // ...and, when it gates a `fn`, a matching `#[cfg(test)]` twin so the
        // surrounding logic stays testable.
        if let Some(fn_name) = fn_name_after_attribute(&lines, idx) {
            if !test_fn_names.contains(&fn_name) {
                violations.push(Violation {
                    file: path.to_owned(),
                    line_num: idx + 1,
                    line: (*line).to_owned(),
                    pattern: "cfg(not(test)) missing #[cfg(test)] twin",
                    reason: format!(
                        "fn `{fn_name}` is #[cfg(not(test))] but has no matching #[cfg(test)] \
                         fn `{fn_name}` in the same file"
                    ),
                });
            }
        }
    }
}

/// Starting just after `attr_idx`, skip blank lines and other `#[...]`
/// attribute lines, then try to read a function name off the first
/// substantive line found. Returns `None` if that line isn't a `fn`
/// declaration (e.g. it's a `use`, a `mod`, or a plain `{`).
fn fn_name_after_attribute(lines: &[&str], attr_idx: usize) -> Option<String> {
    let mut i = attr_idx + 1;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with("#[") {
            i += 1;
            continue;
        }
        return extract_fn_name(trimmed);
    }
    None
}

/// Extract the function name from a line containing a `fn` declaration, e.g.
/// `pub(crate) async fn foo<T>(x: T) -> T {` -> `Some("foo")`. Returns `None`
/// if the line has no standalone `fn` token.
fn extract_fn_name(line: &str) -> Option<String> {
    let mut tokens = line.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "fn" {
            let name_tok = tokens.next()?;
            let end = name_tok.find(['(', '<', ':']).unwrap_or(name_tok.len());
            let name = &name_tok[..end];
            return if name.is_empty() {
                None
            } else {
                Some(name.to_owned())
            };
        }
    }
    None
}

/// Scan backward from `attr_idx` (the `#[cfg(not(test))]` line), skipping
/// blank lines and other attribute lines, to find the contiguous comment
/// block (`//` or `///`, if any) immediately touching the attribute, then
/// check whether ANY line in that block contains `REAL-IO-EXCLUDED:` followed
/// by a non-empty reason.
///
/// Justifications are frequently multi-line -- the marker on the first line
/// of the paragraph, with the reason continuing across further comment lines
/// below it before the attribute -- so this doesn't require the marker to be
/// on the single line touching the attribute, only somewhere in the comment
/// block that does. Both `//` line comments (used on `use`/block gates) and
/// `///` doc comments (used on `fn` gates) are accepted.
fn preceded_by_real_io_excluded_marker(lines: &[&str], attr_idx: usize) -> bool {
    // Skip blank lines and other attributes stacked above `attr_idx` to find
    // where the comment block (if any) ends.
    let mut i = attr_idx;
    let doc_end = loop {
        if i == 0 {
            return false; // nothing precedes the attribute at all
        }
        i -= 1;
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with("#[") {
            continue;
        }
        if !trimmed.starts_with("//") {
            return false; // nearest substantive line isn't a comment
        }
        break i;
    };

    // Walk backward through the contiguous comment block starting at
    // `doc_end`, checking every line in it for the marker.
    let mut j = doc_end;
    loop {
        let trimmed = lines[j].trim();
        if let Some(pos) = trimmed.find(REAL_IO_EXCLUDED_MARKER) {
            if !trimmed[pos + REAL_IO_EXCLUDED_MARKER.len()..]
                .trim()
                .is_empty()
            {
                return true;
            }
        }
        if j == 0 {
            return false;
        }
        j -= 1;
        if !lines[j].trim().starts_with("//") {
            return false;
        }
    }
}

// ── `COVERAGE-CONFIRMED-ARTIFACT` marker audit ──────────────────────────────
//
// A narrower, DIFFERENT category from the `#[cfg(not(test))]` escape hatch
// above: code that IS fully exercised by real, passing tests (every logical
// branch demonstrably runs -- confirmed by direct JSON/HTML segment
// inspection showing every source position has at least one covered
// instantiation) but which `cargo-llvm-cov`'s region-counting mechanism
// still undercounts anyway, most often because of generic-function
// monomorphization: a shared generic helper called with several concrete
// type arguments gets one region-coverage row per source position in the
// summary table, but that table's cross-instantiation merge does not
// consistently mark the position covered even when at least one
// instantiation's copy of it genuinely executed.
//
// Using `#[cfg(not(test))]` isolation for this would be dishonest -- it
// would hide genuinely-tested code from measurement instead of correctly
// counting it as covered -- so it gets its own marker instead, deliberately
// with NO twin requirement (there is no swapped-out real/fake pair here,
// just one real function). Every function tagged with this marker must
// still have a non-empty, reviewable justification, exactly like
// `REAL-IO-EXCLUDED` above, and the marker must actually be attached (via
// an immediately-following doc-comment/attribute block) to a real `fn` --
// not left floating as an unattached comment.

/// Doc-comment marker required on any function claimed to be a confirmed
/// `cargo-llvm-cov` region-counting artifact (tested for real, undercounted
/// anyway) rather than a genuine coverage gap.
const COVERAGE_CONFIRMED_ARTIFACT_MARKER: &str = "COVERAGE-CONFIRMED-ARTIFACT:";

/// Scan a single file's already-read `content` for `COVERAGE-CONFIRMED-ARTIFACT`
/// marker violations: an empty reason, or a marker not attached to a `fn`.
fn scan_coverage_confirmed_artifact_markers(
    path: &Path,
    content: &str,
    violations: &mut Vec<Violation>,
) {
    let lines: Vec<&str> = content.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with("///") {
            continue;
        }
        let Some(pos) = trimmed.find(COVERAGE_CONFIRMED_ARTIFACT_MARKER) else {
            continue;
        };

        let reason = trimmed[pos + COVERAGE_CONFIRMED_ARTIFACT_MARKER.len()..].trim();
        if reason.is_empty() {
            violations.push(Violation {
                file: path.to_owned(),
                line_num: idx + 1,
                line: (*line).to_owned(),
                pattern: "COVERAGE-CONFIRMED-ARTIFACT missing reason",
                reason: "COVERAGE-CONFIRMED-ARTIFACT marker has no non-empty reason after \
                          the colon"
                    .to_owned(),
            });
            continue;
        }

        if fn_name_after_doc_or_attr_block(&lines, idx + 1).is_none() {
            violations.push(Violation {
                file: path.to_owned(),
                line_num: idx + 1,
                line: (*line).to_owned(),
                pattern: "COVERAGE-CONFIRMED-ARTIFACT not attached to a fn",
                reason: "COVERAGE-CONFIRMED-ARTIFACT marker must be immediately followed \
                          (skipping further doc lines/attributes) by a `fn` declaration"
                    .to_owned(),
            });
        }
    }
}

/// Starting at `start_idx`, skip blank lines, further `///` doc-comment
/// lines, and `#[...]` attribute lines, then try to read a function name off
/// the first substantive line found. Returns `None` if that line isn't a
/// `fn` declaration.
fn fn_name_after_doc_or_attr_block(lines: &[&str], start_idx: usize) -> Option<String> {
    let mut i = start_idx;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with("///") || trimmed.starts_with("#[") {
            i += 1;
            continue;
        }
        return extract_fn_name(trimmed);
    }
    None
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write `content` to a temp file (a plain, NON-`leviath-sys` path) and
    /// scan it.
    fn scan_content(content: &str, is_rs: bool) -> Vec<Violation> {
        let dir = TempDir::new().unwrap();
        let name = if is_rs { "test.rs" } else { "ci.yml" };
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let mut violations = Vec::new();
        scan_file(&path, is_rs, &mut violations);
        violations
    }

    /// Like [`scan_content`] but writes under a path with a `leviath-sys`
    /// component, so the file is treated as the one crate permitted to use
    /// `#[cfg(not(test))]`.
    fn scan_sys_content(content: &str) -> Vec<Violation> {
        let dir = TempDir::new().unwrap();
        let sys_dir = dir.path().join("crates").join("leviath-sys").join("src");
        std::fs::create_dir_all(&sys_dir).unwrap();
        let path = sys_dir.join("test.rs");
        std::fs::write(&path, content).unwrap();
        let mut violations = Vec::new();
        scan_file(&path, true, &mut violations);
        violations
    }

    // ── report_violations ────────────────────────────────────────────────────

    #[test]
    fn report_violations_empty_is_ok() {
        assert!(report_violations(vec![]).is_ok());
    }

    #[test]
    fn report_violations_nonempty_is_err() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "#[coverage(off)]").unwrap();
        let v = Violation {
            file: path,
            line_num: 1,
            line: "#[coverage(off)]".to_owned(),
            pattern: "coverage(off)",
            reason: "test".to_owned(),
        };
        assert!(report_violations(vec![v]).is_err());
    }

    // ── scan_workspace_from on real workspace ─────────────────────────────────

    #[test]
    fn scan_workspace_clean_on_real_workspace() {
        // CARGO_MANIFEST_DIR is xtask/, so parent() is the workspace root.
        let xtask_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = xtask_dir
            .parent()
            .expect("xtask must have a parent = workspace root");

        let violations =
            scan_workspace_from(workspace_root).expect("scan_workspace_from should not error");

        assert!(
            violations.is_empty(),
            "Real workspace has suppression violations — review the output above",
        );
    }

    // ── scan_workspace_from on temp dirs ─────────────────────────────────────

    #[test]
    fn scan_workspace_from_empty_dir_is_ok() {
        let dir = TempDir::new().unwrap();
        let violations = scan_workspace_from(dir.path()).unwrap();
        assert!(violations.is_empty());
    }

    #[test]
    fn scan_workspace_from_finds_violation_in_crates_dir() {
        let root = TempDir::new().unwrap();
        let crates = root.path().join("crates");
        let crate_a = crates.join("crate-a").join("src");
        std::fs::create_dir_all(&crate_a).unwrap();
        std::fs::write(crate_a.join("lib.rs"), "#[coverage(off)]\nfn x() {}").unwrap();

        let violations = scan_workspace_from(root.path()).unwrap();
        assert!(!violations.is_empty());
        assert!(violations[0].pattern.contains("coverage(off)"));
    }

    #[test]
    fn scan_workspace_from_finds_violation_in_ci_yml() {
        let root = TempDir::new().unwrap();
        let workflows = root.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(
            workflows.join("ci.yml"),
            "run: cargo install cargo-tarpaulin",
        )
        .unwrap();

        let violations = scan_workspace_from(root.path()).unwrap();
        assert!(!violations.is_empty());
        assert_eq!(violations[0].pattern, "cargo-tarpaulin");
    }

    #[test]
    fn scan_workspace_from_skips_nonexistent_config_files() {
        // A root with no .github/, .cargo-husky/, or .cargo/ should not error.
        let root = TempDir::new().unwrap();
        let result = scan_workspace_from(root.path());
        assert!(result.is_ok());
    }

    // ── Clean files ──────────────────────────────────────────────────────────

    #[test]
    fn clean_rs_file_has_no_violations() {
        let v = scan_content("fn foo() { let x = 1 + 1; }\n#[test]\nfn bar() {}", true);
        assert!(v.is_empty(), "unexpected violations: {v:?}");
    }

    #[test]
    fn clean_config_file_has_no_violations() {
        let v = scan_content(
            "name: CI\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest",
            false,
        );
        assert!(v.is_empty(), "unexpected violations: {v:?}");
    }

    // ── coverage(off) ─────────────────────────────────────────────────────────

    #[test]
    fn detects_coverage_off_in_rs() {
        let v = scan_content("#[coverage(off)]\nfn foo() {}", true);
        assert!(!v.is_empty());
        assert_eq!(v[0].line_num, 1);
        assert!(v[0].pattern.contains("coverage(off)"));
    }

    #[test]
    fn detects_cfg_attr_coverage_off_in_rs() {
        let v = scan_content("#[cfg_attr(test, coverage(off))]\nfn foo() {}", true);
        assert!(!v.is_empty());
    }

    #[test]
    fn coverage_off_not_flagged_in_config_files() {
        // coverage(off) is RS-only; config files should not trigger it.
        let v = scan_content("# coverage(off) comment in yaml", false);
        assert!(
            v.is_empty(),
            "should not flag coverage(off) in config: {v:?}"
        );
    }

    // ── Tarpaulin markers ─────────────────────────────────────────────────────

    #[test]
    fn detects_tarpaulin_include_in_rs() {
        let v = scan_content("// tarpaulin_include\nfn foo() {}", true);
        assert!(!v.is_empty());
        assert_eq!(v[0].line_num, 1);
    }

    #[test]
    fn detects_tarpaulin_include_in_config() {
        let v = scan_content("tarpaulin_include: yes", false);
        assert!(!v.is_empty());
    }

    #[test]
    fn detects_cfg_tarpaulin_in_rs() {
        let v = scan_content("#[cfg(tarpaulin)]\nfn foo() {}", true);
        assert!(!v.is_empty());
        assert_eq!(v[0].line_num, 1);
    }

    #[test]
    fn detects_cfg_not_tarpaulin_in_rs() {
        let v = scan_content("#[cfg(not(tarpaulin))]\nfn foo() {}", true);
        assert!(!v.is_empty());
    }

    #[test]
    fn tarpaulin_cfg_not_flagged_in_config_file() {
        // cfg(tarpaulin) is RS-only; yaml mention should be ignored.
        // `v` is empty (no config-applicable patterns match), so we assert
        // directly rather than using .all() on an empty iterator (which would
        // leave the predicate closure uncovered by LLVM).
        let v = scan_content("# This explains cfg(tarpaulin) usage", false);
        assert!(
            v.is_empty(),
            "should not flag cfg(tarpaulin) in config: {v:?}"
        );
    }

    // ── LCOV / GRCOV ─────────────────────────────────────────────────────────

    #[test]
    fn detects_lcov_excl_start_in_rs() {
        let v = scan_content("// LCOV_EXCL_START\nlet x = 1;\n// LCOV_EXCL_END", true);
        assert!(!v.is_empty());
    }

    #[test]
    fn detects_lcov_excl_in_config() {
        let v = scan_content("flags: LCOV_EXCL_LINE", false);
        assert!(!v.is_empty());
    }

    #[test]
    fn detects_grcov_excl_line_in_rs() {
        let v = scan_content("// GRCOV_EXCL_LINE\nlet x = 1;", true);
        assert!(!v.is_empty());
    }

    #[test]
    fn detects_grcov_excl_in_config() {
        let v = scan_content("flags: GRCOV_EXCL_START", false);
        assert!(!v.is_empty());
    }

    // ── Tarpaulin as a CI tool ────────────────────────────────────────────────

    #[test]
    fn detects_cargo_tarpaulin_in_config() {
        let v = scan_content("run: cargo install cargo-tarpaulin", false);
        assert!(!v.is_empty());
        assert_eq!(v[0].line_num, 1);
    }

    #[test]
    fn detects_cargo_tarpaulin_command_in_config() {
        let v = scan_content("run: cargo tarpaulin --workspace", false);
        assert!(!v.is_empty());
    }

    #[test]
    fn cargo_tarpaulin_not_flagged_in_rs_files() {
        // tarpaulin-as-CI-tool is a config-only check; a comment in RS is fine.
        // `v` is empty (RS scans don't apply the config-only patterns), so we
        // assert directly rather than filtering an empty iterator (which would
        // leave the predicate closure uncovered by LLVM).
        let v = scan_content("// We used to use cargo tarpaulin here", true);
        assert!(
            v.is_empty(),
            "cargo tarpaulin should not be flagged in RS: {v:?}"
        );
    }

    // ── Line numbers ──────────────────────────────────────────────────────────

    #[test]
    fn violation_reports_correct_line_number() {
        let v = scan_content("fn ok() {}\n#[coverage(off)]\nfn bad() {}", true);
        assert!(v.iter().any(|v| v.line_num == 2));
    }

    #[test]
    fn multiple_violations_on_different_lines() {
        let content = "#[coverage(off)]\nfn a() {}\n// LCOV_EXCL_LINE\nfn b() {}";
        let v = scan_content(content, true);
        let line_nums: Vec<usize> = v.iter().map(|v| v.line_num).collect();
        assert!(line_nums.contains(&1));
        assert!(line_nums.contains(&3));
    }

    #[test]
    fn unreadable_file_is_skipped_gracefully() {
        // scan_file silently skips unreadable/missing files; violations stays empty.
        let mut violations = Vec::new();
        scan_file(
            Path::new("/tmp/no_such_file_xyz_abc.rs"),
            true,
            &mut violations,
        );
        assert!(violations.is_empty());
    }

    // ── Directory scanning ────────────────────────────────────────────────────

    #[test]
    fn scan_dir_finds_violations_in_nested_rs_files() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("mod.rs"), "#[coverage(off)]\nfn x() {}").unwrap();
        std::fs::write(dir.path().join("clean.rs"), "fn y() {}").unwrap();

        let mut violations = Vec::new();
        scan_dir(dir.path(), true, &mut violations).unwrap();
        assert!(!violations.is_empty());
        assert!(violations.iter().any(|v| v.line_num == 1));
    }

    #[test]
    fn scan_dir_skips_target_directory() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("gen.rs"), "#[coverage(off)]\nfn x() {}").unwrap();

        let mut violations = Vec::new();
        scan_dir(dir.path(), true, &mut violations).unwrap();
        // Nothing found — target/ was skipped.
        assert!(violations.is_empty());
    }

    #[test]
    fn scan_dir_skips_hidden_directories() {
        let dir = TempDir::new().unwrap();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.rs"), "#[coverage(off)]\nfn x() {}").unwrap();

        let mut violations = Vec::new();
        scan_dir(dir.path(), true, &mut violations).unwrap();
        assert!(violations.is_empty());
    }

    #[test]
    fn scan_dir_ignores_non_rs_files_in_rs_mode() {
        let dir = TempDir::new().unwrap();
        // A yaml file with a banned pattern should not trigger when scanning in RS mode.
        std::fs::write(dir.path().join("ci.yml"), "cargo-tarpaulin").unwrap();

        let mut violations = Vec::new();
        scan_dir(dir.path(), true, &mut violations).unwrap();
        assert!(violations.is_empty());
    }

    #[test]
    fn scan_dir_skips_xtask_directory() {
        let dir = TempDir::new().unwrap();
        let xtask = dir.path().join("xtask");
        std::fs::create_dir(&xtask).unwrap();
        std::fs::write(xtask.join("main.rs"), "#[coverage(off)]\nfn x() {}").unwrap();

        let mut violations = Vec::new();
        scan_dir(dir.path(), true, &mut violations).unwrap();
        // xtask/ should be skipped entirely.
        assert!(violations.is_empty());
    }

    #[test]
    fn scan_dir_with_extensionless_file_is_skipped() {
        // A file with no extension (like LICENSE or Makefile) causes
        // path.extension() to return None, so is_some_and(|e| e == "rs")
        // returns false without calling the predicate.  This exercises the
        // None-case branch of is_some_and (block=1, branch=3) at line 177.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("LICENSE"), "#[coverage(off)]").unwrap();
        let mut violations = Vec::new();
        scan_dir(dir.path(), true, &mut violations).unwrap();
        // Extensionless file must be ignored even if it contains banned text.
        assert!(violations.is_empty());
    }

    #[test]
    fn scan_dir_is_rs_false_short_circuits_extension_check() {
        // Exercises BRDA:177,block=0,branch=1 — the `is_rs=false` short-circuit
        // of `is_rs && path.extension().is_some_and(...)`.
        // When is_rs=false the entire else-if is false without evaluating the
        // extension predicate, so scan_file is never called even for .rs files.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gen.rs"), "#[coverage(off)]\nfn x() {}").unwrap();
        let mut violations = Vec::new();
        scan_dir(dir.path(), false, &mut violations).unwrap();
        assert!(
            violations.is_empty(),
            "is_rs=false should skip all files: {violations:?}"
        );
    }

    // ── scan_dir error path ───────────────────────────────────────────────────

    #[test]
    fn scan_dir_returns_err_for_nonexistent_directory() {
        // read_dir on a nonexistent path returns Err; scan_dir propagates it.
        let mut violations = Vec::new();
        let result = scan_dir(
            Path::new("/tmp/no_such_directory_xyz_abc_123"),
            true,
            &mut violations,
        );
        assert!(
            result.is_err(),
            "scan_dir should fail on a missing directory"
        );
    }

    // ── run_with_root ─────────────────────────────────────────────────────────

    #[test]
    fn run_with_root_ok_on_empty_dir() {
        let dir = TempDir::new().unwrap();
        assert!(run_with_root(dir.path()).is_ok());
    }

    #[test]
    fn run_with_root_propagates_scan_error_when_crates_is_a_file() {
        // If "crates/" is a *file* instead of a directory, scan_dir will fail
        // when it tries to read_dir(crates_file), and run_with_root propagates
        // that error instead of panicking or swallowing it.
        let dir = TempDir::new().unwrap();
        // Create a plain file named "crates" so crates_dir.exists() is true
        // but read_dir fails because it's not a directory.
        std::fs::write(dir.path().join("crates"), "not a dir").unwrap();
        let result = run_with_root(dir.path());
        assert!(
            result.is_err(),
            "expected Err when crates is a file, got: {result:?}"
        );
    }

    // ── `#[cfg(not(test))]` path-based ban ────────────────────────────────────

    #[test]
    fn cfg_not_test_outside_leviath_sys_is_violation() {
        // Ordinary library code may not use `#[cfg(not(test))]` at all.
        let v = scan_content("#[cfg(not(test))]\nfn real_thing() {}\n", true);
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("outside crates/leviath-sys")),
            "expected an outside-leviath-sys violation: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_outside_leviath_sys_even_with_marker_and_twin_is_violation() {
        // No comment can whitelist the attribute outside leviath-sys — the
        // escape hatch is a physical location, not a forgeable marker.
        let v = scan_content(
            "// REAL-IO-EXCLUDED: real terminal write\n#[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
            true,
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("outside crates/leviath-sys")),
            "a marker must not whitelist cfg(not(test)) outside leviath-sys: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_on_use_outside_leviath_sys_is_violation() {
        // Even non-`fn` gates (`use`/block/etc.) are banned outside the crate.
        let v = scan_content("#[cfg(not(test))]\nuse std::io::Write;\n", true);
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("outside crates/leviath-sys")),
            "expected an outside-leviath-sys violation: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_not_scanned_in_config_files() {
        // The escape-hatch audit only runs for `.rs` files (is_rs = true).
        let v = scan_content("#[cfg(not(test))]\nfn real_thing() {}\n", false);
        assert!(v.is_empty(), "config files should not be scanned: {v:?}");
    }

    // ── `#[cfg(not(test))]` audit inside leviath-sys (the one allowed crate) ──

    #[test]
    fn sys_cfg_not_test_fn_without_marker_is_violation() {
        let v = scan_sys_content(
            "#[cfg(not(test))]\nfn real_thing() {}\n\n#[cfg(test)]\nfn real_thing() {}\n",
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing REAL-IO-EXCLUDED marker")),
            "expected a missing-marker violation: {v:?}"
        );
    }

    #[test]
    fn sys_cfg_not_test_fn_without_twin_is_violation() {
        let v = scan_sys_content(
            "/// REAL-IO-EXCLUDED: real terminal write, cannot be tested\n\
             #[cfg(not(test))]\nfn real_thing() {}\n",
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing #[cfg(test)] twin")),
            "expected a missing-twin violation: {v:?}"
        );
    }

    #[test]
    fn sys_cfg_not_test_fn_with_marker_and_twin_is_clean() {
        let v = scan_sys_content(
            "/// REAL-IO-EXCLUDED: real terminal write, cannot be tested\n\
             #[cfg(not(test))]\nfn real_thing() -> std::io::Result<()> { Ok(()) }\n\n\
             #[cfg(test)]\nfn real_thing() -> std::io::Result<()> { Ok(()) }\n",
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn sys_cfg_not_test_line_comment_marker_is_accepted() {
        // A plain `//` marker (not `///`) is accepted — non-`fn` gates in
        // leviath-sys use line comments.
        let v = scan_sys_content(
            "// REAL-IO-EXCLUDED: real terminal write\n#[cfg(not(test))]\n\
             use std::io::Write;\n",
        );
        assert!(v.is_empty(), "line-comment marker should satisfy: {v:?}");
    }

    #[test]
    fn sys_cfg_not_test_marker_with_empty_reason_is_violation() {
        let v = scan_sys_content(
            "/// REAL-IO-EXCLUDED:\n#[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing REAL-IO-EXCLUDED marker")),
            "empty reason should still be flagged: {v:?}"
        );
    }

    #[test]
    fn sys_cfg_not_test_marker_skips_other_attributes_and_blank_lines() {
        let v = scan_sys_content(
            "/// REAL-IO-EXCLUDED: real terminal write, cannot be tested\n\
             \n\
             #[allow(dead_code)]\n\
             #[cfg(not(test))]\n\
             fn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn sys_cfg_not_test_marker_on_first_line_of_multiline_block_is_accepted() {
        let v = scan_sys_content(
            "/// REAL-IO-EXCLUDED: opens the real /dev/tty; exercising it would\n\
             /// write escape bytes to the terminal running `cargo test`.\n\
             #[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn sys_cfg_not_test_fn_name_extraction_handles_pub_async_generic() {
        let v = scan_sys_content(
            "/// REAL-IO-EXCLUDED: real subprocess spawn, cannot be tested\n\
             #[cfg(not(test))]\npub(crate) async fn spawn_it<T>(x: T) -> T { x }\n\n\
             #[cfg(test)]\npub(crate) async fn spawn_it<T>(x: T) -> T { x }\n",
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn sys_cfg_not_test_missing_both_marker_and_twin_reports_two_violations() {
        let v = scan_sys_content("#[cfg(not(test))]\nfn real_thing() {}\n");
        assert_eq!(v.len(), 2, "expected both violations: {v:?}");
    }

    // ── leviath-sys exclusion cap ─────────────────────────────────────────────

    #[test]
    fn sys_cfg_not_test_over_cap_is_violation() {
        // A fake workspace whose leviath-sys crate has MAX_SYS_EXCLUSIONS + 1
        // gates must trip the cap.
        let dir = TempDir::new().unwrap();
        let sys = dir.path().join("crates").join("leviath-sys").join("src");
        std::fs::create_dir_all(&sys).unwrap();
        let mut body = String::new();
        for i in 0..MAX_SYS_EXCLUSIONS + 1 {
            body.push_str(&format!(
                "// REAL-IO-EXCLUDED: leaf {i}\n#[cfg(not(test))]\nfn leaf_{i}() {{}}\n\
                 #[cfg(test)]\nfn leaf_{i}() {{}}\n\n"
            ));
        }
        std::fs::write(sys.join("lib.rs"), body).unwrap();
        let violations = scan_workspace_from(dir.path()).unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v.pattern.contains("cap exceeded in leviath-sys")),
            "expected a cap violation: {violations:?}"
        );
    }

    #[test]
    fn sys_cfg_not_test_at_cap_is_clean() {
        let dir = TempDir::new().unwrap();
        let sys = dir.path().join("crates").join("leviath-sys").join("src");
        std::fs::create_dir_all(&sys).unwrap();
        let mut body = String::new();
        for i in 0..MAX_SYS_EXCLUSIONS {
            body.push_str(&format!(
                "// REAL-IO-EXCLUDED: leaf {i}\n#[cfg(not(test))]\nfn leaf_{i}() {{}}\n\
                 #[cfg(test)]\nfn leaf_{i}() {{}}\n\n"
            ));
        }
        std::fs::write(sys.join("lib.rs"), body).unwrap();
        let violations = scan_workspace_from(dir.path()).unwrap();
        assert!(
            !violations
                .iter()
                .any(|v| v.pattern.contains("cap exceeded")),
            "at-cap should be clean: {violations:?}"
        );
    }

    // ── `COVERAGE-CONFIRMED-ARTIFACT` marker audit ────────────────────────────

    #[test]
    fn coverage_confirmed_artifact_with_reason_and_fn_is_clean() {
        let v = scan_content(
            "/// COVERAGE-CONFIRMED-ARTIFACT: generic monomorphization undercounts \
             this despite every instantiation being covered (confirmed via HTML).\n\
             fn generic_helper() {}\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn coverage_confirmed_artifact_missing_reason_is_violation() {
        let v = scan_content(
            "/// COVERAGE-CONFIRMED-ARTIFACT:\nfn generic_helper() {}\n",
            true,
        );
        assert!(
            v.iter().any(|v| v
                .pattern
                .contains("COVERAGE-CONFIRMED-ARTIFACT missing reason")),
            "expected a missing-reason violation: {v:?}"
        );
    }

    #[test]
    fn coverage_confirmed_artifact_not_attached_to_fn_is_violation() {
        let v = scan_content(
            "/// COVERAGE-CONFIRMED-ARTIFACT: some reason\nstruct NotAFunction;\n",
            true,
        );
        assert!(
            v.iter().any(|v| v
                .pattern
                .contains("COVERAGE-CONFIRMED-ARTIFACT not attached to a fn")),
            "expected a not-attached-to-fn violation: {v:?}"
        );
    }

    #[test]
    fn coverage_confirmed_artifact_as_last_line_of_file_is_violation() {
        let v = scan_content("/// COVERAGE-CONFIRMED-ARTIFACT: some reason", true);
        assert!(
            v.iter().any(|v| v
                .pattern
                .contains("COVERAGE-CONFIRMED-ARTIFACT not attached to a fn")),
            "expected a not-attached-to-fn violation: {v:?}"
        );
    }

    #[test]
    fn coverage_confirmed_artifact_skips_further_doc_lines_and_attributes() {
        let v = scan_content(
            "/// COVERAGE-CONFIRMED-ARTIFACT: some reason, explained further\n\
             /// across multiple doc-comment lines before the attribute.\n\
             #[allow(dead_code)]\n\
             fn generic_helper() {}\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn coverage_confirmed_artifact_in_plain_comment_is_ignored() {
        // A plain `//` comment doesn't count as the CONFIRMED-ARTIFACT
        // doc-comment marker even if it contains the right literal text (this
        // marker is scoped to `///` lines only).
        let v = scan_content(
            "// COVERAGE-CONFIRMED-ARTIFACT: some reason\nfn generic_helper() {}\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn scan_workspace_from_finds_cfg_not_test_violation_in_crates_dir() {
        let root = TempDir::new().unwrap();
        let crate_a = root.path().join("crates").join("crate-a").join("src");
        std::fs::create_dir_all(&crate_a).unwrap();
        std::fs::write(
            crate_a.join("lib.rs"),
            "#[cfg(not(test))]\nfn real_thing() {}\n",
        )
        .unwrap();

        let violations = scan_workspace_from(root.path()).unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v.pattern.contains("cfg(not(test))")),
            "expected a cfg(not(test)) violation: {violations:?}"
        );
    }
}
