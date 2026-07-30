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

use std::sync::OnceLock;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

/// What the reload slot holds: nothing, or the installed OTLP layer.
type OtelSlot = Option<leviath_telemetry::LogLayer>;

/// The handle [`install_otel_layer`] reloads through, parked by [`init`].
static OTEL_HANDLE: OnceLock<reload::Handle<OtelSlot, Registry>> = OnceLock::new();

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
            .with_writer(std::io::stderr)
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
}
