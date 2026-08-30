//! The `exporter = "stdout"` sink: one readable line per event through the
//! process logger.
//!
//! Named "stdout" in config for familiarity with other OTEL tooling, but the
//! lines go through `tracing` to **stderr** like every other Leviath log:
//! stdout is `lev agent-client`'s JSON-RPC channel and must stay clean. (For
//! the same reason this is hand-rolled rather than `opentelemetry-stdout`,
//! which writes to stdout unconditionally.)

use leviath_core::telemetry::{LaneHealth, LogKind, ProviderHealth, TelemetryEvent, TelemetrySink};

/// One readable line for an event.
pub(crate) fn format_event(event: &TelemetryEvent) -> String {
    match event {
        TelemetryEvent::RunStarted {
            run_id,
            agent_name,
            recovered,
            ..
        } => {
            let suffix = if *recovered { " (recovered)" } else { "" };
            format!("run {run_id} started: agent {agent_name}{suffix}")
        }
        TelemetryEvent::StageEntered {
            run_id,
            stage_index,
            stage_name,
            ..
        } => format!("run {run_id} entered stage {stage_index} ({stage_name})"),
        TelemetryEvent::StageExited {
            run_id,
            stage_index,
            stage_name,
            prompt_tokens,
            completion_tokens,
            ..
        } => format!(
            "run {run_id} exited stage {stage_index} ({stage_name}): \
             {prompt_tokens} in, {completion_tokens} out"
        ),
        TelemetryEvent::InferenceCompleted {
            run_id,
            provider,
            model,
            latency_ms,
            prompt_tokens,
            completion_tokens,
            success,
            ..
        } => format!(
            "run {run_id} inference {}: {provider}/{model} {latency_ms}ms, \
             {prompt_tokens} in, {completion_tokens} out",
            if *success { "ok" } else { "failed" }
        ),
        TelemetryEvent::ToolCallCompleted {
            run_id,
            tool_name,
            batch_latency_ms,
            success,
            ..
        } => format!(
            "run {run_id} tool {tool_name} {}: batch {batch_latency_ms}ms",
            if *success { "ok" } else { "failed" }
        ),
        TelemetryEvent::CompactionCompleted {
            run_id, success, ..
        } => format!(
            "run {run_id} compaction {}",
            if *success { "ok" } else { "failed" }
        ),
        TelemetryEvent::RunCompleted {
            run_id,
            status,
            prompt_tokens,
            completion_tokens,
            tool_calls,
            empty_output,
            ..
        } => format!(
            "run {run_id} {status}: {prompt_tokens} in, {completion_tokens} out, \
             {tool_calls} tool calls{}",
            if *empty_output { " (no output)" } else { "" }
        ),
        TelemetryEvent::Log {
            run_id,
            stage_index,
            kind,
            line,
        } => {
            let kind = match kind {
                LogKind::Output => "output",
                LogKind::Runtime => "log",
            };
            format!("run {run_id} stage {stage_index} {kind}: {line}")
        }
    }
}

/// One readable line for a daemon-wide health sample.
pub(crate) fn format_lane_health(health: &LaneHealth) -> String {
    format!(
        "lanes: agents active={} waiting={}, tools {}/{} busy, {} parked, {} queued, \
         dead cycles {}, relief {}",
        health.agents_active,
        health.agents_waiting,
        health.tools_busy,
        health.tools_workers,
        health.tools_parked,
        health.tools_queued,
        health.dead_cycles,
        health.relief_granted,
    )
}

/// One readable line for the providers currently out of service.
///
/// `None` while everything is serving, so a healthy daemon does not narrate a
/// line saying nothing is wrong on every re-drive.
pub(crate) fn format_providers_down(down: &[ProviderHealth]) -> Option<String> {
    if down.is_empty() {
        return None;
    }
    let each = down
        .iter()
        .map(|p| {
            format!(
                "{} ({}, {} failures, retry in {}s)",
                p.provider, p.reason, p.consecutive_failures, p.retry_in_secs
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("providers out of service: {each}"))
}

/// [`TelemetrySink`] that narrates events as `tracing` lines (→ stderr).
pub struct LogSink;

impl TelemetrySink for LogSink {
    fn emit(&self, event: TelemetryEvent) {
        tracing::info!(target: "leviath::telemetry", "{}", format_event(&event));
    }

    fn observe_lanes(&self, health: LaneHealth) {
        tracing::info!(target: "leviath::telemetry", "{}", format_lane_health(&health));
    }

    fn observe_providers(&self, down: &[ProviderHealth]) {
        if let Some(line) = format_providers_down(down) {
            tracing::warn!(target: "leviath::telemetry", "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_formats_to_one_line() {
        let events = [
            (
                TelemetryEvent::RunStarted {
                    run_id: "r1".to_string(),
                    agent_name: "coder".to_string(),
                    model: None,
                    parent_run_id: None,
                    recovered: false,
                    at_ms: 0,
                },
                "run r1 started: agent coder",
            ),
            (
                TelemetryEvent::RunStarted {
                    run_id: "r1".to_string(),
                    agent_name: "coder".to_string(),
                    model: None,
                    parent_run_id: None,
                    recovered: true,
                    at_ms: 0,
                },
                "run r1 started: agent coder (recovered)",
            ),
            (
                TelemetryEvent::StageEntered {
                    run_id: "r1".to_string(),
                    stage_index: 1,
                    stage_name: "build".to_string(),
                    at_ms: 0,
                },
                "run r1 entered stage 1 (build)",
            ),
            (
                TelemetryEvent::StageExited {
                    run_id: "r1".to_string(),
                    stage_index: 1,
                    stage_name: "build".to_string(),
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    at_ms: 0,
                },
                "run r1 exited stage 1 (build): 10 in, 4 out",
            ),
            (
                TelemetryEvent::InferenceCompleted {
                    run_id: "r1".to_string(),
                    stage_name: "build".to_string(),
                    provider: "anthropic".to_string(),
                    model: "m".to_string(),
                    latency_ms: 120,
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    cached_tokens: 0,
                    success: true,
                    cost_usd: Some(0.01),
                },
                "run r1 inference ok: anthropic/m 120ms, 10 in, 4 out",
            ),
            (
                TelemetryEvent::InferenceCompleted {
                    run_id: "r1".to_string(),
                    stage_name: "build".to_string(),
                    provider: "anthropic".to_string(),
                    model: "m".to_string(),
                    latency_ms: 120,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                    success: false,
                    cost_usd: Some(0.01),
                },
                "run r1 inference failed: anthropic/m 120ms, 0 in, 0 out",
            ),
            (
                TelemetryEvent::ToolCallCompleted {
                    run_id: "r1".to_string(),
                    stage_name: "build".to_string(),
                    tool_name: "read_file".to_string(),
                    batch_latency_ms: 30,
                    success: true,
                },
                "run r1 tool read_file ok: batch 30ms",
            ),
            (
                TelemetryEvent::ToolCallCompleted {
                    run_id: "r1".to_string(),
                    stage_name: "build".to_string(),
                    tool_name: "shell".to_string(),
                    batch_latency_ms: 30,
                    success: false,
                },
                "run r1 tool shell failed: batch 30ms",
            ),
            (
                TelemetryEvent::CompactionCompleted {
                    run_id: "r1".to_string(),
                    stage_name: "build".to_string(),
                    success: true,
                },
                "run r1 compaction ok",
            ),
            (
                TelemetryEvent::CompactionCompleted {
                    run_id: "r1".to_string(),
                    stage_name: "build".to_string(),
                    success: false,
                },
                "run r1 compaction failed",
            ),
            (
                TelemetryEvent::RunCompleted {
                    run_id: "r1".to_string(),
                    status: "complete".to_string(),
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    tool_calls: 2,
                    empty_output: false,
                    at_ms: 0,
                },
                "run r1 complete: 10 in, 4 out, 2 tool calls",
            ),
            (
                // Same tallies, but the run changed nothing: the line has to
                // say so, or it reads as a success.
                TelemetryEvent::RunCompleted {
                    run_id: "r1".to_string(),
                    status: "complete".to_string(),
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    tool_calls: 2,
                    empty_output: true,
                    at_ms: 0,
                },
                "run r1 complete: 10 in, 4 out, 2 tool calls (no output)",
            ),
            (
                TelemetryEvent::Log {
                    run_id: "r1".to_string(),
                    stage_index: 0,
                    kind: LogKind::Output,
                    line: "hello".to_string(),
                },
                "run r1 stage 0 output: hello",
            ),
            (
                TelemetryEvent::Log {
                    run_id: "r1".to_string(),
                    stage_index: 0,
                    kind: LogKind::Runtime,
                    line: "[Tokens: 1 in, 1 out]".to_string(),
                },
                "run r1 stage 0 log: [Tokens: 1 in, 1 out]",
            ),
        ];
        for (event, expected) in events {
            assert_eq!(format_event(&event), expected);
        }
    }

    #[test]
    fn emit_routes_through_tracing_without_panicking() {
        let _guard = leviath_testkit::tracing_guard();
        LogSink.emit(TelemetryEvent::RunCompleted {
            run_id: "r1".to_string(),
            status: "complete".to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
            empty_output: false,
            at_ms: 0,
        });
    }

    /// The daemon-wide line reads as one sentence, with the numbers an operator
    /// asks for in the order they ask for them.
    #[test]
    fn lane_health_formats_to_one_line() {
        let line = format_lane_health(&LaneHealth {
            agents_active: 6,
            agents_waiting: 2,
            tools_busy: 8,
            tools_queued: 12,
            tools_parked: 3,
            tools_workers: 8,
            dead_cycles: 4,
            relief_granted: 2,
        });
        assert_eq!(
            line,
            "lanes: agents active=6 waiting=2, tools 8/8 busy, 3 parked, 12 queued, \
             dead cycles 4, relief 2"
        );
        assert!(!line.contains('\n'), "one line: {line}");
    }

    #[test]
    fn observe_lanes_routes_through_tracing_without_panicking() {
        let _guard = leviath_testkit::tracing_guard();
        LogSink.observe_lanes(LaneHealth::default());
    }

    #[test]
    fn providers_down_formats_to_one_line() {
        let line = format_providers_down(&[
            ProviderHealth {
                provider: "openrouter".to_string(),
                reason: "credits-exhausted".to_string(),
                consecutive_failures: 3,
                retry_in_secs: 240,
            },
            ProviderHealth {
                provider: "anthropic".to_string(),
                reason: "auth-failed".to_string(),
                consecutive_failures: 5,
                retry_in_secs: 30,
            },
        ])
        .expect("something is down");
        assert_eq!(
            line,
            "providers out of service: openrouter (credits-exhausted, 3 failures, retry in 240s), \
             anthropic (auth-failed, 5 failures, retry in 30s)"
        );
        assert!(!line.contains('\n'), "one line: {line}");
    }

    /// A healthy daemon must not narrate "nothing is wrong" every 30 seconds.
    #[test]
    fn nothing_down_is_no_line_at_all() {
        assert_eq!(format_providers_down(&[]), None);
        let _guard = leviath_testkit::tracing_guard();
        LogSink.observe_providers(&[]);
    }

    #[test]
    fn observe_providers_routes_through_tracing_without_panicking() {
        let _guard = leviath_testkit::tracing_guard();
        LogSink.observe_providers(&[ProviderHealth {
            provider: "openrouter".to_string(),
            reason: "credits-exhausted".to_string(),
            consecutive_failures: 3,
            retry_in_secs: 240,
        }]);
    }
}
