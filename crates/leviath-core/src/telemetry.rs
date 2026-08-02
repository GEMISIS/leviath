//! The telemetry seam: pure-data lifecycle events and the sink they flow into.
//!
//! The runtime's observability system translates ECS state changes into
//! [`TelemetryEvent`] values and hands them to whatever [`TelemetrySink`] the
//! host installed. The events carry plain data only - no SDK types - so the
//! runtime never depends on an exporter, and tests can assert on the exact
//! event stream with [`MemorySink`]. The OpenTelemetry-backed sink lives in
//! `leviath-telemetry`; a host that installs nothing gets [`NoopSink`].

/// What kind of per-run log line a [`TelemetryEvent::Log`] carries.
///
/// Mirrors the two per-stage files the persistence layer writes: `output.log`
/// (the model's own text) and `logs.log` (tool results, token counts, errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// A line of assistant output (`output.log`).
    Output,
    /// A runtime log line - tool results, token counts, errors (`logs.log`).
    Runtime,
}

/// One observable moment in an agent run's life.
///
/// Timestamps are milliseconds since the Unix epoch (`at_ms`) so a sink can
/// reconstruct span boundaries without sub-second drift; durations are
/// measured wall-clock milliseconds at the point the work actually ran.
#[derive(Debug, Clone, PartialEq)]
pub enum TelemetryEvent {
    /// An agent run became visible to the observer.
    RunStarted {
        run_id: String,
        agent_name: String,
        /// The run-level model hint from spawn metadata, if one was recorded.
        model: Option<String>,
        /// Present when this run is a sub-agent of another run.
        parent_run_id: Option<String>,
        /// True when the run was reloaded from disk rather than freshly
        /// spawned - its earlier life was traced (if at all) by a previous
        /// daemon process, so this trace starts mid-run.
        recovered: bool,
        at_ms: i64,
    },
    /// The run entered a stage (including the first).
    StageEntered {
        run_id: String,
        stage_index: usize,
        stage_name: String,
        at_ms: i64,
    },
    /// The run left a stage; token counts are the stage's own totals.
    StageExited {
        run_id: String,
        stage_index: usize,
        stage_name: String,
        prompt_tokens: usize,
        completion_tokens: usize,
        at_ms: i64,
    },
    /// One inference call finished (successfully or not).
    InferenceCompleted {
        run_id: String,
        stage_name: String,
        provider: String,
        model: String,
        /// Wall-clock time of the provider call, including retries.
        latency_ms: u64,
        prompt_tokens: usize,
        completion_tokens: usize,
        cached_tokens: usize,
        success: bool,
    },
    /// One tool call finished.
    ToolCallCompleted {
        run_id: String,
        stage_name: String,
        tool_name: String,
        /// Wall-clock time of the batch the call ran in. Tool calls execute
        /// in batches and the executor reports one duration per batch, so
        /// every call in a batch carries the same figure.
        batch_latency_ms: u64,
        /// Derived from the `[error] ` result-text convention every executor
        /// uses; a heuristic, not a structured status.
        success: bool,
    },
    /// A context compaction finished.
    CompactionCompleted {
        run_id: String,
        stage_name: String,
        success: bool,
    },
    /// The run reached a terminal status; totals are run-wide.
    RunCompleted {
        run_id: String,
        /// The terminal status label: `complete`, `error`, or `cancelled`.
        status: String,
        prompt_tokens: usize,
        completion_tokens: usize,
        tool_calls: usize,
        /// Whether the run stopped having modified nothing, when its blueprint
        /// gave it a way to. `complete` says the pipeline reached the end, not
        /// that it achieved anything; this is the difference.
        empty_output: bool,
        at_ms: i64,
    },
    /// One per-run log line, as also written to the stage's log files.
    Log {
        run_id: String,
        stage_index: usize,
        kind: LogKind,
        line: String,
    },
}

impl TelemetryEvent {
    /// A stable short name for this event's variant.
    ///
    /// Exists so a test can `assert_eq!(event.kind(), "run_started")` rather
    /// than `assert!(matches!(event, ...))` - the `matches!` non-matching arm
    /// is a region only a *failing* assertion ever reaches, which reads as
    /// uncovered under the workspace's 100% gate. Useful in its own right for
    /// structured logging, where the kind is the field worth indexing on.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::StageEntered { .. } => "stage_entered",
            Self::StageExited { .. } => "stage_exited",
            Self::InferenceCompleted { .. } => "inference_completed",
            Self::ToolCallCompleted { .. } => "tool_call_completed",
            Self::CompactionCompleted { .. } => "compaction_completed",
            Self::RunCompleted { .. } => "run_completed",
            Self::Log { .. } => "log",
        }
    }

    /// The run this event belongs to.
    pub fn run_id(&self) -> &str {
        match self {
            Self::RunStarted { run_id, .. }
            | Self::StageEntered { run_id, .. }
            | Self::StageExited { run_id, .. }
            | Self::InferenceCompleted { run_id, .. }
            | Self::ToolCallCompleted { run_id, .. }
            | Self::CompactionCompleted { run_id, .. }
            | Self::RunCompleted { run_id, .. }
            | Self::Log { run_id, .. } => run_id,
        }
    }
}

/// How the daemon as a whole is doing, sampled once per safety re-drive.
///
/// Deliberately not a [`TelemetryEvent`]: every variant of that enum belongs to
/// one run, and this belongs to none of them. The distinction is the point. A
/// daemon whose lanes are full and whose runs have all stopped moving emits no
/// per-run telemetry at all, precisely because nothing is happening, so the
/// silence that issue #191 reported was indistinguishable from an idle night.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaneHealth {
    /// Agents doing work, or ready to.
    pub agents_active: usize,
    /// Agents blocked on input, a child, or a prompt.
    pub agents_waiting: usize,
    /// Tool batches holding lane capacity and running.
    pub tools_busy: usize,
    /// Tool batches waiting for lane capacity.
    pub tools_queued: usize,
    /// Tool batches parked on an unbounded wait, holding no capacity.
    pub tools_parked: usize,
    /// The tool lane's concurrency cap, including any relief granted.
    pub tools_workers: usize,
    /// Consecutive re-drives that found a lane at capacity and no run moving.
    pub dead_cycles: u32,
    /// Extra tool-lane capacity handed out on this sample, if any.
    pub relief_granted: usize,
}

/// One provider the daemon has stopped sending work to, sampled alongside
/// [`LaneHealth`].
///
/// Daemon-wide for the same reason as `LaneHealth`: a provider out of credits
/// belongs to no single run, and the runs it kills emit nothing useful because
/// they die before doing anything (issue #201).
///
/// `reason` is the label rather than the runtime's own enum: this crate sits
/// below `leviath-providers`, so the type that names it is not in scope here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderHealth {
    /// The provider taken out of service.
    pub provider: String,
    /// Why, as a stable lowercase label (`credits-exhausted`, `auth-failed`).
    pub reason: String,
    /// Consecutive failures accumulated against it.
    pub consecutive_failures: u32,
    /// Seconds until it is probed again.
    pub retry_in_secs: u64,
}

/// Where telemetry events go.
///
/// Implementations must tolerate being called from the engine's tick loop:
/// `emit` should hand off or record cheaply, never block on network I/O.
pub trait TelemetrySink: Send + Sync {
    /// Record one event.
    fn emit(&self, event: TelemetryEvent);

    /// Record one daemon-wide health sample. Default: ignore it, so a sink that
    /// only cares about runs needs no changes.
    fn observe_lanes(&self, _health: LaneHealth) {}

    /// Record which providers are currently out of service, sampled on the same
    /// re-drive tick as [`TelemetrySink::observe_lanes`]. Default: ignore it,
    /// so an existing sink keeps compiling unchanged.
    fn observe_providers(&self, _down: &[ProviderHealth]) {}

    /// Flush any buffered export before shutdown. Default: nothing buffered.
    fn force_flush(&self) {}
}

/// The sink used when no telemetry backend is installed: drops everything.
pub struct NoopSink;

impl TelemetrySink for NoopSink {
    fn emit(&self, _event: TelemetryEvent) {}
}

/// A sink that records every event in memory, for tests to assert on.
#[derive(Default)]
pub struct MemorySink {
    events: std::sync::Mutex<Vec<TelemetryEvent>>,
    lanes: std::sync::Mutex<Vec<LaneHealth>>,
    providers: std::sync::Mutex<Vec<Vec<ProviderHealth>>>,
    flushes: std::sync::atomic::AtomicUsize,
}

impl MemorySink {
    /// A snapshot of everything emitted so far, in order.
    pub fn events(&self) -> Vec<TelemetryEvent> {
        self.events.lock().expect("telemetry event lock").clone()
    }

    /// Every lane-health sample recorded so far, in order.
    pub fn lane_samples(&self) -> Vec<LaneHealth> {
        self.lanes.lock().expect("telemetry lane lock").clone()
    }

    /// Every provider-health sample recorded so far, in order.
    pub fn provider_samples(&self) -> Vec<Vec<ProviderHealth>> {
        self.providers
            .lock()
            .expect("telemetry provider lock")
            .clone()
    }

    /// How many times `force_flush` was called.
    pub fn flush_count(&self) -> usize {
        self.flushes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl TelemetrySink for MemorySink {
    fn emit(&self, event: TelemetryEvent) {
        self.events
            .lock()
            .expect("telemetry event lock")
            .push(event);
    }

    fn observe_lanes(&self, health: LaneHealth) {
        self.lanes.lock().expect("telemetry lane lock").push(health);
    }

    fn observe_providers(&self, down: &[ProviderHealth]) {
        self.providers
            .lock()
            .expect("telemetry provider lock")
            .push(down.to_vec());
    }

    fn force_flush(&self) {
        self.flushes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_started(run_id: &str) -> TelemetryEvent {
        TelemetryEvent::RunStarted {
            run_id: run_id.to_string(),
            agent_name: "coder".to_string(),
            model: Some("claude-sonnet-5".to_string()),
            parent_run_id: None,
            recovered: false,
            at_ms: 1_000,
        }
    }

    #[test]
    fn memory_sink_records_events_in_order() {
        let sink = MemorySink::default();
        sink.emit(run_started("r1"));
        sink.emit(TelemetryEvent::StageEntered {
            run_id: "r1".to_string(),
            stage_index: 0,
            stage_name: "plan".to_string(),
            at_ms: 1_001,
        });
        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind(), "run_started");
        assert_eq!(events[1].kind(), "stage_entered");
    }

    #[test]
    fn memory_sink_records_lane_samples_and_the_noop_ignores_them() {
        let sink = MemorySink::default();
        assert!(sink.lane_samples().is_empty());
        sink.observe_lanes(LaneHealth {
            dead_cycles: 3,
            ..Default::default()
        });
        assert_eq!(sink.lane_samples().len(), 1);
        assert_eq!(sink.lane_samples()[0].dead_cycles, 3);

        // The default trait body: a sink that only cares about runs drops it.
        NoopSink.observe_lanes(LaneHealth::default());
    }

    #[test]
    fn memory_sink_records_provider_samples_and_the_noop_ignores_them() {
        let sink = MemorySink::default();
        assert!(sink.provider_samples().is_empty());
        let down = ProviderHealth {
            provider: "openrouter".to_string(),
            reason: "credits-exhausted".to_string(),
            consecutive_failures: 3,
            retry_in_secs: 240,
        };
        sink.observe_providers(std::slice::from_ref(&down));
        // The empty sample matters too: it is how a collector sees a provider
        // come back, not just go away.
        sink.observe_providers(&[]);
        assert_eq!(sink.provider_samples().len(), 2);
        assert_eq!(sink.provider_samples()[0], vec![down]);
        assert!(sink.provider_samples()[1].is_empty());

        NoopSink.observe_providers(&[]);
    }

    #[test]
    fn memory_sink_counts_flushes() {
        let sink = MemorySink::default();
        assert_eq!(sink.flush_count(), 0);
        sink.force_flush();
        sink.force_flush();
        assert_eq!(sink.flush_count(), 2);
    }

    #[test]
    fn noop_sink_accepts_events_and_default_flush() {
        let sink = NoopSink;
        sink.emit(run_started("r1"));
        // The trait's default force_flush is a no-op; exercise it through the
        // trait object the runtime actually holds.
        let boxed: Box<dyn TelemetrySink> = Box::new(NoopSink);
        boxed.force_flush();
    }

    #[test]
    fn run_id_reaches_every_variant() {
        let events = [
            run_started("r1"),
            TelemetryEvent::StageEntered {
                run_id: "r1".to_string(),
                stage_index: 0,
                stage_name: "plan".to_string(),
                at_ms: 0,
            },
            TelemetryEvent::StageExited {
                run_id: "r1".to_string(),
                stage_index: 0,
                stage_name: "plan".to_string(),
                prompt_tokens: 10,
                completion_tokens: 5,
                at_ms: 0,
            },
            TelemetryEvent::InferenceCompleted {
                run_id: "r1".to_string(),
                stage_name: "plan".to_string(),
                provider: "anthropic".to_string(),
                model: "claude-sonnet-5".to_string(),
                latency_ms: 120,
                prompt_tokens: 10,
                completion_tokens: 5,
                cached_tokens: 0,
                success: true,
            },
            TelemetryEvent::ToolCallCompleted {
                run_id: "r1".to_string(),
                stage_name: "build".to_string(),
                tool_name: "read_file".to_string(),
                batch_latency_ms: 8,
                success: true,
            },
            TelemetryEvent::CompactionCompleted {
                run_id: "r1".to_string(),
                stage_name: "build".to_string(),
                success: true,
            },
            TelemetryEvent::RunCompleted {
                run_id: "r1".to_string(),
                status: "complete".to_string(),
                prompt_tokens: 10,
                completion_tokens: 5,
                tool_calls: 1,
                empty_output: false,
                at_ms: 0,
            },
            TelemetryEvent::Log {
                run_id: "r1".to_string(),
                stage_index: 0,
                kind: LogKind::Runtime,
                line: "[Tokens: 10 in, 5 out]".to_string(),
            },
        ];
        let kinds: Vec<&str> = events.iter().map(TelemetryEvent::kind).collect();
        assert_eq!(
            kinds,
            [
                "run_started",
                "stage_entered",
                "stage_exited",
                "inference_completed",
                "tool_call_completed",
                "compaction_completed",
                "run_completed",
                "log",
            ]
        );
        for event in &events {
            assert_eq!(event.run_id(), "r1");
        }
    }

    #[test]
    fn event_clone_debug_and_eq() {
        let event = run_started("r1");
        let cloned = event.clone();
        assert_eq!(event, cloned);
        assert!(format!("{event:?}").contains("RunStarted"));
        assert_ne!(LogKind::Output, LogKind::Runtime);
        assert!(format!("{:?}", LogKind::Output).contains("Output"));
    }
}
