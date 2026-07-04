//! Leviath xtask — dev-tool automation for the workspace.
//!
//! Run via: `cargo xtask <subcommand>`
//!
//! Subcommands:
//!   coverage          Run cargo-llvm-cov and enforce 100% (regions/lines/functions/branches).
//!   check-exclusions  Verify no coverage-suppression markers exist anywhere in the codebase.
//!   check-ceiling     Verify xtask/src/coverage.rs's coverage ceiling wasn't silently raised.

mod check_ceiling;
mod check_exclusions;
mod coverage;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(String::as_str).unwrap_or("help");
    dispatch(subcommand)
}

/// Route a subcommand string to the appropriate handler.
///
/// Extracted from `main` so it can be unit-tested without spawning processes.
pub fn dispatch(subcommand: &str) -> Result<()> {
    dispatch_with(
        subcommand,
        coverage::run,
        check_exclusions::run,
        check_ceiling::run,
    )
}

/// Route a subcommand string to the provided handler closures.
///
/// `run_cov`, `run_excl`, and `run_ceiling` replace the real `coverage::run`,
/// `check_exclusions::run`, and `check_ceiling::run` in unit tests, making
/// every match arm reachable without invoking external tooling.
pub fn dispatch_with(
    subcommand: &str,
    run_cov: impl FnOnce() -> Result<()>,
    run_excl: impl FnOnce() -> Result<()>,
    run_ceiling: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match subcommand {
        "coverage" => run_cov(),
        "check-exclusions" => run_excl(),
        "check-ceiling" => run_ceiling(),
        "help" | "--help" | "-h" => {
            println!("Usage: cargo xtask <subcommand>");
            println!();
            println!("Subcommands:");
            println!("  coverage          Run test coverage and enforce 100%% across all metrics");
            println!(
                "  check-exclusions  Scan codebase for forbidden coverage-suppression markers"
            );
            println!(
                "  check-ceiling     Verify the coverage ceiling wasn't silently raised (set \
                 CEILING_BASELINE_REF to override the default 'HEAD' comparison ref)"
            );
            Ok(())
        }
        other => anyhow::bail!("Unknown subcommand: '{other}'. Run `cargo xtask help` for usage."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Named stub function ────────────────────────────────────────────────────
    //
    // Using a named `fn` item rather than `|| Ok(())` closures avoids creating
    // 8 unique LLVM closure regions that are each only "covered" when they happen
    // to be the dispatched arm.  A named function's body is covered as soon as it
    // is called ONCE across the entire test suite — all other tests that pass it
    // as the "other" arm share that single coverage hit.

    fn always_ok() -> Result<()> {
        Ok(())
    }

    /// Covers `always_ok`'s body so every other test that passes it as a stub
    /// (and may not call it) benefits from this single coverage hit.
    #[test]
    fn always_ok_stub_returns_ok() {
        assert!(always_ok().is_ok());
    }

    // ── Help variants ────────────────────────────────────────────────────────

    #[test]
    fn dispatch_help_exits_ok() {
        assert!(dispatch("help").is_ok());
    }

    #[test]
    fn dispatch_help_flag_exits_ok() {
        assert!(dispatch("--help").is_ok());
    }

    #[test]
    fn dispatch_short_help_flag_exits_ok() {
        assert!(dispatch("-h").is_ok());
    }

    // ── Unknown subcommand ───────────────────────────────────────────────────

    #[test]
    fn dispatch_unknown_subcommand_returns_err() {
        let err = dispatch("frobnicate").unwrap_err();
        assert!(
            err.to_string().contains("frobnicate"),
            "error message should mention the unknown subcommand"
        );
    }

    #[test]
    fn dispatch_empty_string_returns_err() {
        assert!(dispatch("").is_err());
    }

    // ── Default (no args) behaviour ───────────────────────────────────────────

    #[test]
    fn dispatch_uses_help_as_default_when_no_args() {
        // Simulates `args.get(1).unwrap_or("help")` in main().
        let subcommand = (None as Option<&str>).unwrap_or("help");
        assert!(dispatch(subcommand).is_ok());
    }

    // ── coverage, check-exclusions, and check-ceiling arms via dispatch_with ──

    #[test]
    fn dispatch_with_coverage_calls_run_cov() {
        let mut called = false;
        // always_ok stubs the other two arms; its body is covered by always_ok_stub_returns_ok.
        dispatch_with(
            "coverage",
            || {
                called = true;
                Ok(())
            },
            always_ok,
            always_ok,
        )
        .unwrap();
        assert!(called, "run_cov should have been called");
    }

    #[test]
    fn dispatch_with_check_exclusions_calls_run_excl() {
        let mut called = false;
        dispatch_with(
            "check-exclusions",
            always_ok,
            || {
                called = true;
                Ok(())
            },
            always_ok,
        )
        .unwrap();
        assert!(called, "run_excl should have been called");
    }

    #[test]
    fn dispatch_with_check_ceiling_calls_run_ceiling() {
        let mut called = false;
        dispatch_with("check-ceiling", always_ok, always_ok, || {
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(called, "run_ceiling should have been called");
    }

    #[test]
    fn dispatch_with_coverage_propagates_error() {
        let result = dispatch_with(
            "coverage",
            || anyhow::bail!("simulated coverage failure"),
            always_ok, // never called; covered by always_ok_stub_returns_ok
            always_ok,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated coverage failure"));
    }

    #[test]
    fn dispatch_with_check_exclusions_propagates_error() {
        let result = dispatch_with(
            "check-exclusions",
            always_ok, // never called; covered by always_ok_stub_returns_ok
            || anyhow::bail!("simulated exclusion failure"),
            always_ok,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated exclusion failure"));
    }

    #[test]
    fn dispatch_with_check_ceiling_propagates_error() {
        let result = dispatch_with("check-ceiling", always_ok, always_ok, || {
            anyhow::bail!("simulated ceiling failure")
        });
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated ceiling failure"));
    }
}
