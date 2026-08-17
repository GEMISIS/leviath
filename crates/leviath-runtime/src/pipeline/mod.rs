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
mod transition;
pub use transition::*;
mod hooks;
pub use hooks::*;
mod watchdog;
pub use watchdog::*;
mod requirements;
pub use requirements::*;
mod spawn;
pub use spawn::*;
mod transition_choice;
pub use transition_choice::*;
mod tool_stages;
pub use tool_stages::*;
mod messaging;
pub use messaging::*;
mod persist;
pub use persist::*;
mod compaction;
pub use compaction::*;
mod tool_results;
pub use tool_results::*;
mod gate;
pub use gate::*;
mod tools;
pub use tools::*;
mod response;
pub use response::*;
mod inference;
pub use inference::*;
mod resolve;
pub use resolve::*;
mod stall;
pub use stall::*;
mod wedge;
pub use wedge::*;
mod circuit;
pub use circuit::*;
mod calibration;
pub use calibration::*;

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
pub struct AwaitingInference;

/// Transient tag: the agent just entered a stage (index + name). The
/// [`sync_tool_stages`] system reads it to notify the [`ToolService`] of the
/// stage change, then removes it. Carries the data so the tool service need not
/// query the world.
#[derive(Component, Debug, Clone)]
pub struct StageJustEntered {
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
    /// exhausted list means "nowhere left to go" (issue #201).
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
pub struct InferenceStage {
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
    /// Opt-in: perform an exact pre-inference token count and reject requests
    /// that would overflow the model's context window (see
    /// `InferenceJob::exact_token_counting`). Off by default.
    pub exact_token_counting: bool,
}

/// Truncate `text` to at most `max_chars` characters, never splitting a
/// multi-byte UTF-8 char. `max_chars` is an approximate char budget the caller
/// derives from a token estimate.
fn truncate_on_char_boundary(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests;
