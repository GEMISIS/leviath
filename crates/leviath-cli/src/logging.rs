//! Process-wide logging: the subscriber `main` installs, with a reloadable
//! slot for the OTLP log-export layer.
//!
//! The subscriber must exist before any subcommand logs, but the
//! `[observability]` config that decides whether daemon logs also export over
//! OTLP is only read later (by the daemon, after `Config::load`). Bridging
//! that gap is what the reload slot is for: [`init`] installs the fmt layer
//! plus an empty slot and parks the reload handle in a static;
//! `set_otel_layer` fills the slot once the daemon has built its
//! exporter. Nothing goes to **stdout** - `lev agent-client` uses it as its
//! JSON-RPC channel, and a stray log line there would corrupt the stream a
//! host is parsing. Lines go to stderr, and in the daemon also to its own
//! capped file (see [`attach_log_file`] and the `daemon_log` module); a
//! daemon whose stderr is not a terminal writes the file alone.
//!
//! stderr is not safe either while a full-screen TUI is up, which is what
//! [`hold_for_tui`] exists for. `lev setup` and `lev dash` own the alternate
//! screen on stdout, but stderr is the same terminal, so a log line lands
//! inside the frame. Raw mode makes it worse than untidy: `OPOST` is off, so
//! the newline is a bare line feed with no carriage return and each line
//! starts where the last one ended, staircasing across the screen. And
//! ratatui only redraws cells it believes changed, so nothing ever paints
//! over the mess. A verification call at `debug` was enough to fill the
//! wizard with what looked like garbage.
//!
//! So while a TUI holds the terminal, log lines are buffered instead of
//! written, and flushed to stderr when it lets go. Nothing is lost, and
//! nothing lands on the screen while somebody is looking at it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

mod daemon_log;

pub use daemon_log::{daemon_log_path, serve_log_path};

/// What the reload slot holds: nothing, or the installed OTLP layer.
type OtelSlot = Option<leviath_telemetry::LogLayer>;

/// The handle [`set_otel_layer`] reloads through, parked by [`init`].
static OTEL_HANDLE: OnceLock<reload::Handle<OtelSlot, Registry>> = OnceLock::new();

/// Whether a TUI currently owns the terminal.
static TUI_HOLDS_TERMINAL: AtomicBool = AtomicBool::new(false);

/// This process's log file, once [`attach_log_file`] has run: the daemon's
/// `daemon.log`, a server's `serve-<name>.log`. Every other `lev` process
/// leaves it empty, and the file writer then discards.
static LOG_FILE: OnceLock<daemon_log::DaemonLog> = OnceLock::new();

/// Whether the stderr writer still writes. Cleared by [`attach_log_file`]
/// when stderr is not a terminal: a daemon started detached or by a
/// supervisor has nobody reading it, and a supervisor that captures stderr
/// would otherwise keep an uncapped copy of the file.
static STDERR_MIRROR: AtomicBool = AtomicBool::new(true);

/// Lines written while the terminal was held, waiting to be flushed.
static PARKED: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// The most [`PARKED`] may hold. A long `lev dash` session with `--verbose`
/// would otherwise hold every debug line for the life of the session; past
/// this the oldest bytes go and the release says how many.
const PARKED_CAP: usize = 1024 * 1024;

/// Bytes dropped from [`PARKED`] since the last release, reported on release.
static PARKED_DROPPED: AtomicUsize = AtomicUsize::new(0);

/// Append `buf` to `parked`, keeping at most `cap` bytes by discarding the
/// oldest, and return how many bytes were discarded.
///
/// Pure over its arguments so the arithmetic is tested with a small cap
/// rather than by writing a megabyte through the global.
fn park(parked: &mut Vec<u8>, buf: &[u8], cap: usize) -> usize {
    let total = parked.len().saturating_add(buf.len());
    let overflow = total.saturating_sub(cap);
    if overflow == 0 {
        parked.extend_from_slice(buf);
        return 0;
    }
    // Drop from the front of what is already parked first; only a single
    // write larger than the whole cap reaches into `buf` itself.
    let from_parked = overflow.min(parked.len());
    parked.drain(..from_parked);
    let from_buf = overflow - from_parked;
    parked.extend_from_slice(&buf[from_buf..]);
    overflow
}

/// Where a log line goes: straight to stderr, or into [`PARKED`] until the
/// terminal is free.
///
/// One writer rather than a runtime swap of the subscriber, because the
/// subscriber is installed once, process-wide, before any subcommand knows
/// whether it will draw.
struct TerminalAwareWriter;

impl Write for TerminalAwareWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if TUI_HOLDS_TERMINAL.load(Ordering::Relaxed) {
            // A poisoned lock means a thread panicked while parking a line. The
            // buffer is still a valid buffer, so it is taken back rather than
            // handled: a logging call is the worst place to raise a second
            // panic, and there is nothing here that a poisoned flag protects.
            let dropped = park(
                &mut PARKED.lock().unwrap_or_else(PoisonError::into_inner),
                buf,
                PARKED_CAP,
            );
            PARKED_DROPPED.fetch_add(dropped, Ordering::Relaxed);
            return Ok(buf.len());
        }
        if !STDERR_MIRROR.load(Ordering::Relaxed) {
            return Ok(buf.len());
        }
        std::io::stderr().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if TUI_HOLDS_TERMINAL.load(Ordering::Relaxed) || !STDERR_MIRROR.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::io::stderr().flush()
    }
}

/// The file layer's writer: appends to the attached log file, or discards
/// when this process has none.
struct DaemonLogWriter;

impl Write for DaemonLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(log) = LOG_FILE.get() {
            log.append(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The file layer's writer factory; a named function for the reason
/// [`writer`] gives.
fn daemon_writer() -> DaemonLogWriter {
    DaemonLogWriter
}

/// The layer that writes the process's log file: the same lines as stderr,
/// with no colour codes. A function rather than a value inside [`init`] so a
/// test can put the same layer on a thread-scoped subscriber.
fn daemon_file_layer<S>(level: &str) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(daemon_writer)
        .with_filter(EnvFilter::new(level))
}

/// Start writing this process's log lines to the file at `path`:
/// [`daemon_log_path`] in the daemon, [`serve_log_path`] in a server. The
/// long-lived processes call it; every other command keeps stderr alone.
///
/// Returns `false` on a second call: the file is attached once for the life
/// of the process. `mirror_stderr` is whether stderr is a terminal, decided
/// by the caller; when it is not, the stderr copy stops here, so a detached
/// process writes the file alone.
pub fn attach_log_file(path: PathBuf, mirror_stderr: bool) -> bool {
    let _ = path.parent().map(leviath_sys::create_private_dir_all);
    let log = daemon_log::DaemonLog::new(path, leviath_core::config::DEFAULT_LOG_FILE_MAX_BYTES);
    if LOG_FILE.set(log).is_err() {
        return false;
    }
    STDERR_MIRROR.store(mirror_stderr, Ordering::Relaxed);
    true
}

/// Apply `[observability] log_file_max_bytes`. `false` when no log file is
/// attached, which is every process but the daemon and a server.
pub fn set_log_file_cap(bytes: u64) -> bool {
    match LOG_FILE.get() {
        Some(log) => {
            log.set_cap(bytes);
            true
        }
        None => false,
    }
}

/// The fmt layer's writer factory.
///
/// A named function rather than a closure at the call site, so the one region
/// this indirection costs is something a test can execute. A closure inside
/// [`init`] only ever runs when this process owns the global subscriber, which
/// under a parallel test runner is whichever test won the slot.
fn writer() -> TerminalAwareWriter {
    TerminalAwareWriter
}

/// Park log output for as long as a TUI owns the terminal.
///
/// Call from the terminal setup that enters the alternate screen, and pair it
/// with [`release_from_tui`] on every exit path including the panic hook.
pub fn hold_for_tui() {
    TUI_HOLDS_TERMINAL.store(true, Ordering::Relaxed);
}

/// Hand the terminal back and flush whatever was logged meanwhile.
///
/// Safe to call when nothing was held: there is simply nothing parked.
pub fn release_from_tui() {
    TUI_HOLDS_TERMINAL.store(false, Ordering::Relaxed);
    let parked = std::mem::take(&mut *PARKED.lock().unwrap_or_else(PoisonError::into_inner));
    let dropped = PARKED_DROPPED.swap(0, Ordering::Relaxed);
    if parked.is_empty() {
        return;
    }
    if dropped > 0 {
        let _ = writeln!(
            std::io::stderr(),
            "[log] {dropped} bytes of output were dropped while the terminal was held"
        );
    }
    let _ = std::io::stderr().write_all(&parked);
    let _ = std::io::stderr().flush();
}

/// Install the process-wide subscriber: fmt → stderr at `info` (`debug` when
/// verbose), the same lines into the daemon's file once one is attached, plus
/// the empty reloadable OTLP slot.
///
/// The filter is the literal `info`/`debug` directive for `--verbose`, not
/// `RUST_LOG`: `EnvFilter::new` parses its argument and never reads the
/// environment. That is deliberate - a daemon started by a supervisor inherits
/// whatever `RUST_LOG` the service file happens to carry, and the flag is the
/// one switch the user actually set.
///
/// Callable any number of times without panicking; the first global
/// subscriber and the first parked handle win. `main` calls it exactly once,
/// so in the real process the two are the same subscriber - the losing-race
/// cases exist only inside the test binary, where other tests own the global
/// slot.
pub fn init(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let (otel_layer, handle) = reload::Layer::new(None as OtelSlot);
    let subscriber = tracing_subscriber::registry()
        .with(otel_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_filter(EnvFilter::new(level)),
        )
        .with(daemon_file_layer(level));
    let _ = subscriber.try_init();
    let _ = OTEL_HANDLE.set(handle);
}

/// Put the daemon's OTLP log-export layer in the reload slot, or `None` to
/// empty it. Returns whether the slot was written - `false` when [`init`]
/// hasn't run (a library consumer with its own subscriber) or the slot is
/// gone.
///
/// Takes an `Option` because `[observability]` reloads: a user who turns
/// export off, or moves from the OTLP exporter to the stdout one, has to stop
/// the daemon's own log lines reaching a collector they are no longer pointing
/// at. The slot is a `reload::Layer`, so emptying it is the same operation as
/// filling it.
pub(crate) fn set_otel_layer(layer: Option<leviath_telemetry::LogLayer>) -> bool {
    match OTEL_HANDLE.get() {
        Some(handle) => handle.reload(layer).is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};

    /// An OTLP bridge layer wired to an in-memory exporter the test can read.
    fn bridge_with_exporter() -> (leviath_telemetry::LogLayer, InMemoryLogExporter) {
        let exporter = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let sink = leviath_telemetry::OtelSink::new(
            opentelemetry_sdk::trace::SdkTracerProvider::builder().build(),
            opentelemetry_sdk::metrics::SdkMeterProvider::builder().build(),
            provider,
        );
        (sink.tracing_log_layer(), exporter)
    }

    /// One test drives the whole lifecycle: the `OnceLock` handle is
    /// process-wide, so ordering between separate tests would race under the
    /// parallel test runner. The forwarding assertions run against a
    /// thread-scoped subscriber wired to the handle this test parks itself -
    /// the *global* subscriber slot belongs to whichever test wins it
    /// (testkit's `AlwaysOnSubscriber` usually does in a full run).
    #[test]
    fn init_parks_the_handle_and_install_forwards_events() {
        // Before any handle is parked: nothing to install into.
        let (layer, _exporter) = bridge_with_exporter();
        assert!(!set_otel_layer(Some(layer)));

        // Park a handle whose subscriber this thread controls.
        let (otel_layer, handle) = reload::Layer::new(None as OtelSlot);
        assert!(
            OTEL_HANDLE.set(handle).is_ok(),
            "this test parks the handle first"
        );
        let subscriber = tracing_subscriber::registry().with(otel_layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let (layer, exporter) = bridge_with_exporter();
        assert!(set_otel_layer(Some(layer)));
        tracing::info!(target: "leviath::logging::test", "forwarded line");
        let emitted = exporter.get_emitted_logs().unwrap();
        assert!(
            emitted
                .iter()
                .any(|log| format!("{:?}", log.record.body()).contains("forwarded line")),
            "{emitted:?}"
        );
        // The OTel stack's own targets are filtered out of the bridge.
        tracing::info!(target: "opentelemetry_sdk", "feedback line");
        let emitted = exporter.get_emitted_logs().unwrap();
        assert!(
            !emitted
                .iter()
                .any(|log| format!("{:?}", log.record.body()).contains("feedback line"))
        );

        // The real init path: never panics, keeps the parked handle, and the
        // slot stays reloadable afterwards.
        init(false);
        init(true);
        let (layer, _exporter) = bridge_with_exporter();
        assert!(set_otel_layer(Some(layer)));
    }

    /// The hold is what keeps a log line out of a wizard someone is reading,
    /// and the release is what keeps it from being lost instead. Both halves
    /// live in one test because the flag is process-wide: a second test
    /// toggling it in parallel would park the first one's writes.
    #[test]
    fn holding_the_terminal_parks_output_until_it_is_released() {
        // Released is the resting state, so a write goes straight out. A real
        // byte rather than an empty slice: `write_all` of nothing never calls
        // `write`, and the pass-through then depended on some other test
        // happening to log while nothing held the terminal.
        release_from_tui();
        assert!(!TUI_HOLDS_TERMINAL.load(Ordering::Relaxed));
        assert_eq!(writer().write(b"\n").expect("stderr accepts a write"), 1);
        writer().flush().expect("stderr accepts a flush");

        hold_for_tui();
        writer()
            .write_all(b"parked line\n")
            .expect("a held write is buffered, never refused");
        // A flush while held must not reach the terminal either, or the point
        // of buffering is lost on the very next `tracing` call.
        writer().flush().expect("a held flush is a no-op");
        assert_eq!(
            PARKED.lock().expect("uncontended").as_slice(),
            b"parked line\n"
        );

        release_from_tui();
        assert!(
            PARKED.lock().expect("uncontended").is_empty(),
            "release hands the buffer to stderr and empties it"
        );

        // A daemon whose stderr is not a terminal writes its file alone: the
        // stderr writer accepts and drops. Same test, same reason: the flag
        // is process-wide.
        STDERR_MIRROR.store(false, Ordering::Relaxed);
        assert_eq!(writer().write(b"\n").expect("muted"), 1);
        writer().flush().expect("a muted flush is a no-op");
        STDERR_MIRROR.store(true, Ordering::Relaxed);

        // Past the cap the oldest bytes go, the count is kept for the release
        // to report, and the release clears it. Same test, same reason: the
        // flag and the counter are process-wide.
        hold_for_tui();
        let big = vec![b'x'; PARKED_CAP + 16];
        writer().write_all(&big).expect("a held write is buffered");
        assert_eq!(PARKED.lock().expect("uncontended").len(), PARKED_CAP);
        assert_eq!(PARKED_DROPPED.load(Ordering::Relaxed), 16);
        // Trimmed before the release: releasing the megabyte writes a
        // megabyte-long line to the process's stderr, and on a CI runner that
        // line stalled the log pipe for a quarter of an hour and took the whole
        // test binary with it. A few bytes stay so the release still has
        // something to write and the dropped-bytes line still prints.
        PARKED.lock().expect("uncontended").truncate(64);
        release_from_tui();
        assert_eq!(PARKED_DROPPED.load(Ordering::Relaxed), 0);
        // Releasing twice is what the panic hook plus `Drop` actually does, and
        // with nothing parked it must stay quiet rather than write an empty
        // line.
        release_from_tui();
    }

    /// One test owns the daemon-log static, for the same reason the OTEL
    /// handle has one: it is process-wide, and two tests attaching would race.
    #[test]
    fn attaching_the_log_file_routes_the_file_layer_into_it() {
        // Before anything is attached: the cap has nowhere to go, and the file
        // writer accepts a line and drops it.
        assert!(!set_log_file_cap(1));
        assert_eq!(daemon_writer().write(b"x").expect("discarded"), 1);
        daemon_writer().flush().expect("nothing to flush");

        // Attached as if stderr were a terminal, so the mirror flag stays as
        // it is: the terminal-hold test owns that flag and exercises the
        // muted arms itself.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("logs").join("daemon.log");
        assert!(attach_log_file(path.clone(), true), "the first attach wins");
        assert!(
            !attach_log_file(path.clone(), true),
            "and the second is refused"
        );
        assert!(set_log_file_cap(1024 * 1024));

        // The layer `init` installs, on a subscriber this thread controls.
        let subscriber = tracing_subscriber::registry().with(daemon_file_layer("info"));
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info!(target: "leviath::logging::test", "a line for the file");
        tracing::debug!(target: "leviath::logging::test", "filtered out");
        let written = std::fs::read_to_string(&path).expect("the file exists");
        assert!(written.contains("a line for the file"), "{written}");
        assert!(!written.contains("filtered out"), "{written}");
        assert!(!written.contains('\u{1b}'), "no colour codes: {written}");

        // A file that cannot be reopened is an error the layer swallows and
        // the writer itself reports. The open handle would keep accepting
        // writes to an unlinked file, so the cap forces a roll, which drops
        // the handle and reopens where a file now sits in the directory's
        // place.
        drop(_guard);
        std::fs::remove_dir_all(dir.path()).expect("gone");
        std::fs::write(dir.path(), b"").expect("a file where the dir was");
        assert!(set_log_file_cap(1));
        assert!(daemon_writer().write(b"x").is_err());
        let _ = std::fs::remove_file(dir.path());
    }

    /// The cap is a ring: dropping comes off the front of what is parked
    /// first, and only a single write bigger than the cap loses its own head.
    #[test]
    fn park_keeps_the_newest_bytes_and_counts_the_rest() {
        let mut parked = Vec::new();
        assert_eq!(park(&mut parked, b"abc", 8), 0);
        assert_eq!(parked, b"abc");
        // Two over: the two oldest go.
        assert_eq!(park(&mut parked, b"defghij", 8), 2);
        assert_eq!(parked, b"cdefghij");
        // One write larger than the whole cap keeps its own tail.
        assert_eq!(park(&mut parked, b"0123456789", 8), 10);
        assert_eq!(parked, b"23456789");
    }
}
