//! Leviath CLI - `lev` command-line interface.

use clap::Parser;
use leviath_cli::dispatch::{dispatch, Commands};
use tracing::info;

/// Leviath CLI - Agent framework with structured context windows
#[derive(Parser)]
#[command(name = "lev")]
#[command(about = "Leviath agent framework CLI", long_about = None)]
#[command(version)]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

/// COVERAGE-CONFIRMED-ARTIFACT: the `info!("Leviath CLI v{}", ...)` call
/// below genuinely executes on every real invocation of this binary
/// (confirmed via HTML: `tests/cli_dispatch.rs`'s spawns of the real `lev`
/// binary give it a nonzero hit count), but llvm-cov's tracing-macro
/// message-literal region-counting quirk (the same one documented on
/// `config.rs`'s `log_permissive_perms_warning`) still reports that
/// literal's region as a miss. Unlike that lib-crate example, no
/// `#[cfg(not(test))]`/`#[cfg(test)]` twin can isolate it here: this
/// binary is only ever compiled one way -- `cli_dispatch.rs` exercises it
/// by spawning the real, normally-built `lev` binary as a subprocess, which
/// never has `cfg(test)` active (that only applies to code compiled by
/// `cargo test` itself, not to a separately-built binary artifact it
/// spawns) -- so there is no test-only compilation path to swap the real
/// call out from under.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt().with_env_filter(level).init();

    info!("Leviath CLI v{}", env!("CARGO_PKG_VERSION"));

    dispatch(cli.command).await
}
