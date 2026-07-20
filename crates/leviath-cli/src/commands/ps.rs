//! `lev ps` — list the agents running in the shared-world daemon.
//!
//! Queries the daemon over its control socket and prints one line per run. The
//! query + formatting cores are tested here; the socket-path resolution + connect
//! live in the binary behind [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_runtime::components::AgentStatus;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};

/// Arguments for `lev ps` (none yet).
#[derive(clap::Args, Debug, Clone, Default)]
pub struct PsArgs {}

/// A short human label for an agent status.
fn status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Active => "active",
        AgentStatus::Waiting => "waiting",
        AgentStatus::Complete => "complete",
        AgentStatus::Error { .. } => "error",
        AgentStatus::Cancelled => "cancelled",
    }
}

/// Render a run listing as aligned `RUN  STATUS` lines (or a friendly note when
/// empty).
pub fn format_runs(runs: &[(String, AgentStatus)]) -> String {
    if runs.is_empty() {
        return "no agents running".to_string();
    }
    let width = runs.iter().map(|(id, _)| id.len()).max().unwrap_or(0);
    runs.iter()
        .map(|(id, status)| format!("{id:<width$}  {}", status_label(status)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Query the daemon for its runs and print the formatted listing.
pub async fn send_list(client: &ControlClient) -> anyhow::Result<()> {
    match client.list().await {
        Ok(ControlResponse::List { runs }) => {
            println!("{}", format_runs(&runs));
            Ok(())
        }
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

    #[test]
    fn status_labels_cover_every_variant() {
        assert_eq!(status_label(&AgentStatus::Idle), "idle");
        assert_eq!(status_label(&AgentStatus::Active), "active");
        assert_eq!(status_label(&AgentStatus::Waiting), "waiting");
        assert_eq!(status_label(&AgentStatus::Complete), "complete");
        assert_eq!(
            status_label(&AgentStatus::Error {
                message: "x".to_string()
            }),
            "error"
        );
        assert_eq!(status_label(&AgentStatus::Cancelled), "cancelled");
    }

    #[test]
    fn format_runs_aligns_and_handles_empty() {
        assert_eq!(format_runs(&[]), "no agents running");
        let runs = vec![
            ("run-a".to_string(), AgentStatus::Active),
            ("longer-run".to_string(), AgentStatus::Complete),
        ];
        let out = format_runs(&runs);
        assert!(out.contains("run-a"));
        assert!(out.contains("active"));
        assert!(out.contains("longer-run  complete"));
    }

    fn fake_daemon(socket: std::path::PathBuf, response_line: &'static str) -> JoinHandle<()> {
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let _request = lines.next_line().await.unwrap();
            write_half
                .write_all(response_line.as_bytes())
                .await
                .unwrap();
            write_half.write_all(b"\n").await.unwrap();
        })
    }

    async fn list(response_line: &'static str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let server = fake_daemon(socket.clone(), response_line);
        let result = send_list(&ControlClient::new(&socket)).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn send_list_prints_runs() {
        assert!(
            list(r#"{"result":"list","runs":[["run-a","Active"]]}"#)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn send_list_rejects_unexpected_response() {
        let err = list(r#"{"result":"ok","ok":true}"#).await.unwrap_err();
        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn send_list_errors_when_daemon_absent() {
        let err = send_list(&ControlClient::new("/nonexistent/leviath-ctl.sock"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}
