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

/// The sink the config asks for, or `None` when telemetry is off (disabled,
/// `exporter = "none"`, or an OTLP pipeline that failed to build - the last
/// is logged and dropped rather than failing the daemon: observability must
/// never stop the work it observes).
pub fn build_sink(cfg: &ObservabilityConfig) -> Option<Arc<dyn TelemetrySink>> {
    if !cfg.enabled {
        return None;
    }
    match cfg.exporter {
        TelemetryExporterKind::None => None,
        TelemetryExporterKind::Stdout => Some(Arc::new(LogSink)),
        TelemetryExporterKind::Otlp => {
            // The OTLP exporters construct a blocking reqwest client, which
            // panics when built on a tokio runtime thread (the daemon calls
            // this from one); build on a plain thread instead.
            let cfg = cfg.clone();
            let built = std::thread::spawn(move || OtelSink::from_config(&cfg))
                .join()
                .expect("exporter construction reports errors rather than panicking");
            match built {
                Ok(sink) => Some(Arc::new(sink)),
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
    fn stdout_exporter_builds_the_log_sink() {
        assert!(build_sink(&config(true, TelemetryExporterKind::Stdout)).is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn otlp_exporter_builds_from_a_runtime_thread() {
        // The regression this guards: blocking-client construction panics on a
        // tokio thread unless it's hopped to a plain one.
        let sink = build_sink(&config(true, TelemetryExporterKind::Otlp));
        assert!(sink.is_some());
    }
}
