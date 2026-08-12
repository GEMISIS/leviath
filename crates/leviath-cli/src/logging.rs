//! Process-wide logging: the subscriber `main` installs, with a reloadable
//! slot for the OTLP log-export layer.
//!
//! The subscriber must exist before any subcommand logs, but the
//! `[observability]` config that decides whether daemon logs also export over
//! OTLP is only read later (by the daemon, after `Config::load`). Bridging
//! that gap is what the reload slot is for: [`init`] installs the fmt layer
//! plus an empty slot and parks the reload handle in a static;
//! [`install_otel_layer`] fills the slot once the daemon has built its
//! exporter. Everything stays on **stderr** - `lev agent-client` uses stdout
//! as its JSON-RPC channel, and a stray log line there would corrupt the
//! stream a host is parsing.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

/// What the reload slot holds: nothing, or the installed OTLP layer.
type OtelSlot = Option<leviath_telemetry::LogLayer>;

/// The handle [`install_otel_layer`] reloads through, parked by [`init`].
static OTEL_HANDLE: OnceLock<reload::Handle<OtelSlot, Registry>> = OnceLock::new();

/// Whether a TUI currently owns the terminal.
static TUI_HOLDS_TERMINAL: AtomicBool = AtomicBool::new(false);

/// Lines written while the terminal was held, waiting to be flushed.
static PARKED: Mutex<Vec<u8>> = Mutex::new(Vec::new());

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
            // A poisoned lock means another thread panicked mid-write. Dropping
            // the line beats propagating a panic out of a logging call, which
            // would turn a stray debug line into a crash.
            if let Ok(mut parked) = PARKED.lock() {
                parked.extend_from_slice(buf);
            }
            return Ok(buf.len());
        }
        std::io::stderr().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if TUI_HOLDS_TERMINAL.load(Ordering::Relaxed) {
            return Ok(());
        }
        std::io::stderr().flush()
    }
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
    let parked = match PARKED.lock() {
        Ok(mut parked) if !parked.is_empty() => std::mem::take(&mut *parked),
        _ => return,
    };
    let _ = std::io::stderr().write_all(&parked);
    let _ = std::io::stderr().flush();
}

/// Install the process-wide subscriber: fmt → stderr at `info` (`debug` when
/// verbose), plus the empty reloadable OTLP slot.
///
/// Callable any number of times without panicking; the first global
/// subscriber and the first parked handle win. `main` calls it exactly once,
/// so in the real process the two are the same subscriber - the losing-race
/// cases exist only inside the test binary, where other tests own the global
/// slot.
pub fn init(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let (otel_layer, handle) = reload::Layer::new(None as OtelSlot);
    let subscriber = tracing_subscriber::registry().with(otel_layer).with(
        tracing_subscriber::fmt::layer()
            .with_writer(|| TerminalAwareWriter)
            .with_filter(EnvFilter::new(level)),
    );
    let _ = subscriber.try_init();
    let _ = OTEL_HANDLE.set(handle);
}

/// Fill the reload slot with the daemon's OTLP log-export layer. Returns
/// whether the layer was installed - `false` when [`init`] hasn't run (a
/// library consumer with its own subscriber) or the slot is gone.
pub fn install_otel_layer(layer: leviath_telemetry::LogLayer) -> bool {
    match OTEL_HANDLE.get() {
        Some(handle) => handle.reload(Some(layer)).is_ok(),
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
        assert!(!install_otel_layer(layer));

        // Park a handle whose subscriber this thread controls.
        let (otel_layer, handle) = reload::Layer::new(None as OtelSlot);
        assert!(
            OTEL_HANDLE.set(handle).is_ok(),
            "this test parks the handle first"
        );
        let subscriber = tracing_subscriber::registry().with(otel_layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let (layer, exporter) = bridge_with_exporter();
        assert!(install_otel_layer(layer));
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
        assert!(install_otel_layer(layer));
    }

    /// The hold is what keeps a log line out of a wizard someone is reading,
    /// and the release is what keeps it from being lost instead. Both halves
    /// live in one test because the flag is process-wide: a second test
    /// toggling it in parallel would park the first one's writes.
    #[test]
    fn holding_the_terminal_parks_output_until_it_is_released() {
        // Released is the resting state, so a write goes straight out.
        release_from_tui();
        assert!(!TUI_HOLDS_TERMINAL.load(Ordering::Relaxed));
        TerminalAwareWriter.write_all(b"").expect("stderr accepts a write");
        TerminalAwareWriter.flush().expect("stderr accepts a flush");

        hold_for_tui();
        TerminalAwareWriter
            .write_all(b"parked line\n")
            .expect("a held write is buffered, never refused");
        // A flush while held must not reach the terminal either, or the point
        // of buffering is lost on the very next `tracing` call.
        TerminalAwareWriter.flush().expect("a held flush is a no-op");
        assert_eq!(
            PARKED.lock().expect("uncontended").as_slice(),
            b"parked line\n"
        );

        release_from_tui();
        assert!(
            PARKED.lock().expect("uncontended").is_empty(),
            "release hands the buffer to stderr and empties it"
        );
        // Releasing twice is what the panic hook plus `Drop` actually does, and
        // with nothing parked it must stay quiet rather than write an empty
        // line.
        release_from_tui();
    }
}
