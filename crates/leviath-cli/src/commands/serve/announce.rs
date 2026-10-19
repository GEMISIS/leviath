//! What `lev serve` tells the program that started it.
//!
//! By default that is one line of prose on stdout once the server listens.
//! With `--json`, stdout carries JSON lines instead, one object per line and
//! nothing else: the daemon's start-up as it goes, the daemon being replaced
//! or started, and a `ready` object naming the URL the server bound, its pid
//! and build, and the daemon's. A failure before that ends with an `error`
//! line whose `code` a program can match on. With `--ready-file`, the same
//! `ready` object is also written to a file, owner-only and in one step, and
//! the file is removed again when the server stops cleanly.
//!
//! Logs and the daemon's start-up shown for a person stay on stderr either
//! way. The token never appears in anything written here.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use leviath_runtime::control_socket::{
    DaemonIdentity, StartupEvent, StartupProgress, StartupWatch,
};
use serde::Serialize;

use crate::daemon::startup_view::BringUpNotices;

/// The version of the `ready` object. A field added later leaves it alone;
/// a field removed or changed in meaning raises it.
pub const READY_SCHEMA: u32 = 1;

/// The shortest gap between two `daemon_starting` lines for one step, so a
/// step counting through thousands of items prints a few lines a second
/// rather than one per item. A new step is always printed at once.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Why `lev serve` failed to start, as the `code` of its `error` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The configuration, a flag, or `LEVIATH_DAEMON` could not be used.
    ConfigInvalid,
    /// No API token was given.
    TokenMissing,
    /// The address could not be resolved or bound.
    BindFailed,
    /// The TLS flags, certificate or key could not be used.
    TlsInvalid,
    /// No daemon could be reached, started or waited for.
    DaemonUnavailable,
    /// The `--ready-file` could not be written.
    ReadyFileFailed,
    /// The server panicked before it was ready.
    InternalError,
}

/// The daemon a server serves, once it has been brought up.
pub struct Daemon {
    /// The client the server reaches it through.
    pub control: leviath_runtime::control_socket::ControlClient,
    /// Who it said it is, when it said.
    pub identity: Option<DaemonIdentity>,
}

/// The `daemon` part of the `ready` object.
#[derive(Serialize)]
struct DaemonRef {
    pid: u32,
    version: String,
    build: String,
}

/// The object a server announces once it is listening.
#[derive(Serialize)]
struct ReadyLine<'a> {
    event: &'static str,
    schema: u32,
    url: String,
    pid: u32,
    version: &'static str,
    build: &'static str,
    server_instance: &'static str,
    daemon: Option<&'a DaemonRef>,
}

/// A `daemon_starting` line: the progress as the daemon reports it, with
/// `detail` always present so every line has the same fields.
#[derive(Serialize)]
struct StartingLine<'a> {
    event: &'static str,
    step: &'a str,
    done: u64,
    total: u64,
    detail: Option<&'a str>,
}

/// The `error` line a server that failed to start ends with.
#[derive(Serialize)]
struct ErrorLine {
    event: &'static str,
    code: ErrorCode,
    message: String,
}

/// The last `daemon_starting` line written, and when.
struct Shown {
    progress: StartupProgress,
    at: Instant,
}

/// Says what `lev serve` is doing to whoever started it. See the module
/// documentation for what it writes where.
pub struct Announcer {
    json: bool,
    ready_file: Option<PathBuf>,
    out: Mutex<Box<dyn Write + Send>>,
    /// What a person waiting on the daemon is shown, on stderr.
    prose: StartupWatch,
    /// What a failure now is reported as.
    stage: Mutex<ErrorCode>,
    daemon: Mutex<Option<DaemonRef>>,
    shown: Mutex<Option<Shown>>,
    every: Duration,
    /// Told the bound address once the server listens, for a test waiting on
    /// it.
    observer: Mutex<Option<tokio::sync::oneshot::Sender<SocketAddr>>>,
    /// Whether the server has said it is ready.
    ready: AtomicBool,
    /// What this server wrote to the ready file, to tell its own file from
    /// one something else put there since.
    written: Mutex<Option<String>>,
}

impl Announcer {
    /// An announcer for `args`, writing to `out` (stdout), that shows a
    /// person the daemon's start-up through `prose`.
    pub fn new(
        args: &super::ServeArgs,
        out: Box<dyn Write + Send>,
        prose: StartupWatch,
    ) -> Arc<Self> {
        Arc::new(Self {
            json: args.json,
            // Absolute from the start, so the file written and the file
            // removed on the way out are the same one, and a bare name has a
            // directory to be written into.
            ready_file: args
                .ready_file
                .as_ref()
                .map(|path| std::path::absolute(path).unwrap_or(path.clone())),
            out: Mutex::new(out),
            prose,
            stage: Mutex::new(ErrorCode::ConfigInvalid),
            daemon: Mutex::default(),
            shown: Mutex::default(),
            every: PROGRESS_EVERY,
            observer: Mutex::default(),
            ready: AtomicBool::new(false),
            written: Mutex::default(),
        })
    }

    /// An announcer for `args` that writes nothing anywhere.
    #[cfg(test)]
    pub(super) fn silent(args: &super::ServeArgs) -> Arc<Self> {
        Self::new(args, Box::new(std::io::sink()), Arc::new(|_| {}))
    }

    /// This announcer, also telling `observer` the bound address.
    #[cfg(test)]
    pub(super) fn observed_by(
        self: Arc<Self>,
        observer: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
    ) -> Arc<Self> {
        *leviath_core::sync::lock(&self.observer) = observer;
        self
    }

    /// A failure from now on is reported as `code`.
    pub fn stage(&self, code: ErrorCode) {
        *leviath_core::sync::lock(&self.stage) = code;
    }

    /// Write `value` as one JSON line, when `--json` asked for them.
    fn line(&self, value: &impl Serialize) {
        if !self.json {
            return;
        }
        let mut text = serde_json::to_string(value).expect("an event always serializes");
        text.push('\n');
        self.write(&text);
    }

    fn write(&self, text: &str) {
        let mut out = leviath_core::sync::lock(&self.out);
        let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
    }

    /// Where the daemon's start-up is now, as a `daemon_starting` line: at
    /// once for a new step or detail, and at most every `every` while one
    /// step counts, though a step's last item is always shown. A daemon that
    /// has not begun its first step says nothing.
    fn progress(&self, now: &StartupProgress) {
        if !self.json || now.step.is_empty() {
            return;
        }
        let mut shown = leviath_core::sync::lock(&self.shown);
        let due = shown.as_ref().is_none_or(|last| {
            last.progress.step != now.step
                || last.progress.detail != now.detail
                || last.progress.total != now.total
                || (last.progress.done != now.done
                    && (last.at.elapsed() >= self.every || now.done == now.total))
        });
        if !due {
            return;
        }
        self.line(&StartingLine {
            event: "daemon_starting",
            step: &now.step,
            done: now.done,
            total: now.total,
            detail: now.detail.as_deref(),
        });
        *shown = Some(Shown {
            progress: now.clone(),
            at: Instant::now(),
        });
    }

    /// The daemon is ready and is `identity`, which the `ready` object
    /// names.
    pub fn daemon_ready(&self, identity: Option<&DaemonIdentity>) {
        *leviath_core::sync::lock(&self.daemon) = identity.map(|daemon| DaemonRef {
            pid: daemon.pid,
            version: daemon.version.clone(),
            build: daemon.build.clone(),
        });
        self.line(&serde_json::json!({ "event": "daemon_ready" }));
    }

    /// The server listens on `bound` over `scheme`: write the ready file,
    /// then say so on stdout, as prose or as the `ready` object.
    ///
    /// Mints this process's `server_instance` if nothing has yet, so the
    /// `ready` object and every subscription report the same one.
    pub(super) fn listening(&self, scheme: &str, bound: SocketAddr) -> anyhow::Result<()> {
        let url = format!("{scheme}://{bound}");
        let daemon = leviath_core::sync::lock(&self.daemon);
        let ready = ReadyLine {
            event: "ready",
            schema: READY_SCHEMA,
            url: url.clone(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION"),
            build: crate::daemon::setup::CURRENT_BUILD,
            server_instance: super::events::server_instance(),
            daemon: daemon.as_ref(),
        };
        let mut text = serde_json::to_string(&ready).expect("the ready object always serializes");
        text.push('\n');
        drop(daemon);
        if let Some(path) = &self.ready_file {
            self.stage(ErrorCode::ReadyFileFailed);
            leviath_sys::perms::write_private(path, text.as_bytes()).map_err(|e| {
                anyhow::anyhow!("could not write the ready file {}: {e}", path.display())
            })?;
            *leviath_core::sync::lock(&self.written) = Some(text.clone());
        }
        self.ready.store(true, Ordering::SeqCst);
        match self.json {
            true => self.write(&text),
            false => self.write(&format!("Leviath API server listening on {url}\n")),
        }
        if let Some(observer) = leviath_core::sync::lock(&self.observer).take() {
            // A test that already gave up waiting does not stop the server.
            let _ = observer.send(bound);
        }
        Ok(())
    }

    /// The server has stopped cleanly: remove the ready file, so a file that
    /// is there names a server that is. Only a plain file still holding what
    /// this server wrote is removed; one replaced since, or a symlink, is
    /// someone else's and left alone.
    pub(super) fn stopped(&self) {
        let ours = leviath_core::sync::lock(&self.written).clone();
        if let Some(path) = &self.ready_file
            && ours.is_some()
            && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
            && std::fs::read_to_string(path).ok() == ours
        {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The server panicked with `message`: before it was ready, that is the
    /// `error` line a program waiting on it needs. Never waits on stdout, in
    /// case the panic happened while it was being written.
    pub fn panicked(&self, message: &str) {
        if !self.json || self.ready.load(Ordering::SeqCst) {
            return;
        }
        let line = ErrorLine {
            event: "error",
            code: ErrorCode::InternalError,
            message: message.to_string(),
        };
        let mut text = serde_json::to_string(&line).expect("an event always serializes");
        text.push('\n');
        if let Ok(mut out) = self.out.try_lock() {
            let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
        }
    }

    /// Report `result`, the end of a server that may not have started: a
    /// failure is an `error` line with the code of the step it failed at.
    pub(super) fn finish(&self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        result.inspect_err(|e| {
            let code = *leviath_core::sync::lock(&self.stage);
            self.line(&ErrorLine {
                event: "error",
                code,
                message: format!("{e:#}"),
            });
        })
    }
}

impl BringUpNotices for Announcer {
    fn replacing(&self, line: &str) {
        eprintln!("{line}");
        self.line(&serde_json::json!({ "event": "daemon_replacing", "message": line }));
    }

    fn spawned(&self, pid: u32) {
        self.line(&serde_json::json!({ "event": "daemon_spawned", "pid": pid }));
    }

    fn watch(self: Arc<Self>) -> StartupWatch {
        Arc::new(move |event| {
            (self.prose)(event);
            if let StartupEvent::Progress(now) = event {
                self.progress(now);
            }
        })
    }
}

#[cfg(test)]
#[path = "announce_tests.rs"]
mod tests;
