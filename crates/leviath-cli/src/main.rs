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
use crossterm::ExecutableCommand;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tracing::info;

use leviath_cli::commands;
use leviath_cli::commands::dashboard::{CrosstermEventSource, DashboardArgs, TerminalSetup};
use leviath_cli::dispatch::{Commands, RiskyExecutors, dispatch};

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
        real_run(args).await
    }

    async fn ps(&self, _args: commands::ps::PsArgs) -> anyhow::Result<()> {
        commands::ps::send_list(&control_client()?).await
    }

    async fn msg(&self, args: commands::ctl::MsgArgs) -> anyhow::Result<()> {
        commands::ctl::send_message(&control_client()?, &args).await
    }

    async fn cancel(&self, args: commands::ctl::CancelArgs) -> anyhow::Result<()> {
        commands::ctl::cancel_run(&control_client()?, &args).await
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
        // The HTTP API is a gateway to the shared-world daemon: ensure it's
        // running, then serve, routing agent actions through its control socket.
        ensure_daemon_running().await?;
        commands::serve::execute(args, control_client()?).await
    }

    async fn daemon(&self, args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
        use commands::daemon::DaemonAction;
        match args.action {
            None => real_daemon(args).await,
            Some(DaemonAction::Start) => real_daemon_start().await,
            Some(DaemonAction::Stop) => real_daemon_stop().await,
            Some(DaemonAction::Status) => real_daemon_status().await,
            Some(DaemonAction::Restart) => real_daemon_restart().await,
        }
    }
}

/// Real `lev run`: ensure the daemon is running (auto-start it detached if not),
/// then resolve the blueprint + task and spawn the agent into the shared world.
/// Wiring only — the request-building + daemon exchange (`daemon::client`) are
/// unit-tested; the cwd/home resolution, process spawn, and socket connect are
/// the un-unit-testable slivers kept here.
async fn real_run(args: commands::run::RunArgs) -> anyhow::Result<()> {
    let path = args.path.ok_or_else(|| {
        anyhow::anyhow!("a blueprint path is required (e.g. `lev run agents/coder \"task\"`)")
    })?;
    let task = args
        .task
        .ok_or_else(|| anyhow::anyhow!("a task is required (e.g. `-t \"do the thing\"`)"))?;

    ensure_daemon_running().await?;
    let workdir = std::env::current_dir()?.to_string_lossy().to_string();
    let spawn_args =
        leviath_cli::daemon::client::resolve_spawn_args(&path, &task, args.model, &workdir)?;
    leviath_cli::daemon::client::send_spawn(&control_client()?, spawn_args).await
}

/// Ensure a daemon is listening on the control port, auto-starting a detached
/// `lev daemon` process if none is. Best-effort with a bounded wait for the
/// port to become reachable. The reachability check is the tested
/// [`leviath_runtime::control_socket::is_daemon_running`]; only the real
/// subprocess spawn + poll live here.
async fn ensure_daemon_running() -> anyhow::Result<()> {
    use leviath_runtime::control_socket::is_daemon_running;
    let id = leviath_cli::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    if is_daemon_running(&id) {
        return Ok(()); // already running
    }
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    leviath_sys::process::configure_detached(&mut cmd);
    cmd.spawn()?;
    for _ in 0..100 {
        if is_daemon_running(&id) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("the leviath daemon did not start within 5s");
}

/// `lev daemon start`: auto-start a detached daemon if none is running.
async fn real_daemon_start() -> anyhow::Result<()> {
    ensure_daemon_running().await?;
    println!("leviath daemon is running");
    Ok(())
}

/// `lev daemon stop`: ask the running daemon to shut down, then wait for it to
/// exit. The request-building is the tested `daemon::send_shutdown`; the
/// readiness poll over the real socket is the untestable sliver.
async fn real_daemon_stop() -> anyhow::Result<()> {
    use leviath_runtime::control_socket::is_daemon_running;
    let id = leviath_cli::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    if !is_daemon_running(&id) {
        println!("daemon not running");
        return Ok(());
    }
    commands::daemon::send_shutdown(&control_client()?).await?;
    for _ in 0..100 {
        if !is_daemon_running(&id) {
            println!("daemon stopped");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("the leviath daemon did not shut down within 5s");
}

/// `lev daemon status`: report whether the daemon is running and its agent count.
async fn real_daemon_status() -> anyhow::Result<()> {
    use leviath_runtime::control_socket::{ControlResponse, is_daemon_running};
    let id = leviath_cli::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    let running = is_daemon_running(&id);
    let count = if running {
        match control_client()?.list().await {
            Ok(ControlResponse::List { runs }) => runs.len(),
            _ => 0,
        }
    } else {
        0
    };
    println!("{}", commands::daemon::format_status(running, count));
    Ok(())
}

/// `lev daemon restart`: stop the running daemon (if any), then start a fresh one
/// — which reloads persisted agents on startup.
async fn real_daemon_restart() -> anyhow::Result<()> {
    real_daemon_stop().await?;
    real_daemon_start().await
}

/// Real `lev daemon`: bind the platform control socket and drive the shared world
/// until Ctrl-C. Wiring only — the world, host, tool service, and spawner it
/// composes (`daemon::setup`) plus the control transport (`control_socket`:
/// bind/accept/handle) are all unit-tested. Only the real accept loop + signal
/// I/O are the un-unit-testable slivers kept here in the (coverage-unmeasured)
/// binary.
async fn real_daemon(args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
    use leviath_cli::daemon::setup::{control_address, setup_daemon_host};
    use leviath_runtime::control_socket::{
        bind_control_listener, control_id_from_str, handle_connection,
    };

    let config = leviath_cli::config::Config::load().unwrap_or_default();
    let runs_dir = leviath_cli::runstate::runs_dir();
    let id = match args.socket {
        Some(ref s) => control_id_from_str(s),
        None => control_address().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve a home directory for the control socket")
        })?,
    };

    // `bind_control_listener` enforces the single-instance guarantee and is fully
    // unit-tested; only driving its `accept` in a loop is the untestable sliver.
    let mut listener = bind_control_listener(&id)?;
    let mut host = setup_daemon_host(config, runs_dir, tokio::runtime::Handle::current()).await;

    // Accept connections and feed control ops to the host.
    let (op_tx, op_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            let op_tx = op_tx.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, op_tx).await;
            });
        }
    });

    // Ctrl-C shuts the world down cleanly.
    let shutdown = host.world_mut().shutdown_handle();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.notify_one();
    });

    info!("leviath daemon listening");
    println!("leviath daemon listening");
    host.serve(op_rx).await;
    Ok(())
}

/// Build a control client pointed at the daemon's control socket.
fn control_client() -> anyhow::Result<leviath_runtime::control_socket::ControlClient> {
    let id = leviath_cli::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    Ok(leviath_runtime::control_socket::ControlClient::new(id))
}

/// Real `lev dash`: supplies the real crossterm terminal backend and event
/// source to the library's fully-tested `dashboard::execute_with`. Wiring
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
