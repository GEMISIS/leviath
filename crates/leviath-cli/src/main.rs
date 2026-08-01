//! Leviath CLI - `lev` command-line interface (binary entry point).
//!
//! This binary is deliberately thin: it is the *composition root* where real
//! I/O is constructed and wired into the library's already-tested command
//! cores. `cargo xtask coverage` measures `--lib` only, never `--bin`, so the
//! genuinely un-unit-testable slivers below - taking over the real terminal
//! (`lev dash`), reading real stdin (`lev setup` interactive), and delegating
//! to the library's real command entrypoints - live here rather than behind a
//! `#[cfg(not(test))]` coverage escape hatch in library code.

use std::fs::File;
use std::io;

use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tracing::info;

use leviath_cli::commands;
use leviath_cli::commands::dashboard::{CrosstermEventSource, DashboardArgs, TerminalSetup};
use leviath_cli::dispatch::{Commands, RiskyExecutors, apply_region_flags, dispatch};

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
    // Pre-scan argv for dynamic `--<region>` seed flags on `run` (region names
    // are blueprint-defined, so clap can't declare them), then parse the rest
    // and fold the extracted flags back in (both steps are tested lib seams).
    let (argv, region_flags) =
        commands::run::extract_region_flags(std::env::args().collect::<Vec<_>>());
    let mut cli = Cli::parse_from(argv);
    apply_region_flags(&mut cli.command, region_flags);

    // Initialize tracing (fmt → stderr, plus the reloadable OTLP log-export
    // slot the daemon fills when `[observability]` asks for it). Logs go to
    // stderr, never stdout: `lev agent-client` uses stdout as its JSON-RPC
    // protocol channel, and a stray log line there would corrupt the stream a
    // host is parsing.
    leviath_cli::logging::init(cli.verbose);

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

    async fn respond(&self, args: commands::ctl::RespondArgs) -> anyhow::Result<()> {
        commands::ctl::respond(&control_client()?, &args).await
    }

    async fn setup(&self, args: commands::setup::SetupArgs) -> anyhow::Result<()> {
        real_setup(args).await
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

    async fn agent_client(
        &self,
        args: commands::agent_client::AgentClientArgs,
    ) -> anyhow::Result<()> {
        // Like `serve`, this is a client of the shared-world daemon - ensure it's
        // running, then speak the Agent Client Protocol over real stdio, routing
        // agent actions through its control socket. The protocol loop
        // (`agent_client::serve_over`) is fully unit-tested over an in-memory
        // duplex; only the real stdio + socket wiring lives here.
        ensure_daemon_running().await?;
        // The directory `lev agent-client` was launched from is the default
        // working dir for sessions whose `session/new` omits `cwd`.
        let default_cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        commands::agent_client::serve_over(
            tokio::io::BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
            control_client()?,
            args,
            leviath_cli::runstate::runs_dir(),
            default_cwd,
        )
        .await
    }

    async fn daemon(&self, args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
        use commands::daemon::DaemonAction;
        match args.action {
            None => real_daemon(args).await,
            Some(DaemonAction::Start) => real_daemon_start().await,
            Some(DaemonAction::Stop) => real_daemon_stop().await,
            Some(DaemonAction::Status) => real_daemon_status().await,
            Some(DaemonAction::Restart) => real_daemon_restart().await,
            Some(DaemonAction::Install) => real_daemon_install(),
            Some(DaemonAction::Uninstall) => real_daemon_uninstall(),
        }
    }

    async fn auth(&self, args: commands::auth::AuthArgs) -> anyhow::Result<()> {
        commands::auth::execute(args).await
    }

    async fn mcp(&self, args: commands::mcp::McpArgs) -> anyhow::Result<()> {
        // The command logic is the tested `mcp::execute_with`; only the real
        // paths, browser launcher, and clock are composed here. The config
        // load propagates: a config that exists but doesn't parse must fail
        // the command, not silently drop the user's credential-store choice
        // and env allowlist (a missing file still loads as defaults).
        let config = leviath_cli::config::Config::load()?;
        let env = commands::mcp::McpEnv {
            config_path: leviath_cli::config::Config::config_path(),
            store_path: leviath_mcp::AuthStore::default_path().ok_or_else(|| {
                anyhow::anyhow!("could not resolve a home directory for the MCP auth store")
            })?,
            opener: std::sync::Arc::new(leviath_sys::open_url),
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            tools_dir: leviath_core::tools_dir(),
            // Resolved here, once, so a keychain that cannot be reached fails
            // the command instead of silently writing a refresh token to disk.
            credential_store: leviath_cli::credentials::store_for(config.security.credential_store)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            allow_env_vars: config.security.allow_env_vars,
        };
        commands::mcp::execute_with(args, &env).await
    }
}

/// Real `lev run`: ensure the daemon is running (auto-start it detached if not),
/// then resolve the blueprint + task and spawn the agent into the shared world.
/// Wiring only - the request-building + daemon exchange (`daemon::client`) are
/// unit-tested; the cwd/home resolution, process spawn, and socket connect are
/// the un-unit-testable slivers kept here.
async fn real_run(args: commands::run::RunArgs) -> anyhow::Result<()> {
    let path = args.path.ok_or_else(|| {
        anyhow::anyhow!("a blueprint name or path is required (e.g. `lev run coder -t \"task\"`)")
    })?;
    let task = args
        .task
        .ok_or_else(|| anyhow::anyhow!("a task is required (e.g. `-t \"do the thing\"`)"))?;

    ensure_daemon_running().await?;
    let workdir = commands::run::effective_workdir(args.workdir, std::env::current_dir()?)?;
    let spawn_args = leviath_cli::daemon::client::resolve_spawn_args(
        &path,
        &task,
        args.model,
        &workdir,
        args.yolo,
        args.allow,
        args.max_depth,
        args.regions,
        args.no_seed_commands,
    )?;
    leviath_cli::daemon::client::send_spawn(&control_client()?, spawn_args).await
}

/// Ensure a daemon is listening on the control port, auto-starting a detached
/// `lev daemon` process if none is. Best-effort with a bounded wait for the
/// port to become reachable. The reachability check is the tested
/// [`leviath_runtime::control_socket::is_daemon_running`]; only the real
/// subprocess spawn + poll live here.
async fn ensure_daemon_running() -> anyhow::Result<()> {
    use leviath_cli::daemon::setup::{
        CURRENT_BUILD, control_address, daemon_build_is_stale, read_build_marker,
    };
    use leviath_runtime::control_socket::is_daemon_running;
    let id = control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    if is_daemon_running(&id) {
        if !daemon_build_is_stale(read_build_marker().as_deref()) {
            return Ok(()); // already running the current build
        }
        // A daemon from an older build is running. It cannot pick up new code, so
        // restart it cleanly - it reloads its persisted agents on startup, so
        // in-flight runs survive the swap.
        eprintln!("leviath daemon is on an older build; restarting to load {CURRENT_BUILD}…");
        // Shut down quietly (straight over the control socket) rather than via
        // `daemon::send_shutdown`, whose stdout "daemon shutting down" line would
        // corrupt `lev agent-client`'s JSON-RPC protocol channel.
        let _ = control_client()?
            .request(&leviath_runtime::control_socket::ControlRequest::Shutdown)
            .await;
        for _ in 0..100 {
            if !is_daemon_running(&id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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
    // Ask politely first. If the control channel refuses or is wedged, fall back
    // to signalling the recorded process - otherwise a daemon that cannot be
    // talked to cannot be stopped either, and `lev daemon restart` (which stops
    // before it starts) could never recover.
    if let Err(e) = commands::daemon::send_shutdown(&control_client()?).await {
        let dir = leviath_cli::daemon::setup::control_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the daemon pid"))?;
        match leviath_runtime::control_socket::ControlToken::read_pid(&dir) {
            Some(pid) => {
                eprintln!("control channel did not answer ({e}); signalling pid {pid}");
                let _ = leviath_sys::kill_process_group(pid);
            }
            None => return Err(e),
        }
    }
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
    // Supervision is best-effort information: on a platform with no supported
    // supervisor there is simply nothing to report.
    if let Ok(unit) = resolve_service_unit() {
        println!(
            "{}",
            commands::daemon_service::format_supervision(unit.path.exists(), &unit.path)
        );
    }
    Ok(())
}

/// `lev daemon restart`: stop the running daemon (if any), then start a fresh one -
/// which reloads persisted agents on startup.
async fn real_daemon_restart() -> anyhow::Result<()> {
    real_daemon_stop().await?;
    real_daemon_start().await
}

/// Resolve the platform's service definition for this installation. Wiring only -
/// the rendering, paths, and command lines are the tested
/// `commands::daemon_service` core; this supplies the real exe path, home
/// directory, and uid.
fn resolve_service_unit() -> anyhow::Result<commands::daemon_service::ServiceUnit> {
    let user_home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the service file"))?;
    let leviath_home = leviath_cli::config::leviath_home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a leviath home directory"))?
        .join(".leviath");
    let exe = std::env::current_exe()?;
    commands::daemon_service::service_unit(
        &exe,
        &leviath_home,
        &commands::daemon_service::config_home(&user_home)?,
        leviath_sys::current_uid(),
    )
}

/// Run a supervisor command (`launchctl` / `systemctl`), reporting its stderr on
/// failure. The real subprocess spawn - the argv it runs is built and tested in
/// `commands::daemon_service`.
fn run_supervisor(cmd: &(String, Vec<String>)) -> anyhow::Result<()> {
    let out = std::process::Command::new(&cmd.0).args(&cmd.1).output()?;
    if out.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "`{} {}` failed: {}",
        cmd.0,
        cmd.1.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// `lev daemon install`: write the platform service file and hand it to the
/// supervisor, so the daemon starts at login and is restarted if it ever dies.
fn real_daemon_install() -> anyhow::Result<()> {
    let unit = resolve_service_unit()?;
    let path = commands::daemon_service::install(&unit)?;
    println!("wrote {}", path.display());
    // Re-registering a live service is an error on both platforms; drop any
    // previous registration first so `install` is idempotent.
    let _ = run_supervisor(&unit.deactivate);
    remove_legacy_services();
    run_supervisor(&unit.activate)?;
    println!("the leviath daemon is now supervised and will restart automatically");
    Ok(())
}

/// `lev daemon uninstall`: deregister from the supervisor and remove the file.
fn real_daemon_uninstall() -> anyhow::Result<()> {
    let unit = resolve_service_unit()?;
    // Deregistration fails when nothing is registered; that's the desired end
    // state either way, so only the file removal is reported.
    let _ = run_supervisor(&unit.deactivate);
    remove_legacy_services();
    if commands::daemon_service::uninstall(&unit)? {
        println!("removed {}", unit.path.display());
    } else {
        println!("no leviath service was installed");
    }
    Ok(())
}

/// Deregister and delete any service registration left under a previous
/// label (`daemon_service::LEGACY_SERVICE_LABELS`), so a rename never leaves
/// a second supervised daemon behind. Best-effort by design: on a machine
/// that never had the old label, every step is a no-op.
#[cfg(target_os = "macos")]
fn remove_legacy_services() {
    let Some(user_home) = dirs::home_dir() else {
        return;
    };
    let Ok(config_home) = commands::daemon_service::config_home(&user_home) else {
        return;
    };
    for (path, bootout) in
        commands::daemon_service::legacy_cleanup(&config_home, leviath_sys::current_uid())
    {
        let _ = run_supervisor(&bootout);
        if std::fs::remove_file(&path).is_ok() {
            println!("removed legacy service file {}", path.display());
        }
    }
}

/// Only macOS ever shipped under a different label; elsewhere there is
/// nothing legacy to clean up.
#[cfg(not(target_os = "macos"))]
fn remove_legacy_services() {}

/// Real `lev daemon`: bind the platform control socket and drive the shared world
/// until Ctrl-C. Wiring only - the world, host, tool service, and spawner it
/// composes (`daemon::setup`) plus the control transport (`control_socket`:
/// bind/accept/handle) are all unit-tested. Only the real accept loop + signal
/// I/O are the un-unit-testable slivers kept here in the (coverage-unmeasured)
/// binary.
async fn real_daemon(args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
    use leviath_cli::daemon::setup::{control_address, setup_daemon_host};
    use leviath_runtime::control_socket::{
        bind_control_listener, control_id_from_str, handle_connection,
    };

    // Refuse to start on a config that exists but doesn't parse. The old
    // `unwrap_or_default()` silently ran the daemon on defaults - every
    // configured section (permissions, limits, observability, providers)
    // ignored with nothing in the log. A missing file still loads as
    // defaults; only a broken one is fatal, and the parse error lands in
    // `daemon.log` for whoever finds the daemon not running.
    let config = leviath_cli::config::Config::load()
        .map_err(|e| anyhow::anyhow!("daemon refusing to start on a broken config: {e}"))?;
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
    // A fresh token per daemon: whoever cannot read our own directory cannot
    // drive the control channel. This is what authenticates callers on Windows,
    // where there is no kernel peer check to fall back on.
    let control_dir = leviath_cli::daemon::setup::control_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control token"))?;
    let token = leviath_runtime::control_socket::ControlToken::create(&control_dir)?;
    // Recorded so `lev daemon stop` can fall back to signalling us if the
    // control channel ever stops answering.
    let _ = leviath_runtime::control_socket::ControlToken::write_pid(&control_dir);
    // Record the build we started from so a later CLI can detect stale code and
    // restart us (must happen right after we win the single-instance bind).
    leviath_cli::daemon::setup::write_build_marker();
    let mut host = setup_daemon_host(config, runs_dir, tokio::runtime::Handle::current()).await;

    // Accept connections and feed control ops to the host; `Subscribe`
    // connections stream world events from the host's event sender.
    let (op_tx, op_rx) = tokio::sync::mpsc::unbounded_channel();
    let events = host.event_sender();
    tokio::spawn(async move {
        // `Ok(None)` means someone connected but is not this user: the listener
        // has already closed that connection and logged it. Skip and keep
        // serving rather than treating it as a fatal accept error.
        while let Ok(accepted) = listener.accept().await {
            let Some(stream) = accepted else { continue };
            let op_tx = op_tx.clone();
            let events = events.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, op_tx, events, Some(token)).await;
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
    let dir = leviath_cli::daemon::setup::control_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control token"))?;
    Ok(leviath_runtime::control_socket::ControlClient::for_home(
        id, &dir,
    ))
}

/// Real `lev dash`: supplies the real crossterm terminal backend and event
/// source to the library's fully-tested `dashboard::execute_with`. Wiring
/// only - the loop, rendering, input handling, and engine setup it composes
/// are all exercised under `cargo test`.
async fn real_dashboard(_args: DashboardArgs) -> anyhow::Result<()> {
    // The dashboard is a client of the shared-world daemon: ensure it's running,
    // then observe/control it over the control socket.
    ensure_daemon_running().await?;
    let control = control_client()?;
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
    };
    let mut events = CrosstermEventSource::new();
    commands::dashboard::execute_with(control, &mut setup, &mut events, real_yank).await
}

/// Real clipboard copy for the dashboard's `y` keypress: try a native tool,
/// then fall back to writing the OSC52 escape sequence to the real controlling
/// terminal / stdout. The native-tool + fallback branch logic is unit-tested in
/// `yank_to_clipboard_via`; `leviath_sys::osc52_write_via`'s branches are
/// unit-tested via injected fakes. The two real-I/O leaves it composes here -
/// opening `/dev/tty` and acquiring `stdout()` - are the un-unit-testable slivers.
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

/// Real `lev setup`: the real config paths, the real environment, a real
/// browser for the "open the signup page" key, a real TTY check, and the real
/// network-backed provider verifier - everything the library's tested
/// `execute_with` takes as a seam.
///
/// The verification task is spawned here rather than inside `execute_with`
/// because `LiveVerifier` is the one piece that opens a socket; the library
/// never instantiates it, so no unit test can reach the network through it.
async fn real_setup(args: commands::setup::SetupArgs) -> anyhow::Result<()> {
    use commands::setup::{SetupEnv, import, verification_loop};
    use leviath_cli::commands::setup::verify::{LiveVerifier, SkipVerifier};

    let home = leviath_cli::config::leviath_home_dir().unwrap_or_default();
    let env = SetupEnv {
        config_path: leviath_cli::config::Config::config_path(),
        agents_dir: commands::setup::real_agents_dir(Some(&home)),
        roots: import::Roots::new(
            home,
            dirs::config_dir().unwrap_or_default(),
            std::env::current_dir().unwrap_or_default(),
        ),
        env_lookup: Box::new(|name| std::env::var(name).ok()),
        opener: std::sync::Arc::new(leviath_sys::open_url),
    };
    if args.non_interactive {
        return commands::setup::run_non_interactive(&args, &env);
    }
    if !std::io::IsTerminal::is_terminal(&io::stdout()) {
        return commands::setup::execute_with(
            &args,
            &env,
            &mut CrosstermSetup {
                viewport: Viewport::Fullscreen,
            },
            &mut CrosstermEventSource::new(),
            false,
        )
        .await;
    }

    let mut wizard = commands::setup::build_wizard(&env);
    if let Some((requests, replies)) = wizard.take_verify_ends() {
        if args.no_verify {
            tokio::spawn(verification_loop(SkipVerifier, requests, replies));
        } else {
            tokio::spawn(verification_loop(LiveVerifier, requests, replies));
        }
    }
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
    };
    let mut events = CrosstermEventSource::new();
    commands::setup::execute_core(&mut wizard, &env, &mut setup, &mut events).await
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
        // Mouse capture is what delivers wheel events, and it routes click-drag
        // to the dashboard's own text selection (drag to highlight, release to
        // copy). Hold Shift (or Option on macOS Terminal) to bypass capture and
        // use the terminal's native selection instead.
        io::stdout()
            .execute(EnableMouseCapture)
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
        // Released before leaving the alternate screen, and unconditionally: a
        // terminal left in mouse-reporting mode emits escape sequences into the
        // user's shell on every click afterwards.
        io::stdout().execute(DisableMouseCapture).ok();
        disable_raw_mode().ok();
        io::stdout().execute(LeaveAlternateScreen).ok();
    }

    fn print_done(&self) {
        println!("Dashboard closed.");
    }
}
