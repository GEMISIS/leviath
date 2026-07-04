//! Guard against silently raising the coverage ceiling in
//! `xtask/src/coverage.rs` (`MAX_MISSED_REGIONS`/`MAX_MISSED_LINES`/
//! `MAX_MISSED_FUNCTIONS`).
//!
//! Those constants are a deliberate, evidence-based ratchet (see their own
//! doc comment in `coverage.rs`) -- every value was set from real, repeated
//! CI measurements, not chosen arbitrarily. Nothing stops a developer from
//! bumping one of them to make an inconvenient coverage failure go away,
//! though, since they're just `const` declarations like any other. This
//! check makes that a deliberate, visible act instead of a silent one: it
//! diffs the current values against a baseline git ref and fails if any
//! increased, with no built-in bypass. If a ceiling genuinely needs to rise
//! (a new, real, evidenced measurement-jitter finding), the fix is to gather
//! that evidence the same way the current values were set, not to quietly
//! edit past this check.

use anyhow::{Context, Result};

// ── Git access (injectable for testing) ─────────────────────────────────────

/// Abstraction over reading a file's content at a given git ref — inject a
/// mock in tests.
pub trait GitShow {
    /// Returns the content of `path` as it existed at `git_ref`, or `Ok(None)`
    /// if the ref/path can't be resolved (e.g. a shallow clone that never
    /// fetched that ref, or the very first commit with no parent) — treated
    /// as "nothing to compare against" rather than an error, since this
    /// check's job is to catch *regressions*, not to demand git history that
    /// may not exist in every environment it runs in.
    fn show(&self, git_ref: &str, path: &str) -> Result<Option<String>>;
}

pub struct RealGitShow;

impl GitShow for RealGitShow {
    fn show(&self, git_ref: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{git_ref}:{path}");
        let out = std::process::Command::new("git")
            .args(["show", &spec])
            .output()
            .context("failed to spawn git — is git installed and in PATH?")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    }
}

// ── Ceiling parsing/diffing (pure, testable) ─────────────────────────────────

const COVERAGE_RS_PATH: &str = "xtask/src/coverage.rs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceiling {
    pub regions: u64,
    pub lines: u64,
    pub functions: u64,
}

/// Extract `MAX_MISSED_REGIONS`/`MAX_MISSED_LINES`/`MAX_MISSED_FUNCTIONS`'s
/// literal values from `coverage.rs`'s source text.
///
/// Deliberately a plain string scan, not a `syn`-based parse: these are three
/// fixed, simple `const NAME: u64 = N;` declarations whose exact text this
/// file controls, so a full Rust parser would be a heavy dependency for no
/// real robustness gain here.
pub fn parse_ceiling(content: &str) -> Result<Ceiling> {
    Ok(Ceiling {
        regions: parse_const(content, "MAX_MISSED_REGIONS")?,
        lines: parse_const(content, "MAX_MISSED_LINES")?,
        functions: parse_const(content, "MAX_MISSED_FUNCTIONS")?,
    })
}

fn parse_const(content: &str, name: &str) -> Result<u64> {
    let needle = format!("const {name}: u64 = ");
    let start = content
        .find(&needle)
        .with_context(|| format!("could not find `{needle}` in {COVERAGE_RS_PATH}"))?
        + needle.len();
    let rest = &content[start..];
    let end = rest
        .find(';')
        .with_context(|| format!("no terminating ';' after `{needle}`"))?;
    rest[..end]
        .trim()
        .parse::<u64>()
        .with_context(|| format!("`{name}`'s value is not a valid u64"))
}

/// Return one human-readable line per metric that increased from `baseline`
/// to `current` — empty if the ceiling didn't rise on any metric.
pub fn ceiling_increases(baseline: &Ceiling, current: &Ceiling) -> Vec<String> {
    let mut increases = Vec::new();
    if current.regions > baseline.regions {
        increases.push(format!(
            "MAX_MISSED_REGIONS: {} -> {}",
            baseline.regions, current.regions
        ));
    }
    if current.lines > baseline.lines {
        increases.push(format!(
            "MAX_MISSED_LINES: {} -> {}",
            baseline.lines, current.lines
        ));
    }
    if current.functions > baseline.functions {
        increases.push(format!(
            "MAX_MISSED_FUNCTIONS: {} -> {}",
            baseline.functions, current.functions
        ));
    }
    increases
}

// ── Public entry point ──────────────────────────────────────────────────────

pub fn run() -> Result<()> {
    let baseline_ref = std::env::var("CEILING_BASELINE_REF").unwrap_or_else(|_| "HEAD".to_owned());
    run_with(&RealGitShow, &baseline_ref, COVERAGE_RS_PATH)
}

/// Core logic, parameterized by the git backend, baseline ref, and file path
/// for testability.
pub fn run_with(git: &dyn GitShow, baseline_ref: &str, path: &str) -> Result<()> {
    let current_content =
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let current = parse_ceiling(&current_content)?;

    let Some(baseline_content) = git.show(baseline_ref, path)? else {
        println!(
            "[check-ceiling] Could not read {path} at '{baseline_ref}' -- skipping the \
             ceiling-regression check (no baseline to compare against, e.g. a shallow clone \
             that never fetched that ref, or this is the repo's first commit)."
        );
        return Ok(());
    };
    let baseline = parse_ceiling(&baseline_content)?;

    let increases = ceiling_increases(&baseline, &current);
    if !increases.is_empty() {
        let mut msg = format!(
            "[check-ceiling] The coverage ceiling in {path} was raised compared to \
             '{baseline_ref}', with no way to auto-verify that's backed by real evidence:\n"
        );
        for line in &increases {
            msg.push_str("  ");
            msg.push_str(line);
            msg.push('\n');
        }
        msg.push_str(
            "Raising this ceiling is only appropriate after gathering fresh, real CI \
             measurements the same way the current values were set (see MAX_MISSED_REGIONS's \
             own doc comment in xtask/src/coverage.rs) -- this check has no bypass by design, \
             so if that's genuinely what happened here, get another reviewer to confirm the \
             evidence rather than silently raising the number to make a failure go away.",
        );
        anyhow::bail!(msg);
    }

    println!("[check-ceiling] Coverage ceiling not raised vs '{baseline_ref}'. ✓");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_ceiling ─────────────────────────────────────────────────────────

    fn sample_source(regions: u64, lines: u64, functions: u64) -> String {
        format!(
            "const MAX_MISSED_REGIONS: u64 = {regions};\n\
             const MAX_MISSED_LINES: u64 = {lines};\n\
             const MAX_MISSED_FUNCTIONS: u64 = {functions};\n"
        )
    }

    #[test]
    fn parse_ceiling_extracts_all_three_values() {
        let content = sample_source(100, 40, 10);
        let ceiling = parse_ceiling(&content).unwrap();
        assert_eq!(
            ceiling,
            Ceiling {
                regions: 100,
                lines: 40,
                functions: 10
            }
        );
    }

    #[test]
    fn parse_ceiling_ignores_surrounding_content() {
        let content = format!(
            "//! Some doc comment\nuse anyhow::Result;\n\n{}\n\nfn other() {{}}\n",
            sample_source(5, 6, 7)
        );
        let ceiling = parse_ceiling(&content).unwrap();
        assert_eq!(ceiling.regions, 5);
        assert_eq!(ceiling.lines, 6);
        assert_eq!(ceiling.functions, 7);
    }

    #[test]
    fn parse_ceiling_missing_constant_is_err() {
        let content = "const MAX_MISSED_REGIONS: u64 = 1;\n";
        let result = parse_ceiling(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("MAX_MISSED_LINES"));
    }

    #[test]
    fn parse_ceiling_missing_semicolon_is_err() {
        let content = "const MAX_MISSED_REGIONS: u64 = 1\n";
        let result = parse_ceiling(content);
        assert!(result.is_err());
    }

    #[test]
    fn parse_ceiling_non_numeric_value_is_err() {
        let content = "const MAX_MISSED_REGIONS: u64 = not_a_number;\n";
        let result = parse_ceiling(content);
        assert!(result.is_err());
    }

    // ── ceiling_increases ─────────────────────────────────────────────────────

    #[test]
    fn ceiling_increases_empty_when_unchanged() {
        let c = Ceiling {
            regions: 10,
            lines: 5,
            functions: 2,
        };
        assert!(ceiling_increases(&c, &c).is_empty());
    }

    #[test]
    fn ceiling_increases_empty_when_lowered() {
        let baseline = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let current = Ceiling {
            regions: 50,
            lines: 20,
            functions: 5,
        };
        assert!(ceiling_increases(&baseline, &current).is_empty());
    }

    #[test]
    fn ceiling_increases_detects_regions_increase() {
        let baseline = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let current = Ceiling {
            regions: 200,
            lines: 40,
            functions: 10,
        };
        let increases = ceiling_increases(&baseline, &current);
        assert_eq!(increases, vec!["MAX_MISSED_REGIONS: 100 -> 200"]);
    }

    #[test]
    fn ceiling_increases_detects_lines_increase() {
        let baseline = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let current = Ceiling {
            regions: 100,
            lines: 90,
            functions: 10,
        };
        let increases = ceiling_increases(&baseline, &current);
        assert_eq!(increases, vec!["MAX_MISSED_LINES: 40 -> 90"]);
    }

    #[test]
    fn ceiling_increases_detects_functions_increase() {
        let baseline = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let current = Ceiling {
            regions: 100,
            lines: 40,
            functions: 20,
        };
        let increases = ceiling_increases(&baseline, &current);
        assert_eq!(increases, vec!["MAX_MISSED_FUNCTIONS: 10 -> 20"]);
    }

    #[test]
    fn ceiling_increases_detects_all_three_at_once() {
        let baseline = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let current = Ceiling {
            regions: 200,
            lines: 90,
            functions: 20,
        };
        let increases = ceiling_increases(&baseline, &current);
        assert_eq!(increases.len(), 3);
    }

    // ── run_with (mock git) ───────────────────────────────────────────────────

    struct MockGitShow {
        response: Result<Option<String>, String>,
    }

    impl GitShow for MockGitShow {
        fn show(&self, _git_ref: &str, _path: &str) -> Result<Option<String>> {
            match &self.response {
                Ok(content) => Ok(content.clone()),
                Err(e) => anyhow::bail!("{e}"),
            }
        }
    }

    fn write_temp_coverage_rs(dir: &std::path::Path, ceiling: Ceiling) -> std::path::PathBuf {
        let path = dir.join("coverage.rs");
        std::fs::write(
            &path,
            sample_source(ceiling.regions, ceiling.lines, ceiling.functions),
        )
        .unwrap();
        path
    }

    #[test]
    fn run_with_unchanged_ceiling_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let ceiling = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let path = write_temp_coverage_rs(dir.path(), ceiling);
        let git = MockGitShow {
            response: Ok(Some(sample_source(100, 40, 10))),
        };
        let result = run_with(&git, "HEAD", path.to_str().unwrap());
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn run_with_lowered_ceiling_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let ceiling = Ceiling {
            regions: 50,
            lines: 20,
            functions: 5,
        };
        let path = write_temp_coverage_rs(dir.path(), ceiling);
        let git = MockGitShow {
            response: Ok(Some(sample_source(100, 40, 10))),
        };
        let result = run_with(&git, "HEAD", path.to_str().unwrap());
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn run_with_raised_ceiling_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let ceiling = Ceiling {
            regions: 500,
            lines: 40,
            functions: 10,
        };
        let path = write_temp_coverage_rs(dir.path(), ceiling);
        let git = MockGitShow {
            response: Ok(Some(sample_source(100, 40, 10))),
        };
        let result = run_with(&git, "origin/main", path.to_str().unwrap());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("MAX_MISSED_REGIONS: 100 -> 500"), "{msg}");
        assert!(msg.contains("origin/main"), "{msg}");
    }

    #[test]
    fn run_with_no_baseline_available_is_ok_and_skips() {
        let dir = tempfile::tempdir().unwrap();
        let ceiling = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let path = write_temp_coverage_rs(dir.path(), ceiling);
        let git = MockGitShow { response: Ok(None) };
        let result = run_with(&git, "origin/main", path.to_str().unwrap());
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn run_with_git_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let ceiling = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let path = write_temp_coverage_rs(dir.path(), ceiling);
        let git = MockGitShow {
            response: Err("simulated git failure".to_owned()),
        };
        let result = run_with(&git, "origin/main", path.to_str().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated git failure"));
    }

    #[test]
    fn run_with_missing_current_file_is_err() {
        let git = MockGitShow {
            response: Ok(Some(sample_source(100, 40, 10))),
        };
        let result = run_with(&git, "HEAD", "/no/such/file/coverage.rs");
        assert!(result.is_err());
    }

    #[test]
    fn run_with_malformed_baseline_content_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let ceiling = Ceiling {
            regions: 100,
            lines: 40,
            functions: 10,
        };
        let path = write_temp_coverage_rs(dir.path(), ceiling);
        let git = MockGitShow {
            response: Ok(Some("not valid coverage.rs content".to_owned())),
        };
        let result = run_with(&git, "HEAD", path.to_str().unwrap());
        assert!(result.is_err());
    }

    // ── RealGitShow ───────────────────────────────────────────────────────────

    #[test]
    fn real_git_show_reads_this_repos_own_coverage_rs_at_head() {
        // Exercises the real subprocess path against a ref/path that
        // genuinely exists in this checkout.
        let result = RealGitShow.show("HEAD", "xtask/src/coverage.rs");
        assert!(result.is_ok(), "{result:?}");
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn real_git_show_nonexistent_ref_returns_none() {
        let result = RealGitShow.show(
            "definitely-not-a-real-ref-xyz-12345",
            "xtask/src/coverage.rs",
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn real_git_show_nonexistent_path_returns_none() {
        let result = RealGitShow.show("HEAD", "no/such/path/in/the/repo.rs");
        assert!(result.is_ok(), "{result:?}");
        assert!(result.unwrap().is_none());
    }
}
