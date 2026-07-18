//! Leviath xtask — dev-tool automation for the workspace.
//!
//! Run via: `cargo xtask <subcommand>`
//!
//! Subcommands:
//!   coverage                    Compute all packages, aggregate, enforce a hard 100%.
//!   coverage --package <pkg>    Compute just one package's coverage (CI fan-out).
//!   coverage --gate             Aggregate collected per-package JSONs and enforce 100%.
//!   check-exclusions            Verify no coverage-suppression markers exist anywhere.

mod check_exclusions;
mod coverage;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dispatch(&args)
}

/// Route the CLI arguments (after the binary name) to the appropriate handler.
///
/// Extracted from `main` so it can be unit-tested without spawning processes.
pub fn dispatch(args: &[String]) -> Result<()> {
    dispatch_with(args, coverage::run, check_exclusions::run)
}

/// Route the CLI arguments to the provided handler closures.
///
/// `run_cov` and `run_excl` replace the real `coverage::run` and
/// `check_exclusions::run` in unit tests, making every match arm reachable
/// without invoking external tooling.
pub fn dispatch_with(
    args: &[String],
    run_cov: impl FnOnce(coverage::CoverageMode) -> Result<()>,
    run_excl: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");
    match subcommand {
        "coverage" => {
            let mode = coverage::CoverageMode::parse(&args[1..])?;
            run_cov(mode)
        }
        "check-exclusions" => run_excl(),
        "help" | "--help" | "-h" => {
            println!("Usage: cargo xtask <subcommand>");
            println!();
            println!("Subcommands:");
            println!("  coverage                  Compute all packages, aggregate, enforce 100%%");
            println!("  coverage --package <pkg>  Compute one package's coverage (CI fan-out)");
            println!(
                "  coverage --gate           Aggregate collected per-package JSONs, gate 100%%"
            );
            println!(
                "  check-exclusions          Scan codebase for forbidden coverage-suppression markers"
            );
            Ok(())
        }
        other => anyhow::bail!("Unknown subcommand: '{other}'. Run `cargo xtask help` for usage."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coverage::CoverageMode;

    // ── Named stub functions ───────────────────────────────────────────────────
    //
    // Using named `fn` items rather than `|| Ok(())` closures avoids creating
    // unique LLVM closure regions that are each only "covered" when they happen
    // to be the dispatched arm.  A named function's body is covered as soon as it
    // is called ONCE across the entire test suite — all other tests that pass it
    // as the "other" arm share that single coverage hit.

    fn always_ok() -> Result<()> {
        Ok(())
    }

    /// A `run_cov` stub matching `impl FnOnce(CoverageMode) -> Result<()>`.
    fn cov_ok(_mode: CoverageMode) -> Result<()> {
        Ok(())
    }

    /// Covers the stub bodies so every other test that passes them as a stub
    /// (and may not call them) benefits from this single coverage hit.
    #[test]
    fn stubs_return_ok() {
        assert!(always_ok().is_ok());
        assert!(cov_ok(CoverageMode::All).is_ok());
    }

    /// Build an owned-args slice from string literals (dispatch takes `&[String]`).
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    // ── Help variants ────────────────────────────────────────────────────────

    #[test]
    fn dispatch_help_exits_ok() {
        assert!(dispatch(&args(&["help"])).is_ok());
    }

    #[test]
    fn dispatch_help_flag_exits_ok() {
        assert!(dispatch(&args(&["--help"])).is_ok());
    }

    #[test]
    fn dispatch_short_help_flag_exits_ok() {
        assert!(dispatch(&args(&["-h"])).is_ok());
    }

    // ── Unknown subcommand ───────────────────────────────────────────────────

    #[test]
    fn dispatch_unknown_subcommand_returns_err() {
        let err = dispatch(&args(&["frobnicate"])).unwrap_err();
        assert!(
            err.to_string().contains("frobnicate"),
            "error message should mention the unknown subcommand"
        );
    }

    #[test]
    fn dispatch_empty_string_returns_err() {
        assert!(dispatch(&args(&[""])).is_err());
    }

    // ── Default (no args) behaviour ───────────────────────────────────────────

    #[test]
    fn dispatch_uses_help_as_default_when_no_args() {
        // Empty args slice → args.first() is None → defaults to "help".
        assert!(dispatch(&[]).is_ok());
    }

    // ── coverage arm: mode parsing + dispatch ─────────────────────────────────

    #[test]
    fn dispatch_with_coverage_no_args_parses_all_and_calls_run_cov() {
        let mut got = None;
        dispatch_with(
            &args(&["coverage"]),
            |mode| {
                got = Some(mode);
                Ok(())
            },
            always_ok,
        )
        .unwrap();
        assert_eq!(got, Some(CoverageMode::All));
    }

    #[test]
    fn dispatch_with_coverage_gate_parses_gate() {
        let mut got = None;
        dispatch_with(
            &args(&["coverage", "--gate"]),
            |mode| {
                got = Some(mode);
                Ok(())
            },
            always_ok,
        )
        .unwrap();
        assert_eq!(got, Some(CoverageMode::Gate));
    }

    #[test]
    fn dispatch_with_coverage_package_parses_package() {
        let mut got = None;
        dispatch_with(
            &args(&["coverage", "--package", "leviath-core"]),
            |mode| {
                got = Some(mode);
                Ok(())
            },
            always_ok,
        )
        .unwrap();
        assert_eq!(got, Some(CoverageMode::Package("leviath-core".to_owned())));
    }

    #[test]
    fn dispatch_with_coverage_bad_arg_returns_err_without_calling_run_cov() {
        // Mode parsing fails before run_cov is ever invoked.
        let result = dispatch_with(
            &args(&["coverage", "--bogus"]),
            cov_ok, // never called; covered by stubs_return_ok
            always_ok,
        );
        assert!(
            result.is_err(),
            "unknown coverage flag must error: {result:?}"
        );
    }

    #[test]
    fn dispatch_with_coverage_propagates_error() {
        let result = dispatch_with(
            &args(&["coverage"]),
            |_mode| anyhow::bail!("simulated coverage failure"),
            always_ok, // never called; covered by stubs_return_ok
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated coverage failure"));
    }

    // ── check-exclusions arm ──────────────────────────────────────────────────

    #[test]
    fn dispatch_with_check_exclusions_calls_run_excl() {
        let mut called = false;
        dispatch_with(&args(&["check-exclusions"]), cov_ok, || {
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(called, "run_excl should have been called");
    }

    #[test]
    fn dispatch_with_check_exclusions_propagates_error() {
        let result = dispatch_with(
            &args(&["check-exclusions"]),
            cov_ok, // never called; covered by stubs_return_ok
            || anyhow::bail!("simulated exclusion failure"),
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("simulated exclusion failure"));
    }
}
