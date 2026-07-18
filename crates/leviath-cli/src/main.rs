//! Leviath CLI - `lev` command-line interface (binary entry point).
//!
//! This binary is deliberately thin: it is the *composition root* where real
//! I/O is constructed and wired into the library's already-tested command
//! cores. `cargo xtask coverage` measures `--lib` only, never `--bin`, so the
//! genuinely un-unit-testable slivers below — taking over the real terminal
//! (`lev dash`), reading real stdin (`lev setup` interactive), and delegating
//! to the library's real command entrypoints — live here rather than behind a
//! `#[cfg(not(test))]` coverage escape hatch in library code.

use std::io;

use clap::Parser;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tracing::info;

use leviath_cli::commands;
use leviath_cli::commands::dashboard::{CrosstermEventSource, DashboardArgs, TerminalSetup};
use leviath_cli::config::Config;
use leviath_cli::dispatch::{dispatch, Commands, RiskyExecutors};

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt().with_env_filter(level).init();

    info!("Leviath CLI v{}", env!("CARGO_PKG_VERSION"));

    dispatch(cli.command, &RealExecutors).await
}

/// The real implementation of [`RiskyExecutors`]: wires the process's real
/// terminal / stdin / network / subprocess I/O into the library's tested
/// command cores. Never compiled into the coverage-measured `--lib` build.
struct RealExecutors;

impl RiskyExecutors for RealExecutors {
    async fn run(&self, args: commands::run::RunArgs) -> anyhow::Result<()> {
        commands::run::execute(args).await
    }

    async fn setup(&self, args: commands::setup::SetupArgs) -> anyhow::Result<()> {
        real_setup(args)
    }

    async fn dashboard(&self, args: DashboardArgs) -> anyhow::Result<()> {
        real_dashboard(args).await
    }

    async fn serve(&self, args: commands::serve::ServeArgs) -> anyhow::Result<()> {
        commands::serve::execute(args).await
    }

    async fn worker(&self, args: commands::run::WorkerArgs) -> anyhow::Result<()> {
        commands::run::execute_worker(args).await
    }
}

/// Real `lev setup`: the branch on `--non-interactive` plus the interactive
/// path's real `stdin().lock()`. The two cores it calls
/// (`run_non_interactive_setup` / `run_interactive_setup`) are unit-tested.
fn real_setup(args: commands::setup::SetupArgs) -> anyhow::Result<()> {
    let mut config = Config::load().unwrap_or_default();
    let save_path = Config::config_path();

    if args.non_interactive {
        return commands::setup::run_non_interactive_setup(&mut config, &args, &save_path);
    }

    let stdin = io::stdin();
    commands::setup::run_interactive_setup(&mut config, &mut stdin.lock(), &save_path)
}

/// Real `lev dash`: supplies the real crossterm terminal backend and event
/// source to the library's fully-tested [`dashboard::execute_with`]. Wiring
/// only — the loop, rendering, input handling, and engine setup it composes
/// are all exercised under `cargo test`.
async fn real_dashboard(_args: DashboardArgs) -> anyhow::Result<()> {
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
    };
    let mut events = CrosstermEventSource::new();
    commands::dashboard::execute_with(&mut setup, &mut events).await
}

/// Real [`TerminalSetup`]: enables raw mode, enters/leaves the real alternate
/// screen, and builds a real `CrosstermBackend` on `stdout`. Lives in the
/// binary because it can only be exercised against a real terminal.
struct CrosstermSetup {
    viewport: Viewport,
}

impl TerminalSetup for CrosstermSetup {
    type B = ratatui::backend::CrosstermBackend<io::Stdout>;

    fn enable(&mut self) -> anyhow::Result<()> {
        enable_raw_mode().map_err(anyhow::Error::from)?;
        io::stdout()
            .execute(EnterAlternateScreen)
            .map_err(anyhow::Error::from)?;
        Ok(())
    }

    fn create_terminal(&mut self) -> anyhow::Result<Terminal<Self::B>> {
        let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: self.viewport.clone(),
            },
        )
        .map_err(anyhow::Error::from)
    }

    fn disable(&mut self) {
        disable_raw_mode().ok();
        io::stdout().execute(LeaveAlternateScreen).ok();
    }

    fn print_done(&self) {
        println!("Dashboard closed.");
    }
}
