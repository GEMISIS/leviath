//! Client-side helpers for talking to the shared-world daemon: building a spawn
//! request from local inputs and exchanging it over the control socket. Shared by
//! `lev run` (and reusable by other clients). The socket-path resolution + connect
//! live in the binary; these cores are unit-testable against a fake socket server.

use anyhow::bail;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::host::SpawnArgs;

use crate::commands::run::manifest::find_manifest;
use crate::runstate::new_run_id;

/// Resolve the local inputs of a spawn request: find the manifest, mint a run id
/// from the agent's directory name, and record the working directory.
pub fn resolve_spawn_args(
    path: &str,
    task: &str,
    model: Option<String>,
    workdir: &str,
) -> anyhow::Result<SpawnArgs> {
    let manifest = find_manifest(path)?;
    let agent_name = manifest
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("agent");
    Ok(SpawnArgs {
        run_id: new_run_id(agent_name),
        blueprint_path: manifest.to_string_lossy().to_string(),
        task: task.to_string(),
        model,
        workdir: workdir.to_string(),
        metadata: Default::default(),
        callback_url: None,
    })
}

/// Send a resolved spawn request to the daemon and report the outcome, printing
/// the new run id on success.
pub async fn send_spawn(client: &ControlClient, spawn_args: SpawnArgs) -> anyhow::Result<()> {
    match client.spawn(spawn_args).await {
        Ok(ControlResponse::Spawned { run_id }) => {
            println!("spawned {run_id}");
            Ok(())
        }
        Ok(ControlResponse::Error { message }) => bail!("spawn failed: {message}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::task::JoinHandle;

    fn write_manifest(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::write(
            dir.join("agent.leviath"),
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../agents/coder/agent.leviath"),
            )
            .unwrap(),
        )
        .unwrap();
        dir.join("agent.leviath")
    }

    #[test]
    fn resolve_spawn_args_finds_manifest_and_builds_request() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);

        let args = resolve_spawn_args(
            manifest.to_str().unwrap(),
            "do it",
            Some("m".to_string()),
            "/work",
        )
        .unwrap();
        assert!(args.run_id.contains("my-agent"));
        assert_eq!(args.task, "do it");
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(args.blueprint_path, manifest.to_string_lossy());
        assert_eq!(args.workdir, "/work");
    }

    #[test]
    fn resolve_spawn_args_errors_on_missing_manifest() {
        assert!(resolve_spawn_args("/no/such/agent", "t", None, "/work").is_err());
    }

    /// Bind a control listener at a fresh id under `dir` and serve one canned
    /// response, returning the id clients connect to and the server task.
    fn fake_daemon(
        dir: &std::path::Path,
        response_line: &'static str,
    ) -> (ControlId, JoinHandle<()>) {
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
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

    async fn send(response_line: &'static str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let (id, server) = fake_daemon(dir.path(), response_line);
        let result = send_spawn(&ControlClient::new(id), SpawnArgs::default()).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn send_spawn_reports_success() {
        assert!(
            send(r#"{"result":"spawned","run_id":"run-9"}"#)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn send_spawn_reports_daemon_error() {
        let err = send(r#"{"result":"error","message":"boom"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn send_spawn_reports_unexpected_response() {
        let err = send(r#"{"result":"ok","ok":true}"#).await.unwrap_err();
        assert!(err.to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn send_spawn_errors_when_daemon_absent() {
        let dir = tempfile::tempdir().unwrap();
        // A control id with no daemon bound to it.
        let id = control_id(&dir.path().join("no-daemon"));
        let err = send_spawn(&ControlClient::new(id), SpawnArgs::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}
