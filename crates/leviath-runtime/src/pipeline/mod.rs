//! The ECS pipeline (Phase 2): components + systems that drive every agent
//! through check-input → infer → tools → apply → repeat, entirely as data.
//!
//! Agents are entities; their execution phase is a **marker component**
//! (`ReadyToInfer`, `AwaitingInference`, …) so systems can query by phase. A
//! system never blocks on I/O: the dispatch systems hand work to the async
//! bridges (`inference_bridge`, [`crate::tool_bridge`]) and the collect
//! systems apply the results on a later tick. This module is built alongside the
//! existing imperative engine; the two are unified in a later phase.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use leviath_providers::{InferenceRequest, Provider, Tool};

use crate::compaction_bridge::{CompactionJob, CompactionOutcome, run_compaction_job};
use crate::components::{
    AgentMessage, AgentState, AgentStatus, AwaitingInteraction, ContextWindow, InferenceConfig,
    MessageInbox,
};
use crate::fanout::FanOutWaiting;
use crate::inference_bridge::{InferenceJob, InferenceOutcome, run_inference_job};
use crate::inference_pool::InferencePools;
use crate::interaction_hub::InteractionHub;
use crate::persistence::{RunMetadata, TokenTotals, build_context_snapshot, build_run_meta};
use crate::persistence_bridge::{PersistJob, PersistMsg};
use crate::providers::ProviderRegistry;
use crate::tool_bridge::{BoxedToolExec, ToolJob, ToolOutcome};

// Sections of the former single-file pipeline, one per concern.
mod gate_check;
pub(crate) use gate_check::gate_blocks;
mod transition;
#[cfg(test)]
pub(crate) use transition::find_conditioned_edge;
pub use transition::{AgentBlueprint, StageCursor, force_transition, is_terminal_status};
pub(crate) use transition::{
    AwaitingTransitionChoice, StageEntry, StageInferences, StageSetup, StageSetups, VisitCounts,
    WaitingForChildren, apply_stage_context, attach_stage_components, emit_stage_transition,
    enter_stage, fail_stage, fail_stage_world, find_conditioned_edge_ref, hold_for_gate,
    region_digest, resolve_transition,
};
mod hooks;
#[cfg(test)]
pub(crate) use hooks::TerminalHookFired;
pub(crate) use hooks::{
    run_after_inference_hooks, run_before_inference_hooks, run_stage_enter_hooks,
    run_stage_exit_hooks, run_terminal_hooks, run_tool_call_hooks,
};
mod convergence;
pub(crate) use convergence::track_stage_progress;
mod watchdog;
pub use watchdog::WORKSPACE_CHECK_INTERVAL;
#[cfg(test)]
pub(crate) use watchdog::{StuckMetrics, detect_stuck, hottest_edit, note_stuck};
pub(crate) use watchdog::{
    check_workspace_health, detect_stuck_stage, enforce_max_iterations, note_error,
    note_max_iterations, note_unusable_split,
};
mod requirements;
#[cfg(test)]
pub(crate) use requirements::{DEFAULT_REQUIRED_REENTRY_CAP, unmet_required_regions};
pub(crate) use requirements::{
    FanOutReentries, GateDecision, OutputReentries, RequiredReentries, gate_requires_children,
    require_context_regions, require_fan_out, require_final_output,
};
mod spawn;
#[cfg(test)]
pub(crate) use spawn::spawn_agent;
#[cfg(test)]
pub(crate) use spawn::{DEFAULT_CONTEXT_WINDOW_TOKENS, stage_setup_from};
pub use spawn::{ResolvedStage, SeededSpawn, spawn_agent_seeded};
mod transition_choice;
pub(crate) use transition_choice::{
    AwaitingTransitionResponse, TransitionResults, collect_transition_choice,
    dispatch_transition_choice,
};
#[cfg(test)]
pub(crate) use transition_choice::{build_transition_prompt, match_transition_choice};
mod tool_stages;
pub(crate) use tool_stages::{
    poll_dynamic_tool_refresh, refresh_advertised_tools, sync_tool_stages,
};
mod messaging;
pub(crate) use messaging::{MessageIntake, deliver_messages};
mod persist;
pub use persist::PersistWatermark;
#[cfg(test)]
pub(crate) use persist::{
    BROADCAST_LOG_LINE_MAX_BYTES, PERSIST_HEARTBEAT_SECS, reconcile_stage_ledger,
};
pub(crate) use persist::{PersistenceStage, dispatch_persistence, reflect_interaction_status};
mod compaction;
pub(crate) use compaction::{
    AwaitingCompaction, CompactionResults, PendingEdgeCompact, apply_edge_transform,
    collect_compaction, compaction_request, dispatch_compaction, dispatch_edge_compact,
};
pub use compaction::{CompactionSettings, is_stage_specific};
mod tool_results;
pub(crate) use tool_results::{
    ToolResults, apply_one_tool_result, apply_tool_results, collect_tools,
};
#[cfg(test)]
pub(crate) use tool_results::{
    annotate_path_errors, apply_file_tracking, stage_modifying_tools, truncate_file,
};
mod gate;
pub(crate) use gate::taint_block_message;
pub use gate::{GateScriptRules, PolicyGate, ToolSensitivities};
mod tools;
pub(crate) use tools::{
    AwaitingTools, ContextToolResults, ToolServiceRes, ToolStage, ToolsNeedRefresh,
    call_had_no_effect, dispatch_tools, merge_in_call_order, one_line,
};
pub use tools::{DynamicTools, ToolProgress, ToolService, noop_progress};
#[cfg(test)]
pub(crate) use tools::{barrier_then, cut_off_arguments_refusal, invalid_args_refusal};
mod response;
pub use response::StageLedger;
#[cfg(test)]
pub(crate) use response::{GlobalNudge, MAX_CUT_OFF_NUDGES, edited_path, to_inference_result};
pub(crate) use response::{
    InferenceResults, ProcessResponse, ReadyForTools, ReadyForTransition, ResolveTransition,
    StageIoBuffer, StageOutcome, StageProgress, collect_inference, handle_empty_response,
    inject_system_nudge, process_response,
};
mod inference;
#[cfg(test)]
pub(crate) use inference::{
    BATCH_TOOL_HINT, WINDOWS_SHELL_HINT, build_request, hint_blocks, shell_guidance_for,
};
pub(crate) use inference::{
    InFlightWork, abort_terminal_work, dispatch_inference, retry_policy_for, track_in_flight,
};
mod resolve;
pub use resolve::{
    ModelDefaults, ToolCatalog, ToolOwners, bare_default_model, expand_connector_grants,
    filter_tools_for_stage, model_key, providers_tried, resolve_stage_model, resolve_stages,
};
mod stall;
pub use stall::{DEFAULT_STALL_TIMEOUT_SECS, PausedForSetup, StallTimeout};
pub(crate) use stall::{
    DispatchStall, HeldInference, HeldLane, StallClock, StallReason, fail_stalled_dispatch,
    note_stall,
};
mod wedge;
#[cfg(test)]
pub(crate) use wedge::Wedged;
pub(crate) use wedge::fail_wedged_runs;
pub use wedge::{DEFAULT_WEDGE_TIMEOUT_SECS, WedgeTimeout};
mod circuit;
pub(crate) use circuit::rotate_open_circuits;
pub use circuit::{
    CircuitPolicy, DEFAULT_CIRCUIT_COOLDOWN_SECS, DEFAULT_FAILURES_BEFORE_OPEN,
    ProviderCircuitState, ProviderCircuits, SLOW_FAILURE_MULTIPLIER,
};
mod calibration;
pub(crate) use calibration::{
    PromptCalibration, PromptEstimate, calibrated_tokens, needs_eviction_calibrated,
};

// ─── Phase marker components (an agent is in exactly one) ────────────────────
//
// A marker's presence is a claim that some system has this agent queued, which
// is what keeps it reachable. Anything new here must also be added to
// [`Unreachable`], or the wedge watchdog will read an agent resting on it as one
// nothing can drive.

/// The agent is active and ready to build a request and (permits allowing)
/// dispatch inference.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyToInfer;

/// Inference has been dispatched to the pool; the agent is waiting for its
/// result (which the inference-collect system will apply).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AwaitingInference;

/// Transient tag: the agent just entered a stage (index + name). The
/// [`sync_tool_stages`] system reads it to notify the [`ToolService`] of the
/// stage change, then removes it. Carries the data so the tool service need not
/// query the world.
#[derive(Component, Debug, Clone)]
pub(crate) struct StageJustEntered {
    /// The new stage's index.
    pub index: usize,
    /// The new stage's name.
    pub name: String,
}

// ─── Per-agent stage data the dispatch system reads ──────────────────────────

/// Resolved inference parameters for the agent's current stage, set when it
/// enters that stage. Pure data - the dispatch system reads it to build the
/// request.
#[derive(Component, Debug, Clone)]
pub struct StageInference {
    /// Registered provider to call.
    pub provider_name: String,
    /// Model id (also the key into the per-model inference pools).
    pub model: String,
    /// Tools advertised at this stage.
    pub tools: Vec<Tool>,
    /// Optional allow-list of tool names (`None`/empty = all `tools`).
    pub tool_filter: Option<Vec<String>>,
    /// Providers to fail over to, best first, when the current one turns out
    /// to be unusable. Consumed from the front by `collect_inference`, so an
    /// exhausted list means "nowhere left to go".
    pub fallbacks: Vec<leviath_core::blueprint::ModelEntry>,
    /// The output shape resolved for this stage, carried alongside the tools it
    /// was already folded into. Dispatch reads it to know which format label to
    /// record and, when the author supplied a schema, what to validate against.
    pub output: Option<leviath_core::output::OutputSpec>,
}

// ─── World resources for the inference stage ─────────────────────────────────

/// The registered providers, as a world resource.
#[derive(Resource)]
pub struct Providers(pub ProviderRegistry);

/// The operator's retry schedule for inference, from `[limits]`.
///
/// A world resource rather than constants because the daemon serves it from
/// `[limits] inference_retry_attempts` and `inference_retry_base_ms`. Absent
/// means the built-in schedule, which is what these defaults are.
///
/// Only the two ordinary-failure numbers are configurable. The capacity
/// schedule and the total-backoff ceiling stay fixed (see
/// [`crate::inference_bridge::RetryPolicy`]): they exist to bound a provider
/// outage, and a bound an operator can raise without limit is not one.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceRetryTuning {
    /// Total attempts including the first. See
    /// [`crate::inference_bridge::RetryPolicy::max_attempts`].
    pub max_attempts: u32,
    /// The first backoff after an ordinary transient failure, in milliseconds,
    /// doubling per retry. See
    /// [`crate::inference_bridge::RetryPolicy::base_delay`].
    pub base_delay_ms: u64,
}

impl Default for InferenceRetryTuning {
    fn default() -> Self {
        Self {
            max_attempts: crate::inference_bridge::DEFAULT_RETRY_ATTEMPTS,
            base_delay_ms: crate::inference_bridge::DEFAULT_RETRY_BASE_DELAY_MS,
        }
    }
}

/// The plumbing the inference-dispatch system needs: the per-model pools, the
/// channel to report outcomes on, the tick wake handle, and a runtime handle to
/// spawn the (bounded, per-request) worker tasks onto.
#[derive(Resource, Clone)]
pub(crate) struct InferenceStage {
    /// Per-model concurrency pools.
    pub pools: Arc<InferencePools>,
    /// Where completed inferences are reported.
    pub outcomes: UnboundedSender<InferenceOutcome>,
    /// Where completed *transition-choice* inferences are reported (a separate
    /// lane so the collect systems don't confuse a routing decision with a normal
    /// agent turn).
    pub transition_outcomes: UnboundedSender<InferenceOutcome>,
    /// Where completed *compaction* jobs (LLM context summarization) are
    /// reported - again a separate lane so a summary isn't mistaken for a turn.
    pub compaction_outcomes: UnboundedSender<crate::compaction_bridge::CompactionOutcome>,
    /// Where completed *content-summary transform* jobs are reported (the
    /// Summarize context-transform lane - see `context_transform`).
    pub content_summary_outcomes: UnboundedSender<crate::compaction_bridge::CompactionOutcome>,
    /// Signalled when an inference completes, to wake the tick loop.
    pub wake: Arc<Notify>,
    /// Runtime the worker tasks are spawned onto.
    pub runtime: Handle,
    /// Whether a model that can stream is called that way. On by default; see
    /// `InferenceJob::stream`.
    pub stream_inference: bool,
}

/// Truncate `text` to at most `max_chars` characters, never splitting a
/// multi-byte UTF-8 char. `max_chars` is an approximate char budget the caller
/// derives from a token estimate.
fn truncate_on_char_boundary(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests;
