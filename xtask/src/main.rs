//! Leviath xtask — dev-tool automation for the workspace.
//!
//! Run via: `cargo xtask <subcommand>`
//!
//! Subcommands:
//!   `coverage`                  Gate every workspace package at a hard 100%.
//!   `coverage --package <pkg>`  Gate just one package (the CI per-package fan-out).

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
    dispatch_with(args, coverage::run)
}

/// Route the CLI arguments to the provided handler closure.
///
/// `run_cov` replaces the real `coverage::run` in unit tests, making every
/// match arm reachable without invoking external tooling.
pub fn dispatch_with(
    args: &[String],
    run_cov: impl FnOnce(coverage::CoverageMode) -> Result<()>,
) -> Result<()> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");
    match subcommand {
        "coverage" => {
            let mode = coverage::CoverageMode::parse(&args[1..])?;
            run_cov(mode)
        }
        "help" | "--help" | "-h" => {
            println!("Usage: cargo xtask <subcommand>");
            println!();
            println!("Subcommands:");
            println!("  coverage                  Gate every workspace package at 100%%");
            println!("  coverage --package <pkg>  Gate one package (CI per-package fan-out)");
            Ok(())
        }
        other => anyhow::bail!("Unknown subcommand: '{other}'. Run `cargo xtask help` for usage."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coverage::CoverageMode;

    // ── Named stub function ─────────────────────────────────────────────────────
    //
    // Using a named `fn` item rather than a `|| Ok(())` closure avoids creating a
    // unique LLVM closure region that is only "covered" when it happens to be the
    // dispatched arm. A named function's body is covered as soon as it is called
    // ONCE across the whole test suite.

    /// A `run_cov` stub matching `impl FnOnce(CoverageMode) -> Result<()>`.
    fn cov_ok(_mode: CoverageMode) -> Result<()> {
        Ok(())
    }

    /// Covers the stub body so tests that pass it without calling it still get
    /// this single coverage hit.
    #[test]
    fn stub_returns_ok() {
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
        dispatch_with(&args(&["coverage"]), |mode| {
            got = Some(mode);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, Some(CoverageMode::All));
    }

    #[test]
    fn dispatch_with_coverage_package_parses_package() {
        let mut got = None;
        dispatch_with(&args(&["coverage", "--package", "leviath-core"]), |mode| {
            got = Some(mode);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, Some(CoverageMode::Package("leviath-core".to_owned())));
    }

    #[test]
    fn dispatch_with_coverage_bad_arg_returns_err_without_calling_run_cov() {
        // Mode parsing fails before run_cov is ever invoked.
        let result = dispatch_with(
            &args(&["coverage", "--bogus"]),
            cov_ok, // never called; covered by stub_returns_ok
        );
        assert!(
            result.is_err(),
            "unknown coverage flag must error: {result:?}"
        );
    }

    #[test]
    fn dispatch_with_coverage_propagates_error() {
        let result = dispatch_with(&args(&["coverage"]), |_mode| {
            anyhow::bail!("simulated coverage failure")
        });
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("simulated coverage failure")
        );
    }
}
