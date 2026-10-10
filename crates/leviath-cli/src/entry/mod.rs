//! The composition root behind the `lev` binary, public so a fork can wrap it.
//!
//! [`run`] is everything `lev` does, from parsing argv to exiting; the binary's
//! `main` is the allocator plus a call to it. A wrapper with its own `main` -
//! another name, extra setup, argv rewritten before lev sees it - calls [`run`]
//! or [`run_from`] the same way.
//!
//! This is where real terminal, stdin, socket and subprocess I/O is wired into
//! the library's tested command cores. Like `main.rs`, the directory is left
//! out of `cargo xtask coverage`, and CI asks for maintainer sign-off on any
//! change to it: what lives here is the un-unit-testable sliver, so it stays
//! wiring only.

use std::io;
use std::process::ExitCode;

use clap::Parser;
use leviath_runtime::control_socket::RESTART_GRACE;
use ratatui::Viewport;
use tracing::info;

use crate::commands;
use crate::commands::dashboard::{CrosstermEventSource, DashboardArgs};
use crate::daemon::startup_view::StartupView;
use crate::dispatch::{Commands, RiskyExecutors, apply_region_flags, dispatch};

mod daemon;
mod terminal;

/// The target on this module's log lines, so stderr and `daemon.log` show
/// `lev:` (the binary's name) and not the module path.
const LOG_TARGET: &str = "lev";

use daemon::{
    ensure_daemon_running, real_daemon, real_daemon_install, real_daemon_restart,
    real_daemon_start, real_daemon_status, real_daemon_stop, real_daemon_uninstall,
};
use terminal::{CrosstermSetup, real_dashboard, real_rage, real_setup};

/// Leviath CLI - Agent framework with structured context windows
#[derive(Parser)]
#[command(name = "lev")]
#[command(about = "Leviath agent framework CLI", long_about = None)]
#[command(version, long_version = crate::dispatch::long_version())]
// clap cannot group subcommands under headings, so `lev --help` renders the
// categorized `COMMANDS_HELP` (via `after_help` in `HELP_TEMPLATE`) instead of
// clap's flat list. The library owns both, held to the `Commands` enum by a test.
#[command(help_template = crate::dispatch::HELP_TEMPLATE)]
#[command(after_help = crate::dispatch::COMMANDS_HELP)]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

/// Run lev on this process's arguments, exactly as the `lev` binary does, and
/// hand back the code to exit with.
///
/// Everything lev has to say it prints itself: clap's help, version and usage
/// errors, and a failed command's error. The code is 0 for success, 2 for a
/// command line clap refused, and 1 for a command that failed. It never exits
/// the process, so a wrapper's own cleanup still runs.
///
/// Two things a wrapping binary has to know:
///
/// - lev starts its daemon by running its own executable as `<exe> daemon`, so
///   a wrapper must hand that argv to [`run`] (or [`run_from`]) unchanged.
/// - The allocator is the binary's choice, not this crate's. `lev` uses
///   mimalloc and calls `leviath_alloc::use_purge_at_free_unless_overridden`
///   first thing in `main` (see its `main.rs`); a wrapper that wants the same
///   memory behaviour from a long-running daemon does the same.
pub fn run() -> ExitCode {
    run_from(std::env::args())
}

/// [`run`] on the given argv instead of the process's, with the program name
/// first, so a wrapper can add, drop or rewrite arguments before lev parses
/// them.
pub fn run_from(argv: impl IntoIterator<Item = String>) -> ExitCode {
    // Pre-scan argv for dynamic `--<region>` seed flags on `run` (region names
    // are blueprint-defined, so clap can't declare them), then parse the rest
    // and fold the extracted flags back in (both steps are tested lib seams).
    let (argv, region_flags) = commands::run::extract_region_flags(argv.into_iter().collect());
    let mut cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            let hint = commands::run::show::parse_hint(&argv, e.use_stderr());
            hint.into_iter().for_each(|line| eprintln!("{line}"));
            return ExitCode::from(e.exit_code() as u8);
        }
    };
    apply_region_flags(&mut cli.command, region_flags);

    // An explicit runtime instead of `#[tokio::main]` for one number: script
    // providers execute every in-flight inference call on a blocking-pool
    // thread, and tokio's default cap of 512 silently gated
    // `[limits] max_concurrent_inferences` above that - a 1024-permit pool
    // queued at the thread layer where nothing measured or reported it. 2048
    // covers the largest supported pool; threads are spawned on demand and
    // reaped when idle, so an idle daemon pays nothing for the headroom.
    let ran = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(2048)
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(async_main(cli)));
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        // What `main` returning the error would print.
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    // Logs go to stderr, never stdout: `lev agent-client` speaks JSON-RPC on
    // stdout, and a stray log line there would corrupt the host's stream.
    crate::logging::init(cli.verbose);

    let banner = crate::dispatch::banner(&cli.command);
    banner
        .into_iter()
        .for_each(|line| info!(target: LOG_TARGET, "{line}"));
    if crate::dispatch::reaches_daemon(&cli.command) {
        let notice = crate::daemon::build::mixed_notice_here();
        notice.into_iter().for_each(|line| eprintln!("{line}"));
    }

    dispatch(cli.command, &RealExecutors).await
}

/// The real implementation of [`RiskyExecutors`]: wires the process's real
/// terminal / stdin / network / subprocess I/O into the library's tested
/// command cores.
struct RealExecutors;

/// Runs `lev deps install` shell commands through the platform shell.
struct SystemRunner;

impl commands::deps::CommandRunner for SystemRunner {
    fn run(&self, command: &str) -> Result<(), String> {
        let status = if cfg!(windows) {
            leviath_sys::child_command("cmd")
                .arg("/C")
                .arg(command)
                .status()
        } else {
            leviath_sys::child_command("sh")
                .arg("-c")
                .arg(command)
                .status()
        };
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("command exited with {s}")),
            Err(e) => Err(format!("could not run command: {e}")),
        }
    }
}

/// Asks `lev deps install` confirmations on the real terminal.
struct StdinPrompt;

impl commands::deps::Prompt for StdinPrompt {
    fn confirm(&self, message: &str) -> bool {
        use std::io::Write;
        print!("{message} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }
}

impl RiskyExecutors for RealExecutors {
    async fn run(&self, args: commands::run::RunArgs) -> anyhow::Result<()> {
        real_run(args).await
    }

    async fn ps(&self, args: commands::ps::PsArgs) -> anyhow::Result<()> {
        // Here and not in the command's core, so no test ever takes the
        // notice off a real home.
        crate::home_backup::tell_once();
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

    async fn interactions(&self, args: commands::ctl::InteractionsArgs) -> anyhow::Result<()> {
        commands::ctl::interactions(&control_client()?, &args).await
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

    async fn rage(&self, args: commands::rage::RageArgs) -> anyhow::Result<()> {
        real_rage(args).await
    }

    async fn dashboard(&self, args: DashboardArgs) -> anyhow::Result<()> {
        real_dashboard(args).await
    }

    async fn serve(&self, args: commands::serve::ServeArgs) -> anyhow::Result<()> {
        // A server is long-lived and often runs under nohup or a supervisor,
        // so it keeps its own capped log like the daemon does: one file per
        // server, named for `--name` or the port. The cap is read once here,
        // since a server has no reload path for `[observability]`.
        let log_name = commands::serve::log_name(&args);
        if let Some(path) = crate::logging::serve_log_path(&log_name)
            && crate::logging::attach_log_file(
                path.clone(),
                std::io::IsTerminal::is_terminal(&io::stderr()),
            )
        {
            let cap = crate::config::Config::load()
                .map(|config| config.observability.log_file_max_bytes)
                .unwrap_or(leviath_core::config::DEFAULT_LOG_FILE_MAX_BYTES);
            crate::logging::set_log_file_cap(cap);
            info!(target: LOG_TARGET, path = %path.display(), "leviath serve writing its log here");
        }
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
            crate::runstate::runs_dir(),
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
            Some(DaemonAction::ConvertRuns(args)) => commands::daemon::convert_runs(&args),
        }
    }

    async fn auth(&self, args: commands::auth::AuthArgs) -> anyhow::Result<()> {
        commands::auth::execute(args, commands::auth::AuthEnv::real()).await
    }

    async fn mcp(&self, args: commands::mcp::McpArgs) -> anyhow::Result<()> {
        // The command logic is the tested `mcp::execute_with`; only the real
        // paths, browser launcher, and clock are composed here. The config
        // load propagates: a config that exists but doesn't parse must fail
        // the command, not silently drop the user's credential-store choice
        // and env allowlist (a missing file still loads as defaults).
        let config = crate::config::Config::load()?;
        let store_path = leviath_mcp::AuthStore::default_path().ok_or_else(|| {
            anyhow::anyhow!("could not resolve a home directory for the MCP auth store")
        })?;
        // Resolved here, once, so a keychain that cannot be reached fails the
        // whole command.
        let store = crate::credentials::store_for(config.security.credential_store)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let env = commands::mcp::McpEnv {
            config_path: crate::config::Config::config_path(),
            grants: crate::credentials::McpGrants::new(store_path, Ok(store)),
            opener: std::sync::Arc::new(leviath_sys::open_url),
            now: leviath_core::duration::now_secs() as u64,
            tools_dir: leviath_core::tools_dir(),
            allow_env_vars: config.security.allow_env_vars,
            // A person is waiting at a terminal, so the handshake keeps the
            // deadline that is right for one.
            connect_timeout: leviath_mcp::DEFAULT_CONNECT_TIMEOUT,
        };
        commands::mcp::execute_with(args, &env).await
    }

    async fn providers(&self, args: commands::providers::ProvidersArgs) -> anyhow::Result<()> {
        let env = commands::providers::ProvidersEnv {
            bedrock_control_url: None,
            bedrock_mantle_url: None,
            config_path: crate::config::Config::config_path(),
        };
        commands::providers::execute_with(args, &env).await
    }

    async fn deps(&self, args: commands::deps::DepsArgs) -> anyhow::Result<()> {
        // The command logic is the tested `deps::execute_with`; only the real
        // machine seams - the config path, env/PATH reads, the shell and the
        // stdin prompt - are composed here.
        let env = commands::deps::DepsEnv {
            config_path: crate::config::Config::config_path(),
            agents_dir: leviath_core::paths::agents_dir(),
            probe: Box::new(crate::dependencies::SystemProbe),
            runner: std::sync::Arc::new(SystemRunner),
            prompt: Box::new(StdinPrompt),
            os: commands::deps::host_os(),
        };
        commands::deps::execute_with(args, &env)
    }

    async fn update(&self, args: commands::update::UpdateArgs) -> anyhow::Result<()> {
        real_update(args).await
    }
}

/// Real `lev update`: this machine, plus a terminal's way to run a command and
/// ask a question, wired into the tested core.
///
/// The machine half lives in `UpdateEnv::real` rather than here, because
/// `GET /api/update` needs exactly the same discovery and only differs in what
/// it is willing to do with the answer.
async fn real_update(args: commands::update::UpdateArgs) -> anyhow::Result<()> {
    // A config that will not load is not a reason to refuse the check: the
    // default is on, and `lev update` on a machine with a broken config is
    // exactly when somebody wants to know whether a newer build exists.
    let update_check = crate::config::Config::load()
        .map(|c| c.update_check)
        .unwrap_or(true);
    let env = commands::update::UpdateEnv::real_with_config(
        std::sync::Arc::new(run_upgrade),
        std::sync::Arc::new(ask_yes_no),
        update_check,
    );
    commands::update::execute_blocking(args, env, env!("CARGO_PKG_VERSION")).await
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
        let allowed = crate::config::Config::load()
            .map(|c| c.security.allowed_workdirs)
            .unwrap_or_default();
        // `--yolo` means unattended, so it takes the warn-and-proceed path even
        // on a terminal: the flag's whole meaning is "do not stop to ask".
        let interactive = std::io::IsTerminal::is_terminal(&io::stdin()) && args.yolo.is_none();
        let ok = crate::workdir_guard::check(
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
    // The daemon upgrades an installed blueprint of an earlier release as it
    // starts, so that one starts it, and waits, before it is read.
    if commands::run::installed_old_format(path) {
        ensure_daemon_running().await?;
    }
    // Read here, where the paths the user typed still mean what they meant.
    let parts = commands::run::attach::attach_all(&args.attach, &std::env::current_dir()?)?;
    let spawn_args = commands::run::request::read_run_flags(commands::run::request::RunFlags {
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
        parts,
    })?;
    // After the resolve: no `--task` opens an editor a person can sit in for
    // twenty minutes, and a run that was never going to happen (a bad path,
    // a typo'd region) should not start a daemon.
    ensure_daemon_running().await?;
    crate::daemon::client::send_spawn_batch(&control_client()?, spawn_args, args.count, args.json)
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

/// Build a control client pointed at the daemon's control socket.
///
/// For one-shot commands: an absent daemon is reported at once, with the
/// advice to start it. The build id lets the client tell a daemon that
/// restarted from one that was updated under it. A daemon still starting is waited on, showing how far along it is.
fn control_client() -> anyhow::Result<leviath_runtime::control_socket::ControlClient> {
    Ok(quiet_control_client()?.with_startup_watch(StartupView::on_stderr(true).watch()))
}

/// [`control_client`] waiting on a starting daemon in silence.
fn quiet_control_client() -> anyhow::Result<leviath_runtime::control_socket::ControlClient> {
    let id = crate::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    let dir = crate::daemon::setup::control_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control token"))?;
    Ok(
        leviath_runtime::control_socket::ControlClient::for_home(id, &dir)
            .with_build(crate::daemon::setup::CURRENT_BUILD),
    )
}

/// [`control_client`] for the front-ends that outlive a daemon: `lev serve`,
/// `lev dash`, `lev agent-client`. These wait a restart out instead of
/// failing the request that landed in it - see
/// [`RESTART_GRACE`].
fn long_lived_control_client() -> anyhow::Result<leviath_runtime::control_socket::ControlClient> {
    Ok(quiet_control_client()?.with_reconnect_grace(RESTART_GRACE))
}
