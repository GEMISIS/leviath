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
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tracing::info;

use leviath_cli::commands;
use leviath_cli::commands::dashboard::{CrosstermEventSource, DashboardArgs, TerminalSetup};
use leviath_cli::daemon::readiness::poll_until;
use leviath_cli::dispatch::{Commands, RiskyExecutors, apply_region_flags, dispatch};

/// mimalloc instead of the platform allocator. The daemon's workload is a
/// stream of large, variably-sized, short-lived allocations (assembled
/// inference requests, context snapshots, tool results) interleaved with
/// long-lived small ones; the system allocator strands the freed spans behind
/// the live objects and RSS never comes back down (measured: a 22 MB live
/// footprint under 293 MB of retained RSS after a five-agent burst). mimalloc
/// returns freed pages to the OS aggressively, so RSS tracks what the process
/// actually holds.
#[cfg(feature = "mimalloc-allocator")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Leviath CLI - Agent framework with structured context windows
#[derive(Parser)]
#[command(name = "lev")]
#[command(about = "Leviath agent framework CLI", long_about = None)]
#[command(version)]
// clap cannot group subcommands under headings, so `lev --help` renders the
// categorized `COMMANDS_HELP` (via `after_help` in `HELP_TEMPLATE`) instead of
// clap's flat list. The library owns both, held to the `Commands` enum by a test.
#[command(help_template = leviath_cli::dispatch::HELP_TEMPLATE)]
#[command(after_help = leviath_cli::dispatch::COMMANDS_HELP)]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

fn main() -> anyhow::Result<()> {
    // Purge freed memory at free time (leviath-alloc has the full why): an
    // idle daemon otherwise parks its burst memory as unflagged freed pages
    // the OS keeps charging to it. Applied in-binary so every lev process
    // behaves the same however it was started; a user-exported
    // MIMALLOC_PURGE_DELAY always wins.
    #[cfg(feature = "mimalloc-allocator")]
    leviath_alloc::use_purge_at_free_unless_overridden();

    // An explicit runtime instead of `#[tokio::main]` for one number: script
    // providers execute every in-flight inference call on a blocking-pool
    // thread, and tokio's default cap of 512 silently gated
    // `[limits] max_concurrent_inferences` above that - a 1024-permit pool
    // queued at the thread layer where nothing measured or reported it. 2048
    // covers the largest supported pool; threads are spawned on demand and
    // reaped when idle, so an idle daemon pays nothing for the headroom.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(2048)
        .build()?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
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

    async fn ps(&self, args: commands::ps::PsArgs) -> anyhow::Result<()> {
        commands::ps::send_list(&control_client()?, &args).await
    }

    async fn msg(&self, args: commands::ctl::MsgArgs) -> anyhow::Result<()> {
        commands::ctl::send_message(&control_client()?, &args).await
    }

    async fn cancel(&self, args: commands::ctl::CancelArgs) -> anyhow::Result<()> {
        commands::ctl::cancel_run(&control_client()?, &args).await
    }

    async fn pause(&self, args: commands::ctl::PauseArgs) -> anyhow::Result<()> {
        commands::ctl::pause_run(&control_client()?, &args).await
    }

    async fn resume(&self, args: commands::ctl::ResumeArgs) -> anyhow::Result<()> {
        commands::ctl::resume_run(&control_client()?, &args).await
    }

    async fn respond(&self, args: commands::ctl::RespondArgs) -> anyhow::Result<()> {
        commands::ctl::respond(&control_client()?, &args).await
    }

    async fn doctor(&self, args: commands::doctor::DoctorArgs) -> anyhow::Result<()> {
        real_doctor(args).await
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
        commands::serve::execute(
            args,
            long_lived_control_client()?,
            std::sync::Arc::new(run_upgrade_captured),
        )
        .await
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
            long_lived_control_client()?,
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
        commands::auth::execute(args, commands::auth::AuthEnv::real()).await
    }

    async fn mcp(&self, args: commands::mcp::McpArgs) -> anyhow::Result<()> {
        // `lev mcp serve` is the other direction entirely: Leviath as the MCP
        // server a host agent launches. The protocol loop is the tested
        // `mcp::serve::serve_over`; only real stdio, the control client and the
        // daemon auto-start are composed here. The daemon is started lazily by
        // the first tool call that needs it (hosts launch servers eagerly),
        // and a failed start is retried on the next call rather than cached.
        let args = match args.route() {
            commands::mcp::McpRoute::Serve(serve) => {
                // Claude Code says which project it is in; other hosts launch
                // from wherever they run (Claude Desktop from `$HOME`).
                let default_cwd = std::env::var("CLAUDE_PROJECT_DIR")
                    .ok()
                    .filter(|p| std::path::Path::new(p).is_absolute())
                    .or_else(|| {
                        std::env::current_dir()
                            .ok()
                            .map(|p| p.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default();
                let env = commands::mcp::serve::McpServeEnv {
                    runs_dir: leviath_cli::runstate::runs_dir(),
                    default_cwd,
                    tools_dir: leviath_core::tools_dir(),
                    agents_dir: leviath_core::agents_dir(),
                    home: dirs::home_dir(),
                    allowed_workdirs: leviath_cli::config::Config::load()
                        .map(|c| c.security.allowed_workdirs)
                        .unwrap_or_default(),
                    daemon_ready: std::sync::Arc::new(|| {
                        Box::pin(async { ensure_daemon_running().await.map_err(|e| e.to_string()) })
                    }),
                };
                return commands::mcp::serve::serve_over(
                    tokio::io::BufReader::new(tokio::io::stdin()),
                    tokio::io::stdout(),
                    long_lived_control_client()?,
                    serve,
                    env,
                )
                .await;
            }
            commands::mcp::McpRoute::Manage(args) => args,
        };
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
            // A person is waiting at a terminal, so the handshake keeps the
            // deadline that is right for one.
            connect_timeout: leviath_mcp::DEFAULT_CONNECT_TIMEOUT,
        };
        commands::mcp::execute_with(args, &env).await
    }

    async fn providers(&self, args: commands::providers::ProvidersArgs) -> anyhow::Result<()> {
        let env = commands::providers::ProvidersEnv {
            config_path: leviath_cli::config::Config::config_path(),
        };
        commands::providers::execute_with(args, &env).await
    }

    async fn update(&self, args: commands::update::UpdateArgs) -> anyhow::Result<()> {
        real_update(args)
    }
}

/// Real `lev update`: this machine, plus a terminal's way to run a command and
/// ask a question, wired into the tested core.
///
/// The machine half lives in `UpdateEnv::real` rather than here, because
/// `GET /api/update` needs exactly the same discovery and only differs in what
/// it is willing to do with the answer.
fn real_update(args: commands::update::UpdateArgs) -> anyhow::Result<()> {
    // A config that will not load is not a reason to refuse the check: the
    // default is on, and `lev update` on a machine with a broken config is
    // exactly when somebody wants to know whether a newer build exists.
    let update_check = leviath_cli::config::Config::load()
        .map(|c| c.update_check)
        .unwrap_or(true);
    let env = commands::update::UpdateEnv::real_with_config(
        std::sync::Arc::new(run_upgrade),
        std::sync::Arc::new(ask_yes_no),
        update_check,
    );
    commands::update::execute_with(&args, &env, env!("CARGO_PKG_VERSION"))
}

/// Run the upgrade command, letting it draw on the terminal it inherits - a
/// package manager's progress output is most of what makes the wait bearable.
fn run_upgrade(argv: &[String]) -> anyhow::Result<()> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("no upgrade command to run"))?;
    let status = leviath_sys::child_command(program)
        .args(rest)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run `{program}`: {e}"))?;
    match status.success() {
        true => Ok(()),
        false => anyhow::bail!("`{}` exited with {status}", argv.join(" ")),
    }
}

/// Run an upgrade command for `POST /api/update`.
///
/// The opposite of [`run_upgrade`] in all three ways that matter: there is no
/// terminal for a package manager to draw on, no stdin for it to block the
/// server on waiting for an answer nobody will type, and the console that
/// pressed the button only ever sees what this function puts in the error - so
/// the output is captured and [`commands::update::captured_outcome`], which is
/// where the judgement lives, folds it in.
fn run_upgrade_captured(argv: &[String]) -> anyhow::Result<()> {
    let Some((program, rest)) = argv.split_first() else {
        anyhow::bail!("no upgrade command to run");
    };
    let output = leviath_sys::child_command(program)
        .args(rest)
        .stdin(std::process::Stdio::null())
        .output();
    commands::update::captured_outcome(argv, output)
}

/// Ask a yes/no question on the real terminal.
///
/// Without a terminal the answer is no, never a hang: `lev update` in CI is
/// `--yes` plus whatever that flag deliberately does not cover, and a prompt
/// waiting forever on a closed stdin would be the worst of both.
fn ask_yes_no(question: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !io::stdin().is_terminal() {
        println!("  {question} [no terminal to ask on, so: no]");
        return false;
    }
    print!("  {question} [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    match io::stdin().read_line(&mut answer) {
        Ok(_) => matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

/// Real `lev run`: ensure the daemon is running (auto-start it detached if not),
/// then resolve the blueprint + task and spawn the agent into the shared world.
/// Wiring only - the request-building + daemon exchange (`daemon::client`) are
/// unit-tested; the cwd/home resolution, process spawn, and socket connect are
/// the un-unit-testable slivers kept here.
async fn real_run(args: commands::run::RunArgs) -> anyhow::Result<()> {
    // No PATH means the current directory, which is what `find_manifest`'s
    // directory branch already handles and what the docs have always promised.
    let path = args.path.as_deref().unwrap_or(".");
    let workdir = commands::run::effective_workdir(args.workdir, std::env::current_dir()?)?;
    // Confirm a workdir an agent probably should not be pointed at. Before
    // resolving the task, so a cancelled run has not opened an editor first;
    // `--yolo` and any non-terminal caller proceed with a warning rather than
    // being refused.
    {
        let allowed = leviath_cli::config::Config::load()
            .map(|c| c.security.allowed_workdirs)
            .unwrap_or_default();
        // `--yolo` means unattended, so it takes the warn-and-proceed path even
        // on a terminal: the flag's whole meaning is "do not stop to ask".
        let interactive = std::io::IsTerminal::is_terminal(&io::stdin()) && !args.yolo;
        let ok = leviath_cli::workdir_guard::check(
            std::path::Path::new(&workdir),
            dirs::home_dir().as_deref(),
            &allowed,
            interactive,
            &mut CrosstermSetup {
                viewport: Viewport::Fullscreen,
                mouse_capture: false,
                enabled: false,
            },
            &mut CrosstermEventSource::open(),
        )
        .await;
        if !ok {
            println!("cancelled");
            return Ok(());
        }
    }
    let spawn_args = leviath_cli::daemon::client::resolve_spawn_args(
        leviath_cli::daemon::client::LaunchRequest {
            path,
            task: args.task.as_deref(),
            stdin_is_terminal: &|| std::io::IsTerminal::is_terminal(&io::stdin()),
            model: args.model,
            workdir: &workdir,
            yolo: args.yolo,
            allow: args.allow,
            max_depth: args.max_depth,
            regions: args.regions,
            no_seed_commands: args.no_seed_commands,
            output_request: commands::run::output_request(
                args.output_format,
                args.output_instructions,
                args.output_schema,
            )?,
        },
    )?;
    // Deliberately after the resolve, not before. No `--task` opens an editor,
    // and a user can sit in vim for twenty minutes: checking daemon liveness
    // and build staleness first would mean spawning against a socket last
    // verified a third of an hour ago. It also stops a run that was never going
    // to happen (a bad path, a typo'd region) from auto-starting a daemon.
    leviath_cli::daemon::client::refuse_wait_with_count(args.count, args.wait)?;
    ensure_daemon_running().await?;
    if args.wait {
        return leviath_cli::daemon::client::spawn_and_wait(
            &long_lived_control_client()?,
            spawn_args,
            args.json,
            &leviath_cli::runstate::runs_dir(),
        )
        .await;
    }
    leviath_cli::daemon::client::send_spawn_batch(
        &control_client()?,
        spawn_args,
        args.count,
        args.json,
    )
    .await
}

/// `lev doctor`: run the wiring checks, starting the daemon first so the fourth
/// one has something to hand off to.
///
/// A daemon that will not start is reported *as* the fourth check failing, not
/// propagated: the whole point of the command is to say whether the credentials
/// or the daemon is at fault, and aborting here would answer neither.
/// `--no-daemon` skips both the auto-start and the check, so a caller who only
/// wants to test credentials never causes a daemon to exist; `--offline`
/// stops earlier still and skips it for the same reason. The checks
/// themselves are the tested `commands::doctor::run_checks`; the auto-start and
/// socket connect are the un-unit-testable slivers kept here.
async fn real_doctor(args: commands::doctor::DoctorArgs) -> anyhow::Result<()> {
    use commands::doctor::DaemonTarget;
    if args.no_daemon || args.offline {
        return commands::doctor::execute(args, DaemonTarget::Skip).await;
    }
    let started = ensure_daemon_running()
        .await
        .and_then(|()| control_client());
    match started {
        Ok(client) => commands::doctor::execute(args, DaemonTarget::Client(&client)).await,
        Err(e) => commands::doctor::execute(args, DaemonTarget::Unavailable(e.to_string())).await,
    }
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
    let running = is_daemon_running(&id);
    let steps = leviath_cli::daemon::lifecycle::start_steps(
        running,
        running && daemon_build_is_stale(read_build_marker().as_deref()),
    );
    if !steps.spawn {
        return Ok(());
    }
    if steps.shutdown_first {
        eprintln!("leviath daemon is on an older build; restarting to load {CURRENT_BUILD}…");
        // Shut down quietly (straight over the control socket) rather than via
        // `daemon::send_shutdown`, whose stdout "daemon shutting down" line would
        // corrupt `lev agent-client`'s JSON-RPC protocol channel.
        let _ = control_client()?
            .request(&leviath_runtime::control_socket::ControlRequest::Shutdown)
            .await;
        poll_until(&mut || !is_daemon_running(&id)).await;
    }
    let exe = std::env::current_exe()?;
    // The daemon is a background process: started from Explorer or a
    // service it has no console to share, so a raw spawn would open one.
    let mut cmd = leviath_sys::child_command(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    leviath_sys::process::configure_detached(&mut cmd);
    cmd.spawn()?;
    if poll_until(&mut || is_daemon_running(&id)).await {
        return Ok(());
    }
    anyhow::bail!(
        "the leviath daemon did not start within {:?}",
        leviath_cli::daemon::readiness::READY_TIMEOUT
    );
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
    use leviath_cli::daemon::lifecycle::{StopFallback, stop_fallback, stop_outcome};
    if !is_daemon_running(&id) {
        println!("{}", stop_outcome(false, false).unwrap_or_default());
        return Ok(());
    }
    // Ask politely first; the fallback for a refusal is `stop_fallback`'s call.
    if let Err(e) = commands::daemon::send_shutdown(&control_client()?).await {
        let dir = leviath_cli::daemon::setup::control_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the daemon pid"))?;
        match stop_fallback(leviath_runtime::control_socket::ControlToken::read_pid(
            &dir,
        )) {
            StopFallback::Signal(pid) => {
                eprintln!("control channel did not answer ({e}); signalling pid {pid}");
                let _ = leviath_sys::kill_process_group(pid);
            }
            StopFallback::Propagate => return Err(e),
        }
    }
    match stop_outcome(true, poll_until(&mut || !is_daemon_running(&id)).await) {
        Ok(line) => {
            println!("{line}");
            Ok(())
        }
        Err(e) => anyhow::bail!(e),
    }
}

/// `lev daemon status`: report whether the daemon is running and its agent count.
async fn real_daemon_status() -> anyhow::Result<()> {
    use leviath_runtime::control_socket::{ControlResponse, is_daemon_running};
    let id = leviath_cli::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    let running = is_daemon_running(&id);
    let count = if running {
        match control_client()?.list().await {
            Ok(ControlResponse::List { runs, .. }) => runs.len(),
            _ => 0,
        }
    } else {
        0
    };
    // Supervision is best-effort information: on a platform with no supported
    // supervisor there is simply nothing to report.
    let supervision = resolve_service_unit()
        .ok()
        .map(|unit| commands::daemon_service::format_supervision(unit.path.exists(), &unit.path));
    for line in leviath_cli::daemon::lifecycle::status_lines(running, count, supervision) {
        println!("{line}");
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
    let out = leviath_sys::child_command(&cmd.0).args(&cmd.1).output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(commands::daemon_service::supervisor_failure(
        cmd,
        &out.stderr,
    ))
}

/// `lev daemon install`: write the platform service file and hand it to the
/// supervisor, so the daemon starts at login and is restarted if it ever dies.
fn real_daemon_install() -> anyhow::Result<()> {
    let unit = resolve_service_unit()?;
    let lines = commands::daemon_service::install_with(
        &unit,
        &mut run_supervisor,
        &mut remove_legacy_services,
    )?;
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// `lev daemon uninstall`: deregister from the supervisor and remove the file.
fn real_daemon_uninstall() -> anyhow::Result<()> {
    let unit = resolve_service_unit()?;
    let lines = commands::daemon_service::uninstall_with(
        &unit,
        &mut run_supervisor,
        &mut remove_legacy_services,
    )?;
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

/// Deregister and delete any service registration left under a previous
/// label (`daemon_service::LEGACY_SERVICE_LABELS`), so a rename never leaves
/// a second supervised daemon behind. Best-effort by design: on a machine
/// that never had the old label, every step is a no-op.
#[cfg(target_os = "macos")]
fn remove_legacy_services() -> Vec<std::path::PathBuf> {
    commands::daemon_service::remove_legacy_with(
        dirs::home_dir(),
        leviath_sys::current_uid(),
        &mut |bootout| {
            let _ = run_supervisor(bootout);
        },
        &mut |path| std::fs::remove_file(path).is_ok(),
    )
}

/// Only macOS ever shipped under a different label; elsewhere there is
/// nothing legacy to clean up.
#[cfg(not(target_os = "macos"))]
fn remove_legacy_services() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// Real `lev daemon`: bind the platform control socket and drive the shared world
/// until Ctrl-C. Wiring only - the world, host, tool service, and spawner it
/// composes (`daemon::setup`) plus the control transport (`control_socket`:
/// bind/accept/handle) are all unit-tested. Only the real accept loop + signal
/// I/O are the un-unit-testable slivers kept here in the (coverage-unmeasured)
/// binary.
async fn real_daemon(args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
    use leviath_cli::daemon::setup::{control_address, setup_daemon_host};
    use leviath_runtime::control_socket::{
        DaemonIdentity, bind_control_listener, control_id_from_str, handle_connection_as,
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
    // Fallible because building a provider's outbound HTTPS client can fail -
    // in practice when the machine's root certificate store cannot be read. The
    // daemon refuses to start rather than accepting runs it could never infer
    // for, and the error names the cause instead of a panic backtrace.
    let mut host = setup_daemon_host(config, runs_dir, tokio::runtime::Handle::current()).await?;

    // Accept connections and feed control ops to the host; `Subscribe`
    // connections stream world events from the host's event sender.
    let (op_tx, op_rx) = tokio::sync::mpsc::unbounded_channel();
    let events = host.event_sender();
    // Who this daemon is, told to every client that asks in its handshake. A
    // long-lived client (`lev serve`, `lev dash`, the ACP bridge) compares it
    // against its own build to tell a restart from an update.
    // It also reports which tool credentials this process can see, because that
    // is a fact only this process holds: a client asking its own environment is
    // answering for a different one.
    let identity = DaemonIdentity::this_process(leviath_cli::daemon::setup::CURRENT_BUILD)
        .with_tool_env(leviath_cli::daemon::setup::visible_tool_env());
    tokio::spawn(async move {
        // `Ok(None)` means someone connected but is not this user: the listener
        // has already closed that connection and logged it. Skip and keep
        // serving rather than treating it as a fatal accept error.
        while let Ok(accepted) = listener.accept().await {
            let Some(stream) = accepted else { continue };
            let op_tx = op_tx.clone();
            let events = events.clone();
            let token = token.clone();
            let identity = identity.clone();
            tokio::spawn(async move {
                let _ = handle_connection_as(stream, op_tx, events, Some(token), identity).await;
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
///
/// For one-shot commands: an absent daemon is reported at once, with the
/// advice to start it. The build id lets the client tell a daemon that
/// restarted from one that was updated under it.
fn control_client() -> anyhow::Result<leviath_runtime::control_socket::ControlClient> {
    let id = leviath_cli::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    let dir = leviath_cli::daemon::setup::control_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control token"))?;
    Ok(
        leviath_runtime::control_socket::ControlClient::for_home(id, &dir)
            .with_build(leviath_cli::daemon::setup::CURRENT_BUILD),
    )
}

/// [`control_client`] for the front-ends that outlive a daemon: `lev serve`,
/// `lev dash`, `lev agent-client`. These wait a restart out instead of
/// failing the request that landed in it - see
/// [`RESTART_GRACE`](leviath_runtime::control_socket::RESTART_GRACE).
fn long_lived_control_client() -> anyhow::Result<leviath_runtime::control_socket::ControlClient> {
    Ok(control_client()?.with_reconnect_grace(leviath_runtime::control_socket::RESTART_GRACE))
}

/// Real `lev dash`: supplies the real crossterm terminal backend and event
/// source to the library's fully-tested `dashboard::execute_with`. Wiring
/// only - the loop, rendering, input handling, and engine setup it composes
/// are all exercised under `cargo test`.
async fn real_dashboard(_args: DashboardArgs) -> anyhow::Result<()> {
    // The dashboard is a client of the shared-world daemon: ensure it's running,
    // then observe/control it over the control socket.
    ensure_daemon_running().await?;
    let control = long_lived_control_client()?;
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
        mouse_capture: true,
        enabled: false,
    };
    let mut events = CrosstermEventSource::open();
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
    use leviath_cli::commands::setup::signin::{LiveAuthorizer, signin_loop};
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
        // Shared with the dashboard, so an import somebody has already turned
        // down is not proposed again the next time they run setup.
        ui_state_path: leviath_cli::ui_state::default_path(),
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
                mouse_capture: false,
                enabled: false,
            },
            &mut CrosstermEventSource::open(),
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
    if let Some((requests, events)) = wizard.take_signin_ends() {
        let authorizer = LiveAuthorizer::real(env.opener.clone(), &env.config_path);
        tokio::spawn(signin_loop(authorizer, requests, events));
    }
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
        mouse_capture: true,
        enabled: false,
    };
    let mut events = CrosstermEventSource::open();
    commands::setup::execute_core(&mut wizard, &env, &mut setup, &mut events).await
}

/// Real [`TerminalSetup`]: enables raw mode, enters/leaves the real alternate
/// screen, and builds a real `CrosstermBackend` on `stdout`. Lives in the
/// binary because it can only be exercised against a real terminal.
///
/// `mouse_capture` is per-surface: the dashboard wants wheel events and its
/// own click-drag selection, and the setup wizard wants clicks on its rows and
/// buttons. It stays off for the workdir prompt, which is one question with
/// two answers and nothing to aim at.
struct CrosstermSetup {
    viewport: Viewport,
    mouse_capture: bool,
    /// True between a successful `enable()` and the matching `disable()`, so
    /// teardown (explicit, `Drop`, or the panic hook) runs exactly once.
    enabled: bool,
}

/// Restore the terminal from raw mode / alternate screen / mouse capture.
/// Safe to call redundantly: every step is a no-op when already released.
fn restore_terminal() {
    io::stdout().execute(PopKeyboardEnhancementFlags).ok();
    io::stdout().execute(DisableMouseCapture).ok();
    disable_raw_mode().ok();
    io::stdout().execute(LeaveAlternateScreen).ok();
}

/// Whether a `CrosstermSetup` currently holds the terminal; read by the panic
/// hook so a panic after clean teardown doesn't re-issue restore sequences
/// into a healthy shell.
static TERMINAL_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install (once) a chained panic hook that restores the terminal *before*
/// the default hook prints the panic message - otherwise a panic mid-loop
/// leaves the shell in raw mode with the message drawn into the vanished
/// alternate screen.
fn install_terminal_restore_panic_hook() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TERMINAL_HELD.load(std::sync::atomic::Ordering::SeqCst) {
                restore_terminal();
                // The screen is back, so parked lines can be shown, and the
                // panic message that follows is not competing with the frame.
                // Clearing the flag also stops a loop still running on another
                // thread from re-parking output nobody will ever flush.
                TERMINAL_HELD.store(false, std::sync::atomic::Ordering::SeqCst);
                leviath_cli::logging::release_from_tui();
            }
            previous(info);
        }));
    });
}

impl TerminalSetup for CrosstermSetup {
    type B = ratatui::backend::CrosstermBackend<io::Stdout>;

    fn run_editor(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        leviath_sys::editor::launch(path)
    }

    fn enable(&mut self) -> anyhow::Result<()> {
        install_terminal_restore_panic_hook();
        enable_raw_mode().map_err(anyhow::Error::from)?;
        // The kitty keyboard protocol is what lets Ctrl+Enter (start the run
        // from the new-run task) arrive as anything other than Enter. Asked
        // for only where the terminal says it can, and popped again in
        // `restore_terminal`.
        if matches!(
            crossterm::terminal::supports_keyboard_enhancement(),
            Ok(true)
        ) {
            io::stdout()
                .execute(PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
                ))
                .map_err(anyhow::Error::from)?;
        }
        io::stdout()
            .execute(EnterAlternateScreen)
            .map_err(anyhow::Error::from)?;
        if self.mouse_capture {
            // Mouse capture is what delivers wheel events, and it routes
            // click-drag to the dashboard's own text selection (drag to
            // highlight, release to copy). Hold Shift (or Option on macOS
            // Terminal) to bypass capture and use the terminal's native
            // selection instead.
            io::stdout()
                .execute(EnableMouseCapture)
                .map_err(anyhow::Error::from)?;
        }
        self.enabled = true;
        TERMINAL_HELD.store(true, std::sync::atomic::Ordering::SeqCst);
        // stderr is this same terminal, so a log line from here on would land
        // inside the frame - and in raw mode, without a carriage return,
        // staircase across it. Park them until the screen is ours again.
        leviath_cli::logging::hold_for_tui();
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
        if !self.enabled {
            return;
        }
        self.enabled = false;
        TERMINAL_HELD.store(false, std::sync::atomic::Ordering::SeqCst);
        // Flushes whatever was logged while the screen was held, so `-v`
        // diagnostics are waiting in the scrollback rather than lost.
        leviath_cli::logging::release_from_tui();
        // Mouse release runs before leaving the alternate screen, and
        // unconditionally (even when capture was never enabled - it's a
        // no-op then): a terminal left in mouse-reporting mode emits escape
        // sequences into the user's shell on every click afterwards.
        restore_terminal();
    }

    fn print_done(&self) {
        println!("Dashboard closed.");
    }
}

/// Covers early-return paths (`?` between `enable()` and the loop's own
/// teardown): the terminal is restored on unwind, and the `enabled` guard
/// makes the common explicit-`disable()`-then-drop sequence a single restore.
impl Drop for CrosstermSetup {
    fn drop(&mut self) {
        self.disable();
    }
}
