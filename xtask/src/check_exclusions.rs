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
//! * `.githooks/`           — commit hooks
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

    // CI and config files
    for rel in &[
        ".github/workflows/ci.yml",
        ".github/workflows/alpha.yml",
        ".github/workflows/beta.yml",
        ".github/workflows/prod.yml",
        ".githooks/pre-commit",
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
    }
}

// ── `#[cfg(not(test))]` escape-hatch audit ──────────────────────────────────
//
// `#[cfg(not(test))]` / `#[cfg(test)]` twins are a *legitimate* pattern for
// code that genuinely cannot be exercised by a real test: real terminal/TTY
// interaction (e.g. `dashboard/mod.rs`'s `enter_alt_screen`/`leave_alt_screen`
// entering/leaving the real alternate screen, `dashboard/helpers.rs`'s
// `open_controlling_tty` opening a real `/dev/tty`), real blocking stdin
// reads, real subprocess spawns, and the `cargo llvm-cov` tracing-macro
// region-counting quirk documented in `xtask/src/coverage.rs` (a macro
// field-capture sub-region reads zero even though the branch executes and is
// asserted on). It is NOT a general-purpose way to dodge writing a real test
// for code that could otherwise be made testable, so this check keeps it
// honest and auditable rather than banning it outright:
//
// 1. Every `#[cfg(not(test))]`-gated `fn`/method must be immediately preceded
//    (skipping over blank lines and other attributes) by a `///` doc comment
//    containing `COVERAGE-EXCLUDED: <non-empty reason>` — a human-readable
//    justification, reviewable in every PR that adds or touches one. The
//    justification may span multiple `///` lines (marker on the first line,
//    reason continuing below it); the marker just has to appear somewhere in
//    the contiguous doc-comment block touching the attribute.
// 2. Every `#[cfg(not(test))]`-gated `fn NAME` must have a matching
//    `#[cfg(test)]`-gated `fn NAME` (same name) elsewhere in the same file —
//    proof the real body was actually swapped for a no-op twin rather than
//    just hidden from the test binary with nothing standing in for it.
//
// This is a line/regex-style scan (matching the rest of this file), not a
// full parser: it looks for the exact `#[cfg(not(test))]` / `#[cfg(test)]`
// attribute on its own line, and reads the function name off the next
// non-blank, non-attribute line by splitting on the `fn` keyword token. It is
// deliberately tolerant of being fooled by unusual formatting — the existing
// scanner in this file makes the same trade-off.

/// Doc-comment marker required immediately before every `#[cfg(not(test))]`
/// function/method, explaining why the real body cannot be exercised by a
/// real test.
const COVERAGE_EXCLUDED_MARKER: &str = "COVERAGE-EXCLUDED:";

/// Scan a single file's already-read `content` for `#[cfg(not(test))]`
/// escape-hatch violations (see the module doc above for the two rules).
fn scan_cfg_not_test_escape_hatches(path: &Path, content: &str, violations: &mut Vec<Violation>) {
    let lines: Vec<&str> = content.lines().collect();

    // Every `fn NAME` immediately gated by `#[cfg(test)]` anywhere in the
    // file — the pool of legitimate "twins" for rule 2.
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
        // If the attribute doesn't gate a `fn`/method declaration (e.g. it
        // gates a `use` statement or a plain block), it's out of scope for
        // this check entirely.
        let Some(fn_name) = fn_name_after_attribute(&lines, idx) else {
            continue;
        };

        if !preceded_by_coverage_excluded_marker(&lines, idx) {
            violations.push(Violation {
                file: path.to_owned(),
                line_num: idx + 1,
                line: (*line).to_owned(),
                pattern: "cfg(not(test)) missing COVERAGE-EXCLUDED marker",
                reason: format!(
                    "fn `{fn_name}` is #[cfg(not(test))] but is not immediately preceded by a \
                     `/// COVERAGE-EXCLUDED: <reason>` doc comment explaining why it cannot be \
                     exercised by a real test"
                ),
            });
        }

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
/// blank lines and other attribute lines, to find the contiguous `///`
/// doc-comment block (if any) immediately touching the attribute, then check
/// whether ANY line in that block contains `COVERAGE-EXCLUDED:` followed by a
/// non-empty reason.
///
/// Justifications are frequently multi-line -- the marker on the first line
/// of the paragraph, with the reason continuing across further `///` lines
/// below it before the attribute -- so this doesn't require the marker to be
/// on the single line touching the attribute, only somewhere in the doc
/// comment that does.
fn preceded_by_coverage_excluded_marker(lines: &[&str], attr_idx: usize) -> bool {
    // Skip blank lines and other attributes stacked above `attr_idx` to find
    // where the doc-comment block (if any) ends.
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
        if !trimmed.starts_with("///") {
            return false; // nearest substantive line isn't a doc comment
        }
        break i;
    };

    // Walk backward through the contiguous `///` block starting at
    // `doc_end`, checking every line in it for the marker.
    let mut j = doc_end;
    loop {
        let trimmed = lines[j].trim();
        if let Some(pos) = trimmed.find(COVERAGE_EXCLUDED_MARKER) {
            if !trimmed[pos + COVERAGE_EXCLUDED_MARKER.len()..]
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
        if !lines[j].trim().starts_with("///") {
            return false;
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write `content` to a temp file and scan it.
    fn scan_content(content: &str, is_rs: bool) -> Vec<Violation> {
        let dir = TempDir::new().unwrap();
        let name = if is_rs { "test.rs" } else { "ci.yml" };
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let mut violations = Vec::new();
        scan_file(&path, is_rs, &mut violations);
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
        // A root with no .github/, .githooks/, or .cargo/ should not error.
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

    // ── `#[cfg(not(test))]` escape-hatch audit ────────────────────────────────

    #[test]
    fn cfg_not_test_fn_without_marker_is_violation() {
        let v = scan_content(
            "#[cfg(not(test))]\nfn real_thing() {}\n\n#[cfg(test)]\nfn real_thing() {}\n",
            true,
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing COVERAGE-EXCLUDED marker")),
            "expected a missing-marker violation: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_fn_without_twin_is_violation() {
        let v = scan_content(
            "/// COVERAGE-EXCLUDED: real terminal write, cannot be tested\n\
             #[cfg(not(test))]\nfn real_thing() {}\n",
            true,
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing #[cfg(test)] twin")),
            "expected a missing-twin violation: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_fn_with_marker_and_twin_is_clean() {
        let v = scan_content(
            "/// COVERAGE-EXCLUDED: real terminal write, cannot be tested\n\
             #[cfg(not(test))]\nfn real_thing() -> std::io::Result<()> { Ok(()) }\n\n\
             #[cfg(test)]\nfn real_thing() -> std::io::Result<()> { Ok(()) }\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_on_use_statement_is_out_of_scope() {
        // `#[cfg(not(test))]` gating a `use` (not a `fn`) is not this check's
        // concern -- no marker or twin is required.
        let v = scan_content("#[cfg(not(test))]\nuse std::io::Write;\n", true);
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_on_block_is_out_of_scope() {
        // Mirrors `runstate.rs`/`setup.rs`'s real pattern: `#[cfg(not(test))]`
        // gating a bare `{ ... }` block inside a function body, not a `fn`
        // itself.
        let v = scan_content(
            "fn wrapper() {\n    #[cfg(not(test))]\n    {\n        do_real_thing();\n    }\n}\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_marker_with_empty_reason_is_violation() {
        let v = scan_content(
            "/// COVERAGE-EXCLUDED:\n#[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
            true,
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing COVERAGE-EXCLUDED marker")),
            "empty reason should still be flagged: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_marker_in_non_doc_comment_is_violation() {
        // A plain `//` comment (not `///`) doesn't count as the doc-comment
        // marker even if it contains the right literal text.
        let v = scan_content(
            "// COVERAGE-EXCLUDED: real terminal write\n#[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
            true,
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing COVERAGE-EXCLUDED marker")),
            "plain comment should not satisfy the marker requirement: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_marker_skips_other_attributes_and_blank_lines() {
        let v = scan_content(
            "/// COVERAGE-EXCLUDED: real terminal write, cannot be tested\n\
             \n\
             #[allow(dead_code)]\n\
             #[cfg(not(test))]\n\
             fn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_marker_on_first_line_of_multiline_doc_block_is_accepted() {
        // Mirrors the real-world convention used throughout this codebase:
        // the marker on the first `///` line, with the reason continuing
        // across further contiguous `///` lines before the attribute.
        let v = scan_content(
            "/// COVERAGE-EXCLUDED: llvm-cov's tracing-macro region-counting\n\
             /// quirk -- the branch executes and is asserted on, but the\n\
             /// macro's field-capture sub-region still reads zero.\n\
             #[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_multiline_doc_block_without_marker_anywhere_is_violation() {
        let v = scan_content(
            "/// This body does something real.\n\
             /// It cannot be tested for reasons.\n\
             #[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\nfn real_thing() {}\n",
            true,
        );
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing COVERAGE-EXCLUDED marker")),
            "expected a missing-marker violation: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_twin_lookup_skips_attributes_and_blank_lines_before_fn() {
        let v = scan_content(
            "/// COVERAGE-EXCLUDED: real terminal write, cannot be tested\n\
             #[cfg(not(test))]\nfn real_thing() {}\n\n\
             #[cfg(test)]\n\n#[allow(dead_code)]\nfn real_thing() {}\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_fn_name_extraction_handles_pub_async_generic() {
        let v = scan_content(
            "/// COVERAGE-EXCLUDED: real subprocess spawn, cannot be tested\n\
             #[cfg(not(test))]\npub(crate) async fn spawn_it<T>(x: T) -> T { x }\n\n\
             #[cfg(test)]\npub(crate) async fn spawn_it<T>(x: T) -> T { x }\n",
            true,
        );
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_not_scanned_in_config_files() {
        // The escape-hatch audit only runs for `.rs` files (is_rs = true).
        let v = scan_content("#[cfg(not(test))]\nfn real_thing() {}\n", false);
        assert!(v.is_empty(), "config files should not be scanned: {v:?}");
    }

    #[test]
    fn cfg_not_test_as_first_line_of_file_has_no_preceding_line() {
        // attr_idx == 0: there's nothing to scan backward over, so the
        // marker requirement is unmet.
        let v = scan_content("#[cfg(not(test))]\nfn real_thing() {}\n", true);
        assert!(
            v.iter()
                .any(|v| v.pattern.contains("missing COVERAGE-EXCLUDED marker")),
            "expected a missing-marker violation: {v:?}"
        );
    }

    #[test]
    fn cfg_not_test_followed_by_non_fn_declaration_is_out_of_scope() {
        let v = scan_content("#[cfg(not(test))]\nstruct RealThing;\n", true);
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_as_last_line_of_file_is_out_of_scope() {
        // fn_name_after_attribute walks off the end of the file without
        // finding a substantive line.
        let v = scan_content("#[cfg(not(test))]", true);
        assert!(v.is_empty(), "expected no violations: {v:?}");
    }

    #[test]
    fn cfg_not_test_missing_both_marker_and_twin_reports_two_violations() {
        let v = scan_content("#[cfg(not(test))]\nfn real_thing() {}\n", true);
        assert_eq!(v.len(), 2, "expected both violations: {v:?}");
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
