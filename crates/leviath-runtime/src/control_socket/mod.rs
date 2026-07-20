//! The local control transport: newline-delimited JSON
//! [`ControlRequest`]/[`ControlResponse`] frames between clients (the TUI/CLI)
//! and the world host, over a platform-native local socket.
//!
//! The wire protocol and its dispatch to the host are transport-agnostic and
//! live here; the actual socket is provided per platform so each uses its native,
//! access-controlled local IPC:
//!
//! - **Unix** → a Unix-domain socket (a filesystem path, guarded by file perms).
//! - **Windows** → a named pipe (`\\.\pipe\…`, guarded by its security
//!   descriptor).
//!
//! Each platform module exposes the same small surface — [`ControlId`],
//! [`control_id`], [`bind_control_listener`], [`ControlListener::accept`],
//! [`connect`], and [`is_daemon_running`] — over which the shared
//! [`handle_connection`] (generic over any `AsyncRead + AsyncWrite`) and
//! [`ControlClient`] operate. It is the default, always-on management channel
//! (the opt-in HTTP API that `lev serve` toggles is a separate surface).

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::components::AgentStatus;
use crate::host::{ControlOp, SpawnArgs};
use leviath_core::interaction::{InteractionRequest, InteractionResponse};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    ClientStream, ControlId, ControlListener, ServerStream, bind_control_listener, connect,
    control_id, control_id_from_str, is_daemon_running,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    ClientStream, ControlId, ControlListener, ServerStream, bind_control_listener, connect,
    control_id, control_id_from_str, is_daemon_running,
};

/// A control request over the wire. Agents are addressed by run id (the stable
/// id), except `Message`, which targets an agent id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Spawn a new agent.
    Spawn {
        /// The spawn request.
        args: SpawnArgs,
    },
    /// Query a run's status.
    Status {
        /// The run to query.
        run_id: String,
    },
    /// Pause a run.
    Pause {
        /// The run to pause.
        run_id: String,
    },
    /// Resume a paused run.
    Resume {
        /// The run to resume.
        run_id: String,
    },
    /// Cancel a run.
    Cancel {
        /// The run to cancel.
        run_id: String,
    },
    /// List every known live run and its status.
    List,
    /// Deliver a message to a running agent.
    Message {
        /// Target agent id.
        agent_id: String,
        /// Message body.
        content: String,
        /// Optional target region.
        #[serde(default)]
        target_region: Option<String>,
    },
    /// List open interactions awaiting an answer.
    ListInteractions,
    /// Answer an open interaction.
    AnswerInteraction {
        /// The answer (its `request_id` selects the interaction).
        response: InteractionResponse,
    },
    /// Cancel an open interaction.
    CancelInteraction {
        /// The interaction id to cancel.
        request_id: String,
    },
}

/// A control response over the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ControlResponse {
    /// A new agent was spawned; carries its run id.
    Spawned {
        /// The new run's id.
        run_id: String,
    },
    /// A run's status (or `None` if there is no such run).
    Status {
        /// The status, if the run exists.
        status: Option<AgentStatus>,
    },
    /// A boolean outcome (pause/resume/cancel/message).
    Ok {
        /// Whether the operation applied.
        ok: bool,
    },
    /// A listing of runs and their statuses.
    List {
        /// `(run_id, status)` pairs.
        runs: Vec<(String, AgentStatus)>,
    },
    /// A listing of open interactions.
    Interactions {
        /// `(agent_id, request)` pairs.
        interactions: Vec<(String, InteractionRequest)>,
    },
    /// The request could not be parsed.
    Error {
        /// A human-readable message.
        message: String,
    },
}

/// Translate a parsed request into a [`ControlOp`], forward it to the host, and
/// await the reply as a [`ControlResponse`]. A closed host channel (shutting
/// down) yields the operation's neutral result.
async fn dispatch(req: ControlRequest, op_tx: &UnboundedSender<ControlOp>) -> ControlResponse {
    match req {
        ControlRequest::Spawn { args } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Spawn { args, reply });
            match rx.await {
                Ok(Ok(run_id)) => ControlResponse::Spawned { run_id },
                Ok(Err(message)) => ControlResponse::Error { message },
                Err(_) => ControlResponse::Error {
                    message: "daemon is shutting down".to_string(),
                },
            }
        }
        ControlRequest::Status { run_id } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Status { run_id, reply });
            ControlResponse::Status {
                status: rx.await.unwrap_or(None),
            }
        }
        ControlRequest::Pause { run_id } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Pause { run_id, reply });
            ControlResponse::Ok {
                ok: rx.await.unwrap_or(false),
            }
        }
        ControlRequest::Resume { run_id } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Resume { run_id, reply });
            ControlResponse::Ok {
                ok: rx.await.unwrap_or(false),
            }
        }
        ControlRequest::Cancel { run_id } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Cancel { run_id, reply });
            ControlResponse::Ok {
                ok: rx.await.unwrap_or(false),
            }
        }
        ControlRequest::List => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::List { reply });
            ControlResponse::List {
                runs: rx.await.unwrap_or_default(),
            }
        }
        ControlRequest::Message {
            agent_id,
            content,
            target_region,
        } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Message {
                agent_id,
                content,
                target_region,
                reply,
            });
            ControlResponse::Ok {
                ok: rx.await.unwrap_or(false),
            }
        }
        ControlRequest::ListInteractions => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::ListInteractions { reply });
            ControlResponse::Interactions {
                interactions: rx.await.unwrap_or_default(),
            }
        }
        ControlRequest::AnswerInteraction { response } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::AnswerInteraction { response, reply });
            ControlResponse::Ok {
                ok: rx.await.unwrap_or(false),
            }
        }
        ControlRequest::CancelInteraction { request_id } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::CancelInteraction { request_id, reply });
            ControlResponse::Ok {
                ok: rx.await.unwrap_or(false),
            }
        }
    }
}

/// Serve one accepted connection: read newline-delimited requests, dispatch each
/// to the host via `op_tx`, and write back its response line. Returns when the
/// client hangs up or on an I/O error. A malformed request line gets an `Error`
/// response and the connection continues.
///
/// Generic over the stream so the same logic serves a Unix socket or a Windows
/// named pipe. The accept loop that produces the streams (and owns the socket's
/// lifecycle) lives with the daemon; this is the reusable per-connection half.
pub async fn handle_connection<S>(
    stream: S,
    op_tx: UnboundedSender<ControlOp>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<ControlRequest>(&line) {
            Ok(req) => dispatch(req, &op_tx).await,
            Err(e) => ControlResponse::Error {
                message: format!("invalid request: {e}"),
            },
        };
        // `ControlResponse` is a plain serde enum — serialization is infallible.
        let mut out = serde_json::to_string(&response).expect("ControlResponse serializes");
        out.push('\n');
        // A failed write means the client hung up; the next read returns EOF and
        // the loop ends cleanly, so the write error needs no separate handling.
        let _ = write_half.write_all(out.as_bytes()).await;
    }
    Ok(())
}

/// The client half of the control transport: connects to the daemon's control
/// socket (resolved from a [`ControlId`]), sends one [`ControlRequest`], and
/// reads back its [`ControlResponse`]. A fresh connection per request keeps it
/// simple and stateless.
pub struct ControlClient {
    id: ControlId,
}

impl ControlClient {
    /// A client for the control socket identified by `id`.
    pub fn new(id: impl Into<ControlId>) -> Self {
        Self { id: id.into() }
    }

    /// Send one request and await its response. Errors if the daemon can't be
    /// reached, the connection closes before a reply, or the reply doesn't parse.
    pub async fn request(&self, req: &ControlRequest) -> std::io::Result<ControlResponse> {
        let stream = connect(&self.id).await?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut line = serde_json::to_string(req).expect("ControlRequest serializes");
        line.push('\n');
        // A failed write means the peer is already gone; the read below then sees
        // EOF and returns the error, so the write needs no separate propagation.
        let _ = write_half.write_all(line.as_bytes()).await;

        let mut lines = BufReader::new(read_half).lines();
        match lines.next_line().await? {
            Some(resp_line) => serde_json::from_str(&resp_line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "control connection closed before a response",
            )),
        }
    }

    /// Spawn a new agent.
    pub async fn spawn(&self, args: SpawnArgs) -> std::io::Result<ControlResponse> {
        self.request(&ControlRequest::Spawn { args }).await
    }

    /// Query a run's status.
    pub async fn status(&self, run_id: &str) -> std::io::Result<ControlResponse> {
        self.request(&ControlRequest::Status {
            run_id: run_id.to_string(),
        })
        .await
    }

    /// List every known live run.
    pub async fn list(&self) -> std::io::Result<ControlResponse> {
        self.request(&ControlRequest::List).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// A fake host: drains ControlOps and replies with scripted values.
    fn spawn_fake_host(mut rx: mpsc::UnboundedReceiver<ControlOp>) {
        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                match op {
                    ControlOp::Spawn { args, reply } => {
                        // A sentinel run id makes the fake host fail the spawn.
                        let result = if args.run_id == "FAIL" {
                            Err("bad blueprint".to_string())
                        } else {
                            Ok(args.run_id)
                        };
                        let _ = reply.send(result);
                    }
                    ControlOp::Status { reply, .. } => {
                        let _ = reply.send(Some(AgentStatus::Active));
                    }
                    ControlOp::Pause { reply, .. }
                    | ControlOp::Resume { reply, .. }
                    | ControlOp::Cancel { reply, .. } => {
                        let _ = reply.send(true);
                    }
                    ControlOp::Message { reply, .. }
                    | ControlOp::AnswerInteraction { reply, .. }
                    | ControlOp::CancelInteraction { reply, .. } => {
                        let _ = reply.send(true);
                    }
                    ControlOp::List { reply } => {
                        let _ = reply.send(vec![("run-a".to_string(), AgentStatus::Active)]);
                    }
                    ControlOp::ListInteractions { reply } => {
                        let _ = reply.send(vec![]);
                    }
                }
            }
        });
    }

    /// A bound listener at a fresh control id under a temp dir (kept alive by the
    /// returned `TempDir`), plus that id for clients to connect to.
    fn test_listener() -> (ControlListener, ControlId, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let listener = bind_control_listener(&id).unwrap();
        (listener, id, dir)
    }

    async fn round_trip(req: &ControlRequest) -> ControlResponse {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, _dir) = test_listener();
        tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let _ = handle_connection(stream, op_tx).await;
        });

        let stream = connect(&id).await.unwrap();
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut line = serde_json::to_string(req).unwrap();
        line.push('\n');
        write_half.write_all(line.as_bytes()).await.unwrap();

        let mut lines = BufReader::new(read_half).lines();
        let resp_line = lines.next_line().await.unwrap().unwrap();
        serde_json::from_str(&resp_line).unwrap()
    }

    #[tokio::test]
    async fn status_request_round_trips() {
        let resp = round_trip(&ControlRequest::Status {
            run_id: "run-a".to_string(),
        })
        .await;
        assert_eq!(
            resp,
            ControlResponse::Status {
                status: Some(AgentStatus::Active)
            }
        );
    }

    #[tokio::test]
    async fn control_ops_round_trip() {
        for req in [
            ControlRequest::Pause {
                run_id: "r".to_string(),
            },
            ControlRequest::Resume {
                run_id: "r".to_string(),
            },
            ControlRequest::Cancel {
                run_id: "r".to_string(),
            },
            ControlRequest::Message {
                agent_id: "a".to_string(),
                content: "hi".to_string(),
                target_region: None,
            },
            ControlRequest::AnswerInteraction {
                response: InteractionResponse::text("q1", "yes"),
            },
            ControlRequest::CancelInteraction {
                request_id: "q1".to_string(),
            },
        ] {
            assert_eq!(round_trip(&req).await, ControlResponse::Ok { ok: true });
        }
    }

    #[tokio::test]
    async fn spawn_request_round_trips() {
        let resp = round_trip(&ControlRequest::Spawn {
            args: SpawnArgs {
                run_id: "run-9".to_string(),
                blueprint_path: "/agents/x".to_string(),
                task: "do it".to_string(),
                model: None,
                workdir: "/w".to_string(),
                metadata: Default::default(),
            },
        })
        .await;
        assert_eq!(
            resp,
            ControlResponse::Spawned {
                run_id: "run-9".to_string()
            }
        );
    }

    #[tokio::test]
    async fn spawn_error_from_host_becomes_error_response() {
        let resp = round_trip(&ControlRequest::Spawn {
            args: SpawnArgs {
                run_id: "FAIL".to_string(),
                ..Default::default()
            },
        })
        .await;
        assert_eq!(
            std::mem::discriminant(&resp),
            std::mem::discriminant(&ControlResponse::Error {
                message: String::new()
            })
        );
    }

    #[tokio::test]
    async fn list_interactions_round_trips() {
        let resp = round_trip(&ControlRequest::ListInteractions).await;
        assert_eq!(
            resp,
            ControlResponse::Interactions {
                interactions: vec![]
            }
        );
    }

    #[tokio::test]
    async fn list_request_round_trips() {
        let resp = round_trip(&ControlRequest::List).await;
        assert_eq!(
            resp,
            ControlResponse::List {
                runs: vec![("run-a".to_string(), AgentStatus::Active)]
            }
        );
    }

    /// A connected `(client, server)` stream pair, plus the `TempDir` keeping the
    /// listener's socket alive, for driving `handle_connection` directly.
    async fn connected_pair() -> (ClientStream, ServerStream, tempfile::TempDir) {
        let (mut listener, id, dir) = test_listener();
        let (client, server) = tokio::join!(connect(&id), listener.accept());
        (client.unwrap(), server.unwrap(), dir)
    }

    #[tokio::test]
    async fn malformed_request_gets_error_and_connection_continues() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server, _dir) = connected_pair().await;
        let handle = tokio::spawn(async move { handle_connection(server, op_tx).await });

        let (read_half, mut write_half) = tokio::io::split(client);
        // A blank line (skipped) then garbage (error) then a valid request.
        write_half.write_all(b"\nnot json\n").await.unwrap();
        let mut lines = BufReader::new(read_half).lines();
        let err_line = lines.next_line().await.unwrap().unwrap();
        let resp: ControlResponse = serde_json::from_str(&err_line).unwrap();
        assert_eq!(
            std::mem::discriminant(&resp),
            std::mem::discriminant(&ControlResponse::Error {
                message: String::new()
            })
        );

        // Connection still usable.
        let mut valid = serde_json::to_string(&ControlRequest::List).unwrap();
        valid.push('\n');
        write_half.write_all(valid.as_bytes()).await.unwrap();
        let ok_line = lines.next_line().await.unwrap().unwrap();
        let ok: ControlResponse = serde_json::from_str(&ok_line).unwrap();
        assert_eq!(
            std::mem::discriminant(&ok),
            std::mem::discriminant(&ControlResponse::List { runs: vec![] })
        );

        // Close the client so the handler sees EOF and returns cleanly.
        drop(write_half);
        drop(lines);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn invalid_utf8_line_ends_connection_with_error() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server, _dir) = connected_pair().await;
        let handle = tokio::spawn(async move { handle_connection(server, op_tx).await });

        let (_read_half, mut write_half) = tokio::io::split(client);
        // Invalid UTF-8 makes the line reader return an I/O error, which
        // handle_connection propagates.
        write_half.write_all(&[0xff, 0xfe, b'\n']).await.unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn client_round_trips_status_and_list() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, _dir) = test_listener();
        tokio::spawn(async move {
            for _ in 0..3 {
                let stream = listener.accept().await.unwrap();
                let op_tx = op_tx.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, op_tx).await;
                });
            }
        });
        let client = ControlClient::new(id);

        let spawned = client
            .spawn(SpawnArgs {
                run_id: "r-c".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            spawned,
            ControlResponse::Spawned {
                run_id: "r-c".to_string()
            }
        );

        let status = client.status("run-a").await.unwrap();
        assert_eq!(
            status,
            ControlResponse::Status {
                status: Some(AgentStatus::Active)
            }
        );
        let list = client.list().await.unwrap();
        assert_eq!(
            std::mem::discriminant(&list),
            std::mem::discriminant(&ControlResponse::List { runs: vec![] })
        );
    }

    #[tokio::test]
    async fn client_errors_when_daemon_absent() {
        let dir = tempfile::tempdir().unwrap();
        // A control id under a path with no daemon bound to it.
        let id = control_id(&dir.path().join("no-daemon-here"));
        assert!(ControlClient::new(id).list().await.is_err());
    }

    /// Bind a listener and serve exactly one connection by writing `bytes`
    /// verbatim (a canned "response"), then closing.
    async fn raw_server(bytes: &'static [u8]) -> (ControlId, tempfile::TempDir) {
        let (mut listener, id, dir) = test_listener();
        tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (_r, mut w) = tokio::io::split(stream);
            let _ = w.write_all(bytes).await;
        });
        (id, dir)
    }

    #[tokio::test]
    async fn client_errors_on_unparseable_response() {
        // Valid UTF-8 but not a ControlResponse → InvalidData.
        let (id, _dir) = raw_server(b"not json\n").await;
        let err = ControlClient::new(id).list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn client_errors_on_invalid_utf8_response() {
        // Invalid UTF-8 makes the response line reader itself error.
        let (id, _dir) = raw_server(&[0xff, 0xfe, b'\n']).await;
        assert!(ControlClient::new(id).list().await.is_err());
    }

    #[tokio::test]
    async fn client_errors_on_closed_connection_without_reply() {
        // A server that accepts, drains the request, then drops without replying.
        let (mut listener, id, _dir) = test_listener();
        tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            // Drain the request line first, so dropping the stream is a clean EOF
            // rather than a connection reset from unread data.
            let (read_half, _write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await;
        });

        let err = ControlClient::new(id).list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn bind_rejects_when_daemon_already_running() {
        let (_live, id, _dir) = test_listener(); // first daemon holds the socket
        let err = bind_control_listener(&id).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn is_daemon_running_reflects_a_live_listener() {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        assert!(!is_daemon_running(&id)); // nothing bound yet
        let _live = bind_control_listener(&id).unwrap();
        assert!(is_daemon_running(&id)); // now a daemon answers
    }

    #[tokio::test]
    async fn dispatch_returns_neutral_when_host_gone() {
        // No host draining the channel; the receiver is dropped, so each op's
        // reply channel drops and dispatch falls back to the neutral value.
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        drop(op_rx);
        assert_eq!(
            dispatch(
                ControlRequest::Status {
                    run_id: "r".to_string()
                },
                &op_tx
            )
            .await,
            ControlResponse::Status { status: None }
        );
        assert_eq!(
            dispatch(
                ControlRequest::Cancel {
                    run_id: "r".to_string()
                },
                &op_tx
            )
            .await,
            ControlResponse::Ok { ok: false }
        );
        assert_eq!(
            dispatch(ControlRequest::List, &op_tx).await,
            ControlResponse::List { runs: vec![] }
        );
        assert_eq!(
            dispatch(ControlRequest::ListInteractions, &op_tx).await,
            ControlResponse::Interactions {
                interactions: vec![]
            }
        );
        assert_eq!(
            std::mem::discriminant(
                &dispatch(
                    ControlRequest::Spawn {
                        args: SpawnArgs::default()
                    },
                    &op_tx
                )
                .await
            ),
            std::mem::discriminant(&ControlResponse::Error {
                message: String::new()
            })
        );
    }
}
