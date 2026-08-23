//! What the host broadcasts as the world changes.
//!
//! Two sources feed one stream. The coarse per-run variants come from the
//! host's change-detection pass, which compares each run against the [`Emitted`]
//! snapshot it kept from the previous cycle; the fine-grained ones are pushed at
//! the source by pipeline systems through [`WorldEventSink`]. Kept beside the
//! snapshot type rather than in the host, because the two only make sense
//! together: the snapshot exists to decide what is worth emitting.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::components::AgentStatus;
use leviath_core::interaction::InteractionRequest;

/// A change in the world, broadcast to subscribers (the HTTP/WS gateway and
/// in-process embedders) so they get pushed updates instead of polling. The
/// coarse per-run variants
/// (`Spawned`/`Status`/`Renamed`/`Tokens`/`Context`/`Completed`) are emitted by
/// the host's change-detection pass as it drives the world;
/// `StageTransition`/`ToolCallStarted`/`ToolCallFinished`/`Log` are pushed at
/// the source by pipeline systems through [`WorldEventSink`]. Streamed over the
/// control transport via `ControlRequest::Subscribe`.
///
/// Deliberately *not* `#[non_exhaustive]`. A catch-all arm in the websocket
/// gateway is how a variant gets declared, mapped, documented and then quietly
/// never surfaced to a client under a generic envelope; making every consumer
/// match exhaustively turns "somebody forgot to wire this up" from a runtime
/// surprise into a compile error. The cost is that adding a variant is a
/// breaking change for an out-of-workspace embedder, which is the right price:
/// a new event kind changes what a subscriber sees either way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorldEvent {
    /// A run's spend crossed a threshold the operator asked to hear about.
    ///
    /// One event per threshold, the first time the total passes it. A run that
    /// crosses several between two passes gets one for each, in order, so a
    /// consumer that acts on the highest sees them all.
    Spend {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The threshold that was crossed, in dollars.
        threshold_usd: f64,
        /// What the run has spent in total, in dollars.
        total_usd: f64,
        /// Whether every call in that total could be priced. When false the
        /// real figure is higher by however much went unpriced, so a consumer
        /// must not present it as the invoice.
        exact: bool,
        /// The stage the run was in when it crossed - the one doing the
        /// spending. The full per-stage breakdown is in `stages.json`.
        stage: String,
    },

    /// A run first appeared in the world.
    Spawned {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The blueprint / agent name.
        blueprint: String,
        /// The run that spawned this one, when it is a sub-agent.
        ///
        /// A subscriber building a run tree otherwise has to fetch every new
        /// run to find out where it hangs, and a fan-out of thirty workers is
        /// thirty fetches for a fact the spawn already knew.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<String>,
    },
    /// A run's status, stage, iteration, or tool-call count changed.
    Status {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// Short status label (`active`, `waiting`, `complete`, …).
        status: String,
        /// The current stage name.
        stage: String,
        /// The current iteration.
        iteration: usize,
        /// Cumulative tool calls.
        tool_calls: usize,
        /// Whether the current stage accepts messages.
        accepts_messages: bool,
        /// Why the run is parked, when it is.
        ///
        /// A subscriber watching live otherwise sees a run turn `waiting` or
        /// `paused` and has to fetch the run to learn whether that means "go
        /// and answer something" or "its workers are still going" - which is
        /// the guess this vocabulary exists to remove.
        wait_reason: Option<leviath_core::run_meta::WaitReason>,
        /// The run's generated title, once it has one.
        ///
        /// Carried on every status frame rather than only on the one that
        /// announced it: [`Renamed`](Self::Renamed) is the moment, this is the
        /// fact, and a subscriber that joined or reconnected after the moment
        /// picks the name up from the next status without a fetch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// A run acquired a title, or had the one it was showing replaced.
    ///
    /// A run starts untitled and is named a moment later, once the one-shot
    /// titling call comes back. That rename is the one field guaranteed to
    /// change shortly after a run starts and then never again, so without an
    /// event for it every client either polls each new run or shows the wrong
    /// name until something unrelated makes it re-read.
    Renamed {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The title the run now goes by.
        title: String,
    },
    /// A run's token totals changed.
    Tokens {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// Cumulative prompt tokens.
        prompt_tokens: usize,
        /// Cumulative completion tokens.
        completion_tokens: usize,
        /// Cumulative cached tokens.
        cached_tokens: usize,
        /// Cumulative cache-write tokens.
        cache_write_tokens: usize,
    },
    /// A run's context-window token usage changed.
    Context {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// Current context tokens.
        total_tokens: usize,
        /// Max context tokens.
        max_tokens: usize,
    },
    /// A run raised a new interaction awaiting an answer.
    Interaction {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The interaction request.
        request: InteractionRequest,
    },
    /// A run reached a terminal status.
    Completed {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The terminal status label.
        status: String,
        /// What the run handed back, when it submitted anything.
        ///
        /// Carried on the event rather than left for the consumer to read off
        /// disk: this fires the moment the run goes terminal, and the persist
        /// tick that writes `meta.json` has not necessarily run yet. A webhook
        /// or websocket consumer reading the file would race it and report a
        /// finished run with no answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_output: Option<leviath_core::output::FinalOutput>,
    },
    /// A run moved from one stage to another. Emitted by the transition systems
    /// at the moment the new stage is entered (the initial stage at spawn is
    /// covered by [`WorldEvent::Spawned`], not by this).
    StageTransition {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The stage being left.
        from: String,
        /// The stage being entered.
        to: String,
        /// How many times the destination stage has been entered, this entry
        /// included.
        iteration: usize,
    },
    /// A tool call was handed to the async tool lane for execution. Inline
    /// calls (context tools, refusals, gate blocks) resolve without touching
    /// the lane and don't produce this event.
    ToolCallStarted {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The provider-assigned tool call id.
        call_id: String,
        /// The tool name.
        tool: String,
    },
    /// A lane-executed tool call returned. Paired with
    /// [`WorldEvent::ToolCallStarted`] by `call_id`.
    ToolCallFinished {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The provider-assigned tool call id.
        call_id: String,
        /// The tool name.
        tool: String,
        /// Whether the call took effect (`false` for `[error]`/`[blocked]`/
        /// `[unavailable]` results).
        ok: bool,
        /// The result, flattened to one line and truncated.
        summary: String,
    },
    /// A run produced a per-agent log/output line (readable assistant output or
    /// an operational `[Tokens: …]` / `[tool] …` / `[error] …` line).
    Log {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The log line text.
        line: String,
    },
}

/// Dollars as millionths of a dollar, saturating.
///
/// The unit [`Emitted::cost_micros`] and the spend thresholds are both kept in,
/// so the comparison between them is integer and exact. A negative or
/// non-finite input is zero: neither is a sum of money.
pub(super) fn usd_to_micros(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    (usd * 1_000_000.0).round() as u64
}

impl WorldEvent {
    /// The run id this event belongs to. Every variant carries one; this saves
    /// consumers an exhaustive match (which, with the enum non-exhaustive,
    /// they could not write anyway).
    pub fn run_id(&self) -> &str {
        match self {
            WorldEvent::Spawned { run_id, .. }
            | WorldEvent::Status { run_id, .. }
            | WorldEvent::Renamed { run_id, .. }
            | WorldEvent::Tokens { run_id, .. }
            | WorldEvent::Context { run_id, .. }
            | WorldEvent::Interaction { run_id, .. }
            | WorldEvent::Completed { run_id, .. }
            | WorldEvent::StageTransition { run_id, .. }
            | WorldEvent::ToolCallStarted { run_id, .. }
            | WorldEvent::ToolCallFinished { run_id, .. }
            | WorldEvent::Spend { run_id, .. }
            | WorldEvent::Log { run_id, .. } => run_id,
        }
    }
}

/// A world resource holding a clone of the host's [`WorldEvent`] broadcast
/// sender, so ECS systems (e.g. the persistence drain) can push events - notably
/// per-agent [`WorldEvent::Log`] lines - into the same stream the control
/// transport serves. Absent in worlds that don't stream (test / `lev run`), where
/// systems that depend on it become no-ops.
// `Resource` moved from `bevy_ecs::system` to `bevy_ecs::resource` in 0.19.
#[derive(bevy_ecs::resource::Resource, Clone)]
pub struct WorldEventSink(pub broadcast::Sender<WorldEvent>);

/// A short, stable status label for [`WorldEvent`]. Part of the daemon's wire
/// contract (the REST WebSocket forwards it verbatim), so it comes from the one
/// table on [`AgentStatus`] rather than a copy that could drift from it.
pub(super) fn status_str(status: &AgentStatus) -> &'static str {
    status.label()
}

/// The last-emitted snapshot of an agent, for change detection.
#[derive(Clone, Hash)]
pub(super) struct Emitted {
    pub(super) status: &'static str,
    pub(super) stage: String,
    pub(super) iteration: usize,
    pub(super) tool_calls: usize,
    pub(super) accepts_messages: bool,
    pub(super) prompt_tokens: usize,
    pub(super) completion_tokens: usize,
    pub(super) cached_tokens: usize,
    pub(super) cache_write_tokens: usize,
    pub(super) context_tokens: usize,
    /// What the run had spent as of the last pass, in millionths of a dollar,
    /// so a crossing is recognised by comparing against this rather than by
    /// re-deriving it.
    ///
    /// An integer because this struct is hashed for the progress fingerprint,
    /// and because it makes the comparison exact - a sub-cent call still counts
    /// instead of rounding away.
    pub(super) cost_micros: u64,
    /// Whether that figure covers every call. A run with unpriced calls has
    /// spent at least this much, and the event says which it is.
    pub(super) cost_exact: bool,
    pub(super) terminal: bool,
    /// Why the run is parked, so a change of reason counts as a change worth
    /// telling subscribers about.
    pub(super) wait_reason: Option<leviath_core::run_meta::WaitReason>,
    /// The run's title as of the last pass, so the pass that first sees one can
    /// announce the rename.
    ///
    /// Deliberately *not* part of the status change key: a rename is not a move
    /// in execution state, and a status frame that repeated the stage and
    /// iteration it already sent would say nothing new. The title rides the
    /// next status frame that fires on its own.
    pub(super) title: Option<String>,
}

#[cfg(test)]
mod spend_tests {
    use super::*;

    /// Money is compared in whole micros, so the threshold check is exact and
    /// a sub-cent call still moves the total instead of rounding away.
    #[test]
    fn usd_to_micros_is_exact_and_refuses_what_is_not_money() {
        assert_eq!(usd_to_micros(1.0), 1_000_000);
        assert_eq!(usd_to_micros(0.000_001), 1, "a millionth still counts");
        assert_eq!(usd_to_micros(27.5), 27_500_000);
        // Neither of these is a sum of money, and either would otherwise
        // become a nonsense integer.
        assert_eq!(usd_to_micros(-5.0), 0);
        assert_eq!(usd_to_micros(f64::NAN), 0);
        assert_eq!(usd_to_micros(f64::INFINITY), 0);
        assert_eq!(usd_to_micros(0.0), 0);
    }

    /// Every event names the run it is about, which is what a per-run
    /// subscription filters on. A variant that answered wrongly here would be
    /// delivered to the wrong subscriber, or to none.
    #[test]
    fn a_spend_event_names_its_run() {
        let event = WorldEvent::Spend {
            run_id: "run-spendy".into(),
            agent_id: "agent-spendy".into(),
            threshold_usd: 25.0,
            total_usd: 27.5,
            exact: true,
            stage: "analyze".into(),
        };
        assert_eq!(event.run_id(), "run-spendy");
    }
}
