//! `lev daemon` — run and manage the shared-world daemon.
//!
//! With no action, `lev daemon` runs the daemon in the foreground: it binds the
//! control socket, drives the one shared world, and (on restart) reloads any
//! agents persisted under the runs directory. That execution — binding a real
//! socket, spawning a detached process, polling for readiness — is real I/O
//! routed through [`crate::dispatch::RiskyExecutors`] and implemented by the
//! binary (`main.rs`). This module defines the arguments plus the testable
//! request/formatting cores the binary composes.

use anyhow::bail;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};

/// Arguments for `lev daemon`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct DaemonArgs {
    /// Lifecycle action. Omitted, `lev daemon` runs the daemon in the foreground.
    #[command(subcommand)]
    pub action: Option<DaemonAction>,
    /// Override the control socket / pipe (default: `<leviath-home>/.leviath`).
    #[arg(long, global = true)]
    pub socket: Option<String>,
}

/// Lifecycle actions for the shared-world daemon.
#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum DaemonAction {
    /// Start the daemon in the background (a no-op if one is already running).
    Start,
    /// Shut the running daemon down.
    Stop,
    /// Report whether the daemon is running and how many agents it hosts.
    Status,
    /// Restart the daemon (stop, then start) — reloading persisted agents.
    Restart,
    /// Register the daemon with the OS supervisor (launchd / systemd --user) so
    /// it starts at login and is restarted automatically if it ever dies.
    Install,
    /// Deregister the daemon from the OS supervisor.
    Uninstall,
}

/// Ask the daemon to shut down and report the outcome.
pub async fn send_shutdown(client: &ControlClient) -> anyhow::Result<()> {
    match client.shutdown().await {
        Ok(ControlResponse::Ok { ok: true }) => {
            println!("daemon shutting down");
            Ok(())
        }
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); is it running?"),
    }
}

/// The `lev daemon status` line for a `running` daemon hosting `run_count` agents.
pub fn format_status(running: bool, run_count: usize) -> String {
    if !running {
        return "daemon not running".to_string();
    }
    let plural = if run_count == 1 { "" } else { "s" };
    format!("daemon running ({run_count} agent{plural})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::task::JoinHandle;

    #[test]
    fn format_status_covers_running_singular_plural_and_stopped() {
        assert_eq!(format_status(false, 0), "daemon not running");
        assert_eq!(format_status(true, 1), "daemon running (1 agent)");
        assert_eq!(format_status(true, 3), "daemon running (3 agents)");
    }

    /// Bind a control listener at a fresh id under `dir` and serve one canned
    /// response, returning the id clients connect to and the server task.
    fn fake_daemon(
        dir: &std::path::Path,
        response_line: &'static str,
    ) -> (leviath_runtime::control_socket::ControlId, JoinHandle<()>) {
        let id = leviath_runtime::control_socket::control_id(dir);
        let mut listener = leviath_runtime::control_socket::bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _request = lines.next_line().await.unwrap();
            write_half
                .write_all(response_line.as_bytes())
                .await
                .unwrap();
            write_half.write_all(b"\n").await.unwrap();
        });
        (id, handle)
    }

    async fn shutdown(response_line: &'static str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon(dir.path(), response_line);
        let result = send_shutdown(&ControlClient::new(id)).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn send_shutdown_reports_success() {
        assert!(shutdown(r#"{"result":"ok","ok":true}"#).await.is_ok());
    }

    #[tokio::test]
    async fn send_shutdown_rejects_unexpected_response() {
        let err = shutdown(r#"{"result":"spawned","run_id":"x"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn send_shutdown_errors_when_daemon_absent() {
        let dir = tempfile::tempdir().unwrap();
        let id = leviath_runtime::control_socket::control_id(&dir.path().join("no-daemon"));
        let err = send_shutdown(&ControlClient::new(id)).await.unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}
