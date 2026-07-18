//! Leviath CLI - `lev` command-line interface (binary entry point).
//!
//! This binary is deliberately thin: it is the *composition root* where real
//! I/O is constructed and wired into the library's already-tested command
//! cores. `cargo xtask coverage` measures `--lib` only, never `--bin`, so the
//! genuinely un-unit-testable slivers below — taking over the real terminal
//! (`lev dash`), reading real stdin (`lev setup` interactive), and delegating
//! to the library's real command entrypoints — live here rather than behind a
//! `#[cfg(not(test))]` coverage escape hatch in library code.

use std::fs::File;
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
        commands::run::execute(args, real_foreground_io()).await
    }

    async fn setup(&self, args: commands::setup::SetupArgs) -> anyhow::Result<()> {
        // The interactive arm reads the process's real stdin; the branch +
        // config wiring is the tested `setup::execute_with`.
        commands::setup::execute_with(&args, || io::stdin().lock())
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

/// Real `lev dash`: supplies the real crossterm terminal backend and event
/// source to the library's fully-tested [`dashboard::execute_with`]. Wiring
/// only — the loop, rendering, input handling, and engine setup it composes
/// are all exercised under `cargo test`.
async fn real_dashboard(_args: DashboardArgs) -> anyhow::Result<()> {
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
    };
    let mut events = CrosstermEventSource::new();
    commands::dashboard::execute_with(&mut setup, &mut events, real_yank).await
}

/// Real clipboard copy for the dashboard's `y` keypress: try a native tool,
/// then fall back to writing the OSC52 escape sequence to the real controlling
/// terminal / stdout. The native-tool + fallback branch logic is unit-tested in
/// `yank_to_clipboard_via`; `leviath_sys::osc52_write_via`'s branches are
/// unit-tested via injected fakes. The two real-I/O leaves it composes here —
/// opening `/dev/tty` and acquiring `stdout()` — are the un-unit-testable slivers.
fn real_yank(text: &str) -> bool {
    commands::dashboard::yank_to_clipboard_via(text, |t| {
        let mut out = io::stdout();
        leviath_sys::osc52_write_via(t, open_controlling_tty, &mut out)
    })
}

/// Open the process's controlling terminal (`/dev/tty` on Unix) for writing the
/// OSC52 clipboard escape sequence. Errors on non-Unix, where `real_yank` then
/// falls back to stdout.
#[cfg(unix)]
fn open_controlling_tty() -> io::Result<File> {
    std::fs::OpenOptions::new().write(true).open("/dev/tty")
}

#[cfg(not(unix))]
fn open_controlling_tty() -> io::Result<File> {
    Err(io::Error::other("no controlling terminal on this platform"))
}

/// Real foreground I/O bundle: real stdin for interactions and the message
/// reader, and the real `is_terminal` probe. Wires the process's real stdin
/// into `run`'s fully-tested foreground cores.
fn real_foreground_io() -> commands::run::ForegroundIo {
    use std::io::IsTerminal;
    commands::run::ForegroundIo {
        ask: |req| {
            leviath_cli::interaction::request_interaction_from_reader(req, &mut io::stdin().lock())
        },
        make_message_reader: || Box::pin(tokio::io::BufReader::new(tokio::io::stdin())),
        stdin_is_terminal: || io::stdin().is_terminal(),
    }
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
