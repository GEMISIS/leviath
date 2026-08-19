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
//! Each platform module exposes the same small surface - [`ControlId`],
//! [`control_id`], [`bind_control_listener`], [`ControlListener::accept`],
//! [`connect`], and [`is_daemon_running`] - over which the shared
//! [`handle_connection`] (generic over any `AsyncRead + AsyncWrite`) and
//! [`ControlClient`] operate. It is the default, always-on management channel
//! (the opt-in HTTP API that `lev serve` toggles is a separate surface).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{broadcast, oneshot};

use crate::components::AgentStatus;
use crate::host::{ControlOp, DaemonHealth, RunListEntry, SpawnArgs, WorldEvent};
use leviath_core::interaction::{InteractionRequest, InteractionResponse};

mod client;
pub use client::{
    CodeMismatch, ControlClient, DEFAULT_CONTROL_TIMEOUT_SECS, LinkStatus, RESTART_GRACE,
    SPAWN_CONTROL_TIMEOUT_SECS, WorldEventStream, request_timeout,
};
#[cfg(test)]
use client::{is_transient, timeout_for};

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

/// Prefix on every response that means "you are not authenticated".
///
/// Shared by both sides rather than matched as prose: the client turns it back
/// into an actionable error naming the token file, and a message the two ends
/// spelled differently would silently stop being recognised.
pub(super) const AUTH_REQUIRED: &str = "authentication";

/// Prefix on the daemon's reply to a request line it could not parse. Shared
/// with the client so it can recognise it and, when the two ends run different
/// code, name that as the cause.
pub(super) const INVALID_REQUEST: &str = "invalid request";

/// The daemon's reply when a request reached it while its serve loop was
/// already gone. Shared with the client so it can recognise it: the request was
/// dropped unprocessed, which makes a retry safe even for a spawn.
pub(super) const SHUTTING_DOWN: &str = "daemon is shutting down";

/// The most one connection may send before the stream is cut.
///
/// A spawn request carries a task string and region seeds, so the cap has to be
/// generous; 8 MiB is far past anything a real caller sends and still bounds
/// what an unauthenticated peer can make the daemon buffer.
const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;

/// A shared secret that proves a control-channel caller is this same user.
///
/// # Why this exists
///
/// On Unix the daemon asks the kernel which uid is on the other end of the
/// socket and refuses anything that is not its own - see the peer check in the
/// `unix` module. Windows offers an equivalent, but reaching it means calling
/// `ImpersonateNamedPipeClient` and comparing security identifiers through raw
/// FFI, and this workspace is `unsafe_code = "forbid"` from top to bottom. So
/// the Windows control channel served *every* connection it accepted: anyone who
/// could reach the pipe could spawn a tool-executing agent and answer its
/// approval prompts.
///
/// A token closes that without any of the FFI. The daemon writes a fresh random
/// secret into its own directory, readable only by the owner, and refuses any
/// connection that cannot quote it. A caller who can read the file is a caller
/// who can already read `config.toml` - so the token grants nothing that was not
/// already reachable, which is exactly the property wanted.
///
/// It is required on every platform, not only Windows. One protocol is easier to
/// reason about than two, the extra round trip on a local socket is
/// unmeasurable, and on Unix it is defence in depth behind the uid check rather
/// than a replacement for it.
#[derive(Clone)]
pub struct ControlToken(String);

impl std::fmt::Debug for ControlToken {
    /// Never render the secret: this type ends up inside daemon state that other
    /// code may reasonably want to `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlToken(<redacted>)")
    }
}

impl ControlToken {
    /// The token file beside the control socket.
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("control.token")
    }

    /// Where the daemon records its own process id.
    ///
    /// So `lev daemon stop` has a way through when the control channel does not
    /// answer - a wedged daemon, or a token file that went missing. Without it
    /// the only recovery was `pkill`, and the advice to "restart it" was advice
    /// that could not work: `restart` stops before it starts, and the stop was
    /// the part that failed.
    pub fn pid_path(dir: &Path) -> PathBuf {
        dir.join("daemon.pid")
    }

    /// Record this process as the running daemon.
    pub fn write_pid(dir: &Path) -> std::io::Result<()> {
        leviath_sys::write_private(
            &Self::pid_path(dir),
            std::process::id().to_string().as_bytes(),
        )
    }

    /// The recorded daemon pid, if one was written and still parses.
    pub fn read_pid(dir: &Path) -> Option<u32> {
        std::fs::read_to_string(Self::pid_path(dir))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Generate a fresh token and write it owner-only.
    ///
    /// Called at bind, so a restarted daemon invalidates every previous token -
    /// a stale one cannot be replayed against the new process.
    pub fn create(dir: &Path) -> std::io::Result<Self> {
        use rand::RngExt as _;
        // 256 bits from the OS generator. Hex rather than raw bytes so the file
        // is a single printable line that a human can compare, and so the value
        // survives being read back as text.
        let bytes: [u8; 32] = rand::rng().random();
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

        std::fs::create_dir_all(dir)?;
        let _ = leviath_sys::secure_dir_perms(dir);
        leviath_sys::write_private(&Self::path(dir), token.as_bytes())?;
        Ok(Self(token))
    }

    /// Read the token a running daemon wrote.
    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let token = std::fs::read_to_string(Self::path(dir))?;
        Ok(Self(token.trim().to_string()))
    }

    /// Whether `presented` is this token, compared in constant time.
    ///
    /// Constant time because the comparison is against a secret and the caller
    /// controls the input: a byte-at-a-time early return leaks the prefix, and
    /// a local attacker can retry without limit.
    pub fn matches(&self, presented: &str) -> bool {
        leviath_core::constant_time_eq(&self.0, presented)
    }

    /// The token itself, for a client that is about to present it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// A control request over the wire. Agents are addressed by run id (the stable
/// id), except `Message`, which targets an agent id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Prove the caller is this user, by quoting the daemon's control token.
    ///
    /// Must be the first request on a connection. Until it succeeds the daemon
    /// answers nothing else - see [`ControlToken`] for why.
    Authenticate {
        /// The token read from `<leviath-home>/control.token`.
        token: String,
        /// Ask the daemon to say who it is in reply ([`ControlResponse::Welcome`]
        /// rather than a bare `Ok`). Defaulted so a daemon that predates the
        /// field reads the request exactly as before, and a client that
        /// predates it keeps getting the `Ok` it expects.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        hello: bool,
    },
    /// Spawn a new agent.
    Spawn {
        /// The spawn request. Boxed because it is much larger than the other
        /// variants' payloads.
        args: Box<SpawnArgs>,
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
    /// Shut the daemon down.
    Shutdown,
    /// Switch this connection to an event stream: the daemon writes newline-JSON
    /// [`WorldEvent`]s until the client disconnects. No per-request reply.
    Subscribe,
}

impl ControlRequest {
    /// Whether repeating this request can never do more than the first send
    /// did. Decides what a client may retry after a request was written and
    /// the daemon vanished before replying: a `List` asked twice is a `List`,
    /// while a `Spawn` asked twice may be two runs.
    ///
    /// The mutating ops that *are* idempotent in effect - a second `Cancel`
    /// of the same run does nothing - are still excluded, because their
    /// second reply says `ok: false`, and the caller would report a cancel
    /// that worked as "no such run".
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Authenticate { .. }
                | Self::Status { .. }
                | Self::List
                | Self::ListInteractions
                | Self::Subscribe
        )
    }
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
    /// A listing of runs, their statuses, and the context needed to read them,
    /// with the daemon's own health alongside.
    List {
        /// One entry per live run.
        runs: Vec<RunListEntry>,
        /// Runs the daemon unloaded recently enough to still report, oldest
        /// first. Separate from `runs` so a caller counting hosted agents, or
        /// asking which runs the daemon still holds, is not answered with runs
        /// that have finished. Defaulted for the same reason as `health`.
        #[serde(default)]
        finished: Vec<RunListEntry>,
        /// How the daemon itself is doing. Defaulted when absent so a listing
        /// from an older daemon still parses.
        #[serde(default)]
        health: DaemonHealth,
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
    /// The answer to an `authenticate` that asked `hello`: who this daemon is.
    ///
    /// Sent instead of `Ok`, and only when asked for, so a client that does not
    /// know this variant never receives it.
    Welcome {
        /// The daemon's identity.
        daemon: DaemonIdentity,
    },
}

/// Which process a control connection reached.
///
/// A daemon reports this in the authentication handshake, and a long-lived
/// client keeps the last one it saw. That is how the client tells three
/// situations apart that all begin with "the connection dropped": the same
/// daemon is back (`pid` and `build` unchanged), the daemon restarted (`pid`
/// moved, `build` did not), or the daemon was *updated* (`build` or `version`
/// moved) - the one case where the client itself may need restarting, because
/// the two ends no longer run the same code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonIdentity {
    /// The Leviath version the daemon was built from (`0.4.0`).
    pub version: String,
    /// The build id, as `lev daemon` records it in `daemon.build`. `unknown`
    /// when the process did not say (an embedder driving the runtime directly).
    #[serde(default = "DaemonIdentity::unknown_build")]
    pub build: String,
    /// The daemon's process id.
    pub pid: u32,
    /// Names - never values - of the tool credentials this process can see in
    /// its own environment, out of the fixed set its binary asked about.
    ///
    /// A daemon's environment is fixed at exec time and no client shares it, so
    /// a tool that reads a key from the environment can be misconfigured in a
    /// way only the daemon can observe: a CLI inspecting *its own* environment
    /// reports the shell it was typed in, which is a different process. That is
    /// how `lev doctor` came to report a working search key while every run
    /// still fell back to Wikipedia, because the daemon had been started from a
    /// shell that did not export it.
    ///
    /// `None` means the process did not say (an older daemon, or an embedder
    /// driving the runtime directly) - which a caller must report as "unknown",
    /// never as "sees nothing". Names only: the value never crosses the wire,
    /// and the set is chosen by the binary rather than scraped, so this cannot
    /// become a way to enumerate a daemon's secrets.
    #[serde(default)]
    pub tool_env: Option<Vec<String>>,
}

impl DaemonIdentity {
    /// The identity of *this* process, given its build id.
    ///
    /// The version is this crate's, which is the workspace's: the daemon and
    /// every client of it are built from the same tree, so comparing versions
    /// across the wire compares releases. The build id lives with the binary
    /// (it hashes the working tree), so the binary passes it in.
    ///
    /// Reports no [`tool_env`](Self::tool_env): which credentials are worth
    /// probing is the binary's business, not the runtime's, so a process that
    /// cares adds them with [`with_tool_env`](Self::with_tool_env).
    pub fn this_process(build: impl Into<String>) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: build.into(),
            pid: std::process::id(),
            tool_env: None,
        }
    }

    /// Report which tool credentials this process can see, by name.
    ///
    /// An empty list is a real answer ("asked, saw none") and is kept as one -
    /// distinct from the `None` of a process that never said.
    pub fn with_tool_env(mut self, names: Vec<String>) -> Self {
        self.tool_env = Some(names);
        self
    }

    /// Whether this process reported seeing the environment variable `name`.
    ///
    /// `None` when it did not report at all, so a caller can say "unknown"
    /// rather than guess in either direction.
    pub fn sees_tool_env(&self, name: &str) -> Option<bool> {
        self.tool_env
            .as_ref()
            .map(|seen| seen.iter().any(|n| n == name))
    }

    /// The build to record when a process does not say. Named rather than a
    /// literal because [`same_code_as`](Self::same_code_as) must recognise it:
    /// an unknown build is compared as "could be the same", never as the word.
    pub fn unknown_build() -> String {
        "unknown".to_string()
    }

    /// Whether the two ends run the same code, as far as they can tell.
    ///
    /// Versions must match. Builds must match too when both are known; a side
    /// that does not know its build cannot contradict the other, so it does
    /// not.
    pub fn same_code_as(&self, other: &Self) -> bool {
        let unknown = Self::unknown_build();
        self.version == other.version
            && (self.build == unknown || other.build == unknown || self.build == other.build)
    }
}

impl std::fmt::Display for DaemonIdentity {
    /// `0.4.0 (build 3ba95219, pid 4242)`: what a log line or a toast says.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (build {}, pid {})",
            self.version, self.build, self.pid
        )
    }
}

/// Translate a parsed request into a [`ControlOp`], forward it to the host, and
/// await the reply as a [`ControlResponse`]. A closed host channel (shutting
/// down) yields the operation's neutral result.
async fn dispatch(req: ControlRequest, op_tx: &UnboundedSender<ControlOp>) -> ControlResponse {
    match req {
        // Handled by `handle_connection` before dispatch is ever reached: it is
        // about the connection, not about the world.
        ControlRequest::Authenticate { .. } => ControlResponse::Ok { ok: true },
        ControlRequest::Spawn { args } => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Spawn { args, reply });
            match rx.await {
                Ok(Ok(run_id)) => ControlResponse::Spawned { run_id },
                Ok(Err(message)) => ControlResponse::Error { message },
                Err(_) => ControlResponse::Error {
                    message: SHUTTING_DOWN.to_string(),
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
            let listing = rx.await.unwrap_or_default();
            ControlResponse::List {
                runs: listing.runs,
                finished: listing.finished,
                health: listing.health,
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
        ControlRequest::Shutdown => {
            let (reply, rx) = oneshot::channel();
            let _ = op_tx.send(ControlOp::Shutdown { reply });
            ControlResponse::Ok {
                ok: rx.await.unwrap_or(false),
            }
        }
        // `Subscribe` is intercepted by `handle_connection` (it streams rather
        // than replies once); reaching here would be a routing bug.
        ControlRequest::Subscribe => ControlResponse::Error {
            message: "subscribe is a streaming request, not a single-reply op".to_string(),
        },
    }
}

/// Stream [`WorldEvent`]s to a subscribed client until it disconnects or the
/// broadcast channel closes. Lagged events are skipped.
///
/// The read half is watched alongside the writes: a subscriber that hangs up
/// is otherwise only noticed when the *next* event's write fails, and an idle
/// daemon may not produce one for hours - each such half-dead connection
/// parked a task and a `broadcast::Receiver` here for the daemon's life
/// (serve's polling loop re-subscribes every 500ms after a drop, and the ACP
/// client subscribes once per prompt turn, so these accumulated fast).
async fn stream_events<R, W>(
    read: &mut tokio::io::Lines<BufReader<R>>,
    write: &mut W,
    mut rx: broadcast::Receiver<WorldEvent>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(event) => {
                    let mut line = serde_json::to_string(&event).expect("WorldEvent serializes");
                    line.push('\n');
                    if write.write_all(line.as_bytes()).await.is_err() {
                        return Ok(()); // client hung up
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            line = read.next_line() => match line {
                // A subscriber has nothing left to say; any line it does send
                // is ignored chatter, not a request.
                Ok(Some(_)) => continue,
                // EOF or a read error: the client is gone.
                Ok(None) | Err(_) => return Ok(()),
            },
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
    events: broadcast::Sender<WorldEvent>,
    token: Option<ControlToken>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_connection_as(
        stream,
        op_tx,
        events,
        token,
        DaemonIdentity::this_process(DaemonIdentity::unknown_build()),
    )
    .await
}

/// [`handle_connection`], introducing the daemon as `identity` to a client that
/// asks. The `lev daemon` binary passes its build id here; the plain form is
/// for embedders and tests, which have no build id to give.
pub async fn handle_connection_as<S>(
    stream: S,
    op_tx: UnboundedSender<ControlOp>,
    events: broadcast::Sender<WorldEvent>,
    token: Option<ControlToken>,
    identity: DaemonIdentity,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_connection_capped(stream, op_tx, events, token, identity, MAX_REQUEST_BYTES).await
}

/// The reply to a successful `authenticate`: `Welcome` when the client asked
/// who it reached, the historical bare `Ok` when it did not.
fn authenticated_reply(hello: bool, identity: &DaemonIdentity) -> ControlResponse {
    match hello {
        true => ControlResponse::Welcome {
            daemon: identity.clone(),
        },
        false => ControlResponse::Ok { ok: true },
    }
}

/// [`handle_connection_as`] with the per-request cap injected.
///
/// The cap is a parameter purely so a test can cross it without pushing 8 MiB
/// through a duplex - and crossing it is the only way to tell a per-request
/// budget from a per-connection one.
async fn handle_connection_capped<S>(
    stream: S,
    op_tx: UnboundedSender<ControlOp>,
    events: broadcast::Sender<WorldEvent>,
    token: Option<ControlToken>,
    identity: DaemonIdentity,
    max_request_bytes: u64,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    // Capped rather than unbounded. On Unix the peer is same-uid and holds the
    // token, so this is only tidiness - but a Windows named pipe is created
    // with a default DACL, which makes an unbounded `lines()` a pre-auth
    // allocation any local user can drive. `take` ends the stream at the cap,
    // so an oversized request reads as a truncated line and is refused by the
    // parse below rather than growing a buffer without limit.
    //
    // The limit is reset after every request, because it bounds *a request*
    // and this connection serves many. Left as a one-shot budget it bounded
    // the connection instead: a long-lived caller - the dashboard holds one
    // open and polls - would be cut off mid-protocol once its traffic summed
    // past the cap, surfacing as a spurious `invalid request` on a perfectly
    // ordinary line.
    let mut lines = BufReader::new(read_half.take(max_request_bytes)).lines();
    // `None` means this daemon runs without a token and every caller is
    // accepted, which is only the case in tests that drive the protocol
    // directly. Production always passes one.
    let mut authenticated = token.is_none();
    while let Some(line) = lines.next_line().await? {
        // Refill this request's budget for the next one.
        lines.get_mut().get_mut().set_limit(max_request_bytes);
        if line.trim().is_empty() {
            continue;
        }

        // Until the caller has proved who it is, `Authenticate` is the only
        // request that gets an answer. Anything else - including a malformed
        // line - is refused and the connection dropped, so an unauthenticated
        // peer cannot sit probing the protocol.
        if !authenticated {
            let refused = match serde_json::from_str::<ControlRequest>(&line) {
                Ok(ControlRequest::Authenticate {
                    token: presented,
                    hello,
                }) => match token.as_ref().is_some_and(|t| t.matches(&presented)) {
                    true => {
                        authenticated = true;
                        write_line(&mut write_half, &authenticated_reply(hello, &identity)).await;
                        continue;
                    }
                    false => "authentication failed",
                },
                _ => "authentication required: send an `authenticate` request first",
            };
            write_line(
                &mut write_half,
                &ControlResponse::Error {
                    message: refused.to_string(),
                },
            )
            .await;
            return Ok(());
        }

        let response = match serde_json::from_str::<ControlRequest>(&line) {
            // Subscribe switches this connection to an event stream and never
            // returns to the request loop. Drop this connection's sender clone
            // after subscribing so the channel closes once the world's sender
            // does (a clean end on daemon shutdown).
            Ok(ControlRequest::Subscribe) => {
                let rx = events.subscribe();
                drop(events);
                return stream_events(&mut lines, &mut write_half, rx).await;
            }
            // Already authenticated: a repeat is harmless, not an error. On a
            // tokenless daemon this is also the only way a client's `hello`
            // gets answered, since no handshake gate ran.
            Ok(ControlRequest::Authenticate { hello, .. }) => authenticated_reply(hello, &identity),
            Ok(req) => dispatch(req, &op_tx).await,
            Err(e) => ControlResponse::Error {
                message: format!("{INVALID_REQUEST}: {e}"),
            },
        };
        write_line(&mut write_half, &response).await;
    }
    Ok(())
}

/// Write one newline-delimited response.
///
/// A failed write means the client hung up; the next read returns EOF and the
/// loop ends cleanly, so the error needs no separate handling.
async fn write_line<W>(write_half: &mut W, response: &ControlResponse)
where
    W: AsyncWrite + Unpin,
{
    // `ControlResponse` is a plain serde enum - serialization is infallible.
    let mut out = serde_json::to_string(response).expect("ControlResponse serializes");
    out.push('\n');
    let _ = write_half.write_all(out.as_bytes()).await;
}

#[cfg(test)]
mod tests {

    // ── Control-channel authentication ──────────────────────────────────────

    /// `lev daemon stop` needs a way through when the control channel does not
    /// answer, because `restart` stops before it starts - so a daemon that had
    /// lost its token file could not be restarted, only `pkill`ed.
    #[test]
    fn the_daemon_pid_round_trips_and_a_missing_or_junk_file_is_no_pid() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ControlToken::read_pid(dir.path()),
            None,
            "no file yet is not a pid"
        );

        ControlToken::write_pid(dir.path()).unwrap();
        assert_eq!(
            ControlToken::read_pid(dir.path()),
            Some(std::process::id()),
            "what was written is what comes back"
        );

        // Trailing whitespace is tolerated; anything that is not a number is
        // not a pid, and must not be reported as one.
        std::fs::write(ControlToken::pid_path(dir.path()), " 4242\n").unwrap();
        assert_eq!(ControlToken::read_pid(dir.path()), Some(4242));
        std::fs::write(ControlToken::pid_path(dir.path()), "not-a-pid").unwrap();
        assert_eq!(ControlToken::read_pid(dir.path()), None);
    }

    /// The token is what stands between another local user and a
    /// tool-executing agent on Windows, where there is no kernel peer check.
    #[test]
    fn a_token_round_trips_through_its_file_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let created = ControlToken::create(dir.path()).unwrap();
        let loaded = ControlToken::load(dir.path()).unwrap();

        assert!(
            created.matches(loaded.expose()),
            "the same secret comes back"
        );
        assert_eq!(created.expose().len(), 64, "256 bits, hex encoded");
        let rendered = created.expose().to_string();
        assert!(
            rendered.chars().all(|c| c.is_ascii_hexdigit()),
            "one printable line: {rendered}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(ControlToken::path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the token must be owner-only");
        }
    }

    /// Every daemon mints its own, so a token from a previous process cannot be
    /// replayed against the one running now.
    #[test]
    fn each_token_is_different() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = ControlToken::create(a.path()).unwrap();
        let second = ControlToken::create(b.path()).unwrap();
        assert!(!first.matches(second.expose()), "tokens must not repeat");
    }

    #[test]
    fn a_wrong_or_truncated_token_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let token = ControlToken::create(dir.path()).unwrap();
        assert!(!token.matches(""));
        assert!(!token.matches("deadbeef"));
        // A correct prefix is still wrong: the compare is over the whole value.
        let half: String = token.expose().chars().take(32).collect();
        assert!(!token.matches(&half));
        assert!(token.matches(token.expose()));
    }

    /// The secret must not leak through a `{:?}` of daemon state that happens
    /// to contain it.
    #[test]
    fn the_token_is_redacted_in_debug_output() {
        let dir = tempfile::tempdir().unwrap();
        let token = ControlToken::create(dir.path()).unwrap();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(token.expose()), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    /// A missing token file must not stop a client from being built. It has two
    /// causes the client cannot tell apart - no daemon, or one running that
    /// predates tokens - and refusing here would make an upgrade unrecoverable:
    /// the CLI could not ask the still-running pre-token daemon to shut down,
    /// so it could neither stop it nor start a replacement, while printing
    /// advice the user was already following.
    #[test]
    fn a_missing_token_still_builds_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let client = ControlClient::for_home(control_id(dir.path()), dir.path());
        let err = client.refused().to_string();
        assert!(err.contains("no control token was found"), "{err}");
        assert!(err.contains("lev daemon restart"), "{err}");
    }

    /// And when we *did* present one and were still refused, the message says
    /// that instead - the two situations need different fixes.
    #[test]
    fn a_rejected_token_reads_differently_from_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let _token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(control_id(dir.path()), dir.path());
        let err = client.refused().to_string();
        assert!(err.contains("refused this client's control token"), "{err}");
        assert!(!err.contains("no control token was found"), "{err}");
    }
    use super::*;
    use tokio::sync::mpsc;

    /// An event sender with no live world behind it (tests that don't stream).
    fn no_events() -> broadcast::Sender<WorldEvent> {
        broadcast::channel(16).0
    }

    /// The one listing row the fake host serves, so the op reply and the
    /// expected response can't drift apart.
    fn listing_entry() -> RunListEntry {
        RunListEntry {
            run_id: "run-a".to_string(),
            title: None,
            status: AgentStatus::Active,
            wait_reason: None,
            stage: "plan".to_string(),
            stage_index: Some(0),
            num_stages: Some(2),
            iteration: 3,
            tool_calls: 7,
            last_progress_at: Some(1_000),
            unattended: false,
            empty_output: false,
            read_paths: None,
            has_final_output: false,
        }
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
                    // Embed-only today: no `ControlRequest` builds one, so this
                    // arm exists to keep the double answering every op rather
                    // than to serve a socket path.
                    ControlOp::Result { reply, .. } => {
                        let _ = reply.send(None);
                    }
                    ControlOp::Pause { reply, .. }
                    | ControlOp::Resume { reply, .. }
                    | ControlOp::Cancel { reply, .. } => {
                        let _ = reply.send(true);
                    }
                    ControlOp::Message { reply, .. }
                    | ControlOp::AnswerInteraction { reply, .. }
                    | ControlOp::CancelInteraction { reply, .. }
                    | ControlOp::Shutdown { reply } => {
                        let _ = reply.send(true);
                    }
                    ControlOp::List { reply } => {
                        let mut ended = listing_entry();
                        ended.run_id = "run-ended".to_string();
                        let _ = reply.send(crate::host::RunListing {
                            runs: vec![listing_entry()],
                            finished: vec![ended],
                            ..Default::default()
                        });
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
            // `accept` yields `Ok(None)` for a peer that is not this user; in a
            // test the only connection is our own, so it is always `Some`.
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let _ = handle_connection(stream, op_tx, no_events(), None).await;
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

    /// The request cap bounds *a request*, and this connection serves many.
    ///
    /// Left as a one-shot budget the `take` bounded the whole connection, so a
    /// long-lived caller - the dashboard holds one open and polls - would be cut
    /// off mid-protocol once its traffic summed past the cap, surfacing as a
    /// spurious refusal on a perfectly ordinary line. Driven with a cap small
    /// enough to cross in three requests rather than by sending 8 MiB.
    #[tokio::test]
    async fn the_request_cap_refills_between_requests() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, _dir) = test_listener();
        let server = tokio::spawn(async move {
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            // A cap just over one request's length: three requests cross it,
            // so a budget that never refills cuts the connection partway.
            handle_connection_capped(
                stream,
                op_tx,
                no_events(),
                None,
                DaemonIdentity::this_process("test"),
                40,
            )
            .await
        });

        let stream = connect(&id).await.unwrap();
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();

        // Several requests down one connection. Each is well under the cap, but
        // together they exceed a budget that is never refilled - which is what
        // the old shape did.
        let req = ControlRequest::List;
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        for _ in 0..3 {
            write_half.write_all(line.as_bytes()).await.unwrap();
            let resp = lines
                .next_line()
                .await
                .expect("the connection stays readable")
                .expect("the connection stays open for every request");
            // The *success* shape specifically. Any error the daemon sends is
            // still a well-formed `ControlResponse`, so merely parsing would
            // have called a refusal a pass - which is exactly what it did on
            // the first version of this test.
            serde_json::from_str::<ControlResponse>(&resp)
                .expect("the daemon answers with a response");
            // Asserted on the wire form rather than `matches!` on the parsed
            // variant: `matches!` leaves a `_ => false` arm nothing executes,
            // and the refusal this test exists to catch is exactly
            // `{"result":"error",...}`.
            assert!(
                !resp.contains(r#""result":"error""#),
                "a request past the first was refused rather than answered"
            );
        }

        // Close the client so the handler sees EOF and returns, rather than
        // being dropped mid-await when the test ends.
        drop(write_half);
        drop(lines);
        server
            .await
            .expect("the handler task joins")
            .expect("it ends cleanly");
    }

    /// Every op this double receives has to be answered. A caller waits on a
    /// oneshot, so an unhandled op is not a wrong answer, it is a hang - and
    /// `ControlOp::Result` is the one op no `ControlRequest` builds, so nothing
    /// else here would ever exercise it.
    #[tokio::test]
    async fn the_fake_host_answers_every_op_including_the_embed_only_one() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);

        let (reply, answer) = tokio::sync::oneshot::channel();
        op_tx
            .send(ControlOp::Result {
                run_id: "run-a".to_string(),
                reply,
            })
            .expect("the fake host is listening");

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), answer)
                .await
                .expect("an unanswered op hangs its caller")
                .expect("the reply channel stays open"),
            None
        );
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
            args: Box::new(SpawnArgs {
                run_id: "run-9".to_string(),
                blueprint_path: "/agents/x".to_string(),
                task: "do it".to_string(),
                regions: Default::default(),
                model: None,
                workdir: "/w".to_string(),
                metadata: Default::default(),
                callback_url: None,
                callback_secret: None,
                yolo: false,
                no_seed_commands: false,
                allow: Vec::new(),
                max_depth: None,
                parent_run_id: None,
                output: None,
            }),
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
            args: Box::new(SpawnArgs {
                run_id: "FAIL".to_string(),
                ..Default::default()
            }),
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
        let mut ended = listing_entry();
        ended.run_id = "run-ended".to_string();
        assert_eq!(
            resp,
            ControlResponse::List {
                runs: vec![listing_entry()],
                finished: vec![ended],
                health: DaemonHealth::default(),
            }
        );
    }

    /// A client built against a newer daemon still has to read an older one's
    /// reply, which carries neither the daemon's health nor the runs it has
    /// finished. Both arrive as empty rather than failing the whole listing.
    #[test]
    fn a_listing_without_the_later_fields_still_parses() {
        let older = r#"{"result":"list","runs":[]}"#;
        assert_eq!(
            serde_json::from_str::<ControlResponse>(older).expect("an older listing parses"),
            ControlResponse::List {
                runs: vec![],
                finished: vec![],
                health: DaemonHealth::default(),
            }
        );
    }

    #[tokio::test]
    async fn shutdown_request_round_trips() {
        assert_eq!(
            round_trip(&ControlRequest::Shutdown).await,
            ControlResponse::Ok { ok: true }
        );
    }

    fn completed(run_id: &str) -> WorldEvent {
        WorldEvent::Completed {
            run_id: run_id.to_string(),
            agent_id: "a".to_string(),
            status: "complete".to_string(),
            final_output: None,
        }
    }

    #[tokio::test]
    async fn dispatch_rejects_subscribe_as_a_single_reply_op() {
        let (op_tx, _rx) = mpsc::unbounded_channel();
        let resp = dispatch(ControlRequest::Subscribe, &op_tx).await;
        assert_eq!(
            std::mem::discriminant(&resp),
            std::mem::discriminant(&ControlResponse::Error {
                message: String::new()
            })
        );
    }

    /// A quiet inbound half for driving `stream_events` directly: the returned
    /// guard keeps the peer's write side open so `next_line` stays pending.
    fn quiet_read_half() -> (
        tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(4096);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_client_read, client_write) = tokio::io::split(client);
        // Leak the unused halves' drop by returning the write guard only; the
        // client read half closing is invisible to the server's reader.
        std::mem::forget(_server_write);
        std::mem::forget(_client_read);
        (BufReader::new(server_read).lines(), client_write)
    }

    #[tokio::test]
    async fn stream_events_skips_lagged_writes_ok_and_stops_on_closed() {
        use tokio::io::AsyncReadExt;
        let (tx, rx) = broadcast::channel::<WorldEvent>(1);
        // Overflow the 1-slot buffer so the receiver lags, then leave one to read.
        tx.send(completed("first")).unwrap();
        tx.send(completed("second")).unwrap();
        tx.send(completed("third")).unwrap();
        drop(tx); // no more senders → Closed once drained

        let (mut lines, _keep_open) = quiet_read_half();
        let (mut w, mut r) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move { stream_events(&mut lines, &mut w, rx).await });
        let mut buf = String::new();
        r.read_to_string(&mut buf).await.unwrap();
        server.await.unwrap().unwrap();
        // The lagged-past earliest events were skipped; the latest was written.
        assert!(buf.contains("third"));
        assert!(!buf.contains("first"));
    }

    #[tokio::test]
    async fn stream_events_returns_when_the_client_hangs_up() {
        let (tx, rx) = broadcast::channel::<WorldEvent>(4);
        tx.send(completed("x")).unwrap();
        let (mut lines, _keep_open) = quiet_read_half();
        let (mut w, r) = tokio::io::duplex(64);
        drop(r); // reader gone → the write fails, ending the stream
        stream_events(&mut lines, &mut w, rx).await.unwrap();
        drop(tx);
    }

    /// A subscriber that closes its half of the connection ends the stream
    /// even when no event ever arrives - previously the daemon-side task (and
    /// its broadcast receiver) lived until the next write failed, which on an
    /// idle daemon is never.
    #[tokio::test]
    async fn stream_events_returns_on_client_eof_without_any_event() {
        let (tx, rx) = broadcast::channel::<WorldEvent>(4);
        let (client, server) = tokio::io::duplex(4096);
        let (server_read, _server_write) = tokio::io::split(server);
        let mut lines = BufReader::new(server_read).lines();
        drop(client); // EOF on the read half, nothing was ever sent

        let (mut w, _r) = tokio::io::duplex(4096);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream_events(&mut lines, &mut w, rx),
        )
        .await
        .expect("EOF must end the stream promptly")
        .unwrap();
        drop(tx);
    }

    /// A line the subscriber sends mid-stream is chatter, not a request: the
    /// stream keeps delivering events after it.
    #[tokio::test]
    async fn stream_events_ignores_subscriber_chatter() {
        use tokio::io::AsyncReadExt;
        let (tx, rx) = broadcast::channel::<WorldEvent>(4);
        let (client, server) = tokio::io::duplex(4096);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_client_read, mut client_write) = tokio::io::split(client);
        std::mem::forget(_server_write);
        std::mem::forget(_client_read);
        let mut lines = BufReader::new(server_read).lines();

        client_write.write_all(b"hello?\n").await.unwrap();
        let (mut w, mut r) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move { stream_events(&mut lines, &mut w, rx).await });
        // Give the chatter a chance to be read, then deliver a real event.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(completed("after-chatter")).unwrap();
        drop(tx);
        let mut buf = String::new();
        r.read_to_string(&mut buf).await.unwrap();
        server.await.unwrap().unwrap();
        assert!(buf.contains("after-chatter"));
    }

    /// `create` reports rather than panicking when its directory cannot be
    /// written - a read-only home should fail the daemon loudly, not leave it
    /// running with no way for clients to authenticate.
    #[test]
    fn creating_a_token_in_an_unwritable_place_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the token's directory would have to be.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        assert!(
            ControlToken::create(&blocker.join("nested")).is_err(),
            "a directory that cannot be created is an error"
        );

        // And the other half: the directory is fine, but the token path itself
        // is already a directory, so the write fails.
        let occupied = dir.path().join("occupied");
        std::fs::create_dir_all(ControlToken::path(&occupied)).unwrap();
        assert!(
            ControlToken::create(&occupied).is_err(),
            "a token file that cannot be written is an error"
        );
    }

    /// `dispatch` has an `Authenticate` arm it never sees in practice, because
    /// `handle_connection` answers that request itself. It exists so the match
    /// is exhaustive; this pins that it is inert rather than doing something.
    #[tokio::test]
    async fn dispatching_authenticate_is_inert() {
        let (op_tx, _op_rx) = mpsc::unbounded_channel();
        let response = dispatch(
            ControlRequest::Authenticate {
                token: "irrelevant".to_string(),
                hello: false,
            },
            &op_tx,
        )
        .await;
        assert!(matches!(response, ControlResponse::Ok { ok: true }));
    }

    /// A client that re-sends `Authenticate` on an already-authenticated
    /// connection is answered, not punished: a repeat is harmless.
    #[tokio::test]
    async fn re_authenticating_on_an_open_connection_is_accepted() {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();

        let server_token = token.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let _ = handle_connection(stream, op_tx, events, Some(server_token)).await;
        });

        let stream = connect(&id).await.unwrap();
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        let hello = serde_json::to_string(&ControlRequest::Authenticate {
            token: token.expose().to_string(),
            hello: false,
        })
        .unwrap();
        for _ in 0..2 {
            write_half
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();
            let resp: ControlResponse =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let rendered = format!("{resp:?}");
            assert!(rendered.starts_with("Ok"), "{rendered}");
        }

        // Hang up so the server sees EOF and its task ends.
        drop(write_half);
        drop(lines);
        server.await.unwrap();
    }

    /// A daemon that hangs up mid-handshake is reported as such rather than as
    /// a mysterious parse failure.
    #[tokio::test]
    async fn a_connection_closed_during_authentication_is_reported() {
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();
        tokio::spawn(async move {
            // Accept, then drop without answering the handshake.
            let _ = listener.accept().await;
        });

        let err = ControlClient::new(id)
            .with_token(token)
            .list()
            .await
            .expect_err("a hang-up during authentication is an error");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("during authentication"), "{err}");
    }

    /// The whole point, end to end over a real socket: a caller that cannot
    /// quote the token gets nothing. Before this, the Windows channel served
    /// every connection it accepted.
    #[tokio::test]
    async fn an_unauthenticated_caller_is_refused_and_disconnected() {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();

        let server_token = token.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let _ = handle_connection(stream, op_tx, events, Some(server_token)).await;
        });

        // A client with no token at all: its first request is `List`, which the
        // daemon must refuse rather than answer. The client turns that refusal
        // into a typed error naming the fix, rather than handing the caller a
        // protocol-level `Error` response to interpret.
        let err = ControlClient::new(id)
            .list()
            .await
            .expect_err("an unauthenticated List must not be served");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("token"), "{err}");
        // The refusal closes the connection, so the server task ends on its own.
        server.await.unwrap();
    }

    /// And a *wrong* token is refused just as firmly as none at all.
    #[tokio::test]
    async fn a_client_presenting_the_wrong_token_is_refused() {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, dir) = test_listener();
        let real = ControlToken::create(dir.path()).unwrap();

        tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let _ = handle_connection(stream, op_tx, events, Some(real)).await;
        });

        // A different daemon's token.
        let other_dir = tempfile::tempdir().unwrap();
        let wrong = ControlToken::create(other_dir.path()).unwrap();
        let err = ControlClient::new(id)
            .with_token(wrong)
            .list()
            .await
            .expect_err("a wrong token is refused");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("refused"), "{err}");
    }

    /// The converse, so the tests above are not passing merely because
    /// everything is refused: the right token gets served.
    #[tokio::test]
    async fn a_client_presenting_the_right_token_is_served() {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();

        let server_token = token.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let _ = handle_connection(stream, op_tx, events, Some(server_token)).await;
        });

        // Loaded the way a real client does, out of the daemon's directory.
        let client = ControlClient::for_home(id, dir.path());
        let response = client
            .list()
            .await
            .expect("an authenticated List is served");
        let rendered = format!("{response:?}");
        assert!(
            rendered.starts_with("List"),
            "expected a run list: {rendered}"
        );
        // The client closes after its one request, ending the server task.
        server.await.unwrap();
    }

    /// The regression that mattered in production: every real daemon requires
    /// a token, and `subscribe` used to skip the handshake entirely - so the
    /// serve gateway's event stream was refused on every connect, read the
    /// refusal as a closed stream, and re-subscribed twice a second forever
    /// while no live event ever reached a WebSocket.
    #[tokio::test]
    async fn subscribe_authenticates_against_a_tokened_daemon() {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();

        let server_events = events.clone();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let _ = handle_connection(stream, op_tx, server_events, Some(token)).await;
        });

        let client = ControlClient::for_home(id, dir.path());
        let mut stream = client.subscribe().await.expect("tokened subscribe works");
        // Emit until the server-side subscription is registered and delivers.
        let event = loop {
            let _ = events.send(completed("tokened-run"));
            tokio::select! {
                e = stream.next() => break e,
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            }
        };
        let rendered = format!("{:?}", event.expect("the event arrives"));
        assert!(rendered.contains("tokened-run"), "{rendered}");
        drop(stream);
        drop(events);
        server.await.unwrap();
    }

    /// A daemon accepting `connections` sequential connections, each served by
    /// `handle_connection` with `token`. For the stale-token tests, where the
    /// first attempt is refused (closing its connection) and the retry opens a
    /// second one.
    fn tokened_daemon(
        mut listener: ControlListener,
        token: ControlToken,
        connections: usize,
    ) -> (broadcast::Sender<WorldEvent>, tokio::task::JoinHandle<()>) {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let server_events = events.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..connections {
                let stream = listener.accept().await.unwrap().unwrap();
                let op_tx = op_tx.clone();
                let events = server_events.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, op_tx, events, Some(token)).await;
                });
            }
        });
        (events, handle)
    }

    /// The daemon mints a fresh token every start. A long-lived client that
    /// cached the previous one must re-read the file and retry, not stay
    /// bricked until IT is restarted too - `lev serve` had to be manually
    /// restarted after every daemon restart because of exactly this.
    #[tokio::test]
    async fn a_stale_token_is_refreshed_and_the_request_retried() {
        let (listener, id, dir) = test_listener();
        // The client caches the token of the "previous" daemon...
        let _stale = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id, dir.path());
        // ...then the daemon restarts and mints a fresh one.
        let fresh = ControlToken::create(dir.path()).unwrap();
        let (_events, server) = tokened_daemon(listener, fresh, 2);

        let response = client
            .list()
            .await
            .expect("refused once, refreshed, retried, served");
        let rendered = format!("{response:?}");
        assert!(rendered.starts_with("List"), "{rendered}");
        server.await.unwrap();
    }

    /// And the same recovery for the event stream.
    #[tokio::test]
    async fn subscribe_retries_with_a_refreshed_token() {
        let (listener, id, dir) = test_listener();
        let _stale = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id, dir.path());
        let fresh = ControlToken::create(dir.path()).unwrap();
        let (events, server) = tokened_daemon(listener, fresh, 2);

        let mut stream = client
            .subscribe()
            .await
            .expect("refused once, refreshed, retried, streaming");
        // Emit until the server-side subscription is registered and delivers.
        let event = loop {
            let _ = events.send(completed("post-restart-run"));
            tokio::select! {
                e = stream.next() => break e,
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            }
        };
        assert!(format!("{:?}", event.expect("the event arrives")).contains("post-restart-run"));
        drop(stream);
        drop(events);
        server.await.unwrap();
    }

    /// A client whose token file does not exist at all cannot refresh either:
    /// the refusal stands, and the error names the missing file.
    #[tokio::test]
    async fn a_missing_token_file_cannot_refresh_and_stays_refused() {
        let (listener, id, dir) = test_listener();
        // No token is ever written into the client's dir; the daemon's token
        // lives elsewhere.
        let other = tempfile::tempdir().unwrap();
        let daemons = ControlToken::create(other.path()).unwrap();
        let (_events, server) = tokened_daemon(listener, daemons, 1);

        let err = ControlClient::for_home(id, dir.path())
            .list()
            .await
            .expect_err("no token and no file to refresh from");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            err.to_string().contains("no control token was found"),
            "{err}"
        );
        server.await.unwrap();
    }

    /// When the file has not changed, a refusal stays a refusal - the retry
    /// only happens when there is a genuinely different token to present.
    #[tokio::test]
    async fn an_unchanged_token_file_is_not_retried() {
        let (listener, id, dir) = test_listener();
        // The client's dir holds a token, but the daemon wants a different one
        // (minted into another dir), and the client's file never changes.
        let _mine = ControlToken::create(dir.path()).unwrap();
        let other = tempfile::tempdir().unwrap();
        let daemons = ControlToken::create(other.path()).unwrap();
        let (_events, server) = tokened_daemon(listener, daemons, 1);

        let err = ControlClient::for_home(id, dir.path())
            .list()
            .await
            .expect_err("an unchanged token cannot get in");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        server.await.unwrap();
    }

    /// A line that is not a `WorldEvent` is skipped, not treated as the end of
    /// the stream: a newer daemon may push variants this client cannot parse,
    /// and ending the stream per unknown event would tear the subscription
    /// down over and over.
    #[tokio::test]
    async fn the_event_stream_skips_lines_it_cannot_parse() {
        let (mut listener, id, _dir) = test_listener();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _subscribe = lines.next_line().await.unwrap();
            let mut payload = String::from("this is not an event\n");
            payload.push_str(&serde_json::to_string(&completed("after-junk")).unwrap());
            payload.push('\n');
            write_half.write_all(payload.as_bytes()).await.unwrap();
            // Drop → EOF after the two lines.
        });

        let mut stream = ControlClient::new(id).subscribe().await.unwrap();
        let event = stream.next().await.expect("the parseable event arrives");
        assert!(format!("{event:?}").contains("after-junk"));
        assert!(stream.next().await.is_none(), "then a clean end");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_streams_events_to_the_client() {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, _dir) = test_listener();
        let server_events = events.clone();
        let server = tokio::spawn(async move {
            // `accept` yields `Ok(None)` for a peer that is not this user; in a
            // test the only connection is our own, so it is always `Some`.
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let _ = handle_connection(stream, op_tx, server_events, None).await;
        });

        let mut stream = ControlClient::new(id).subscribe().await.unwrap();
        // Emit until the server has subscribed and the client receives it.
        let received = loop {
            events.send(completed("run-1")).unwrap();
            tokio::select! {
                e = stream.next() => break e,
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            }
        };
        let received = received.expect("an event should have streamed to the client");
        assert_eq!(
            std::mem::discriminant(&received),
            std::mem::discriminant(&completed("x"))
        );
        // Drop the last sender so the server's stream ends and its task finishes.
        drop(events);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_errors_when_daemon_absent() {
        let dir = tempfile::tempdir().unwrap();
        let client = ControlClient::new(control_id(&dir.path().join("no-daemon")));
        assert!(client.subscribe().await.is_err());
    }

    #[tokio::test]
    async fn subscribe_stream_ends_when_the_daemon_closes() {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (mut listener, id, _dir) = test_listener();
        let server_events = events.clone();
        tokio::spawn(async move {
            // `accept` yields `Ok(None)` for a peer that is not this user; in a
            // test the only connection is our own, so it is always `Some`.
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let _ = handle_connection(stream, op_tx, server_events, None).await;
        });

        let mut stream = ControlClient::new(id).subscribe().await.unwrap();
        // Give the server time to subscribe (and drop its sender clone), then drop
        // the last sender: the channel closes and the stream ends.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(events);
        assert!(stream.next().await.is_none());
    }

    /// A connected `(client, server)` stream pair, plus the `TempDir` keeping the
    /// listener's socket alive, for driving `handle_connection` directly.
    async fn connected_pair() -> (ClientStream, ServerStream, tempfile::TempDir) {
        let (mut listener, id, dir) = test_listener();
        let (client, server) = tokio::join!(connect(&id), listener.accept());
        let server = server
            .expect("accept succeeds")
            .expect("our own connection is admitted");
        (client.unwrap(), server, dir)
    }

    #[tokio::test]
    async fn malformed_request_gets_error_and_connection_continues() {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let (client, server, _dir) = connected_pair().await;
        let handle =
            tokio::spawn(async move { handle_connection(server, op_tx, no_events(), None).await });

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
            std::mem::discriminant(&ControlResponse::List {
                runs: vec![],
                finished: vec![],
                health: DaemonHealth::default(),
            })
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
        let handle =
            tokio::spawn(async move { handle_connection(server, op_tx, no_events(), None).await });

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
            for _ in 0..4 {
                // `accept` yields `Ok(None)` for a peer that is not this user; in a
                // test the only connection is our own, so it is always `Some`.
                let stream = listener
                    .accept()
                    .await
                    .expect("accept succeeds")
                    .expect("our own connection is admitted");
                let op_tx = op_tx.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, op_tx, no_events(), None).await;
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
            std::mem::discriminant(&ControlResponse::List {
                runs: vec![],
                finished: vec![],
                health: DaemonHealth::default(),
            })
        );
        assert_eq!(
            client.shutdown().await.unwrap(),
            ControlResponse::Ok { ok: true }
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
            // `accept` yields `Ok(None)` for a peer that is not this user; in a
            // test the only connection is our own, so it is always `Some`.
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
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
            // `accept` yields `Ok(None)` for a peer that is not this user; in a
            // test the only connection is our own, so it is always `Some`.
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            // Drain the request line first, so dropping the stream is a clean EOF
            // rather than a connection reset from unread data.
            let (read_half, _write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await;
        });

        let err = ControlClient::new(id).list().await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// A daemon that accepts the connection but never answers must not hang the
    /// client: without a timeout, `lev cancel` against a wedged daemon blocks
    /// forever with no output - nothing to see, nothing to act on, and no way
    /// to kill the run.
    #[tokio::test]
    async fn client_times_out_on_a_daemon_that_never_answers() {
        let (mut listener, id, _dir) = test_listener();
        // The server reads the request, then hands both halves back and exits.
        // The test holds them, so the connection stays open with no reply ever
        // sent - a daemon that accepted the work and went quiet. Handing them
        // over (rather than parking the task on a future that never resolves)
        // lets the task actually finish.
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            // `accept` yields `Ok(None)` for a peer that is not this user; in a
            // test the only connection is our own, so it is always `Some`.
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let (read_half, write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await;
            let _ = tx.send((lines, write_half));
        });

        let err = temp_env::async_with_vars([("LEVIATH_CONTROL_TIMEOUT_SECS", Some("1"))], async {
            ControlClient::new(id).list().await.unwrap_err()
        })
        .await;
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("did not respond"), "got: {err}");
        // Held until now so the connection outlived the client's deadline.
        drop(rx);
    }

    #[test]
    fn request_timeout_honors_the_override_and_falls_back() {
        temp_env::with_var("LEVIATH_CONTROL_TIMEOUT_SECS", Some("7"), || {
            assert_eq!(request_timeout(), std::time::Duration::from_secs(7));
        });
        // `0` disables the deadline entirely, for debugging a legitimately slow
        // daemon.
        temp_env::with_var("LEVIATH_CONTROL_TIMEOUT_SECS", Some("0"), || {
            assert_eq!(request_timeout(), std::time::Duration::MAX);
        });
        // Garbage and absence both fall back rather than failing the command.
        temp_env::with_var("LEVIATH_CONTROL_TIMEOUT_SECS", Some("soon"), || {
            assert_eq!(
                request_timeout(),
                std::time::Duration::from_secs(DEFAULT_CONTROL_TIMEOUT_SECS)
            );
        });
        temp_env::with_var_unset("LEVIATH_CONTROL_TIMEOUT_SECS", || {
            assert_eq!(
                request_timeout(),
                std::time::Duration::from_secs(DEFAULT_CONTROL_TIMEOUT_SECS)
            );
        });
    }

    /// A spawn connects the blueprint's MCP servers first (30s each), so it gets
    /// a longer floor than the interactive ops - otherwise a slow-but-succeeding
    /// spawn is reported to the user as a timeout.
    #[test]
    fn spawn_gets_a_longer_deadline_than_other_ops() {
        let spawn = ControlRequest::Spawn {
            args: Box::new(SpawnArgs::default()),
        };
        let cancel = ControlRequest::Cancel {
            run_id: "r".to_string(),
        };
        temp_env::with_var_unset("LEVIATH_CONTROL_TIMEOUT_SECS", || {
            assert_eq!(
                timeout_for(&spawn),
                std::time::Duration::from_secs(SPAWN_CONTROL_TIMEOUT_SECS)
            );
            assert_eq!(
                timeout_for(&cancel),
                std::time::Duration::from_secs(DEFAULT_CONTROL_TIMEOUT_SECS)
            );
        });
        // A configured value larger than the floor wins for both.
        temp_env::with_var("LEVIATH_CONTROL_TIMEOUT_SECS", Some("900"), || {
            assert_eq!(timeout_for(&spawn), std::time::Duration::from_secs(900));
            assert_eq!(timeout_for(&cancel), std::time::Duration::from_secs(900));
        });
        // A deliberately disabled deadline stays disabled, spawn included.
        temp_env::with_var("LEVIATH_CONTROL_TIMEOUT_SECS", Some("0"), || {
            assert_eq!(timeout_for(&spawn), std::time::Duration::MAX);
        });
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
            ControlResponse::List {
                runs: vec![],
                finished: vec![],
                health: DaemonHealth::default(),
            }
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
                        args: Box::new(SpawnArgs::default())
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

    // ── Riding out a daemon restart ─────────────────────────────────────────

    /// A daemon at `id` with `token`, introducing itself as `identity`, that
    /// serves `connections` connections through the real handshake and then
    /// stops accepting. The fake host behind it answers every op.
    ///
    /// The returned task ends only once every connection it served has been
    /// closed, not merely accepted. A test that "restarts" the daemon rebinds
    /// the same id, and on Windows a pipe instance still held by a
    /// connection task makes that bind fail with `AddrInUse`.
    fn identified_daemon(
        mut listener: ControlListener,
        token: ControlToken,
        identity: DaemonIdentity,
        connections: usize,
    ) -> (broadcast::Sender<WorldEvent>, tokio::task::JoinHandle<()>) {
        let (events, _r) = broadcast::channel::<WorldEvent>(16);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        spawn_fake_host(op_rx);
        let server_events = events.clone();
        let handle = tokio::spawn(async move {
            let mut served = Vec::new();
            for _ in 0..connections {
                let stream = listener.accept().await.unwrap().unwrap();
                let op_tx = op_tx.clone();
                let events = server_events.clone();
                let token = token.clone();
                let identity = identity.clone();
                served.push(tokio::spawn(async move {
                    let _ =
                        handle_connection_as(stream, op_tx, events, Some(token), identity).await;
                }));
            }
            // The listener goes now, and so does this task's hold on the event
            // sender: a subscribed connection ends only when every sender is gone,
            // and the test's own clone is the one that should decide that.
            drop(listener);
            drop(server_events);
            for connection in served {
                connection.await.unwrap();
            }
        });
        (events, handle)
    }

    /// An identity for a daemon on the same code as this process, at `pid`.
    fn same_code(pid: u32) -> DaemonIdentity {
        DaemonIdentity {
            pid,
            ..DaemonIdentity::this_process("build-a")
        }
    }

    #[test]
    fn this_process_identity_carries_the_crate_version_and_the_given_build() {
        let me = DaemonIdentity::this_process("abc123");
        assert_eq!(me.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(me.build, "abc123");
        assert_eq!(me.pid, std::process::id());
        assert_eq!(
            me.to_string(),
            format!(
                "{} (build abc123, pid {})",
                env!("CARGO_PKG_VERSION"),
                std::process::id()
            )
        );
    }

    /// A process that never said which credentials it can see is "unknown", not
    /// "sees nothing" - the distinction a caller needs to avoid reporting a
    /// missing key against a daemon that simply predates the field.
    #[test]
    fn an_identity_that_did_not_report_tool_env_answers_unknown() {
        let me = DaemonIdentity::this_process("abc123");
        assert_eq!(me.tool_env, None);
        assert_eq!(me.sees_tool_env("BRAVE_API_KEY"), None);
    }

    /// Reporting an empty list is a real answer - "asked, saw none" - and stays
    /// distinguishable from having said nothing at all.
    #[test]
    fn reporting_no_tool_env_is_different_from_not_reporting() {
        let blind = DaemonIdentity::this_process("abc123").with_tool_env(Vec::new());
        assert_eq!(blind.tool_env, Some(Vec::new()));
        assert_eq!(blind.sees_tool_env("BRAVE_API_KEY"), Some(false));
    }

    #[test]
    fn a_reported_tool_env_answers_by_name() {
        let seeing =
            DaemonIdentity::this_process("abc123").with_tool_env(vec!["BRAVE_API_KEY".to_string()]);
        assert_eq!(seeing.sees_tool_env("BRAVE_API_KEY"), Some(true));
        assert_eq!(seeing.sees_tool_env("OTHER_KEY"), Some(false));
    }

    /// The names travel; nothing else does. A client on an older build reads
    /// the rest of the identity and treats the field as absent.
    #[test]
    fn tool_env_round_trips_and_is_optional_on_the_wire() {
        let seeing =
            DaemonIdentity::this_process("abc123").with_tool_env(vec!["BRAVE_API_KEY".to_string()]);
        let json = serde_json::to_string(&seeing).expect("an identity serializes");
        assert_eq!(
            serde_json::from_str::<DaemonIdentity>(&json).expect("and parses back"),
            seeing
        );
        let older = format!(
            r#"{{"version":"{}","build":"abc123","pid":{}}}"#,
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
        let parsed: DaemonIdentity = serde_json::from_str(&older).expect("an older reply parses");
        assert_eq!(parsed.sees_tool_env("BRAVE_API_KEY"), None);
    }

    /// Which credentials a process can see says nothing about which code it
    /// runs, so it must not make two identities look like different builds.
    #[test]
    fn tool_env_does_not_affect_the_code_comparison() {
        let blind = same_code(1);
        let seeing = same_code(1).with_tool_env(vec!["BRAVE_API_KEY".to_string()]);
        assert!(blind.same_code_as(&seeing));
        assert!(!seeing.to_string().contains("BRAVE"), "{seeing}");
    }

    /// Versions must match; builds must match when both are known; a side that
    /// does not know its build cannot contradict the other.
    #[test]
    fn same_code_compares_versions_and_known_builds() {
        let a = same_code(1);
        let mut b = same_code(2);
        assert!(a.same_code_as(&b), "a different pid is the same code");
        b.build = "build-b".to_string();
        assert!(!a.same_code_as(&b), "a different build is different code");
        b.build = DaemonIdentity::unknown_build();
        assert!(a.same_code_as(&b), "an unknown build cannot contradict");
        assert!(b.same_code_as(&a), "in either direction");
        let mut c = same_code(3);
        c.version = "0.0.1".to_string();
        assert!(!a.same_code_as(&c), "a different version is different code");
    }

    /// The identity's build is optional on the wire (an older daemon never
    /// sends one), and `hello: false` is left off the request entirely so a
    /// daemon that predates the field sees exactly the request it always saw.
    #[test]
    fn the_handshake_additions_are_backward_compatible_on_the_wire() {
        let plain = serde_json::to_value(ControlRequest::Authenticate {
            token: "t".to_string(),
            hello: false,
        })
        .unwrap();
        assert_eq!(
            plain,
            serde_json::json!({"op": "authenticate", "token": "t"})
        );
        let asking = serde_json::to_value(ControlRequest::Authenticate {
            token: "t".to_string(),
            hello: true,
        })
        .unwrap();
        assert_eq!(asking["hello"], true);
        let parsed: ControlRequest =
            serde_json::from_value(serde_json::json!({"op": "authenticate", "token": "t"}))
                .unwrap();
        assert_eq!(
            parsed,
            ControlRequest::Authenticate {
                token: "t".to_string(),
                hello: false
            }
        );
        let identity: DaemonIdentity =
            serde_json::from_value(serde_json::json!({"version": "0.4.0", "pid": 7})).unwrap();
        assert_eq!(identity.build, DaemonIdentity::unknown_build());
    }

    #[test]
    fn only_the_reading_requests_are_read_only() {
        let run = || "r".to_string();
        let read_only = [
            ControlRequest::Authenticate {
                token: run(),
                hello: true,
            },
            ControlRequest::Status { run_id: run() },
            ControlRequest::List,
            ControlRequest::ListInteractions,
            ControlRequest::Subscribe,
        ];
        let mutating = [
            ControlRequest::Spawn {
                args: Box::new(SpawnArgs::default()),
            },
            ControlRequest::Pause { run_id: run() },
            ControlRequest::Resume { run_id: run() },
            ControlRequest::Cancel { run_id: run() },
            ControlRequest::Message {
                agent_id: run(),
                content: run(),
                target_region: None,
            },
            ControlRequest::AnswerInteraction {
                response: InteractionResponse {
                    request_id: run(),
                    value: None,
                    choice_index: None,
                    approved: None,
                    scope: None,
                },
            },
            ControlRequest::CancelInteraction { request_id: run() },
            ControlRequest::Shutdown,
        ];
        assert!(read_only.iter().all(ControlRequest::is_read_only));
        assert!(!mutating.iter().any(ControlRequest::is_read_only));
    }

    /// The reasons a connect can fail that mean "no daemon answered", versus
    /// the ones where something did.
    #[test]
    fn transient_errors_are_the_absent_daemon_ones() {
        use std::io::ErrorKind::*;
        for kind in [
            NotFound,
            ConnectionRefused,
            ConnectionReset,
            ConnectionAborted,
            BrokenPipe,
            UnexpectedEof,
        ] {
            assert!(is_transient(&std::io::Error::new(kind, "x")), "{kind:?}");
        }
        for kind in [PermissionDenied, InvalidData, TimedOut, Unsupported, Other] {
            assert!(!is_transient(&std::io::Error::new(kind, "x")), "{kind:?}");
        }
    }

    /// `ERROR_PIPE_BUSY` has no `ErrorKind` of its own and is retryable by
    /// definition.
    #[cfg(windows)]
    #[test]
    fn a_busy_named_pipe_is_transient() {
        assert!(is_transient(&std::io::Error::from_raw_os_error(231)));
    }

    /// The daemon introduces itself in the handshake, the client records it,
    /// and a client whose build matches sees no mismatch. Before any handshake
    /// the client knows nothing and says so.
    #[tokio::test]
    async fn the_client_learns_who_it_reached_from_the_handshake() {
        let (listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();
        let (_events, server) = identified_daemon(listener, token, same_code(41), 1);
        let client = ControlClient::for_home(id, dir.path()).with_build("build-a");
        assert_eq!(
            client.link(),
            LinkStatus {
                daemon: None,
                restarts: 0,
                reachable: true,
            }
        );
        client.list().await.expect("served");
        assert_eq!(client.link().daemon, Some(same_code(41)));
        assert_eq!(
            client.link().restarts,
            0,
            "learning who it is is not a restart"
        );
        assert!(client.link().reachable);
        assert_eq!(client.code_mismatch(), None);
        server.await.unwrap();
    }

    /// A daemon that predates the question answers the handshake with the
    /// bare `Ok` it always did. That is accepted, and the client simply keeps
    /// not knowing who it reached.
    #[tokio::test]
    async fn a_daemon_that_predates_the_handshake_is_served_without_an_identity() {
        let (mut listener, id, dir) = test_listener();
        let _token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id, dir.path()).with_build("build-a");
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _hello = lines.next_line().await.unwrap();
            write_line(&mut write_half, &ControlResponse::Ok { ok: true }).await;
            let _request = lines.next_line().await.unwrap();
            write_line(&mut write_half, &ControlResponse::Ok { ok: true }).await;
        });
        client.shutdown().await.expect("served");
        assert_eq!(client.link().daemon, None);
        assert_eq!(client.code_mismatch(), None);
        server.await.unwrap();
    }

    /// The plain `handle_connection` (embedders, tests) introduces the daemon
    /// with an unknown build, which a client with a known build accepts as
    /// possibly-the-same code.
    #[tokio::test]
    async fn a_daemon_that_does_not_know_its_build_is_not_a_mismatch() {
        let (listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();
        let (_events, server) = tokened_daemon(listener, token, 1);
        let client = ControlClient::for_home(id, dir.path()).with_build("build-a");
        client.list().await.expect("served");
        let daemon = client.link().daemon.expect("introduced itself");
        assert_eq!(daemon.build, DaemonIdentity::unknown_build());
        assert_eq!(client.code_mismatch(), None);
        server.await.unwrap();
    }

    /// A different daemon behind the same socket - a new pid - counts as a
    /// restart, once per change; the same daemon answering again does not.
    #[tokio::test]
    async fn a_new_daemon_behind_the_socket_counts_as_a_restart() {
        let (listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();
        let (_events, first) = identified_daemon(listener, token.clone(), same_code(1), 2);
        let client = ControlClient::for_home(id.clone(), dir.path()).with_build("build-a");
        client.list().await.expect("served by the first daemon");
        client.list().await.expect("and again");
        assert_eq!(client.link().restarts, 0, "the same daemon twice");
        first.await.unwrap();

        // "Restart": a new listener on the same id, a new pid in the greeting.
        let listener = bind_control_listener(&id).unwrap();
        let (_events, second) = identified_daemon(listener, token, same_code(2), 1);
        client.list().await.expect("served by the second daemon");
        assert_eq!(client.link().restarts, 1);
        assert_eq!(client.link().daemon.map(|d| d.pid), Some(2));
        second.await.unwrap();
    }

    /// A request that lands while no daemon is listening waits for one, and
    /// is served by the daemon that binds a moment later. This is the restart
    /// window: `lev serve` answered 503 across it, `lev ps` printed "Daemon
    /// not reachable", and the ACP bridge refused the turn.
    #[tokio::test]
    async fn a_request_during_the_restart_window_waits_for_the_new_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id.clone(), dir.path())
            .with_reconnect_grace(std::time::Duration::from_secs(5));
        assert!(!is_daemon_running(&id), "nothing listening yet");

        // The daemon comes back a little after the request goes out.
        let late = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            let listener = bind_control_listener(&id).unwrap();
            let (_events, server) = identified_daemon(listener, token, same_code(9), 1);
            server.await.unwrap();
        });
        let response = client.list().await.expect("waited out the window");
        assert!(format!("{response:?}").starts_with("List"));
        assert!(client.link().reachable, "the outage is over");
        late.await.unwrap();
    }

    /// The wait is per outage, not per request. Once the daemon has been gone
    /// longer than the grace, callers fail fast instead of each waiting the
    /// whole grace again - and the link reads as unreachable meanwhile.
    #[tokio::test]
    async fn an_outage_older_than_the_grace_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let client = ControlClient::for_home(id, dir.path())
            .with_reconnect_grace(std::time::Duration::from_millis(30));
        // The first caller pays the grace (a backoff sleep or two)...
        let started = std::time::Instant::now();
        assert!(client.list().await.is_err());
        assert!(started.elapsed() >= std::time::Duration::from_millis(30));
        assert!(!client.link().reachable);
        // ...and every caller after it fails at once.
        let started = std::time::Instant::now();
        assert!(client.list().await.is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(30));
    }

    /// A clone can opt out of the wait: `lev dash` polls on its render loop
    /// and must not freeze the screen for the length of an outage.
    #[tokio::test]
    async fn a_zero_grace_clone_reports_an_absent_daemon_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let patient = ControlClient::for_home(id, dir.path()).with_reconnect_grace(RESTART_GRACE);
        let hasty = patient
            .clone()
            .with_reconnect_grace(std::time::Duration::ZERO);
        let started = std::time::Instant::now();
        assert!(hasty.list().await.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // The clones share what they learn: the patient one now knows too.
        assert!(!patient.link().reachable);
    }

    /// The daemon can vanish *after* the request was written. A request that
    /// only reads is simply asked again of the daemon that comes back...
    #[tokio::test]
    async fn a_read_only_request_cut_off_before_its_reply_is_asked_again() {
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id.clone(), dir.path())
            .with_reconnect_grace(std::time::Duration::from_secs(5));
        // First connection: handshake, read the request, then hang up.
        let dying = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _hello = lines.next_line().await.unwrap();
            write_line(&mut write_half, &authenticated_reply(true, &same_code(1))).await;
            let _request = lines.next_line().await.unwrap();
            drop(listener);
            // Dropping the stream: EOF before any reply.
        });
        // The replacement binds a moment after the first daemon is gone and
        // answers properly.
        let late = tokio::spawn(async move {
            dying.await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let listener = bind_control_listener(&id).unwrap();
            let (_events, server) = identified_daemon(listener, token, same_code(2), 1);
            server.await.unwrap();
        });
        let response = client.list().await.expect("asked again of the new daemon");
        assert!(format!("{response:?}").starts_with("List"));
        late.await.unwrap();
    }

    /// ...while one that mutates is not, because it may already have happened
    /// - the daemon may have spawned the run and died before saying so, and
    /// asking again is how a run gets started twice.
    #[tokio::test]
    async fn a_mutating_request_cut_off_before_its_reply_is_not_repeated() {
        let (mut listener, id, dir) = test_listener();
        let _token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id, dir.path())
            .with_reconnect_grace(std::time::Duration::from_secs(5));
        let dying = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _hello = lines.next_line().await.unwrap();
            write_line(&mut write_half, &authenticated_reply(true, &same_code(1))).await;
            let _request = lines.next_line().await.unwrap();
        });
        let started = std::time::Instant::now();
        let err = client
            .spawn(SpawnArgs::default())
            .await
            .expect_err("a spawn that got no reply is reported, not repeated");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "no wait"
        );
        dying.await.unwrap();
    }

    /// The one mutating reply that *is* safe to repeat: the daemon saying it
    /// was already shutting down, which means the request was dropped
    /// unprocessed. The spawn goes to the daemon that replaces it.
    #[tokio::test]
    async fn a_spawn_the_dying_daemon_dropped_goes_to_its_replacement() {
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id.clone(), dir.path())
            .with_reconnect_grace(std::time::Duration::from_secs(5));
        // A daemon whose host is already gone: `dispatch` cannot deliver the
        // op, and says so.
        let draining_token = token.clone();
        let draining = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (op_tx, op_rx) = mpsc::unbounded_channel();
            drop(op_rx);
            let _ = handle_connection_as(
                stream,
                op_tx,
                no_events(),
                Some(draining_token),
                same_code(1),
            )
            .await;
            drop(listener);
        });
        // Its replacement, a moment later, with a live host.
        let late = tokio::spawn(async move {
            draining.await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let listener = bind_control_listener(&id).unwrap();
            let (_events, server) = identified_daemon(listener, token, same_code(2), 1);
            server.await.unwrap();
        });
        let args = SpawnArgs {
            run_id: "run-x".to_string(),
            ..Default::default()
        };
        let response = client
            .spawn(args)
            .await
            .expect("spawned by the replacement");
        assert_eq!(
            response,
            ControlResponse::Spawned {
                run_id: "run-x".to_string()
            }
        );
        late.await.unwrap();
    }

    /// Without the wait, the same reply is handed back as it always was.
    #[tokio::test]
    async fn a_shutting_down_reply_is_returned_when_there_is_no_grace() {
        let (mut listener, id, dir) = test_listener();
        let token = ControlToken::create(dir.path()).unwrap();
        let client =
            ControlClient::for_home(id, dir.path()).with_reconnect_grace(std::time::Duration::ZERO);
        let draining = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (op_tx, op_rx) = mpsc::unbounded_channel();
            drop(op_rx);
            let _ = handle_connection(stream, op_tx, no_events(), Some(token)).await;
        });
        let response = client.spawn(SpawnArgs::default()).await.unwrap();
        assert_eq!(
            response,
            ControlResponse::Error {
                message: SHUTTING_DOWN.to_string()
            }
        );
        draining.await.unwrap();
    }

    /// The event stream is subscribed with the same patience: a subscribe
    /// during the restart window attaches to the daemon that comes back.
    #[tokio::test]
    async fn a_subscribe_during_the_restart_window_attaches_to_the_new_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id.clone(), dir.path())
            .with_reconnect_grace(std::time::Duration::from_secs(5));
        let (events_tx, events_rx) = tokio::sync::oneshot::channel();
        let late = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let listener = bind_control_listener(&id).unwrap();
            let (events, server) = identified_daemon(listener, token, same_code(3), 1);
            let _ = events_tx.send(events.clone());
            server.await.unwrap();
        });
        let mut stream = client.subscribe().await.expect("attached after the wait");
        let events = events_rx.await.unwrap();
        let event = loop {
            let _ = events.send(completed("late-run"));
            tokio::select! {
                e = stream.next() => break e,
                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
            }
        };
        assert!(format!("{:?}", event.expect("the event arrives")).contains("late-run"));
        drop(stream);
        drop(events);
        late.await.unwrap();
    }

    /// A daemon on different code that answers with something this client
    /// cannot read: not "not reachable", and not a daemon-side bug, but the
    /// one failure a restart of the daemon does not fix. The error says which
    /// process to restart.
    #[tokio::test]
    async fn an_unreadable_reply_from_a_daemon_on_other_code_names_the_mismatch() {
        let (mut listener, id, dir) = test_listener();
        let _token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id, dir.path()).with_build("build-a");
        let mut other = same_code(5);
        other.build = "build-b".to_string();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _hello = lines.next_line().await.unwrap();
            write_line(&mut write_half, &authenticated_reply(true, &other)).await;
            let _request = lines.next_line().await.unwrap();
            let _ = write_half
                .write_all(b"{\"result\":\"from_the_future\"}\n")
                .await;
        });
        let err = client.list().await.expect_err("could not read the reply");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
        let text = err.to_string();
        assert!(text.contains("build build-b"), "{text}");
        assert!(text.contains("built from version"), "{text}");
        assert!(text.contains("restart this process"), "{text}");
        assert!(text.contains("could not be read"), "{text}");
        let mismatch = client.code_mismatch().expect("recorded");
        assert_eq!(mismatch.daemon.build, "build-b");
        assert_eq!(mismatch.client.build, "build-a");
        server.await.unwrap();
    }

    /// The same unreadable reply from a daemon on the *same* code is the plain
    /// parse error it always was: there is no update to blame.
    #[tokio::test]
    async fn an_unreadable_reply_from_the_same_code_is_a_plain_parse_error() {
        let (mut listener, id, dir) = test_listener();
        let _token = ControlToken::create(dir.path()).unwrap();
        let client = ControlClient::for_home(id, dir.path()).with_build("build-a");
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap().unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _hello = lines.next_line().await.unwrap();
            write_line(&mut write_half, &authenticated_reply(true, &same_code(5))).await;
            let _request = lines.next_line().await.unwrap();
            let _ = write_half.write_all(b"not json\n").await;
        });
        let err = client.list().await.expect_err("could not read the reply");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        server.await.unwrap();
    }

    /// And the mirror image: a request the daemon could not read. Named as a
    /// mismatch when the two run different code, passed through as the
    /// daemon's own error when they do not.
    #[tokio::test]
    async fn a_request_the_daemon_cannot_read_is_a_mismatch_only_across_code() {
        for (build, expect_mismatch) in [("build-b", true), ("build-a", false)] {
            let (mut listener, id, dir) = test_listener();
            let _token = ControlToken::create(dir.path()).unwrap();
            let client = ControlClient::for_home(id, dir.path()).with_build("build-a");
            let mut daemon = same_code(5);
            daemon.build = build.to_string();
            let server = tokio::spawn(async move {
                let stream = listener.accept().await.unwrap().unwrap();
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                let _hello = lines.next_line().await.unwrap();
                write_line(&mut write_half, &authenticated_reply(true, &daemon)).await;
                let _request = lines.next_line().await.unwrap();
                write_line(
                    &mut write_half,
                    &ControlResponse::Error {
                        message: format!("{INVALID_REQUEST}: unknown variant `list`"),
                    },
                )
                .await;
            });
            let result = client.list().await;
            match expect_mismatch {
                true => {
                    let err = result.expect_err("named as a mismatch");
                    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
                    assert!(err.to_string().contains("could not read this request"));
                }
                false => {
                    let response = result.expect("the daemon's own error, passed through");
                    assert_eq!(
                        response,
                        ControlResponse::Error {
                            message: format!("{INVALID_REQUEST}: unknown variant `list`"),
                        }
                    );
                }
            }
            server.await.unwrap();
        }
    }
}
