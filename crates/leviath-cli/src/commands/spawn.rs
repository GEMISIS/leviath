//! `lev spawn` — spawn an agent into the running shared-world daemon.
//!
//! This is the shared-world counterpart to `lev run`: instead of launching a
//! per-run worker process, it resolves the blueprint + task locally and asks the
//! daemon (over its control socket) to create the agent in the one shared world,
//! printing the new run id.
//!
//! The pure request-building ([`resolve_spawn_args`]) and the daemon exchange
//! ([`send_spawn`]) are tested here; the thin outer wiring that reads the real
//! cwd + control-socket path lives in the binary behind
//! [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use leviath_runtime::host::SpawnArgs;

use crate::commands::run::manifest::find_manifest;
use crate::runstate::new_run_id;

/// Arguments for `lev spawn`.
#[derive(clap::Args, Debug, Clone)]
pub struct SpawnCmdArgs {
    /// Path to the agent blueprint (a manifest file or its directory).
    pub path: String,
    /// The task for the agent.
    pub task: String,
    /// Model override (`provider/model` or a bare model name).
    #[arg(long)]
    pub model: Option<String>,
}

/// Resolve the local inputs of a spawn request: find the manifest, mint a run
/// id from the agent's directory name, and record `workdir` (the caller passes
/// the resolved working directory).
pub fn resolve_spawn_args(args: &SpawnCmdArgs, workdir: &str) -> anyhow::Result<SpawnArgs> {
    let manifest = find_manifest(&args.path)?;
    let agent_name = manifest
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("agent");
    Ok(SpawnArgs {
        run_id: new_run_id(agent_name),
        blueprint_path: manifest.to_string_lossy().to_string(),
        task: args.task.clone(),
        model: args.model.clone(),
        workdir: workdir.to_string(),
        metadata: Default::default(),
    })
}

/// Send a resolved spawn request to the daemon over `client` and report the
/// outcome, printing the new run id on success.
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
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
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

    fn cmd(path: &str) -> SpawnCmdArgs {
        SpawnCmdArgs {
            path: path.to_string(),
            task: "do it".to_string(),
            model: Some("m".to_string()),
        }
    }

    #[test]
    fn resolve_spawn_args_finds_manifest_and_builds_request() {
        let dir = tempfile::tempdir().unwrap();
        // The manifest lives in a directory named after the agent.
        let agent_dir = dir.path().join("my-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest = write_manifest(&agent_dir);

        let args = resolve_spawn_args(&cmd(manifest.to_str().unwrap()), "/work").unwrap();
        assert!(args.run_id.contains("my-agent"));
        assert_eq!(args.task, "do it");
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(args.blueprint_path, manifest.to_string_lossy());
        assert_eq!(args.workdir, "/work");
    }

    #[test]
    fn resolve_spawn_args_errors_on_missing_manifest() {
        assert!(resolve_spawn_args(&cmd("/no/such/agent"), "/work").is_err());
    }

    /// Bind a fake daemon at `socket` that reads one request and writes
    /// `response_line` back, then closes. Returns its task handle.
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

    async fn send(response_line: &'static str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let server = fake_daemon(socket.clone(), response_line);
        let client = ControlClient::new(&socket);
        let result = send_spawn(&client, SpawnArgs::default()).await;
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
        let client = ControlClient::new("/nonexistent/leviath-ctl.sock");
        let err = send_spawn(&client, SpawnArgs::default()).await.unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}
