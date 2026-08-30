//! Leviath xtask - dev-tool automation for the workspace.
//!
//! Run via: `cargo xtask <subcommand>`
//!
//! Subcommands:
//!   `coverage`                  Gate every workspace package at a hard 100%.
//!   `coverage --package <pkg>`  Gate just one package (the CI per-package fan-out).
//!   `version <X.Y.Z>`           Move the workspace version and roll the changelog.
//!   `version --check`           Verify the version declarations agree (CI runs this).
//!   `docs`                      Check `docs/content/` for dead links and bad frontmatter.
//!   `structure`                 Hold every source file to the production-line limit.
//!   `structure --list`          Print every file's production-line count, longest first.
//!   `prices`                    Refresh the vendor list prices from OpenRouter and LiteLLM.
//!   `prices --check`            Print the diff and fail if the price table would change.

mod coverage;
mod docs;
mod prices;
mod structure;
mod version;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dispatch(&args)
}

/// Route the CLI arguments (after the binary name) to the appropriate handler.
///
/// Extracted from `main` so it can be unit-tested without spawning processes.
pub fn dispatch(args: &[String]) -> Result<()> {
    dispatch_with(
        args,
        coverage::run,
        version::run,
        docs::run,
        structure::run,
        prices::run,
    )
}

/// Route the CLI arguments to the provided handler closures.
///
/// The handlers replace the real ones in unit tests, making every match arm
/// reachable without invoking external tooling.
pub fn dispatch_with(
    args: &[String],
    run_cov: impl FnOnce(coverage::CoverageMode) -> Result<()>,
    run_ver: impl FnOnce(version::VersionMode) -> Result<()>,
    run_docs: impl FnOnce(docs::DocsMode) -> Result<()>,
    run_struct: impl FnOnce(structure::StructureMode) -> Result<()>,
    run_prices: impl FnOnce(prices::PricesMode) -> Result<()>,
) -> Result<()> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");
    match subcommand {
        "coverage" => {
            let mode = coverage::CoverageMode::parse(&args[1..])?;
            run_cov(mode)
        }
        "version" => {
            let mode = version::VersionMode::parse(&args[1..])?;
            run_ver(mode)
        }
        "docs" => {
            let mode = docs::DocsMode::parse(&args[1..])?;
            run_docs(mode)
        }
        "structure" => {
            let mode = structure::StructureMode::parse(&args[1..])?;
            run_struct(mode)
        }
        "prices" => {
            let mode = prices::PricesMode::parse(&args[1..])?;
            run_prices(mode)
        }
        "help" | "--help" | "-h" => {
            println!("Usage: cargo xtask <subcommand>");
            println!();
            println!("Subcommands:");
            println!("  coverage                  Gate every workspace package at 100%%");
            println!("  coverage --package <pkg>  Gate one package (CI per-package fan-out)");
            println!("  version <X.Y.Z>           Move the workspace version, roll the changelog");
            println!("  version --check           Verify the version declarations agree");
            println!("  structure                 Hold every file to the production-line limit");
            println!("  structure --list          Print every file's production-line count");
            println!("  docs                      Check docs/content for dead links + frontmatter");
            println!("  prices                    Refresh the vendor list prices (network)");
            println!("  prices --check            Fail if the price table would change (network)");
            Ok(())
        }
        other => anyhow::bail!("Unknown subcommand: '{other}'. Run `cargo xtask help` for usage."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coverage::CoverageMode;
    use version::VersionMode;

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

    /// A `run_ver` stub matching `impl FnOnce(VersionMode) -> Result<()>`.
    fn ver_ok(_mode: VersionMode) -> Result<()> {
        Ok(())
    }

    /// A `run_struct` stub matching `impl FnOnce(StructureMode) -> Result<()>`.
    fn struct_ok(_m: structure::StructureMode) -> anyhow::Result<()> {
        Ok(())
    }

    fn docs_ok(_mode: docs::DocsMode) -> Result<()> {
        Ok(())
    }

    /// A `run_prices` stub matching `impl FnOnce(PricesMode) -> Result<()>`.
    fn prices_ok(_mode: prices::PricesMode) -> Result<()> {
        Ok(())
    }

    /// Covers the stub body so tests that pass it without calling it still get
    /// this single coverage hit.
    #[test]
    fn stub_returns_ok() {
        assert!(cov_ok(CoverageMode::All).is_ok());
        assert!(ver_ok(VersionMode::Check).is_ok());
        assert!(docs_ok(docs::DocsMode::Check).is_ok());
        assert!(struct_ok(structure::StructureMode::Check).is_ok());
        assert!(prices_ok(prices::PricesMode::Check).is_ok());
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
            ver_ok,
            docs_ok,
            struct_ok,
            prices_ok,
        )
        .unwrap();
        assert_eq!(got, Some(CoverageMode::All));
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
            ver_ok,
            docs_ok,
            struct_ok,
            prices_ok,
        )
        .unwrap();
        assert_eq!(got, Some(CoverageMode::Package("leviath-core".to_owned())));
    }

    #[test]
    fn dispatch_with_coverage_bad_arg_returns_err_without_calling_run_cov() {
        let result = dispatch_with(
            &args(&["coverage", "--bogus"]),
            cov_ok, // never called; covered by stub_returns_ok
            ver_ok,
            docs_ok,
            struct_ok,
            prices_ok,
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
            ver_ok,
            docs_ok,
            struct_ok,
            prices_ok,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("simulated coverage failure")
        );
    }

    // ── docs arm: mode parsing + dispatch ────────────────────────────────────

    #[test]
    fn dispatch_with_docs_parses_check() {
        let mut got = None;
        dispatch_with(
            &args(&["docs"]),
            cov_ok,
            ver_ok,
            |mode| {
                got = Some(mode);
                Ok(())
            },
            struct_ok,
            prices_ok,
        )
        .unwrap();
        assert_eq!(got, Some(docs::DocsMode::Check));
    }

    #[test]
    fn dispatch_with_docs_bad_arg_returns_err_without_calling_run_docs() {
        let result = dispatch_with(
            &args(&["docs", "--fix"]),
            cov_ok,
            ver_ok,
            docs_ok,
            struct_ok,
            prices_ok,
        );
        assert!(
            result.is_err(),
            "an unknown docs flag must error: {result:?}"
        );
    }

    // ── prices arm: mode parsing + dispatch ─────────────────────────────────

    #[test]
    fn dispatch_with_prices_parses_check() {
        let mut got = None;
        dispatch_with(
            &args(&["prices", "--check"]),
            cov_ok,
            ver_ok,
            docs_ok,
            struct_ok,
            |mode| {
                got = Some(mode);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(got, Some(prices::PricesMode::Check));
    }

    #[test]
    fn dispatch_with_prices_bad_arg_returns_err_without_calling_run_prices() {
        let result = dispatch_with(
            &args(&["prices", "--write"]),
            cov_ok,
            ver_ok,
            docs_ok,
            struct_ok,
            prices_ok,
        );
        assert!(
            result.is_err(),
            "an unknown prices flag must error: {result:?}"
        );
    }

    // ── version arm: mode parsing + dispatch ──────────────────────────────────

    #[test]
    fn dispatch_with_version_parses_the_target_version() {
        let mut got = None;
        dispatch_with(
            &args(&["version", "1.2.3"]),
            cov_ok,
            |mode| {
                got = Some(mode);
                Ok(())
            },
            docs_ok,
            struct_ok,
            prices_ok,
        )
        .unwrap();
        assert_eq!(got, Some(VersionMode::Set("1.2.3".to_owned())));
    }

    #[test]
    fn dispatch_with_version_check_parses_check() {
        let mut got = None;
        dispatch_with(
            &args(&["version", "--check"]),
            cov_ok,
            |mode| {
                got = Some(mode);
                Ok(())
            },
            docs_ok,
            struct_ok,
            prices_ok,
        )
        .unwrap();
        assert_eq!(got, Some(VersionMode::Check));
    }

    #[test]
    fn dispatch_with_version_bad_arg_returns_err_without_calling_run_ver() {
        let result = dispatch_with(
            &args(&["version", "not-a-version"]),
            cov_ok,
            ver_ok,
            docs_ok,
            struct_ok,
            prices_ok,
        );
        assert!(
            result.is_err(),
            "a malformed version must error: {result:?}"
        );
    }

    #[test]
    fn dispatch_with_version_propagates_error() {
        let result = dispatch_with(
            &args(&["version", "1.2.3"]),
            cov_ok,
            |_mode| anyhow::bail!("simulated version failure"),
            docs_ok,
            struct_ok,
            prices_ok,
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("simulated version failure")
        );
    }
}
