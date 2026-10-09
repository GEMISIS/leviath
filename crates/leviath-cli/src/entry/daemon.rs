//! Starting, stopping, supervising and being the daemon: the real
//! subprocess, socket and service-manager calls behind `lev daemon`.

use std::io;

use tracing::info;

use super::{LOG_TARGET, control_client};
use crate::commands;
use crate::daemon::readiness::poll_until;
use crate::daemon::startup_view::StartupView;

/// Ensure a daemon is listening on the control port, auto-starting a detached
/// `lev daemon` process if none is. Best-effort with a bounded wait for the
/// port to become reachable. The reachability check is the tested
/// [`leviath_runtime::control_socket::is_daemon_running`]; only the real
/// subprocess spawn + poll live here.
pub(super) async fn ensure_daemon_running() -> anyhow::Result<()> {
    use crate::daemon::{build, setup::control_address, setup::read_build_marker};
    use leviath_runtime::control_socket::is_daemon_running;
    let id = control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    let running = is_daemon_running(&id);
    // Only an older daemon is replaced; a newer one is left running.
    let decision = build::replace(read_build_marker().as_deref(), &build::Build::current());
    let steps = crate::daemon::lifecycle::start_steps(running, decision.replace);
    if !steps.spawn {
        control_client()?.wait_until_started().await?;
        return Ok(());
    }
    if steps.shutdown_first {
        decision
            .say
            .into_iter()
            .for_each(|line| eprintln!("{line}"));
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
        control_client()?.wait_until_started().await?;
        return Ok(());
    }
    anyhow::bail!(
        "the leviath daemon did not start within {:?}",
        crate::daemon::readiness::READY_TIMEOUT
    );
}

/// `lev daemon start`: auto-start a detached daemon if none is running.
pub(super) async fn real_daemon_start() -> anyhow::Result<()> {
    ensure_daemon_running().await?;
    println!("leviath daemon is running");
    Ok(())
}

/// `lev daemon stop`: ask the running daemon to shut down, then wait for it to
/// exit. The request-building is the tested `daemon::send_shutdown`; the
/// readiness poll over the real socket is the untestable sliver.
pub(super) async fn real_daemon_stop() -> anyhow::Result<()> {
    use leviath_runtime::control_socket::is_daemon_running;
    let id = crate::daemon::setup::control_address()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control socket"))?;
    use crate::daemon::lifecycle::{StopFallback, stop_fallback, stop_outcome};
    if !is_daemon_running(&id) {
        println!("{}", stop_outcome(false, false).unwrap_or_default());
        return Ok(());
    }
    // Ask politely first; the fallback for a refusal is `stop_fallback`'s call.
    if let Err(e) = commands::daemon::send_shutdown(&control_client()?).await {
        let dir = crate::daemon::setup::control_dir()
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
pub(super) async fn real_daemon_status() -> anyhow::Result<()> {
    use leviath_runtime::control_socket::{ControlResponse, is_daemon_running};
    let id = crate::daemon::setup::control_address()
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
    let build = crate::daemon::build::status_line_here(running);
    for line in crate::daemon::lifecycle::status_lines(running, count, supervision) {
        println!("{line}");
    }
    build.into_iter().for_each(|line| println!("{line}"));
    Ok(())
}

/// `lev daemon restart`: stop the running daemon (if any), then start a fresh one -
/// which reloads persisted agents on startup.
pub(super) async fn real_daemon_restart() -> anyhow::Result<()> {
    real_daemon_stop().await?;
    real_daemon_start().await
}

/// Resolve the platform's service definition for this installation. Wiring only -
/// the rendering, paths, and command lines are the tested
/// `commands::daemon_service` core; this supplies the real exe path, home
/// directory, and uid.
///
/// The home handed over is the one `LEVIATH_HOME` names, not the data root
/// under it: the daemon joins `.leviath` onto its home itself, so a unit that
/// carried the data root sent it to `~/.leviath/.leviath`, an empty directory
/// with no config in it.
pub(super) fn resolve_service_unit() -> anyhow::Result<commands::daemon_service::ServiceUnit> {
    let user_home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the service file"))?;
    let leviath_home = crate::config::leviath_home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a leviath home directory"))?;
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
pub(super) fn real_daemon_install() -> anyhow::Result<()> {
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
pub(super) fn real_daemon_uninstall() -> anyhow::Result<()> {
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
pub(super) async fn real_daemon(args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
    use crate::daemon::setup::{control_address, setup_daemon_host};
    use leviath_runtime::control_socket::{
        DaemonIdentity, bind_control_listener, control_id_from_str,
    };

    // A config that exists but does not parse is refused rather than run on
    // defaults (a missing one loads as defaults). The log is attached first, so
    // the refusal is the first line in `daemon.log` however the daemon started;
    // its cap follows `[observability]` once the host is up.
    if let Some(path) = crate::logging::daemon_log_path()
        && crate::logging::attach_log_file(
            path.clone(),
            std::io::IsTerminal::is_terminal(&io::stderr()),
        )
    {
        info!(target: LOG_TARGET, path = %path.display(), "leviath daemon writing its log here");
    }
    let config = crate::config::Config::load()
        .map_err(|e| anyhow::anyhow!("daemon refusing to start on a broken config: {e}"))?;
    let runs_dir = crate::runstate::runs_dir();
    let id = match args.socket {
        Some(ref s) => control_id_from_str(s),
        None => control_address().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve a home directory for the control socket")
        })?,
    };

    // Our build is recorded before a client can reach us, and again once the
    // single-instance bind is won, over any loser's.
    crate::daemon::setup::write_build_marker_unless_running(&id);
    let listener = bind_control_listener(&id)?;
    // A fresh token per daemon: whoever cannot read our own directory cannot
    // drive the control channel. This is what authenticates callers on Windows,
    // where there is no kernel peer check to fall back on.
    let control_dir = crate::daemon::setup::control_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the control token"))?;
    let token = leviath_runtime::control_socket::ControlToken::create(&control_dir)?;
    // Recorded so `lev daemon stop` can fall back to signalling us if the
    // control channel ever stops answering.
    let _ = leviath_runtime::control_socket::ControlToken::write_pid(&control_dir);
    crate::daemon::setup::write_build_marker();
    // Who this daemon is, told to every client that asks in its handshake. A
    // long-lived client (`lev serve`, `lev dash`, the ACP bridge) compares it
    // against its own build to tell a restart from an update.
    // It also reports which tool credentials this process can see, because that
    // is a fact only this process holds: a client asking its own environment is
    // answering for a different one.
    let identity = DaemonIdentity::this_process(crate::daemon::setup::CURRENT_BUILD)
        .with_tool_env(crate::daemon::setup::visible_tool_env());
    // Connections are accepted from the start: until the host is serving,
    // each request is answered with the start-up step under way, which the
    // start-up writes to `board`; after, the gate sends them to the host.
    let board = leviath_runtime::control_socket::StartupBoard::default();
    let gate = leviath_runtime::control_socket::ControlGate::new(board.clone());
    gate.accept_all(listener, token, identity);
    // A daemon in the foreground shows its own start-up as a waiting client does.
    let tty = std::io::IsTerminal::is_terminal(&io::stderr());
    let following = StartupView::on_stderr(tty).follow(board.clone());
    // Fallible because building a provider's outbound HTTPS client can fail -
    // in practice when the machine's root certificate store cannot be read. The
    // daemon refuses to start rather than accepting runs it could never infer
    // for, and the error names the cause instead of a panic backtrace.
    let mut host =
        setup_daemon_host(config, runs_dir, tokio::runtime::Handle::current(), &board).await?;

    // Control ops go to the host; a subscription streams its world events.
    let (op_tx, op_rx) = tokio::sync::mpsc::unbounded_channel();
    gate.open(op_tx, host.event_sender());
    following.finish().await;

    // Ctrl-C shuts the world down cleanly.
    let shutdown = host.world_mut().shutdown_handle();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown.notify_one();
    });

    info!(target: LOG_TARGET, "leviath daemon listening");
    println!("leviath daemon listening");
    host.serve(op_rx).await;
    Ok(())
}
