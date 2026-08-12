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
const AUTH_REQUIRED: &str = "authentication";

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
    handle_connection_capped(stream, op_tx, events, token, MAX_REQUEST_BYTES).await
}

/// [`handle_connection`] with the per-request cap injected.
///
/// The cap is a parameter purely so a test can cross it without pushing 8 MiB
/// through a duplex - and crossing it is the only way to tell a per-request
/// budget from a per-connection one.
async fn handle_connection_capped<S>(
    stream: S,
    op_tx: UnboundedSender<ControlOp>,
    events: broadcast::Sender<WorldEvent>,
    token: Option<ControlToken>,
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
                Ok(ControlRequest::Authenticate { token: presented }) => {
                    match token.as_ref().is_some_and(|t| t.matches(&presented)) {
                        true => {
                            authenticated = true;
                            write_line(&mut write_half, &ControlResponse::Ok { ok: true }).await;
                            continue;
                        }
                        false => "authentication failed",
                    }
                }
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
            // Already authenticated: a repeat is harmless, not an error.
            Ok(ControlRequest::Authenticate { .. }) => ControlResponse::Ok { ok: true },
            Ok(req) => dispatch(req, &op_tx).await,
            Err(e) => ControlResponse::Error {
                message: format!("invalid request: {e}"),
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

/// How long a control request waits for the daemon before giving up, when
/// `LEVIATH_CONTROL_TIMEOUT_SECS` is unset.
///
/// Generous enough to cover a busy daemon's control loop, short enough that a
/// wedged one is reported rather than waited on indefinitely.
pub const DEFAULT_CONTROL_TIMEOUT_SECS: u64 = 30;

/// Floor on the deadline for a `Spawn`, which does more work than the other ops:
/// the daemon connects the blueprint's MCP servers before spawning, and each of
/// those has its own 30s connect timeout, so a blueprint declaring several
/// servers can legitimately outlast the ordinary deadline. Without this floor a
/// slow-but-succeeding spawn would be reported to the user as a timeout.
pub const SPAWN_CONTROL_TIMEOUT_SECS: u64 = 300;

/// The deadline for one control request. `LEVIATH_CONTROL_TIMEOUT_SECS`
/// overrides it; `0` disables the deadline (for debugging a daemon that is
/// legitimately slow). An unparseable value falls back to the default.
pub fn request_timeout() -> std::time::Duration {
    let secs = std::env::var("LEVIATH_CONTROL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CONTROL_TIMEOUT_SECS);
    match secs {
        0 => std::time::Duration::MAX,
        secs => std::time::Duration::from_secs(secs),
    }
}

/// The deadline for `req`: [`request_timeout`], raised to at least
/// [`SPAWN_CONTROL_TIMEOUT_SECS`] for a `Spawn`. An explicitly disabled deadline
/// (`0`) stays disabled.
fn timeout_for(req: &ControlRequest) -> std::time::Duration {
    let base = request_timeout();
    match req {
        ControlRequest::Spawn { .. } if base != std::time::Duration::MAX => {
            base.max(std::time::Duration::from_secs(SPAWN_CONTROL_TIMEOUT_SECS))
        }
        _ => base,
    }
}

/// The client half of the control transport: connects to the daemon's control
/// socket (resolved from a [`ControlId`]), sends one [`ControlRequest`], and
/// reads back its [`ControlResponse`]. A fresh connection per request keeps it
/// simple and stateless.
#[derive(Clone)]
pub struct ControlClient {
    id: ControlId,
    /// The token to present, shared across clones so a refresh (see
    /// [`Self::refresh_token`]) reaches every handler holding a clone. A `std`
    /// mutex held only for reads/writes of the option, never across `.await`.
    token: std::sync::Arc<std::sync::Mutex<Option<ControlToken>>>,
    /// Where the token was looked for, so a refusal can name the file - and so
    /// a refusal can re-read it: the daemon mints a fresh token on every start,
    /// which is exactly when a long-lived client's cached copy goes stale.
    token_dir: Option<PathBuf>,
}

impl ControlClient {
    /// A client for the control socket identified by `id`, with no token.
    ///
    /// Only reaches a daemon that runs without one, which in practice means a
    /// test driving the protocol directly. Real callers use
    /// [`with_token`](Self::with_token) - see [`ControlToken`].
    pub fn new(id: impl Into<ControlId>) -> Self {
        Self {
            id: id.into(),
            token: std::sync::Arc::new(std::sync::Mutex::new(None)),
            token_dir: None,
        }
    }

    /// Present `token` on every connection this client opens.
    pub fn with_token(self, token: ControlToken) -> Self {
        *leviath_core::sync::lock(&self.token) = Some(token);
        self
    }

    /// A client that reads the daemon's token out of `dir`.
    ///
    /// A missing token file is **not** an error here. It has two very different
    /// causes - no daemon is running, or one is running that predates tokens -
    /// and the client cannot tell them apart, while the daemon can. Refusing to
    /// even construct a client would make the second case unrecoverable: an
    /// upgraded CLI could not ask the still-running pre-token daemon to shut
    /// down, so it could neither stop it nor start a replacement, and the error
    /// it printed ("Is the daemon running? Start it with `lev daemon start`")
    /// would be advice the user was already following.
    ///
    /// The daemon is the enforcer. A client with no token connects, is refused
    /// if the daemon requires one, and reports *that* - which is accurate.
    pub fn for_home(id: impl Into<ControlId>, dir: &Path) -> Self {
        Self {
            id: id.into(),
            token: std::sync::Arc::new(std::sync::Mutex::new(ControlToken::load(dir).ok())),
            token_dir: Some(dir.to_path_buf()),
        }
    }

    /// The token to present right now, if any.
    fn current_token(&self) -> Option<ControlToken> {
        leviath_core::sync::lock(&self.token).clone()
    }

    /// Re-read the token file after a refusal. Returns `true` when the file
    /// held a *different* token than the cached one - the daemon restarted and
    /// minted afresh, so a retry with the new token can succeed. `false` (file
    /// missing, unreadable, or unchanged) means a retry would be refused the
    /// same way.
    ///
    /// This is what lets a long-lived client (`lev serve`, `lev dash`, the ACP
    /// bridge) survive a daemon restart: tokens are per-daemon-start by design,
    /// and before this the only recovery was restarting the client too.
    fn refresh_token(&self) -> bool {
        let Some(dir) = &self.token_dir else {
            return false;
        };
        let Ok(fresh) = ControlToken::load(dir) else {
            return false;
        };
        let mut cached = leviath_core::sync::lock(&self.token);
        let changed = cached.as_ref().is_none_or(|t| !t.matches(fresh.expose()));
        if changed {
            *cached = Some(fresh);
        }
        changed
    }

    /// Why the daemon refused us, said in terms of what the user can do.
    fn refused(&self) -> std::io::Error {
        let detail = match (self.current_token(), &self.token_dir) {
            (None, Some(dir)) => format!(
                "no control token was found at {}. If a daemon is running, it was \
                 started by a different user or before this file existed - restart \
                 it with `lev daemon restart`.",
                ControlToken::path(dir).display()
            ),
            _ => "the daemon refused this client's control token. Restart it with \
                  `lev daemon restart` to issue a fresh one."
                .to_string(),
        };
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, detail)
    }

    /// Send one request and await its response. Errors if the daemon can't be
    /// reached, does not answer within [`request_timeout`], the connection closes
    /// before a reply, or the reply doesn't parse.
    pub async fn request(&self, req: &ControlRequest) -> std::io::Result<ControlResponse> {
        // The daemon services control ops from a single loop, so one op that
        // takes a long time (or a wedged world) delays every other client. With
        // no deadline, `lev cancel` and the dashboard simply hung - no output, no
        // error, nothing to act on. A timeout turns that into a failure the
        // caller can fall back from.
        let deadline = timeout_for(req);
        tokio::time::timeout(deadline, self.request_with_refresh(req))
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("the daemon did not respond within {}s", deadline.as_secs()),
                ))
            })
    }

    /// [`Self::request_uncapped`], retried once with a re-read token when the
    /// daemon refuses the cached one. The daemon mints a fresh token every
    /// start, so a refusal is most often not an intruder but a restart this
    /// long-lived client slept through - `lev serve` held one client for its
    /// whole life, and a single daemon restart (a crash, `lev daemon restart`,
    /// or `ensure_daemon_running` replacing a stale build) bricked every
    /// control op it made from then on until someone restarted serve too.
    async fn request_with_refresh(&self, req: &ControlRequest) -> std::io::Result<ControlResponse> {
        match self.request_uncapped(req).await {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && self.refresh_token() => {
                self.request_uncapped(req).await
            }
            other => other,
        }
    }

    /// [`Self::request`] without the deadline.
    async fn request_uncapped(&self, req: &ControlRequest) -> std::io::Result<ControlResponse> {
        let stream = connect(&self.id).await?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        self.authenticate(&mut write_half, &mut lines).await?;

        let mut line = serde_json::to_string(req).expect("ControlRequest serializes");
        line.push('\n');
        // A failed write means the peer is already gone; the read below then sees
        // EOF and returns the error, so the write needs no separate propagation.
        let _ = write_half.write_all(line.as_bytes()).await;

        match lines.next_line().await? {
            Some(resp_line) => {
                let parsed: ControlResponse = serde_json::from_str(&resp_line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                // A daemon that refused us: report what to do about it, not the
                // wire message. Reached when this client had no token to send
                // and so never ran the handshake.
                match &parsed {
                    ControlResponse::Error { message } if message.starts_with(AUTH_REQUIRED) => {
                        Err(self.refused())
                    }
                    _ => Ok(parsed),
                }
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "control connection closed before a response",
            )),
        }
    }

    /// Authenticate a fresh connection: send the token (when there is one) and
    /// read the daemon's verdict. On every connection, because the client opens
    /// a fresh one per request, so there is no session to carry the proof
    /// across. With no token nothing is sent - a daemon that predates tokens
    /// serves the request, and one that requires them refuses it, which is the
    /// outcome to report either way.
    async fn authenticate(
        &self,
        write_half: &mut tokio::io::WriteHalf<ClientStream>,
        lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<ClientStream>>>,
    ) -> std::io::Result<()> {
        let Some(token) = self.current_token() else {
            return Ok(());
        };
        let hello = ControlRequest::Authenticate {
            token: token.expose().to_string(),
        };
        let mut line = serde_json::to_string(&hello).expect("ControlRequest serializes");
        line.push('\n');
        let _ = write_half.write_all(line.as_bytes()).await;

        // `.ok().flatten()`: a read error and a clean EOF are the same fact
        // here - the daemon did not answer the handshake - and giving the
        // error its own `?` arm leaves a branch no test can drive.
        match lines.next_line().await.ok().flatten() {
            Some(resp) => match serde_json::from_str::<ControlResponse>(&resp) {
                Ok(ControlResponse::Ok { ok: true }) => Ok(()),
                _ => Err(self.refused()),
            },
            None => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "control connection closed during authentication",
            )),
        }
    }

    /// Spawn a new agent.
    pub async fn spawn(&self, args: SpawnArgs) -> std::io::Result<ControlResponse> {
        self.request(&ControlRequest::Spawn {
            args: Box::new(args),
        })
        .await
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

    /// Ask the daemon to shut down.
    pub async fn shutdown(&self) -> std::io::Result<ControlResponse> {
        self.request(&ControlRequest::Shutdown).await
    }

    /// Open a pushed event stream: connect, authenticate, send `Subscribe`, and
    /// return a reader that yields [`WorldEvent`]s until the daemon closes the
    /// connection. The HTTP/WS gateway uses this instead of polling.
    ///
    /// Authenticated like every other request - it was not, which meant a
    /// production daemon (they all require a token) refused every subscription
    /// before it began: the serve gateway's event loop read the refusal as a
    /// closed stream and silently re-subscribed twice a second forever, and no
    /// live event ever reached a WebSocket. Only tokenless test daemons ever
    /// saw the stream work. Retried once with a re-read token on refusal, same
    /// as [`Self::request`].
    pub async fn subscribe(&self) -> std::io::Result<WorldEventStream> {
        match self.subscribe_uncapped().await {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && self.refresh_token() => {
                self.subscribe_uncapped().await
            }
            other => other,
        }
    }

    /// One subscribe attempt with the currently-cached token.
    async fn subscribe_uncapped(&self) -> std::io::Result<WorldEventStream> {
        let stream = connect(&self.id).await?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        self.authenticate(&mut write_half, &mut lines).await?;
        let mut line =
            serde_json::to_string(&ControlRequest::Subscribe).expect("ControlRequest serializes");
        line.push('\n');
        // A failed write means the peer is already gone; the read side then sees
        // EOF and `next` returns `None`, so the write needs no separate handling.
        let _ = write_half.write_all(line.as_bytes()).await;
        Ok(WorldEventStream {
            lines,
            _write: write_half,
        })
    }
}

/// A reader over a `Subscribe` connection, yielding [`WorldEvent`]s the daemon
/// pushes.
pub struct WorldEventStream {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<ClientStream>>>,
    // Held open so the connection (and thus the subscription) stays alive.
    _write: tokio::io::WriteHalf<ClientStream>,
}

impl WorldEventStream {
    /// The next event, or `None` once the connection closes.
    ///
    /// A line that does not parse as a [`WorldEvent`] is skipped, not treated
    /// as end-of-stream: the daemon may be a newer build whose event enum grew
    /// a variant this client does not know, and ending the stream on the first
    /// such event would put the consumer into a reconnect loop that tears the
    /// subscription down once per unknown event.
    pub async fn next(&mut self) -> Option<WorldEvent> {
        loop {
            let line = self.lines.next_line().await.ok().flatten()?;
            if let Ok(event) = serde_json::from_str(&line) {
                return Some(event);
            }
        }
    }
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
            handle_connection_capped(stream, op_tx, no_events(), None, 40).await
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
}
