//! OpenTelemetry export for Leviath's telemetry event stream (issue #73).
//!
//! The runtime emits pure-data [`TelemetryEvent`](leviath_core::telemetry::TelemetryEvent)s
//! into whatever [`TelemetrySink`] the host installs; this crate provides the
//! sinks that leave the process: [`OtelSink`] (OTLP over HTTP/protobuf - port
//! 4318, never gRPC) and [`LogSink`] (readable lines through `tracing`).
//! [`build_sink`] picks one from the `[observability]` config. The SDK
//! dependency stops here: `leviath-runtime` sees only the trait.

mod log_sink;
mod otel;

use std::sync::Arc;

use leviath_core::config::{ObservabilityConfig, TelemetryExporterKind};
use leviath_core::telemetry::TelemetrySink;

pub use log_sink::LogSink;
pub use otel::OtelSink;

/// A boxed `tracing-subscriber` layer, installable into the CLI's reloadable
/// subscriber slot to forward the process's own log events over OTLP.
pub type LogLayer = Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// What [`build_sink`] hands the host: the event sink for the engine's
/// telemetry resource, plus - for the OTLP exporter - a `tracing` layer that
/// exports the daemon's own log events through the same pipeline.
pub struct BuiltTelemetry {
    /// The sink the runtime's observer emits into.
    pub sink: Arc<dyn TelemetrySink>,
    /// Daemon-level log export (`None` for the stdout exporter, whose events
    /// already flow through `tracing`).
    pub log_layer: Option<LogLayer>,
}

/// The sink the config asks for, or `None` when telemetry is off (disabled,
/// `exporter = "none"`, or an OTLP pipeline that failed to build - the last
/// is logged and dropped rather than failing the daemon: observability must
/// never stop the work it observes).
pub fn build_sink(cfg: &ObservabilityConfig) -> Option<BuiltTelemetry> {
    if !cfg.enabled {
        return None;
    }
    match cfg.exporter {
        TelemetryExporterKind::None => None,
        TelemetryExporterKind::Stdout => Some(BuiltTelemetry {
            sink: Arc::new(LogSink),
            log_layer: None,
        }),
        TelemetryExporterKind::Otlp => {
            // The OTLP exporters construct a blocking reqwest client, which
            // panics when built on a tokio runtime thread (the daemon calls
            // this from one); build on a plain thread instead.
            let cfg = cfg.clone();
            let built = std::thread::spawn(move || OtelSink::from_config(&cfg))
                .join()
                .expect("exporter construction reports errors rather than panicking");
            match built {
                Ok(sink) => {
                    let log_layer = Some(sink.tracing_log_layer());
                    Some(BuiltTelemetry {
                        sink: Arc::new(sink),
                        log_layer,
                    })
                }
                Err(err) => {
                    tracing::warn!("telemetry disabled: {err}");
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(enabled: bool, exporter: TelemetryExporterKind) -> ObservabilityConfig {
        ObservabilityConfig {
            enabled,
            exporter,
            endpoint: None,
            service_name: None,
        }
    }

    #[test]
    fn disabled_config_builds_no_sink() {
        assert!(build_sink(&config(false, TelemetryExporterKind::Otlp)).is_none());
    }

    #[test]
    fn none_exporter_builds_no_sink() {
        assert!(build_sink(&config(true, TelemetryExporterKind::None)).is_none());
    }

    #[test]
    fn stdout_exporter_builds_the_log_sink_without_a_log_layer() {
        let built = build_sink(&config(true, TelemetryExporterKind::Stdout)).unwrap();
        assert!(built.log_layer.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn otlp_exporter_builds_from_a_runtime_thread_with_a_log_layer() {
        // The regression this guards: blocking-client construction panics on a
        // tokio thread unless it's hopped to a plain one.
        let built = build_sink(&config(true, TelemetryExporterKind::Otlp)).unwrap();
        assert!(built.log_layer.is_some());
    }

    #[test]
    fn an_unparseable_endpoint_disables_telemetry_with_a_warning() {
        let cfg = ObservabilityConfig {
            enabled: true,
            exporter: TelemetryExporterKind::Otlp,
            endpoint: Some("not a url at all".to_string()),
            service_name: None,
        };
        assert!(build_sink(&cfg).is_none());
    }
}
