//! The local control transport: a Unix-domain socket that carries newline-
//! delimited JSON [`ControlRequest`]/[`ControlResponse`] frames between clients
//! (the TUI/CLI) and the world host.
//!
//! Each accepted connection is served concurrently. A request is translated into
//! a [`ControlOp`] (with a fresh oneshot), forwarded to the host's control
//! channel, and its reply serialized straight back — so this layer is a thin,
//! transport-only adapter over [`crate::host`]. It is the default, always-on
//! management channel (the opt-in HTTP API that `lev serve` toggles is a separate
//! surface).

#![cfg(unix)]

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::components::AgentStatus;
use crate::host::ControlOp;
use leviath_core::interaction::{InteractionRequest, InteractionResponse};

/// A control request over the wire. Agents are addressed by run id (the stable
/// id), except `Message`, which targets an agent id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
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
/// The accept loop that produces the `UnixStream`s (and owns the socket file's
/// lifecycle) lives with the daemon; this is the reusable per-connection half.
pub async fn handle_connection(
    stream: UnixStream,
    op_tx: UnboundedSender<ControlOp>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
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

/// Bind the daemon's control socket at `path`, enforcing a single instance.
///
/// If a socket file already exists, probe it: a live daemon answering a connect
/// means one is already running (returns [`std::io::ErrorKind::AddrInUse`]); a
/// refused/failed connect means the file is **stale** (a crashed daemon) and is
/// removed before binding. The parent directory is created if needed.
pub fn bind_control_socket(path: &std::path::Path) -> std::io::Result<tokio::net::UnixListener> {
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "a leviath daemon is already running on this control socket",
                ));
            }
            // Nothing is listening — the socket file is stale; clear it. A failed
            // remove just means the bind below reports the problem instead.
            Err(_) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    // A control-socket path always has a parent directory.
    let parent = path.parent().expect("control socket path has a parent");
    std::fs::create_dir_all(parent)?;
    tokio::net::UnixListener::bind(path)
}

/// The client half of the control transport: connects to the daemon's control
/// socket, sends one [`ControlRequest`], and reads back its [`ControlResponse`].
/// A fresh connection per request keeps it simple and stateless.
pub struct ControlClient {
    socket_path: std::path::PathBuf,
}

impl ControlClient {
    /// A client for the control socket at `socket_path`.
    pub fn new(socket_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Send one request and await its response. Errors if the socket can't be
    /// reached, the connection closes before a reply, or the reply doesn't parse.
    pub async fn request(&self, req: &ControlRequest) -> std::io::Result<ControlResponse> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (read_half, mut write_half) = stream.into_split();
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
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;

    /// A fake host: drains ControlOps and replies with scripted values.
    fn spawn_fake_host(mut rx: mpsc::UnboundedReceiver<ControlOp>) {
        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                match op {
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

    async fn round_trip(req: &ControlRequest) -> ControlResponse {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server) = UnixStream::pair().unwrap();
        tokio::spawn(async move {
            let _ = handle_connection(server, op_tx).await;
        });

        let (read_half, mut write_half) = client.into_split();
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

    #[tokio::test]
    async fn malformed_request_gets_error_and_connection_continues() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server) = UnixStream::pair().unwrap();
        let handle = tokio::spawn(async move { handle_connection(server, op_tx).await });

        let (read_half, mut write_half) = client.into_split();
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

    /// Bind a listener at a temp path, accept exactly `n` connections and serve
    /// each with `handle_connection`, returning the socket path (and the tempdir
    /// keeping it alive).
    fn spawn_server(n: usize) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        tokio::spawn(async move {
            for _ in 0..n {
                let (stream, _) = listener.accept().await.unwrap();
                let op_tx = op_tx.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, op_tx).await;
                });
            }
        });
        (dir, path)
    }

    #[tokio::test]
    async fn client_round_trips_status_and_list() {
        let (_dir, path) = spawn_server(2);
        let client = ControlClient::new(path);

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
    async fn client_errors_when_socket_absent() {
        let client = ControlClient::new("/nonexistent/leviath-ctl.sock");
        assert!(client.list().await.is_err());
    }

    #[tokio::test]
    async fn bind_creates_socket_in_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        // A nested path whose parent doesn't exist yet.
        let path = dir.path().join("nested").join("control.sock");
        let listener = bind_control_socket(&path).unwrap();
        assert!(path.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn bind_removes_stale_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        // A leftover regular file where the socket goes: nothing is listening, so
        // it's stale and must be cleared.
        std::fs::write(&path, b"stale").unwrap();
        let listener = bind_control_socket(&path).unwrap();
        assert!(path.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn bind_errors_when_parent_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where a parent directory would need to be created.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("control.sock"); // parent "blocker" is a file
        assert!(bind_control_socket(&path).is_err());
    }

    #[tokio::test]
    async fn bind_rejects_when_daemon_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let _live = bind_control_socket(&path).unwrap(); // first daemon holds it
        let err = bind_control_socket(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    /// A server that writes `bytes` verbatim as the "response" then closes.
    fn spawn_raw_server(bytes: &'static [u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_r, mut w) = stream.into_split();
            let _ = w.write_all(bytes).await;
        });
        (dir, path)
    }

    #[tokio::test]
    async fn client_errors_on_unparseable_response() {
        // Valid UTF-8 but not a ControlResponse → InvalidData.
        let (_dir, path) = spawn_raw_server(b"not json\n");
        let err = ControlClient::new(&path).list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn client_errors_on_invalid_utf8_response() {
        // Invalid UTF-8 makes the response line reader itself error.
        let (_dir, path) = spawn_raw_server(&[0xff, 0xfe, b'\n']);
        assert!(ControlClient::new(&path).list().await.is_err());
    }

    #[tokio::test]
    async fn client_errors_on_closed_connection_without_reply() {
        // A server that accepts then immediately drops the connection (no reply).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream); // hang up before responding
        });

        let client = ControlClient::new(&path);
        let err = client.list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn invalid_utf8_line_ends_connection_with_error() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server) = UnixStream::pair().unwrap();
        let handle = tokio::spawn(async move { handle_connection(server, op_tx).await });

        let (_read_half, mut write_half) = client.into_split();
        // Invalid UTF-8 makes the line reader return an I/O error, which
        // handle_connection propagates.
        write_half.write_all(&[0xff, 0xfe, b'\n']).await.unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_returns_neutral_when_host_gone() {
        // No host draining the channel; the sender's receiver is dropped, so each
        // op's reply channel drops and dispatch falls back to the neutral value.
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
            dispatch(ControlRequest::List, &op_tx).await,
            ControlResponse::List { runs: vec![] }
        );
        assert_eq!(
            dispatch(
                ControlRequest::Pause {
                    run_id: "r".to_string()
                },
                &op_tx
            )
            .await,
            ControlResponse::Ok { ok: false }
        );
    }
}
