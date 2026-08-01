//! The OpenTelemetry-backed sink: events in, OTLP spans/metrics/logs out.
//!
//! Span lifecycle in a polling engine: the observer can't hold RAII span
//! guards across ticks, so this sink keeps the live `agent.run` and
//! `agent.stage` span handles in a map keyed by run id - opened on
//! `RunStarted`/`StageEntered`, ended on `StageExited`/`RunCompleted`. Leaf
//! spans (`agent.inference`, `agent.tool_call`, `agent.compaction`) are
//! emitted retroactively at completion with explicit start/end times,
//! parented to the open stage. A daemon crash loses whatever was open;
//! recovered runs start a fresh trace tagged `leviath.recovered = true`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use leviath_core::config::ObservabilityConfig;
use leviath_core::telemetry::{LaneHealth, LogKind, TelemetryEvent, TelemetrySink};
use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider, UpDownCounter};
use opentelemetry::trace::{
    Span, SpanBuilder, SpanContext, TraceContextExt, Tracer, TracerProvider,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::Tracer as SdkTracer;

/// The default OTLP **HTTP** endpoint. 4318 is the HTTP/protobuf port; a
/// collector's 4317 gRPC listener will not answer these requests.
pub const DEFAULT_ENDPOINT: &str = "http://localhost:4318";

/// The default `service.name` resource attribute.
pub const DEFAULT_SERVICE_NAME: &str = "leviath";

/// The endpoint to export to: config wins, then the standard
/// `OTEL_EXPORTER_OTLP_ENDPOINT`, then [`DEFAULT_ENDPOINT`] - the same
/// file-over-env precedence the provider keys use.
pub(crate) fn resolve_endpoint(cfg: &ObservabilityConfig) -> String {
    cfg.endpoint
        .clone()
        .or_else(|| std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
}

/// The `service.name` to report: config, then `OTEL_SERVICE_NAME`, then
/// [`DEFAULT_SERVICE_NAME`].
pub(crate) fn resolve_service_name(cfg: &ObservabilityConfig) -> String {
    cfg.service_name
        .clone()
        .or_else(|| std::env::var("OTEL_SERVICE_NAME").ok())
        .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string())
}

/// The per-signal OTLP HTTP URL. `with_endpoint` on an HTTP exporter takes
/// the full path (unlike the env var, which names the base), so the signal
/// suffix is appended here.
pub(crate) fn signal_url(base: &str, signal: &str) -> String {
    format!("{}/v1/{signal}", base.trim_end_matches('/'))
}

/// Milliseconds-since-epoch as the `SystemTime` the span API wants.
pub(crate) fn ms_to_time(at_ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(at_ms).unwrap_or(0))
}

/// The log bridge behind a hand-rolled target filter.
///
/// `Layer::with_filter` would be the obvious spelling, but a `Filtered` layer
/// only works when it was part of the subscriber at construction time - this
/// one is swapped into an initially-empty reload slot later, where the
/// missing `FilterId` registration panics. The bridge only reacts to events,
/// so gating `on_event` is complete filtering for it.
struct TargetFilteredBridge<L> {
    inner: L,
    targets: tracing_subscriber::filter::Targets,
}

impl<S, L> tracing_subscriber::Layer<S> for TargetFilteredBridge<L>
where
    S: tracing::Subscriber,
    L: tracing_subscriber::Layer<S>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        if self.targets.would_enable(meta.target(), meta.level()) {
            self.inner.on_event(event, ctx);
        }
    }
}

/// A stage span held open between `StageEntered` and `StageExited`.
struct OpenStage {
    index: usize,
    /// When the stage was entered - the other edge of the duration histogram.
    entered_ms: i64,
    span: opentelemetry_sdk::trace::Span,
}

/// A run span (and its current stage) held open between `RunStarted` and
/// `RunCompleted`.
struct OpenRun {
    span: opentelemetry_sdk::trace::Span,
    stage: Option<OpenStage>,
}

/// The metric instruments, built once.
struct Instruments {
    active: UpDownCounter<i64>,
    tokens: Counter<u64>,
    tool_calls: Counter<u64>,
    stage_duration: Histogram<f64>,
    inference_latency: Histogram<f64>,
    runs: Counter<u64>,
    /// Tool-lane occupancy, re-stated on every health sample. Up-down counters
    /// rather than plain counters because these go both ways, and the sink adds
    /// the delta from the previous sample so a scrape reads the current value.
    lane: LaneInstruments,
}

/// The daemon-wide instruments, kept together because they share the
/// last-sample bookkeeping that turns a level into a delta.
struct LaneInstruments {
    tools_busy: UpDownCounter<i64>,
    tools_queued: UpDownCounter<i64>,
    tools_parked: UpDownCounter<i64>,
    dead_cycles: Counter<u64>,
    relief: Counter<u64>,
    /// The previous sample's levels, so each one can be reported as a delta.
    last: Mutex<LaneLevels>,
}

/// The gauge-shaped parts of [`LaneHealth`], as last reported.
#[derive(Default, Clone, Copy)]
struct LaneLevels {
    tools_busy: i64,
    tools_queued: i64,
    tools_parked: i64,
    dead_cycles: u32,
}

impl LaneInstruments {
    fn new(meter: &Meter) -> Self {
        Self {
            tools_busy: meter
                .i64_up_down_counter("leviath.tool_lane.busy")
                .with_description("Tool batches currently holding lane capacity")
                .build(),
            tools_queued: meter
                .i64_up_down_counter("leviath.tool_lane.queued")
                .with_description("Tool batches waiting for lane capacity")
                .build(),
            tools_parked: meter
                .i64_up_down_counter("leviath.tool_lane.parked")
                .with_description("Tool batches parked on an unbounded wait, holding no capacity")
                .build(),
            dead_cycles: meter
                .u64_counter("leviath.scheduler.dead_cycles.total")
                .with_description("Re-drives that found the lanes full and no run moving")
                .build(),
            relief: meter
                .u64_counter("leviath.tool_lane.relief.total")
                .with_description("Extra tool-lane capacity handed out to break a wedge")
                .build(),
            last: Mutex::new(LaneLevels::default()),
        }
    }

    /// Report one sample, converting the levels into the deltas an up-down
    /// counter wants.
    fn record(&self, health: &LaneHealth) {
        let now = LaneLevels {
            tools_busy: health.tools_busy as i64,
            tools_queued: health.tools_queued as i64,
            tools_parked: health.tools_parked as i64,
            dead_cycles: health.dead_cycles,
        };
        let mut last = self.last.lock().expect("telemetry lane level lock");
        self.tools_busy.add(now.tools_busy - last.tools_busy, &[]);
        self.tools_queued
            .add(now.tools_queued - last.tools_queued, &[]);
        self.tools_parked
            .add(now.tools_parked - last.tools_parked, &[]);
        // A streak that grew is that many more dead cycles; one that reset is
        // not negative progress, it is simply nothing to add.
        self.dead_cycles
            .add(now.dead_cycles.saturating_sub(last.dead_cycles) as u64, &[]);
        self.relief.add(health.relief_granted as u64, &[]);
        *last = now;
    }
}

impl Instruments {
    fn new(meter: &Meter) -> Self {
        Self {
            active: meter
                .i64_up_down_counter("leviath.agents.active")
                .with_description("Currently running agent runs")
                .build(),
            tokens: meter
                .u64_counter("leviath.tokens.total")
                .with_description("Cumulative tokens by provider, model, and kind")
                .build(),
            tool_calls: meter
                .u64_counter("leviath.tool_calls.total")
                .with_description("Tool calls by tool name and outcome")
                .build(),
            stage_duration: meter
                .f64_histogram("leviath.stage_duration")
                .with_description("Wall-clock seconds per stage")
                .with_unit("s")
                .build(),
            inference_latency: meter
                .f64_histogram("leviath.inference_latency")
                .with_description("Per-call inference latency by provider")
                .with_unit("s")
                .build(),
            // Every finished run, tagged with how it ended and whether it
            // produced anything. One counter rather than a separate "empty
            // runs" one: a bare count of empty runs cannot be normalized,
            // whereas this divides into a rate.
            runs: meter
                .u64_counter("leviath.runs.total")
                .with_description(
                    "Finished runs by terminal status and whether they produced output",
                )
                .build(),
            lane: LaneInstruments::new(meter),
        }
    }
}

/// [`TelemetrySink`] that exports to OpenTelemetry providers.
pub struct OtelSink {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
    tracer: SdkTracer,
    logger: opentelemetry_sdk::logs::SdkLogger,
    instruments: Instruments,
    open: Mutex<HashMap<String, OpenRun>>,
}

impl OtelSink {
    /// Wrap already-built providers. This is the seam the tests use (with the
    /// SDK's in-memory exporters); [`OtelSink::from_config`] is the OTLP path.
    pub fn new(
        tracer_provider: SdkTracerProvider,
        meter_provider: SdkMeterProvider,
        logger_provider: SdkLoggerProvider,
    ) -> Self {
        let tracer = tracer_provider.tracer("leviath");
        let logger = logger_provider.logger("leviath");
        let instruments = Instruments::new(&meter_provider.meter("leviath"));
        Self {
            tracer_provider,
            meter_provider,
            logger_provider,
            tracer,
            logger,
            instruments,
            open: Mutex::new(HashMap::new()),
        }
    }

    /// Build the OTLP HTTP/protobuf export pipeline from config + OTEL env
    /// fallbacks. Constructs a blocking HTTP client - call from a plain
    /// thread, not a tokio runtime thread ([`crate::build_sink`] does this).
    pub fn from_config(cfg: &ObservabilityConfig) -> Result<Self, String> {
        use opentelemetry_otlp::WithExportConfig;
        let endpoint = resolve_endpoint(cfg);
        let resource = Resource::builder()
            .with_service_name(resolve_service_name(cfg))
            .build();
        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(signal_url(&endpoint, "traces"))
            .build();
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(signal_url(&endpoint, "metrics"))
            .build();
        let logs = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(signal_url(&endpoint, "logs"))
            .build();
        // The three exporters share endpoint and config, so a build failure
        // hits all of them the same way; one error path reports whichever
        // surfaced first (per-exporter early returns would be branches only
        // that exporter's failure reaches).
        let (spans, metrics, logs) = match (spans, metrics, logs) {
            (Ok(spans), Ok(metrics), Ok(logs)) => (spans, metrics, logs),
            (spans, metrics, logs) => {
                let err = [spans.err(), metrics.err(), logs.err()]
                    .into_iter()
                    .flatten()
                    .next()
                    .expect("the non-Ok arm has at least one error");
                return Err(format!("building the OTLP exporters: {err}"));
            }
        };
        Ok(Self::new(
            SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(spans)
                .build(),
            SdkMeterProvider::builder()
                .with_resource(resource.clone())
                .with_periodic_exporter(metrics)
                .build(),
            SdkLoggerProvider::builder()
                .with_resource(resource)
                .with_batch_exporter(logs)
                .build(),
        ))
    }

    /// A `tracing-subscriber` layer that forwards the process's own `tracing`
    /// events into this sink's OTLP logs pipeline (daemon-level log export).
    ///
    /// These records correlate by resource attributes only - the daemon's
    /// tracing events fire outside any run's spans, so they carry no trace
    /// ids; the per-run [`TelemetryEvent::Log`] records are the correlated
    /// ones. Filtered to INFO, with the OTel/HTTP stack's own targets
    /// silenced: an export failure that logged through this bridge would
    /// otherwise generate more exports. The filtering is done inside
    /// `TargetFilteredBridge` rather than `Layer::with_filter` because a
    /// `Filtered` layer swapped into a reload slot after subscriber
    /// construction has no registered `FilterId` and panics.
    pub fn tracing_log_layer(&self) -> crate::LogLayer {
        use tracing_subscriber::filter::LevelFilter;
        let bridge = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
            &self.logger_provider,
        );
        let targets = tracing_subscriber::filter::Targets::new()
            .with_default(LevelFilter::INFO)
            .with_target("opentelemetry", LevelFilter::OFF)
            .with_target("opentelemetry_sdk", LevelFilter::OFF)
            .with_target("opentelemetry-otlp", LevelFilter::OFF)
            .with_target("hyper", LevelFilter::OFF)
            .with_target("reqwest", LevelFilter::OFF)
            .with_target("h2", LevelFilter::OFF);
        Box::new(TargetFilteredBridge {
            inner: bridge,
            targets,
        })
    }

    /// Start a span at `at_ms`, optionally as a child of `parent`.
    fn start_span(
        &self,
        name: &'static str,
        at_ms: i64,
        attributes: Vec<KeyValue>,
        parent: Option<&SpanContext>,
    ) -> opentelemetry_sdk::trace::Span {
        let builder = SpanBuilder::from_name(name)
            .with_start_time(ms_to_time(at_ms))
            .with_attributes(attributes);
        let cx = match parent {
            Some(parent) => Context::new().with_remote_span_context(parent.clone()),
            None => Context::new(),
        };
        self.tracer.build_with_context(builder, &cx)
    }

    /// Emit a completed leaf span under the run's open stage (or, if no stage
    /// is open, under the run itself), spanning the last `duration_ms`.
    fn emit_leaf(&self, run_id: &str, name: &'static str, duration_ms: u64, attrs: Vec<KeyValue>) {
        let mut open = self.open.lock().expect("telemetry span map lock");
        let Some(run) = open.get_mut(run_id) else {
            return; // events for a run this sink never saw start
        };
        let parent = match run.stage.as_ref() {
            Some(stage) => stage.span.span_context().clone(),
            None => run.span.span_context().clone(),
        };
        // The event fires at completion, so the span covers the last
        // `duration_ms` ending now.
        let end = SystemTime::now();
        let start = end
            .checked_sub(Duration::from_millis(duration_ms))
            .unwrap_or(UNIX_EPOCH);
        let builder = SpanBuilder::from_name(name)
            .with_start_time(start)
            .with_attributes(attrs);
        let cx = Context::new().with_remote_span_context(parent);
        let mut span = self.tracer.build_with_context(builder, &cx);
        span.end_with_timestamp(end);
    }

    /// End the run's open stage span (if any) at `at_ms`.
    fn close_stage(run: &mut OpenRun, at_ms: i64) {
        if let Some(mut stage) = run.stage.take() {
            stage.span.end_with_timestamp(ms_to_time(at_ms));
        }
    }
}

impl TelemetrySink for OtelSink {
    fn emit(&self, event: TelemetryEvent) {
        match event {
            TelemetryEvent::RunStarted {
                run_id,
                agent_name,
                model,
                parent_run_id,
                recovered,
                at_ms,
            } => {
                let mut attrs = vec![
                    KeyValue::new("leviath.run.id", run_id.clone()),
                    KeyValue::new("leviath.agent.name", agent_name),
                    KeyValue::new("leviath.recovered", recovered),
                ];
                if let Some(model) = model {
                    attrs.push(KeyValue::new("leviath.model", model));
                }
                if let Some(parent) = parent_run_id {
                    attrs.push(KeyValue::new("leviath.parent_run.id", parent));
                }
                let span = self.start_span("agent.run", at_ms, attrs, None);
                self.instruments.active.add(1, &[]);
                self.open
                    .lock()
                    .expect("telemetry span map lock")
                    .insert(run_id, OpenRun { span, stage: None });
            }
            TelemetryEvent::StageEntered {
                run_id,
                stage_index,
                stage_name,
                at_ms,
            } => {
                let mut open = self.open.lock().expect("telemetry span map lock");
                let Some(run) = open.get_mut(&run_id) else {
                    return;
                };
                let parent = run.span.span_context().clone();
                let span = self.start_span(
                    "agent.stage",
                    at_ms,
                    vec![
                        KeyValue::new("leviath.stage.name", stage_name),
                        KeyValue::new("leviath.stage.index", stage_index as i64),
                    ],
                    Some(&parent),
                );
                run.stage = Some(OpenStage {
                    index: stage_index,
                    entered_ms: at_ms,
                    span,
                });
            }
            TelemetryEvent::StageExited {
                run_id,
                stage_index,
                stage_name,
                prompt_tokens,
                completion_tokens,
                at_ms,
            } => {
                let mut open = self.open.lock().expect("telemetry span map lock");
                let Some(run) = open.get_mut(&run_id) else {
                    return;
                };
                let Some(stage) = run.stage.as_mut() else {
                    return;
                };
                if stage.index != stage_index {
                    return; // stale exit for a stage this sink isn't holding
                }
                stage
                    .span
                    .set_attribute(KeyValue::new("leviath.tokens.prompt", prompt_tokens as i64));
                stage.span.set_attribute(KeyValue::new(
                    "leviath.tokens.completion",
                    completion_tokens as i64,
                ));
                let duration_s = (at_ms - stage.entered_ms).max(0) as f64 / 1000.0;
                Self::close_stage(run, at_ms);
                self.instruments.stage_duration.record(
                    duration_s,
                    &[KeyValue::new("leviath.stage.name", stage_name)],
                );
            }
            TelemetryEvent::InferenceCompleted {
                run_id,
                stage_name: _,
                provider,
                model,
                latency_ms,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                success,
            } => {
                let token_attrs = [
                    KeyValue::new("leviath.provider", provider.clone()),
                    KeyValue::new("leviath.model", model.clone()),
                ];
                for (kind, count) in [
                    ("prompt", prompt_tokens),
                    ("completion", completion_tokens),
                    ("cached", cached_tokens),
                ] {
                    if count > 0 {
                        let mut attrs = token_attrs.to_vec();
                        attrs.push(KeyValue::new("leviath.tokens.kind", kind));
                        self.instruments.tokens.add(count as u64, &attrs);
                    }
                }
                self.instruments.inference_latency.record(
                    latency_ms as f64 / 1000.0,
                    &[KeyValue::new("leviath.provider", provider.clone())],
                );
                self.emit_leaf(
                    &run_id,
                    "agent.inference",
                    latency_ms,
                    vec![
                        KeyValue::new("leviath.provider", provider),
                        KeyValue::new("leviath.model", model),
                        KeyValue::new("leviath.tokens.prompt", prompt_tokens as i64),
                        KeyValue::new("leviath.tokens.completion", completion_tokens as i64),
                        KeyValue::new("leviath.tokens.cached", cached_tokens as i64),
                        KeyValue::new("leviath.success", success),
                    ],
                );
            }
            TelemetryEvent::ToolCallCompleted {
                run_id,
                stage_name: _,
                tool_name,
                batch_latency_ms,
                success,
            } => {
                self.instruments.tool_calls.add(
                    1,
                    &[
                        KeyValue::new("leviath.tool.name", tool_name.clone()),
                        KeyValue::new("leviath.outcome", if success { "ok" } else { "error" }),
                    ],
                );
                self.emit_leaf(
                    &run_id,
                    "agent.tool_call",
                    batch_latency_ms,
                    vec![
                        KeyValue::new("leviath.tool.name", tool_name),
                        KeyValue::new("leviath.success", success),
                        KeyValue::new("leviath.batch_latency_ms", batch_latency_ms as i64),
                    ],
                );
            }
            TelemetryEvent::CompactionCompleted {
                run_id,
                stage_name: _,
                success,
            } => {
                self.emit_leaf(
                    &run_id,
                    "agent.compaction",
                    0,
                    vec![KeyValue::new("leviath.success", success)],
                );
            }
            TelemetryEvent::RunCompleted {
                run_id,
                status,
                prompt_tokens,
                completion_tokens,
                tool_calls,
                empty_output,
                at_ms,
            } => {
                // Counted before the span lookup: the metric answers "how many
                // runs finished, and how many had nothing to show for it",
                // which must not depend on whether this process happens to
                // hold the run's open span.
                self.instruments.runs.add(
                    1,
                    &[
                        KeyValue::new("leviath.status", status.clone()),
                        KeyValue::new("leviath.empty_output", empty_output),
                    ],
                );
                let mut open = self.open.lock().expect("telemetry span map lock");
                let Some(mut run) = open.remove(&run_id) else {
                    return;
                };
                drop(open);
                // The observer closes the stage first; be defensive anyway so
                // a crash-path completion still ends cleanly.
                Self::close_stage(&mut run, at_ms);
                run.span
                    .set_attribute(KeyValue::new("leviath.status", status));
                run.span
                    .set_attribute(KeyValue::new("leviath.tokens.prompt", prompt_tokens as i64));
                run.span.set_attribute(KeyValue::new(
                    "leviath.tokens.completion",
                    completion_tokens as i64,
                ));
                run.span
                    .set_attribute(KeyValue::new("leviath.tool_calls", tool_calls as i64));
                run.span
                    .set_attribute(KeyValue::new("leviath.empty_output", empty_output));
                run.span.end_with_timestamp(ms_to_time(at_ms));
                self.instruments.active.add(-1, &[]);
            }
            TelemetryEvent::Log {
                run_id,
                stage_index,
                kind,
                line,
            } => {
                let open = self.open.lock().expect("telemetry span map lock");
                let Some(run) = open.get(&run_id) else {
                    return;
                };
                let span_context = match run.stage.as_ref() {
                    Some(stage) => stage.span.span_context().clone(),
                    None => run.span.span_context().clone(),
                };
                let mut record = self.logger.create_log_record();
                record.set_timestamp(SystemTime::now());
                record.set_severity_number(Severity::Info);
                record.set_body(AnyValue::from(line));
                record.set_trace_context(
                    span_context.trace_id(),
                    span_context.span_id(),
                    Some(span_context.trace_flags()),
                );
                record.add_attribute("leviath.run.id", run_id);
                record.add_attribute("leviath.stage.index", stage_index as i64);
                record.add_attribute(
                    "leviath.log.kind",
                    match kind {
                        LogKind::Output => "output",
                        LogKind::Runtime => "runtime",
                    },
                );
                self.logger.emit(record);
            }
        }
    }

    fn observe_lanes(&self, health: LaneHealth) {
        self.instruments.lane.record(&health);
    }

    fn force_flush(&self) {
        let _ = self.tracer_provider.force_flush();
        let _ = self.meter_provider.force_flush();
        let _ = self.logger_provider.force_flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn tracing_log_layer_forwards_app_events_and_filters_noise() {
        let h = harness();
        let subscriber = tracing_subscriber::registry().with(h.sink.tracing_log_layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "leviath::daemon", "exported line");
            tracing::info!(target: "hyper", "http-stack noise");
            tracing::debug!(target: "leviath::daemon", "below the info floor");
        });
        let logs = h.logs.get_emitted_logs().unwrap();
        assert_eq!(logs.len(), 1, "{logs:?}");
        assert!(format!("{:?}", logs[0].record.body()).contains("exported line"));
    }

    struct Harness {
        sink: OtelSink,
        spans: InMemorySpanExporter,
        metrics: InMemoryMetricExporter,
        logs: InMemoryLogExporter,
    }

    fn harness() -> Harness {
        let spans = InMemorySpanExporter::default();
        let metrics = InMemoryMetricExporter::default();
        let logs = InMemoryLogExporter::default();
        let sink = OtelSink::new(
            SdkTracerProvider::builder()
                .with_simple_exporter(spans.clone())
                .build(),
            SdkMeterProvider::builder()
                .with_periodic_exporter(metrics.clone())
                .build(),
            SdkLoggerProvider::builder()
                .with_simple_exporter(logs.clone())
                .build(),
        );
        Harness {
            sink,
            spans,
            metrics,
            logs,
        }
    }

    fn run_started(run_id: &str, at_ms: i64) -> TelemetryEvent {
        TelemetryEvent::RunStarted {
            run_id: run_id.to_string(),
            agent_name: "coder".to_string(),
            model: Some("mock/m".to_string()),
            parent_run_id: Some("r0".to_string()),
            recovered: false,
            at_ms,
        }
    }

    fn stage_entered(run_id: &str, index: usize, at_ms: i64) -> TelemetryEvent {
        TelemetryEvent::StageEntered {
            run_id: run_id.to_string(),
            stage_index: index,
            stage_name: format!("stage{index}"),
            at_ms,
        }
    }

    fn stage_exited(run_id: &str, index: usize, at_ms: i64) -> TelemetryEvent {
        TelemetryEvent::StageExited {
            run_id: run_id.to_string(),
            stage_index: index,
            stage_name: format!("stage{index}"),
            prompt_tokens: 10,
            completion_tokens: 4,
            at_ms,
        }
    }

    fn run_completed(run_id: &str, at_ms: i64) -> TelemetryEvent {
        TelemetryEvent::RunCompleted {
            run_id: run_id.to_string(),
            status: "complete".to_string(),
            prompt_tokens: 10,
            completion_tokens: 4,
            tool_calls: 1,
            empty_output: false,
            at_ms,
        }
    }

    #[test]
    fn run_and_stage_spans_nest_with_explicit_times() {
        let h = harness();
        h.sink.emit(run_started("r1", 1_000));
        h.sink.emit(stage_entered("r1", 0, 1_000));
        h.sink.emit(stage_exited("r1", 0, 3_500));
        h.sink.emit(run_completed("r1", 4_000));

        let spans = h.spans.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 2, "{spans:?}");
        let stage = &spans[0];
        let run = &spans[1];
        assert_eq!(stage.name, "agent.stage");
        assert_eq!(run.name, "agent.run");
        // The stage nests under the run, in the same trace.
        assert_eq!(stage.parent_span_id, run.span_context.span_id());
        assert_eq!(stage.span_context.trace_id(), run.span_context.trace_id());
        // Explicit timestamps from the events, not the wall clock.
        assert_eq!(run.start_time, ms_to_time(1_000));
        assert_eq!(run.end_time, ms_to_time(4_000));
        assert_eq!(stage.start_time, ms_to_time(1_000));
        assert_eq!(stage.end_time, ms_to_time(3_500));
        // Final status and totals landed on the run span.
        assert!(
            run.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "leviath.status")
        );
    }

    #[test]
    fn inference_span_nests_under_the_open_stage() {
        let h = harness();
        h.sink.emit(run_started("r1", 0));
        h.sink.emit(stage_entered("r1", 0, 0));
        h.sink.emit(TelemetryEvent::InferenceCompleted {
            run_id: "r1".to_string(),
            stage_name: "stage0".to_string(),
            provider: "anthropic".to_string(),
            model: "m".to_string(),
            latency_ms: 250,
            prompt_tokens: 10,
            completion_tokens: 4,
            cached_tokens: 2,
            success: true,
        });
        let spans = h.spans.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        let inference = &spans[0];
        assert_eq!(inference.name, "agent.inference");
        // The stage span is still open; harvest its id by closing everything.
        h.sink.emit(stage_exited("r1", 0, 1));
        h.sink.emit(run_completed("r1", 1));
        let spans = h.spans.get_finished_spans().unwrap();
        let stage = spans.iter().find(|s| s.name == "agent.stage").unwrap();
        assert_eq!(inference.parent_span_id, stage.span_context.span_id());
        assert!(
            inference
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "leviath.provider")
        );
    }

    #[test]
    fn leaf_spans_between_stages_nest_under_the_run() {
        let h = harness();
        h.sink.emit(run_started("r1", 0));
        h.sink.emit(stage_entered("r1", 0, 0));
        h.sink.emit(stage_exited("r1", 0, 1));
        h.sink.emit(TelemetryEvent::ToolCallCompleted {
            run_id: "r1".to_string(),
            stage_name: "stage0".to_string(),
            tool_name: "read_file".to_string(),
            batch_latency_ms: 30,
            success: false,
        });
        h.sink.emit(TelemetryEvent::CompactionCompleted {
            run_id: "r1".to_string(),
            stage_name: "stage0".to_string(),
            success: true,
        });
        h.sink.emit(run_completed("r1", 2));
        let spans = h.spans.get_finished_spans().unwrap();
        let run = spans.iter().find(|s| s.name == "agent.run").unwrap();
        let tool = spans.iter().find(|s| s.name == "agent.tool_call").unwrap();
        let compaction = spans.iter().find(|s| s.name == "agent.compaction").unwrap();
        assert_eq!(tool.parent_span_id, run.span_context.span_id());
        assert_eq!(compaction.parent_span_id, run.span_context.span_id());
    }

    #[test]
    fn a_run_without_model_or_parent_omits_those_attributes() {
        let h = harness();
        h.sink.emit(TelemetryEvent::RunStarted {
            run_id: "r1".to_string(),
            agent_name: "coder".to_string(),
            model: None,
            parent_run_id: None,
            recovered: false,
            at_ms: 0,
        });
        h.sink.emit(run_completed("r1", 1));
        let spans = h.spans.get_finished_spans().unwrap();
        let run = spans.iter().find(|s| s.name == "agent.run").unwrap();
        let keys: Vec<&str> = run.attributes.iter().map(|kv| kv.key.as_str()).collect();
        assert!(!keys.contains(&"leviath.model"), "{keys:?}");
        assert!(!keys.contains(&"leviath.parent_run.id"), "{keys:?}");
    }

    #[test]
    fn events_for_an_unknown_run_are_dropped() {
        let h = harness();
        h.sink.emit(stage_entered("ghost", 0, 0));
        h.sink.emit(stage_exited("ghost", 0, 0));
        h.sink.emit(TelemetryEvent::InferenceCompleted {
            run_id: "ghost".to_string(),
            stage_name: "s".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            latency_ms: 1,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            success: true,
        });
        h.sink.emit(TelemetryEvent::Log {
            run_id: "ghost".to_string(),
            stage_index: 0,
            kind: LogKind::Runtime,
            line: "x".to_string(),
        });
        h.sink.emit(run_completed("ghost", 0));
        assert!(h.spans.get_finished_spans().unwrap().is_empty());
        assert!(h.logs.get_emitted_logs().unwrap().is_empty());
    }

    #[test]
    fn a_stale_stage_exit_is_ignored() {
        let h = harness();
        h.sink.emit(run_started("r1", 0));
        h.sink.emit(stage_entered("r1", 1, 0));
        // Wrong index: not the stage the sink holds open.
        h.sink.emit(stage_exited("r1", 3, 5));
        assert!(h.spans.get_finished_spans().unwrap().is_empty());
        // No stage open at all after a real exit: a second exit is a no-op.
        h.sink.emit(stage_exited("r1", 1, 6));
        h.sink.emit(stage_exited("r1", 1, 7));
        assert_eq!(h.spans.get_finished_spans().unwrap().len(), 1);
    }

    #[test]
    fn run_completed_closes_a_still_open_stage() {
        let h = harness();
        h.sink.emit(run_started("r1", 0));
        h.sink.emit(stage_entered("r1", 0, 0));
        h.sink.emit(run_completed("r1", 9));
        let spans = h.spans.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "agent.stage");
        assert_eq!(spans[0].end_time, ms_to_time(9));
    }

    #[test]
    fn log_records_carry_the_open_span_context() {
        let h = harness();
        h.sink.emit(run_started("r1", 0));
        h.sink.emit(stage_entered("r1", 0, 0));
        h.sink.emit(TelemetryEvent::Log {
            run_id: "r1".to_string(),
            stage_index: 0,
            kind: LogKind::Output,
            line: "hello".to_string(),
        });
        h.sink.emit(stage_exited("r1", 0, 1));
        h.sink.emit(TelemetryEvent::Log {
            run_id: "r1".to_string(),
            stage_index: 0,
            kind: LogKind::Runtime,
            line: "between stages".to_string(),
        });
        h.sink.emit(run_completed("r1", 2));

        let logs = h.logs.get_emitted_logs().unwrap();
        assert_eq!(logs.len(), 2);
        let spans = h.spans.get_finished_spans().unwrap();
        let stage = spans.iter().find(|s| s.name == "agent.stage").unwrap();
        let run = spans.iter().find(|s| s.name == "agent.run").unwrap();
        let first = logs[0].record.trace_context().unwrap();
        assert_eq!(first.trace_id, run.span_context.trace_id());
        assert_eq!(first.span_id, stage.span_context.span_id());
        let second = logs[1].record.trace_context().unwrap();
        assert_eq!(second.span_id, run.span_context.span_id());
    }

    #[test]
    fn metrics_flow_from_the_event_stream() {
        let h = harness();
        h.sink.emit(run_started("r1", 0));
        h.sink.emit(stage_entered("r1", 0, 0));
        h.sink.emit(TelemetryEvent::InferenceCompleted {
            run_id: "r1".to_string(),
            stage_name: "stage0".to_string(),
            provider: "anthropic".to_string(),
            model: "m".to_string(),
            latency_ms: 250,
            prompt_tokens: 10,
            completion_tokens: 4,
            cached_tokens: 0,
            success: true,
        });
        h.sink.emit(TelemetryEvent::ToolCallCompleted {
            run_id: "r1".to_string(),
            stage_name: "stage0".to_string(),
            tool_name: "read_file".to_string(),
            batch_latency_ms: 30,
            success: true,
        });
        h.sink.emit(stage_exited("r1", 0, 2_000));
        h.sink.emit(run_completed("r1", 2_000));
        h.sink.force_flush();

        let exported = h.metrics.get_finished_metrics().unwrap();
        let names: Vec<String> = exported
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .map(|m| m.name().to_string())
            .collect();
        for expected in [
            "leviath.agents.active",
            "leviath.tokens.total",
            "leviath.tool_calls.total",
            "leviath.stage_duration",
            "leviath.inference_latency",
            "leviath.runs.total",
        ] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
    }

    /// The empty-run counter has to survive a completion whose span this
    /// process never opened, or a daemon restart would silently under-count
    /// exactly the runs the metric exists to find (issue #192).
    #[test]
    fn a_completion_counts_even_without_an_open_span() {
        let h = harness();
        let TelemetryEvent::RunCompleted { run_id, status, .. } = run_completed("ghost", 10) else {
            unreachable!("run_completed builds a RunCompleted")
        };
        h.sink.emit(TelemetryEvent::RunCompleted {
            run_id,
            status,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
            empty_output: true,
            at_ms: 10,
        });
        h.sink.force_flush();

        let exported = h.metrics.get_finished_metrics().unwrap();
        assert!(
            exported
                .iter()
                .flat_map(|rm| rm.scope_metrics())
                .flat_map(|sm| sm.metrics())
                .any(|m| m.name() == "leviath.runs.total"),
            "a completion with no open span still counts"
        );
    }

    /// The daemon-wide instruments. Lane occupancy goes up and down, so the sink
    /// turns each sample's level into a delta; a dead-cycle streak only ever
    /// grows, and resetting to zero adds nothing rather than going backwards.
    #[test]
    fn lane_health_samples_export_as_deltas() {
        let h = harness();
        h.sink.observe_lanes(LaneHealth {
            tools_busy: 3,
            tools_queued: 5,
            tools_parked: 1,
            tools_workers: 8,
            dead_cycles: 2,
            ..Default::default()
        });
        // Busier, one more dead cycle, and some relief handed out.
        h.sink.observe_lanes(LaneHealth {
            agents_active: 6,
            agents_waiting: 2,
            tools_busy: 8,
            tools_queued: 2,
            tools_parked: 4,
            tools_workers: 8,
            dead_cycles: 3,
            relief_granted: 2,
        });
        // The wedge cleared: the streak resets, which must not subtract.
        h.sink.observe_lanes(LaneHealth {
            tools_workers: 8,
            ..Default::default()
        });
        h.sink.force_flush();

        let exported = h.metrics.get_finished_metrics().unwrap();
        let names: Vec<String> = exported
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .map(|m| m.name().to_string())
            .collect();
        for expected in [
            "leviath.tool_lane.busy",
            "leviath.tool_lane.queued",
            "leviath.tool_lane.parked",
            "leviath.scheduler.dead_cycles.total",
            "leviath.tool_lane.relief.total",
        ] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
    }

    /// The delta arithmetic on its own, where the numbers are readable.
    #[test]
    fn lane_levels_become_deltas_and_streaks_only_climb() {
        let meter = SdkMeterProvider::builder().build().meter("test");
        let lane = LaneInstruments::new(&meter);
        let levels = |lane: &LaneInstruments| *lane.last.lock().unwrap();

        lane.record(&LaneHealth {
            tools_busy: 4,
            tools_queued: 2,
            tools_parked: 1,
            dead_cycles: 5,
            ..Default::default()
        });
        let after = levels(&lane);
        assert_eq!((after.tools_busy, after.tools_queued), (4, 2));
        assert_eq!(after.dead_cycles, 5);

        // A quiet sample takes the levels back down and the streak to zero.
        lane.record(&LaneHealth::default());
        let after = levels(&lane);
        assert_eq!(
            (after.tools_busy, after.tools_queued, after.tools_parked),
            (0, 0, 0)
        );
        assert_eq!(after.dead_cycles, 0, "the streak reset");
    }

    #[test]
    fn endpoint_and_service_resolution_prefer_config_over_env() {
        temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env:4318")),
                ("OTEL_SERVICE_NAME", Some("env-name")),
            ],
            || {
                let mut cfg = ObservabilityConfig::default();
                assert_eq!(resolve_endpoint(&cfg), "http://env:4318");
                assert_eq!(resolve_service_name(&cfg), "env-name");
                cfg.endpoint = Some("http://file:4318".to_string());
                cfg.service_name = Some("file-name".to_string());
                assert_eq!(resolve_endpoint(&cfg), "http://file:4318");
                assert_eq!(resolve_service_name(&cfg), "file-name");
            },
        );
        temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None::<&str>),
                ("OTEL_SERVICE_NAME", None),
            ],
            || {
                let cfg = ObservabilityConfig::default();
                assert_eq!(resolve_endpoint(&cfg), DEFAULT_ENDPOINT);
                assert_eq!(resolve_service_name(&cfg), DEFAULT_SERVICE_NAME);
            },
        );
    }

    #[test]
    fn signal_urls_append_paths_without_doubling_slashes() {
        assert_eq!(
            signal_url("http://localhost:4318", "traces"),
            "http://localhost:4318/v1/traces"
        );
        assert_eq!(
            signal_url("http://localhost:4318/", "logs"),
            "http://localhost:4318/v1/logs"
        );
    }

    #[test]
    fn ms_to_time_clamps_negative_to_epoch() {
        assert_eq!(ms_to_time(-5), UNIX_EPOCH);
        assert_eq!(ms_to_time(1_000), UNIX_EPOCH + Duration::from_secs(1));
    }

    #[test]
    fn otlp_pipeline_exports_to_a_live_http_endpoint() {
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A minimal OTLP receiver: answer every POST with 200.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_seen = hits.clone();
        std::thread::spawn(move || {
            // `flatten` skips accept errors; the thread dies with the process.
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 65536];
                let _ = stream.read(&mut buf);
                hits_seen.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            }
        });

        let cfg = ObservabilityConfig {
            enabled: true,
            exporter: leviath_core::config::TelemetryExporterKind::Otlp,
            endpoint: Some(format!("http://{addr}")),
            service_name: Some("leviath-test".to_string()),
        };
        let sink = OtelSink::from_config(&cfg).unwrap();
        sink.emit(run_started("r1", 0));
        sink.emit(run_completed("r1", 1));
        sink.force_flush();
        assert!(
            hits.load(Ordering::SeqCst) > 0,
            "the exporter never reached the endpoint"
        );
    }
}
