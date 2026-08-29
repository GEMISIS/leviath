//! The client half of the control transport: [`ControlClient`], which dials the
//! daemon's socket once per request, and what it remembers between requests so
//! that a long-lived client rides out a daemon restart.
//!
//! The daemon half - [`handle_connection`](super::handle_connection) and its
//! dispatch - lives in the parent module, alongside the wire types both halves
//! share.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::{
    AUTH_REQUIRED, ClientStream, ControlId, ControlRequest, ControlResponse, ControlToken,
    DaemonIdentity, INVALID_REQUEST, SHUTTING_DOWN, connect,
};
use crate::host::{SpawnArgs, WorldEvent};

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
pub(crate) fn request_timeout() -> std::time::Duration {
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
pub(super) fn timeout_for(req: &ControlRequest) -> std::time::Duration {
    let base = request_timeout();
    match req {
        ControlRequest::Spawn { .. } if base != std::time::Duration::MAX => {
            base.max(std::time::Duration::from_secs(SPAWN_CONTROL_TIMEOUT_SECS))
        }
        _ => base,
    }
}

/// How long a long-lived client keeps trying to reach a daemon that has just
/// stopped answering before it reports the daemon unreachable. Opted into with
/// [`ControlClient::with_reconnect_grace`]; a client has none by default.
///
/// A daemon restart - `lev daemon restart`, a supervisor relaunch after a
/// crash, or `ensure_daemon_running` replacing a stale build after an update -
/// takes well under a second on a warm machine and a few seconds on a slow one,
/// and during that window the socket simply is not there. Before this, every
/// request that landed in the window failed outright: `lev serve` answered 503
/// and the ACP bridge refused the turn, for a daemon that was back a second
/// later. Now a request waits out the window and is served by the new daemon.
///
/// The clock starts at the *first* failure after the daemon was last reached
/// (see [`ControlClient::link`]), not at each request. So a daemon that is
/// genuinely gone costs one caller the grace period, and every caller after
/// that fails fast until it comes back - a long outage never turns into a queue
/// of callers each waiting ten seconds.
///
/// One-shot commands (`lev ps`, `lev cancel`) keep no grace: a daemon that is
/// not running is a fact to report at once, with the advice to start it, and
/// ten seconds of silence before that advice would help nobody.
pub const RESTART_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// The first pause between reconnect attempts inside [`RESTART_GRACE`]. Doubles
/// per attempt up to [`RECONNECT_BACKOFF_CAP`], so a fast restart is noticed
/// fast and a slow one is not polled hundreds of times.
const RECONNECT_BACKOFF_START: std::time::Duration = std::time::Duration::from_millis(50);

/// The longest pause between reconnect attempts.
const RECONNECT_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_millis(500);

/// Everything one client and all its clones share about the daemon on the
/// other end. One lock, held only for the reads and writes below and never
/// across an `.await`.
///
/// Shared across clones on purpose: `lev serve` hands a clone of one client to
/// every request handler and to its event loop, and a token refresh, an
/// identity, or an outage that one of them observes is true for all of them.
#[derive(Default)]
struct Link {
    /// The token to present. Refreshed from disk on a refusal, because the
    /// daemon mints a fresh one on every start.
    token: Option<ControlToken>,
    /// Who was on the other end the last time a handshake completed. `None`
    /// until a daemon has introduced itself, which a daemon that predates the
    /// handshake, or a tokenless test daemon, never does.
    daemon: Option<DaemonIdentity>,
    /// How many times `daemon` has been seen to change. A different process,
    /// or a different build, behind the same socket is a restart.
    restarts: u64,
    /// When the daemon was first found missing, after last being reached.
    /// `None` while it is answering.
    outage_since: Option<std::time::Instant>,
}

/// A snapshot of what a client currently knows about its daemon - see
/// [`ControlClient::link`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkStatus {
    /// The daemon last reached, once one has introduced itself.
    pub daemon: Option<DaemonIdentity>,
    /// How many times a different daemon has appeared behind the socket since
    /// this client was created. A change here is a restart the client lived
    /// through.
    pub restarts: u64,
    /// Whether the last transport attempt reached a daemon at all. `false`
    /// from the first connect failure until the next success; a refusal or a
    /// timeout is not an outage, since something answered.
    pub reachable: bool,
}

/// The daemon and this process no longer run the same code.
///
/// Reported by [`ControlClient::code_mismatch`], and folded into the error a
/// request returns when the two ends have actually stopped understanding each
/// other. The two identities are public so a front-end can render them its own
/// way; [`Display`](std::fmt::Display) says it in one sentence with the remedy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeMismatch {
    /// What the daemon reported.
    pub daemon: DaemonIdentity,
    /// What this process is.
    pub client: DaemonIdentity,
}

impl std::fmt::Display for CodeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the daemon is now version {} (build {}, pid {}), but this process was built from \
             version {} (build {}); restart this process so both ends run the same code",
            self.daemon.version,
            self.daemon.build,
            self.daemon.pid,
            self.client.version,
            self.client.build,
        )
    }
}

/// Where an attempt to make a request failed. Decides whether a retry is safe:
/// a failure before the request was written cannot have had any effect, while
/// one after it may have - the daemon may have spawned the run and died before
/// saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Connecting or authenticating: nothing of the request has left this
    /// process.
    Connect,
    /// The request was written; the failure was in getting a reply.
    Sent,
}

/// One failed attempt: the error and where it happened.
struct AttemptError {
    stage: Stage,
    error: std::io::Error,
}

impl AttemptError {
    fn at(stage: Stage) -> impl FnOnce(std::io::Error) -> Self {
        move |error| Self { stage, error }
    }
}

/// Whether `e` says "no daemon answered", as opposed to "a daemon answered and
/// said no". Only the first kind is worth waiting out.
pub(super) fn is_transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        // The socket file or pipe is not there: between one daemon unlinking
        // it and the next binding it.
        NotFound
        // A stale socket file nothing listens on: the daemon died without
        // unlinking, and its successor has not bound yet.
        | ConnectionRefused
        // The daemon went away mid-connection.
        | ConnectionReset
        | ConnectionAborted
        | BrokenPipe
        | UnexpectedEof
    ) || pipe_busy(e)
}

/// `ERROR_PIPE_BUSY`: a Windows named pipe whose server has no free instance
/// right now, which is what a client sees between the daemon accepting one
/// connection and creating the next listener instance. Retryable by definition;
/// `std` gives it no `ErrorKind` of its own.
#[cfg(windows)]
fn pipe_busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(231)
}

/// No Unix errno means "try again in a moment" the way `ERROR_PIPE_BUSY` does.
#[cfg(not(windows))]
fn pipe_busy(_: &std::io::Error) -> bool {
    false
}

/// The client half of the control transport: connects to the daemon's control
/// socket (resolved from a [`ControlId`]), sends one [`ControlRequest`], and
/// reads back its [`ControlResponse`]. A fresh connection per request keeps it
/// simple and stateless.
///
/// What it remembers between requests is about the *daemon*, not the
/// connection: the token, who answered last, and whether it is answering at all
/// (see [`Self::link`]). That is what lets a long-lived client - `lev serve`, `lev
/// dash`, the ACP bridge - ride out a daemon restart: a request that lands
/// while the daemon is down waits [`RESTART_GRACE`] for it to come back, a
/// refusal after a restart re-reads the token, and a daemon that comes back
/// on a different build is reported as such.
#[derive(Clone)]
pub struct ControlClient {
    id: ControlId,
    /// Shared across clones - see [`Link`].
    link: std::sync::Arc<std::sync::Mutex<Link>>,
    /// Where the token was looked for, so a refusal can name the file - and so
    /// a refusal can re-read it: the daemon mints a fresh token on every start,
    /// which is exactly when a long-lived client's cached copy goes stale.
    token_dir: Option<PathBuf>,
    /// How long to keep trying when the daemon is not answering. Zero means a
    /// failed connect is reported at once.
    grace: std::time::Duration,
    /// Who this process is, for comparing against the daemon's answer.
    own: DaemonIdentity,
}

impl ControlClient {
    /// A client for the control socket identified by `id`, with no token and
    /// no patience: an unreachable daemon is reported at once.
    ///
    /// Only reaches a daemon that runs without a token, which in practice means
    /// a test driving the protocol directly. Real callers use
    /// [`for_home`](Self::for_home) - see [`ControlToken`].
    pub fn new(id: impl Into<ControlId>) -> Self {
        Self {
            id: id.into(),
            link: Default::default(),
            token_dir: None,
            grace: std::time::Duration::ZERO,
            own: DaemonIdentity::this_process(DaemonIdentity::unknown_build()),
        }
    }

    /// Present `token` on every connection this client opens.
    #[cfg(test)]
    pub(crate) fn with_token(self, token: ControlToken) -> Self {
        leviath_core::sync::lock(&self.link).token = Some(token);
        self
    }

    /// Wait up to `grace` for a daemon that has just stopped answering,
    /// instead of reporting it unreachable at once. Zero disables the wait.
    ///
    /// Per clone, not shared: `lev dash` polls the daemon ten times a second
    /// on its render loop and wants that clone to fail fast, while the clone
    /// its cancel/pause keypresses go through should wait a restart out.
    pub fn with_reconnect_grace(mut self, grace: std::time::Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Record this process's build id, so a daemon on a different build can be
    /// told from one that merely restarted. Without it only the version is
    /// compared - see `DaemonIdentity::same_code_as`.
    pub fn with_build(mut self, build: impl Into<String>) -> Self {
        self.own.build = build.into();
        self
    }

    /// A client that reads the daemon's token out of `dir`. Like
    /// [`new`](Self::new) it has no patience for an absent daemon; a
    /// long-lived caller adds [`with_reconnect_grace`](Self::with_reconnect_grace).
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
            link: std::sync::Arc::new(std::sync::Mutex::new(Link {
                token: ControlToken::load(dir).ok(),
                ..Default::default()
            })),
            token_dir: Some(dir.to_path_buf()),
            grace: std::time::Duration::ZERO,
            own: DaemonIdentity::this_process(DaemonIdentity::unknown_build()),
        }
    }

    /// What this client currently knows about the daemon behind its socket.
    ///
    /// Updated by every request and subscription the client (or any clone of
    /// it) makes, so a front-end that already talks to the daemon regularly
    /// can render this without asking again. A front-end that has been idle
    /// sees whatever its last request saw.
    pub fn link(&self) -> LinkStatus {
        let link = leviath_core::sync::lock(&self.link);
        LinkStatus {
            daemon: link.daemon.clone(),
            restarts: link.restarts,
            reachable: link.outage_since.is_none(),
        }
    }

    /// The daemon last reached runs different code from this process, when it
    /// does and has said who it is. `None` while the two match, or while no
    /// daemon has introduced itself yet.
    ///
    /// This is advisory: the two ends may well still understand each other,
    /// and requests keep flowing. It becomes an error only when they stop -
    /// see [`Self::request`].
    pub fn code_mismatch(&self) -> Option<CodeMismatch> {
        let daemon = leviath_core::sync::lock(&self.link).daemon.clone()?;
        (!daemon.same_code_as(&self.own)).then(|| CodeMismatch {
            daemon,
            client: self.own.clone(),
        })
    }

    /// The token to present right now, if any.
    fn current_token(&self) -> Option<ControlToken> {
        leviath_core::sync::lock(&self.link).token.clone()
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
        let mut link = leviath_core::sync::lock(&self.link);
        let changed = link
            .token
            .as_ref()
            .is_none_or(|t| !t.matches(fresh.expose()));
        if changed {
            link.token = Some(fresh);
        }
        changed
    }

    /// Note that a daemon answered: the outage, if one was being timed, is
    /// over.
    fn reached(&self) {
        leviath_core::sync::lock(&self.link).outage_since = None;
    }

    /// Note that no daemon answered, and say whether it is still worth waiting
    /// for one: `true` while the outage is younger than this clone's grace.
    /// The outage clock starts at the first failure and is shared by every
    /// clone, so the wait is per outage, not per caller.
    fn worth_waiting(&self) -> bool {
        let mut link = leviath_core::sync::lock(&self.link);
        let since = *link
            .outage_since
            .get_or_insert_with(std::time::Instant::now);
        !self.grace.is_zero() && since.elapsed() < self.grace
    }

    /// Record who answered a handshake. A daemon that differs from the last
    /// one seen - a new pid, or a new build behind the same socket - is a
    /// restart, and counts as one.
    fn observe(&self, daemon: DaemonIdentity) {
        let mut link = leviath_core::sync::lock(&self.link);
        match &link.daemon {
            Some(known) if known == &daemon => {}
            Some(_) => {
                link.restarts += 1;
                link.daemon = Some(daemon);
            }
            None => link.daemon = Some(daemon),
        }
    }

    /// Why the daemon refused us, said in terms of what the user can do.
    pub(super) fn refused(&self) -> std::io::Error {
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

    /// An error the daemon and this process could not avoid because they no
    /// longer speak the same protocol, when that is what `e` is. A reply that
    /// does not parse, or a request the daemon could not read, is a bug when
    /// both ends run the same code and an *update* when they do not - and the
    /// second one has a remedy the user can act on, so it is named.
    fn explained(&self, e: std::io::Error) -> std::io::Error {
        match (e.kind(), self.code_mismatch()) {
            (std::io::ErrorKind::InvalidData, Some(mismatch)) => std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("{mismatch} (its reply could not be read: {e})"),
            ),
            _ => e,
        }
    }

    /// Send one request and await its response. Errors if the daemon can't be
    /// reached, does not answer within `request_timeout`, the connection closes
    /// before a reply, or the reply doesn't parse.
    ///
    /// A daemon that is not there is waited for, when this client was given a
    /// grace to wait (see [`RESTART_GRACE`]), and one that refuses a stale
    /// token is retried with a fresh one. A daemon
    /// that answers but cannot be understood, when it and this process run
    /// different code, is reported as [`ErrorKind::Unsupported`](std::io::ErrorKind::Unsupported)
    /// with the remedy in the message; that is the one failure a restart of
    /// the daemon does not fix, because the process that needs restarting is
    /// this one.
    pub async fn request(&self, req: &ControlRequest) -> std::io::Result<ControlResponse> {
        // The daemon services control ops from a single loop, so one op that
        // takes a long time (or a wedged world) delays every other client. With
        // no deadline, `lev cancel` and the dashboard simply hung - no output, no
        // error, nothing to act on. A timeout turns that into a failure the
        // caller can fall back from.
        let deadline = timeout_for(req);
        tokio::time::timeout(deadline, self.request_with_recovery(req))
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("the daemon did not respond within {}s", deadline.as_secs()),
                ))
            })
    }

    /// [`Self::attempt`], repeated across the two recoverable failures.
    ///
    /// **A refused token** is retried once with a re-read one. The daemon mints
    /// a fresh token every start, so a refusal is most often not an intruder
    /// but a restart this long-lived client slept through - `lev serve` held
    /// one client for its whole life, and a single daemon restart (a crash,
    /// `lev daemon restart`, or `ensure_daemon_running` replacing a stale
    /// build) bricked every control op it made from then on until someone
    /// restarted serve too. Once, not in a loop: a token that is wrong after a
    /// re-read is wrong.
    ///
    /// **No daemon answering** is retried with backoff for as long as the
    /// outage is younger than this clone's grace, but only when a retry cannot
    /// double an effect: a failure before the request was written, or after it
    /// for a request that only reads. A spawn or a message that got no reply
    /// may or may not have happened, and asking again is how a run gets started
    /// twice; that failure is reported instead. The one exception is a reply
    /// saying the daemon was already shutting down, which means the request was
    /// dropped unprocessed - safe to repeat against its successor.
    async fn request_with_recovery(
        &self,
        req: &ControlRequest,
    ) -> std::io::Result<ControlResponse> {
        let mut refreshed = false;
        let mut backoff = RECONNECT_BACKOFF_START;
        loop {
            match self.attempt(req).await {
                Ok(ControlResponse::Error { message })
                    if message == SHUTTING_DOWN && self.worth_waiting() =>
                {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_CAP);
                }
                // The daemon could not read what this process sent. Between two
                // ends on the same code that is a bug; between an updated
                // daemon and a client that predates it, it is the client's cue
                // to restart, and the error says so.
                Ok(ControlResponse::Error { message }) if message.starts_with(INVALID_REQUEST) => {
                    self.reached();
                    return match self.code_mismatch() {
                        Some(mismatch) => Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            format!("{mismatch} (it could not read this request: {message})"),
                        )),
                        None => Ok(ControlResponse::Error { message }),
                    };
                }
                Ok(response) => {
                    self.reached();
                    return Ok(response);
                }
                Err(AttemptError { error, .. })
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        && !refreshed
                        && self.refresh_token() =>
                {
                    refreshed = true;
                }
                Err(AttemptError { stage, error })
                    if is_transient(&error)
                        && (stage == Stage::Connect || req.is_read_only())
                        && self.worth_waiting() =>
                {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_CAP);
                }
                Err(AttemptError { error, .. }) => return Err(self.explained(error)),
            }
        }
    }

    /// One connection, one request, one reply.
    async fn attempt(&self, req: &ControlRequest) -> Result<ControlResponse, AttemptError> {
        let stream = connect(&self.id)
            .await
            .map_err(AttemptError::at(Stage::Connect))?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        self.authenticate(&mut write_half, &mut lines)
            .await
            .map_err(AttemptError::at(Stage::Connect))?;

        let mut line = serde_json::to_string(req).expect("ControlRequest serializes");
        line.push('\n');
        // A failed write means the peer is already gone; the read below then sees
        // EOF and returns the error, so the write needs no separate propagation.
        let _ = write_half.write_all(line.as_bytes()).await;

        match lines
            .next_line()
            .await
            .map_err(AttemptError::at(Stage::Sent))?
        {
            Some(resp_line) => {
                let parsed: ControlResponse = serde_json::from_str(&resp_line)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                    .map_err(AttemptError::at(Stage::Sent))?;
                // A daemon that refused us: report what to do about it, not the
                // wire message. Reached when this client had no token to send
                // and so never ran the handshake - nothing was served, so this
                // is a connect-stage failure for retry purposes.
                match &parsed {
                    ControlResponse::Error { message } if message.starts_with(AUTH_REQUIRED) => {
                        Err(AttemptError {
                            stage: Stage::Connect,
                            error: self.refused(),
                        })
                    }
                    _ => Ok(parsed),
                }
            }
            None => Err(AttemptError {
                stage: Stage::Sent,
                error: std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "control connection closed before a response",
                ),
            }),
        }
    }

    /// Authenticate a fresh connection: send the token (when there is one) and
    /// read the daemon's verdict. On every connection, because the client opens
    /// a fresh one per request, so there is no session to carry the proof
    /// across. With no token nothing is sent - a daemon that predates tokens
    /// serves the request, and one that requires them refuses it, which is the
    /// outcome to report either way.
    ///
    /// The handshake asks the daemon who it is, and what it says is recorded
    /// (see [`Self::observe`]). A daemon that predates the question answers a
    /// bare `Ok`, which is accepted and records nothing.
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
            hello: true,
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
                Ok(ControlResponse::Welcome { daemon }) => {
                    self.observe(daemon);
                    Ok(())
                }
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
    /// saw the stream work. Recovers the same way [`Self::request`] does: a
    /// stale token is re-read once, and a daemon that is not there is waited
    /// for. Subscribing has no effect to double, so a retry is always safe.
    pub async fn subscribe(&self) -> std::io::Result<WorldEventStream> {
        let mut refreshed = false;
        let mut backoff = RECONNECT_BACKOFF_START;
        loop {
            match self.subscribe_once().await {
                Ok(stream) => {
                    self.reached();
                    return Ok(stream);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        && !refreshed
                        && self.refresh_token() =>
                {
                    refreshed = true;
                }
                Err(e) if is_transient(&e) && self.worth_waiting() => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_CAP);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One subscribe attempt with the currently-cached token.
    async fn subscribe_once(&self) -> std::io::Result<WorldEventStream> {
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
