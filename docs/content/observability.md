---
title: Observability
group: Reference
group_order: 3
order: 8
---

# Observability

Leviath can export structured traces, metrics, and logs over OpenTelemetry.
Everything is off by default; turning it on is one config block, and nothing
about a run changes when it is off.

## Configuration

```toml
[observability]
enabled = true
exporter = "otlp"                     # "otlp" | "stdout" | "none"
endpoint = "http://localhost:4318"    # OTLP over HTTP - 4318, not the 4317 gRPC port
service_name = "leviath"
```

The standard OpenTelemetry environment variables fill any hole the file
leaves: `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_SERVICE_NAME` are honored, and
an explicit config value wins over the environment. `exporter = "stdout"`
narrates the same events as readable log lines on stderr instead of shipping
them anywhere, which is the quickest way to see what would be exported.

> [!NOTE]
> Export is OTLP over HTTP/protobuf on port 4318. The 4317 gRPC endpoint that
> many collector examples use will not work; point Leviath at the HTTP port.

## What gets exported

**Traces.** Every run becomes one trace: an `agent.run` root span, an
`agent.stage` span per stage the workflow passes through, and a per-call
`agent.inference` or `agent.tool_call` span inside the stage. Stage
transitions, retries, and terminal status all land as span attributes, so a
stuck or looping run is visible as a shape, not just a log line.

**Metrics.** Token counters (prompt and completion), stage-duration
histograms, and inference-latency histograms, labeled by agent, stage, and
model.

**Logs.** Log records carry the run's trace ID, so a collector that joins the
three signals can jump from a log line to the exact span that produced it.

## Trying it locally

Any OTLP-over-HTTP collector works. With a local Jaeger all-in-one listening
on 4318, enable the block above, run an agent, and the run appears as a trace
named `agent.run` under the configured `service_name`.

The daemon owns the exporter: it starts with the daemon and flushes on
shutdown, so short-lived CLI invocations do not each pay the setup cost. See
[Daemon](/docs/daemon) for where this fits in the daemon's lifecycle.
