//! `lev msg` / `lev cancel` — control operations on a running agent in the
//! shared-world daemon.
//!
//! Both send a control request over the daemon socket and report the boolean
//! outcome. The request/response cores are tested here; the socket-path
//! resolution + connect live in the binary behind [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};

/// Arguments for `lev msg`.
#[derive(clap::Args, Debug, Clone)]
pub struct MsgArgs {
    /// The target agent id.
    pub agent_id: String,
    /// The message to deliver.
    pub content: String,
}

/// Arguments for `lev cancel`.
#[derive(clap::Args, Debug, Clone)]
pub struct CancelArgs {
    /// The run id to cancel.
    pub run_id: String,
}

/// Send `request` and report the boolean outcome: `ok` prints `applied_msg`, a
/// `false` outcome the `not_found_msg`. A non-`Ok` response or a connect failure
/// is an error.
async fn send_bool(
    client: &ControlClient,
    request: ControlRequest,
    applied_msg: &str,
    not_found_msg: &str,
) -> anyhow::Result<()> {
    match client.request(&request).await {
        Ok(ControlResponse::Ok { ok: true }) => {
            println!("{applied_msg}");
            Ok(())
        }
        Ok(ControlResponse::Ok { ok: false }) => bail!("{not_found_msg}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

/// `lev msg`: deliver a message to a running agent.
pub async fn send_message(client: &ControlClient, args: &MsgArgs) -> anyhow::Result<()> {
    send_bool(
        client,
        ControlRequest::Message {
            agent_id: args.agent_id.clone(),
            content: args.content.clone(),
            target_region: None,
        },
        "message delivered",
        "no agent accepted the message",
    )
    .await
}

/// `lev cancel`: cancel a run.
pub async fn cancel_run(client: &ControlClient, args: &CancelArgs) -> anyhow::Result<()> {
    send_bool(
        client,
        ControlRequest::Cancel {
            run_id: args.run_id.clone(),
        },
        "cancelled",
        "no such run",
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

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

    /// Run `op` against a fake daemon that replies `response_line`.
    async fn with_daemon<F, Fut>(response_line: &'static str, op: F) -> anyhow::Result<()>
    where
        F: FnOnce(ControlClient) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let server = fake_daemon(socket.clone(), response_line);
        let result = op(ControlClient::new(&socket)).await;
        server.await.unwrap();
        result
    }

    fn msg_args() -> MsgArgs {
        MsgArgs {
            agent_id: "a".to_string(),
            content: "hi".to_string(),
        }
    }

    #[tokio::test]
    async fn message_applied() {
        let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
            send_message(&c, &msg_args()).await
        })
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn message_not_delivered() {
        let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
            send_message(&c, &msg_args()).await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("no agent accepted"));
    }

    #[tokio::test]
    async fn cancel_applied() {
        let r = with_daemon(r#"{"result":"ok","ok":true}"#, |c| async move {
            cancel_run(
                &c,
                &CancelArgs {
                    run_id: "r".to_string(),
                },
            )
            .await
        })
        .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn cancel_unknown_run() {
        let r = with_daemon(r#"{"result":"ok","ok":false}"#, |c| async move {
            cancel_run(
                &c,
                &CancelArgs {
                    run_id: "r".to_string(),
                },
            )
            .await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("no such run"));
    }

    #[tokio::test]
    async fn unexpected_response_is_an_error() {
        let r = with_daemon(r#"{"result":"spawned","run_id":"x"}"#, |c| async move {
            cancel_run(
                &c,
                &CancelArgs {
                    run_id: "r".to_string(),
                },
            )
            .await
        })
        .await;
        assert!(r.unwrap_err().to_string().contains("unexpected"));
    }

    #[tokio::test]
    async fn not_reachable_is_an_error() {
        let client = ControlClient::new("/nonexistent/leviath-ctl.sock");
        let err = cancel_run(
            &client,
            &CancelArgs {
                run_id: "r".to_string(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }
}
