//! A run's state: everything about it that changes as it runs.
//!
//! [`RunState`] is one value holding all of it: where the run is in its graph,
//! what it is waiting on, its context, its counters and its spend. The run
//! file stores the state at checkpoints and the [`StateDelta`]s between them,
//! a resumed run is inserted from the last state, and inspecting a live run
//! returns the same type. So what is saved, what is resumed and what is shown
//! can never disagree.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod context;
mod delta;
pub mod inspect;
pub mod journal;

pub use context::{ContextDiff, ContextState, EntryKind, EntryMeta, EntryState, RegionState};
pub use delta::{Change, RunEvent, StateDelta, TransitionReason, TransitionRecord};

use crate::spec::names::{EdgeName, ModelRef, RunId, StageName};
use context::ToolCallState;

/// Everything about a run that changes as it runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunState {
    /// The number of deltas applied to reach this state. The initial state
    /// is 0.
    pub seq: u64,
    /// How the run is doing.
    pub status: RunStatus,
    /// Where it is in its graph.
    pub cursor: Cursor,
    /// What its pipeline is doing.
    pub phase: PipelinePhase,
    /// Whether messages sent to it are delivered now.
    pub accepts_messages: bool,
    /// How many times it has entered each stage.
    pub visits: BTreeMap<StageName, u32>,
    /// The current stage's counters.
    pub progress: StageProgress,
    /// One record per stage, with spend and visits.
    pub ledger: Vec<StageRecord>,
    /// The context window.
    pub context: ContextState,
    /// The tool calls in flight, when there are any.
    pub pending: Option<PendingBatch>,
    /// The fan-out in progress, when there is one.
    pub fan_out: Option<FanOutState>,
    /// Messages waiting for the run to accept them.
    pub inbox: Vec<MessageState>,
    /// Questions put to a person and not yet answered.
    pub interactions: Vec<OpenInteraction>,
    /// Tokens, calls and cost so far.
    pub totals: Totals,
    /// Time spent working.
    pub clock: Clock,
    /// What the run did that its caller should know.
    pub flags: Flags,
    /// The child runs it started.
    pub children: Vec<RunId>,
    /// Its title, once one is made.
    pub title: Option<String>,
    /// Its final output, once one is handed back.
    pub final_output: Option<FinalOutputState>,
    /// Why it is waiting, in words, when it is.
    pub wait_reason: Option<String>,
    /// The last edge it took.
    pub last_transition: Option<TransitionRecord>,
}

impl RunState {
    /// The state a new run starts in, at `entry`.
    pub fn initial(entry: StageName, context: ContextState, accepts_messages: bool) -> Self {
        Self {
            seq: 0,
            status: RunStatus::Idle,
            cursor: Cursor {
                stage: entry,
                visit: String::new(),
                iteration: 0,
            },
            phase: PipelinePhase::ReadyToInfer,
            accepts_messages,
            visits: BTreeMap::new(),
            progress: StageProgress::default(),
            ledger: Vec::new(),
            context,
            pending: None,
            fan_out: None,
            inbox: Vec::new(),
            interactions: Vec::new(),
            totals: Totals::default(),
            clock: Clock::default(),
            flags: Flags::default(),
            children: Vec::new(),
            title: None,
            final_output: None,
            wait_reason: None,
            last_transition: None,
        }
    }
}

/// How a run is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum RunStatus {
    /// Spawned, not yet started.
    Idle,
    /// Working.
    Active,
    /// Waiting on a person, a child run or a tool.
    Waiting,
    /// Paused by a person.
    Paused,
    /// Finished.
    Complete,
    /// Failed.
    Error(String),
    /// Stopped by a person.
    Cancelled,
}

/// Where a run is in its graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Cursor {
    /// The stage it is in.
    pub stage: StageName,
    /// The id of this visit to the stage.
    pub visit: String,
    /// The inference round within the visit.
    pub iteration: u32,
}

/// What a run's pipeline is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum PipelinePhase {
    /// About to send a model request.
    ReadyToInfer,
    /// Waiting on a model reply.
    AwaitingInference,
    /// Waiting on tool calls.
    AwaitingTools,
    /// Waiting on the context being summarized.
    AwaitingCompaction,
    /// Waiting on the model to pick one of these edges.
    AwaitingChoice(Vec<EdgeName>),
    /// Waiting on its child runs.
    WaitingForChildren,
    /// Waiting on fan-out workers.
    FanOut,
    /// Waiting on a person.
    AwaitingPerson,
    /// Stuck: dispatch keeps failing and needs a person.
    Wedged(String),
    /// Paused.
    Paused,
    /// Finished, failed or cancelled.
    Done,
}

/// The current stage's counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StageProgress {
    /// Tool calls made.
    pub total_tool_calls: u32,
    /// Nudges sent for a reply with no tool call.
    pub text_only_nudges: u32,
    /// Nudges sent for a reply cut off by the output cap.
    pub cut_off_nudges: u32,
    /// Whether the next request raises the output cap.
    pub raise_output_cap: bool,
    /// Inference rounds.
    pub iterations: u32,
    /// Tool calls that changed a file.
    pub modifying_tool_calls: u32,
    /// Tool calls that tried to change a file and were blocked.
    pub blocked_modification_calls: u32,
    /// Each region's content digest when the stage started, for `require_region_updated`.
    pub entry_region_digests: BTreeMap<String, u64>,
    /// Times a gate sent the run back into this stage.
    pub gate_reentries: u32,
    /// When the stage started, in unix seconds.
    pub stage_started_at: Option<i64>,
    /// When the run began waiting, in unix seconds.
    pub waiting_since: Option<i64>,
    /// Edits per file path, for the stuck rule.
    pub edits_by_path: BTreeMap<String, u32>,
    /// Whether the stuck rule has fired.
    pub stuck_fired: bool,
    /// Images the stage produced.
    pub images_produced: u32,
    /// Nudges sent for a stage expected to produce an image.
    pub no_image_nudges: u32,
}

/// One stage's record: spend, visits and the models it ran on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StageRecord {
    /// The stage.
    pub stage: StageName,
    /// How it stands.
    pub status: StageStatus,
    /// Whether it was ever entered.
    pub entered: bool,
    /// Its spend, summed over visits.
    pub spend: Spend,
    /// The models it ran on.
    pub models: Vec<ModelRef>,
    /// Each visit.
    pub visits: Vec<VisitRecord>,
    /// Tokens per region at its first call.
    pub region_tokens: BTreeMap<String, u64>,
    /// Prompt tokens of its first call.
    pub first_call_prompt_tokens: Option<u64>,
    /// Whether a runaway-cost warning was logged.
    pub runaway_warned: bool,
    /// Whether its output cap was raised.
    pub output_cap_raised: bool,
    /// When it first started, in unix seconds.
    pub started_at: Option<i64>,
    /// When it last ended, in unix seconds.
    pub ended_at: Option<i64>,
    /// Time spent working in it.
    pub clock: Clock,
}

/// How a stage stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum StageStatus {
    /// Not reached.
    Pending,
    /// Running.
    Active,
    /// Waiting on a person.
    WaitingInput,
    /// Done.
    Complete,
    /// Failed.
    Error,
    /// Passed over.
    Skipped,
}

/// One visit to a stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VisitRecord {
    /// The visit's id.
    pub id: String,
    /// When it started, in unix seconds.
    pub entered_at: i64,
    /// When it ended, in unix seconds.
    pub left_at: Option<i64>,
    /// Its spend.
    pub spend: Spend,
    /// Time spent working in it.
    pub clock: Clock,
}

/// Tokens and cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Spend {
    /// Prompt tokens.
    pub prompt_tokens: u64,
    /// Completion tokens.
    pub completion_tokens: u64,
    /// Prompt tokens read from cache.
    pub cached_tokens: u64,
    /// Prompt tokens written to cache.
    pub cache_write_tokens: u64,
    /// Cost of the calls that could be priced, in US dollars.
    pub priced_usd: f64,
    /// Calls the provider priced itself.
    pub reported_calls: u32,
    /// Calls priced from published rates.
    pub computed_calls: u32,
    /// Calls that could not be priced.
    pub unpriced_calls: u32,
}

/// A run's totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Totals {
    /// Tokens and cost.
    pub spend: Spend,
    /// Tool calls made.
    pub tool_calls: u64,
}

/// Time spent working, not counting time spent waiting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Clock {
    /// Seconds banked from earlier stretches of work.
    pub banked_secs: u64,
    /// When the current stretch began, in unix seconds, while working.
    pub since: Option<i64>,
}

/// What a run did that its caller should know.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Flags {
    /// Files it changed.
    pub modified_files: Vec<String>,
    /// How many files it changed.
    pub modified_file_count: u32,
    /// Whether it ended without output.
    pub empty_output: bool,
    /// Whether it ended with no tool that could hand back output.
    pub no_output_tools: bool,
    /// Web searches run.
    pub searches_run: u32,
    /// Web searches that found nothing.
    pub searches_empty: u32,
    /// Stages cut off by their iteration cap.
    pub max_iterations_hit: u32,
    /// Gates let through after running out of attempts.
    pub gates_forced: u32,
    /// Required regions left empty when the run gave up on them.
    pub required_regions_abandoned: Vec<String>,
    /// Whether the workdir went away mid-run.
    pub workspace_lost: bool,
    /// Whether it handed back a final output.
    pub produced_output: bool,
    /// Times a missing final output was let through.
    pub output_forced: u32,
    /// Fan-outs that ran with fewer workers than asked.
    pub splits_degraded: u32,
    /// Scripts that failed to compile or run.
    pub broken_scripts: Vec<String>,
}

/// Tool calls in flight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PendingBatch {
    /// The calls, in the order the model made them.
    pub calls: Vec<ToolCallState>,
    /// The results that have come back, by call id.
    pub done: BTreeMap<String, ToolResultState>,
}

/// A tool call's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolResultState {
    /// The result's text.
    pub text: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// A fan-out in progress.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FanOutState {
    /// The stage that started it.
    pub stage: StageName,
    /// What it runs and how: the stage's fan-out settings, or the ones a
    /// `fan_out` call gave.
    pub config: crate::spec::graph::FanOutDef,
    /// The most workers at once.
    pub max_workers: u32,
    /// Items not yet started.
    pub queued: Vec<WorkItemState>,
    /// Items running, with the run working on each.
    pub active: Vec<(String, RunId)>,
    /// Items done, with each worker's summary.
    pub done: Vec<(String, String)>,
    /// Items that failed, with why.
    pub failed: Vec<(String, String)>,
    /// Whether starting new workers is paused.
    pub paused: bool,
}

/// One fan-out work item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItemState {
    /// The item's id.
    pub id: String,
    /// The worker's inputs.
    pub inputs: crate::spec::inputs::InputValues,
}

/// A message waiting to be delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MessageState {
    /// Who sent it.
    pub from: String,
    /// Its text.
    pub text: String,
    /// The region it goes to, over the default.
    pub region: Option<String>,
}

/// A question put to a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpenInteraction {
    /// Its id.
    pub id: String,
    /// What was asked.
    pub prompt: String,
    /// The choices offered, if any.
    pub options: Vec<String>,
}

/// A run's final output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FinalOutputState {
    /// The output.
    pub content: String,
    /// Its format label.
    pub format: Option<String>,
    /// The stage that handed it back.
    pub stage: StageName,
    /// When, in unix seconds.
    pub submitted_at: i64,
    /// Whether it was cut to fit the size limit.
    pub truncated: bool,
}

#[cfg(test)]
#[path = "tests.rs"]
pub(crate) mod tests;
