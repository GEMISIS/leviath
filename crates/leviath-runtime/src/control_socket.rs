//! The local control transport: a loopback TCP socket that carries newline-
//! delimited JSON [`ControlRequest`]/[`ControlResponse`] frames between clients
//! (the TUI/CLI) and the world host.
//!
//! A loopback socket (rather than a Unix-domain socket) is used so the transport
//! is one code path on every platform — `tokio` has no async Unix-domain socket
//! on Windows, whereas TCP is uniform. The daemon binds an ephemeral
//! `127.0.0.1` port and publishes it to a small *port file*; clients read the
//! port from that file to connect. Binding to loopback keeps the channel local
//! to the machine.
//!
//! Each accepted connection is served concurrently. A request is translated into
//! a [`ControlOp`] (with a fresh oneshot), forwarded to the host's control
//! channel, and its reply serialized straight back — so this layer is a thin,
//! transport-only adapter over [`crate::host`]. It is the default, always-on
//! management channel (the opt-in HTTP API that `lev serve` toggles is a separate
//! surface).

use std::net::{Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::components::AgentStatus;
use crate::host::{ControlOp, SpawnArgs};
use leviath_core::interaction::{InteractionRequest, InteractionResponse};

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
/// The accept loop that produces the `TcpStream`s (and owns the port file's
/// lifecycle) lives with the daemon; this is the reusable per-connection half.
pub async fn handle_connection(
    stream: TcpStream,
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

/// Read a control port from a port file, returning `None` if the file is
/// missing or does not contain a valid port number.
fn read_port_file(port_file: &std::path::Path) -> Option<u16> {
    std::fs::read_to_string(port_file)
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
}

/// True if a daemon is currently answering on the port published in
/// `port_file` (a `127.0.0.1:<port>` connect succeeds). `false` when the file is
/// missing, holds no valid port, or the port refuses connections (stale).
pub fn is_daemon_running(port_file: &std::path::Path) -> bool {
    read_port_file(port_file)
        .is_some_and(|port| std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok())
}

/// Enforce a single daemon instance against a published `port_file`.
///
/// If `port_file` already names a live daemon ([`is_daemon_running`]), this
/// returns [`std::io::ErrorKind::AddrInUse`]. Otherwise the file is **stale** (a
/// crashed daemon, or none) and this returns `Ok(())` — the caller is free to
/// bind.
///
/// The caller then binds an ephemeral loopback port and hands it to
/// [`publish_port`]. Binding lives with the caller (the binary) because a raw
/// `TcpListener::bind` cannot fail deterministically under test.
pub fn check_single_instance(port_file: &std::path::Path) -> std::io::Result<()> {
    if is_daemon_running(port_file) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "a leviath daemon is already running on this control port",
        ));
    }
    Ok(())
}

/// Publish the daemon's bound `port` to `port_file`, creating the parent
/// directory if needed, so clients can find it.
pub fn publish_port(port_file: &std::path::Path, port: u16) -> std::io::Result<()> {
    // A port-file path always has a parent directory.
    let parent = port_file.parent().expect("port file path has a parent");
    std::fs::create_dir_all(parent)?;
    std::fs::write(port_file, port.to_string())
}

/// The client half of the control transport: reads the daemon's port from a
/// port file, connects to `127.0.0.1:<port>`, sends one [`ControlRequest`], and
/// reads back its [`ControlResponse`]. A fresh connection per request keeps it
/// simple and stateless.
pub struct ControlClient {
    port_file: std::path::PathBuf,
}

impl ControlClient {
    /// A client that resolves the daemon's port from the port file at
    /// `port_file`.
    pub fn new(port_file: impl Into<std::path::PathBuf>) -> Self {
        Self {
            port_file: port_file.into(),
        }
    }

    /// Resolve the daemon's loopback address from the port file, erroring (as
    /// "not connected") when the file is missing or holds no valid port.
    fn addr(&self) -> std::io::Result<SocketAddr> {
        let port = read_port_file(&self.port_file).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no leviath daemon control port is published",
            )
        })?;
        Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    }

    /// Send one request and await its response. Errors if the daemon can't be
    /// reached, the connection closes before a reply, or the reply doesn't parse.
    pub async fn request(&self, req: &ControlRequest) -> std::io::Result<ControlResponse> {
        let stream = TcpStream::connect(self.addr()?).await?;
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
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    /// A connected loopback TCP pair (client, server) — the TCP analogue of
    /// `UnixStream::pair`, used to drive `handle_connection` directly.
    async fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (client.unwrap(), accepted.unwrap().0)
    }

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

    async fn round_trip(req: &ControlRequest) -> ControlResponse {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server) = tcp_pair().await;
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
        let (client, server) = tcp_pair().await;
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

    /// Bind a loopback listener, publish its port to a temp port file, accept
    /// exactly `n` connections and serve each with `handle_connection`, returning
    /// the port-file path (and the tempdir keeping it alive).
    async fn spawn_server(n: usize) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("ctl.port");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        std::fs::write(
            &port_file,
            listener.local_addr().unwrap().port().to_string(),
        )
        .unwrap();
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
        (dir, port_file)
    }

    #[tokio::test]
    async fn client_round_trips_status_and_list() {
        let (_dir, path) = spawn_server(3).await;
        let client = ControlClient::new(path);

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
    async fn client_errors_when_port_file_absent() {
        let client = ControlClient::new("/nonexistent/leviath-ctl.port");
        assert!(client.list().await.is_err());
    }

    #[tokio::test]
    async fn client_errors_when_port_file_holds_no_valid_port() {
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("ctl.port");
        std::fs::write(&port_file, b"not-a-port").unwrap();
        let err = ControlClient::new(&port_file).list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn client_errors_when_port_is_dead() {
        // A valid port that nothing listens on: `addr()` resolves, but the TCP
        // connect is refused (exercising the connect `?` after `addr`).
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener); // free the port so connects are refused
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("ctl.port");
        std::fs::write(&port_file, dead_port.to_string()).unwrap();
        assert!(ControlClient::new(&port_file).list().await.is_err());
    }

    #[test]
    fn publish_port_creates_port_file_in_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        // A nested path whose parent doesn't exist yet.
        let path = dir.path().join("nested").join("control.port");
        publish_port(&path, 54321).unwrap();
        assert_eq!(read_port_file(&path), Some(54321));
    }

    #[test]
    fn publish_port_errors_when_parent_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where a parent directory would need to be created.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("control.port"); // parent "blocker" is a file
        assert!(publish_port(&path, 1234).is_err());
    }

    #[test]
    fn publish_port_errors_when_target_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.port");
        std::fs::create_dir(&path).unwrap(); // the write target is a directory
        assert!(publish_port(&path, 1234).is_err());
    }

    #[test]
    fn single_instance_ok_when_no_port_file() {
        let dir = tempfile::tempdir().unwrap();
        // Absent file → free to bind.
        check_single_instance(&dir.path().join("control.port")).unwrap();
    }

    #[tokio::test]
    async fn single_instance_ignores_stale_port_file() {
        // A port file pointing at a dead port is stale → free to bind.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.port");
        std::fs::write(&path, dead_port.to_string()).unwrap();
        check_single_instance(&path).unwrap();
    }

    #[tokio::test]
    async fn single_instance_rejects_when_daemon_already_running() {
        // A live listener on the published port → already running.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.port");
        std::fs::write(&path, port.to_string()).unwrap();
        let err = check_single_instance(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    /// A server that writes `bytes` verbatim as the "response" then closes,
    /// publishing its port to a temp port file.
    async fn spawn_raw_server(bytes: &'static [u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("ctl.port");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        std::fs::write(
            &port_file,
            listener.local_addr().unwrap().port().to_string(),
        )
        .unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_r, mut w) = stream.into_split();
            let _ = w.write_all(bytes).await;
        });
        (dir, port_file)
    }

    #[tokio::test]
    async fn client_errors_on_unparseable_response() {
        // Valid UTF-8 but not a ControlResponse → InvalidData.
        let (_dir, path) = spawn_raw_server(b"not json\n").await;
        let err = ControlClient::new(&path).list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn client_errors_on_invalid_utf8_response() {
        // Invalid UTF-8 makes the response line reader itself error.
        let (_dir, path) = spawn_raw_server(&[0xff, 0xfe, b'\n']).await;
        assert!(ControlClient::new(&path).list().await.is_err());
    }

    #[tokio::test]
    async fn client_errors_on_closed_connection_without_reply() {
        // A server that accepts then immediately drops the connection (no reply).
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("ctl.port");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        std::fs::write(
            &port_file,
            listener.local_addr().unwrap().port().to_string(),
        )
        .unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Drain the request line first, so dropping the stream is a clean FIN
            // (unread data would instead make the OS send an RST): the client then
            // observes a plain EOF before any reply, not a connection-reset error.
            let (read_half, _write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await;
            // hang up before responding
        });

        let client = ControlClient::new(&port_file);
        let err = client.list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn invalid_utf8_line_ends_connection_with_error() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server) = tcp_pair().await;
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
        // Spawn with the host gone yields a shutting-down error.
        let spawn_resp = dispatch(
            ControlRequest::Spawn {
                args: SpawnArgs::default(),
            },
            &op_tx,
        )
        .await;
        assert_eq!(
            std::mem::discriminant(&spawn_resp),
            std::mem::discriminant(&ControlResponse::Error {
                message: String::new()
            })
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
            resp,
            ControlResponse::Error {
                message: "bad blueprint".to_string()
            }
        );
    }
}
