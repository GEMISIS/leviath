//! `RunState`: everything about a run that has to survive, at one step.
//!
//! The same value a live daemon answers `Inspect` with and a run file's
//! checkpoints hold, so a run read while it is going and the same run read
//! from disk after it finished are one shape.

use async_graphql::{Enum, ID, SimpleObject};
use leviath_graphql_derive::mirror;
use leviath_runtime::state::{
    Clock, Cursor, FanOutState, FinalOutputState, Flags, MessageState, OpenInteraction,
    PendingBatch, PipelinePhase, PointProgress, RunState as CoreState, RunStatus as CoreStatus,
    Spend, StageProgress as CoreProgress, StageRecord as CoreRecord, StageStatus, Totals,
    TransitionReason as CoreReason, TransitionRecord, VisitRecord,
};

use super::super::super::scalars::{BigInt, Decimal, Timestamp};
use super::super::manifest::runtime::WorkerFailurePolicy;
use super::context::{ContextState, StateToolCall};
use super::values::{InputEntry, entries};
use super::{big, saturating};

pub(crate) use super::context::StatePart;

/// Where a run is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RunStateStatus {
    /// Placed, and not yet moving.
    Idle,
    /// Inferring, calling tools, moving between stages.
    Active,
    /// Waiting on a person or on its children.
    Waiting,
    /// Paused.
    Paused,
    /// Finished.
    Complete,
    /// Failed; `error` says how.
    Error,
    /// Stopped from outside.
    Cancelled,
}

/// Where in its stage a run is: the stage, the visit and the turn.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateCursor {
    /// The stage.
    pub(crate) stage: String,
    /// This visit to it.
    pub(crate) visit: String,
    /// Inference turns taken in this visit.
    pub(crate) iteration: i32,
}

impl From<&Cursor> for StateCursor {
    fn from(cursor: &Cursor) -> Self {
        Self {
            stage: cursor.stage.to_string(),
            visit: cursor.visit.clone(),
            iteration: saturating(cursor.iteration),
        }
    }
}

/// What the engine is doing with a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum PipelinePhaseKind {
    /// Ready for its next model call.
    ReadyToInfer,
    /// A model call is out.
    AwaitingInference,
    /// A batch of tool calls is running.
    AwaitingTools,
    /// The context is being summarized.
    AwaitingCompaction,
    /// The model has to choose an edge; `edges` lists them.
    AwaitingChoice,
    /// Waiting for its child runs.
    WaitingForChildren,
    /// Running a fan-out.
    FanOut,
    /// Waiting for a person.
    AwaitingPerson,
    /// Stuck where nothing will move it; `reason` says why.
    Wedged,
    /// Paused.
    Paused,
    /// Finished.
    Done,
}

/// What the engine is doing with a run, with what that phase carries.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunPhase {
    /// The phase.
    pub(crate) kind: PipelinePhaseKind,
    /// The edges an `AWAITING_CHOICE` run chooses between.
    pub(crate) edges: Vec<String>,
    /// Why a `WEDGED` run is stuck.
    pub(crate) reason: Option<String>,
}

impl From<&PipelinePhase> for RunPhase {
    fn from(phase: &PipelinePhase) -> Self {
        let plain = |kind| Self {
            kind,
            edges: Vec::new(),
            reason: None,
        };
        match phase {
            PipelinePhase::ReadyToInfer => plain(PipelinePhaseKind::ReadyToInfer),
            PipelinePhase::AwaitingInference => plain(PipelinePhaseKind::AwaitingInference),
            PipelinePhase::AwaitingTools => plain(PipelinePhaseKind::AwaitingTools),
            PipelinePhase::AwaitingCompaction => plain(PipelinePhaseKind::AwaitingCompaction),
            PipelinePhase::AwaitingChoice(edges) => Self {
                kind: PipelinePhaseKind::AwaitingChoice,
                edges: edges.iter().map(ToString::to_string).collect(),
                reason: None,
            },
            PipelinePhase::WaitingForChildren => plain(PipelinePhaseKind::WaitingForChildren),
            PipelinePhase::FanOut => plain(PipelinePhaseKind::FanOut),
            PipelinePhase::AwaitingPerson => plain(PipelinePhaseKind::AwaitingPerson),
            PipelinePhase::Wedged(reason) => Self {
                kind: PipelinePhaseKind::Wedged,
                edges: Vec::new(),
                reason: Some(reason.clone()),
            },
            PipelinePhase::Paused => plain(PipelinePhaseKind::Paused),
            PipelinePhase::Done => plain(PipelinePhaseKind::Done),
        }
    }
}

/// How many times a run has entered one stage.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StageVisitCount {
    /// The stage.
    pub(crate) stage: String,
    /// How many times it was entered.
    pub(crate) visits: i32,
}

/// A count kept against one key: a region's digest, a file's edits.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct KeyedCount {
    /// What is counted.
    pub(crate) key: String,
    /// The count.
    pub(crate) count: BigInt,
}

/// The counters the engine keeps for the stage a run is in.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StageProgress {
    /// Tool calls made in the stage.
    pub(crate) total_tool_calls: i32,
    /// Nudges sent after a reply with no tool call.
    pub(crate) text_only_nudges: i32,
    /// Nudges sent after a reply cut off by the output cap.
    pub(crate) cut_off_nudges: i32,
    /// Whether the next reply gets a raised output cap.
    pub(crate) raise_output_cap: bool,
    /// Inference turns taken in the stage.
    pub(crate) iterations: i32,
    /// Tool calls that changed something.
    pub(crate) modifying_tool_calls: i32,
    /// Modifying calls that were blocked.
    pub(crate) blocked_modification_calls: i32,
    /// Each region's content digest when the stage was entered.
    pub(crate) entry_region_digests: Vec<KeyedCount>,
    /// How many times a gate sent the run back into the stage.
    pub(crate) gate_reentries: i32,
    /// When the stage was entered.
    pub(crate) stage_started_at: Option<Timestamp>,
    /// When the run started waiting, while it waits.
    pub(crate) waiting_since: Option<Timestamp>,
    /// Edits made to each file.
    pub(crate) edits_by_path: Vec<KeyedCount>,
    /// Whether stuck detection has fired in the stage.
    pub(crate) stuck_fired: bool,
    /// Images produced in the stage.
    pub(crate) images_produced: i32,
    /// Nudges sent after a reply that should have produced an image.
    pub(crate) no_image_nudges: i32,
}

impl From<&CoreProgress> for StageProgress {
    fn from(p: &CoreProgress) -> Self {
        Self {
            total_tool_calls: saturating(p.total_tool_calls),
            text_only_nudges: saturating(p.text_only_nudges),
            cut_off_nudges: saturating(p.cut_off_nudges),
            raise_output_cap: p.raise_output_cap,
            iterations: saturating(p.iterations),
            modifying_tool_calls: saturating(p.modifying_tool_calls),
            blocked_modification_calls: saturating(p.blocked_modification_calls),
            entry_region_digests: p
                .entry_region_digests
                .iter()
                .map(|(key, digest)| KeyedCount {
                    key: key.clone(),
                    count: big(*digest),
                })
                .collect(),
            gate_reentries: saturating(p.gate_reentries),
            stage_started_at: p.stage_started_at.map(Timestamp),
            waiting_since: p.waiting_since.map(Timestamp),
            edits_by_path: p
                .edits_by_path
                .iter()
                .map(|(key, edits)| KeyedCount {
                    key: key.clone(),
                    count: BigInt(i64::from(*edits)),
                })
                .collect(),
            stuck_fired: p.stuck_fired,
            images_produced: saturating(p.images_produced),
            no_image_nudges: saturating(p.no_image_nudges),
        }
    }
}

/// Tokens and money spent.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateSpend {
    /// Input tokens, cached ones included.
    pub(crate) prompt_tokens: BigInt,
    /// Output tokens.
    pub(crate) completion_tokens: BigInt,
    /// Input tokens read from the provider's cache.
    pub(crate) cached_tokens: BigInt,
    /// Tokens written to the provider's cache.
    pub(crate) cache_write_tokens: BigInt,
    /// What the calls cost, in US dollars, as far as they were priced.
    pub(crate) priced_usd: Decimal,
    /// Calls whose provider reported the cost.
    pub(crate) reported_calls: i32,
    /// Calls whose cost was computed from a price table.
    pub(crate) computed_calls: i32,
    /// Calls nothing could price.
    pub(crate) unpriced_calls: i32,
}

impl From<&Spend> for StateSpend {
    fn from(s: &Spend) -> Self {
        Self {
            prompt_tokens: big(s.prompt_tokens),
            completion_tokens: big(s.completion_tokens),
            cached_tokens: big(s.cached_tokens),
            cache_write_tokens: big(s.cache_write_tokens),
            priced_usd: Decimal(s.priced_usd),
            reported_calls: saturating(s.reported_calls),
            computed_calls: saturating(s.computed_calls),
            unpriced_calls: saturating(s.unpriced_calls),
        }
    }
}

/// Working time: seconds banked, and when the running clock started.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateClock {
    /// Seconds of work banked so far.
    pub(crate) banked_secs: BigInt,
    /// When the clock started running, while it runs.
    pub(crate) since: Option<Timestamp>,
}

impl From<&Clock> for StateClock {
    fn from(clock: &Clock) -> Self {
        Self {
            banked_secs: big(clock.banked_secs),
            since: clock.since.map(Timestamp),
        }
    }
}

/// Where a run is among the checkpoints of the stage it is in: the questions
/// a stage puts to a person before it may move on.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct CheckpointProgress {
    /// The checkpoint it is on, by its position in the stage's list.
    pub(crate) index: i32,
    /// The rounds of revision taken at that checkpoint.
    pub(crate) round: i32,
    /// While the checkpoint is put to a person: the document it shows them.
    pub(crate) document: Option<String>,
}

impl From<&PointProgress> for CheckpointProgress {
    fn from(point: &PointProgress) -> Self {
        Self {
            index: saturating(point.cursor),
            round: saturating(point.round),
            document: point.asking.clone(),
        }
    }
}

/// Where one stage of a run's ledger stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum LedgerStageStatus {
    /// Not entered yet.
    Pending,
    /// The run is in it.
    Active,
    /// Waiting for a person or for the runs it started.
    WaitingInput,
    /// Paused, or held until the machine can take the run back.
    Paused,
    /// Done.
    Complete,
    /// Failed.
    Error,
    /// Cancelled.
    Cancelled,
    /// Passed over.
    Skipped,
}

impl From<StageStatus> for LedgerStageStatus {
    fn from(status: StageStatus) -> Self {
        match status {
            StageStatus::Pending => Self::Pending,
            StageStatus::Active => Self::Active,
            StageStatus::WaitingInput => Self::WaitingInput,
            StageStatus::Paused => Self::Paused,
            StageStatus::Complete => Self::Complete,
            StageStatus::Error => Self::Error,
            StageStatus::Cancelled => Self::Cancelled,
            StageStatus::Skipped => Self::Skipped,
        }
    }
}

/// One visit to a stage.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct LedgerVisit {
    /// The visit.
    pub(crate) id: ID,
    /// When it began.
    pub(crate) entered_at: Timestamp,
    /// When it ended, once it has.
    pub(crate) left_at: Option<Timestamp>,
    /// What it spent.
    pub(crate) spend: StateSpend,
    /// Its working time.
    pub(crate) clock: StateClock,
}

impl From<&VisitRecord> for LedgerVisit {
    fn from(v: &VisitRecord) -> Self {
        Self {
            id: ID(v.id.clone()),
            entered_at: Timestamp(v.entered_at),
            left_at: v.left_at.map(Timestamp),
            spend: StateSpend::from(&v.spend),
            clock: StateClock::from(&v.clock),
        }
    }
}

/// One stage's line in a run's ledger.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct LedgerStage {
    /// The stage.
    pub(crate) stage: String,
    /// Where it stands.
    pub(crate) status: LedgerStageStatus,
    /// Whether it has been entered at all.
    pub(crate) entered: bool,
    /// What it spent across every visit.
    pub(crate) spend: StateSpend,
    /// The models it ran on, as `provider/model`.
    pub(crate) models: Vec<String>,
    /// Each visit, oldest first.
    pub(crate) visits: Vec<LedgerVisit>,
    /// Each region's size the last time it was measured, in tokens.
    pub(crate) region_tokens: Vec<KeyedCount>,
    /// The prompt size of its first model call.
    pub(crate) first_call_prompt_tokens: Option<BigInt>,
    /// Whether the run was warned that this stage is running away.
    pub(crate) runaway_warned: bool,
    /// Whether its output cap was raised.
    pub(crate) output_cap_raised: bool,
    /// When it started.
    pub(crate) started_at: Option<Timestamp>,
    /// When it ended.
    pub(crate) ended_at: Option<Timestamp>,
    /// Its working time.
    pub(crate) clock: StateClock,
}

impl From<&CoreRecord> for LedgerStage {
    fn from(r: &CoreRecord) -> Self {
        Self {
            stage: r.stage.to_string(),
            status: LedgerStageStatus::from(r.status),
            entered: r.entered,
            spend: StateSpend::from(&r.spend),
            models: r.models.iter().map(ToString::to_string).collect(),
            visits: r.visits.iter().map(LedgerVisit::from).collect(),
            region_tokens: r
                .region_tokens
                .iter()
                .map(|(key, tokens)| KeyedCount {
                    key: key.clone(),
                    count: big(*tokens),
                })
                .collect(),
            first_call_prompt_tokens: r.first_call_prompt_tokens.map(big),
            runaway_warned: r.runaway_warned,
            output_cap_raised: r.output_cap_raised,
            started_at: r.started_at.map(Timestamp),
            ended_at: r.ended_at.map(Timestamp),
            clock: StateClock::from(&r.clock),
        }
    }
}

/// One tool call's result, once it is in.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolCallReply {
    /// The provider's id for the call it answers.
    pub(crate) call_id: String,
    /// The result, as the model reads it.
    pub(crate) text: String,
    /// Whether the call failed.
    pub(crate) is_error: bool,
}

/// The batch of tool calls a run has out.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct PendingToolBatch {
    /// The calls, in the order the model made them.
    pub(crate) calls: Vec<StateToolCall>,
    /// The results that are in.
    pub(crate) done: Vec<ToolCallReply>,
}

impl From<&PendingBatch> for PendingToolBatch {
    fn from(batch: &PendingBatch) -> Self {
        Self {
            calls: batch.calls.iter().map(StateToolCall::from).collect(),
            done: batch
                .done
                .iter()
                .map(|(call_id, result)| ToolCallReply {
                    call_id: call_id.clone(),
                    text: result.text.clone(),
                    is_error: result.is_error,
                })
                .collect(),
        }
    }
}

/// One fan-out item waiting for a worker.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FanOutItemState {
    /// The item's own id.
    pub(crate) id: ID,
    /// The inputs its worker gets.
    pub(crate) inputs: Vec<InputEntry>,
}

/// One fan-out item and what became of it: the worker running it, its summary,
/// or why it failed.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FanOutItemOutcome {
    /// The item's own id.
    pub(crate) item_id: ID,
    /// The worker run, the summary or the failure.
    pub(crate) detail: String,
}

/// Where a fan-out's worker graph comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum FanOutWorkerKind {
    /// An installed blueprint.
    Blueprint,
    /// A blueprint read from its directory on the daemon's machine.
    BlueprintFile,
    /// A stage of this run's own graph.
    Stage,
    /// A blueprint chosen by a query.
    Query,
}

/// A fan-out in progress: the stage running it, its queue, and its workers.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FanOutProgress {
    /// The stage running the fan-out.
    pub(crate) stage: String,
    /// Where its workers' graph comes from.
    pub(crate) worker_kind: FanOutWorkerKind,
    /// The blueprint, stage or query that names it.
    pub(crate) worker: String,
    /// The stage that merges the results, when there is one.
    pub(crate) merge_stage: Option<String>,
    /// What one worker failing does to the rest.
    pub(crate) on_worker_failure: WorkerFailurePolicy,
    /// How many workers run at once. Null means every item at once.
    pub(crate) max_workers: Option<i32>,
    /// Items not started yet.
    pub(crate) queued: Vec<FanOutItemState>,
    /// Items running, each with its worker's run id as `detail`.
    pub(crate) active: Vec<FanOutItemOutcome>,
    /// Items finished, each with its summary as `detail`.
    pub(crate) done: Vec<FanOutItemOutcome>,
    /// Items failed, each with the failure as `detail`.
    pub(crate) failed: Vec<FanOutItemOutcome>,
    /// Whether the fan-out is paused.
    pub(crate) paused: bool,
}

/// Pairs of an item id and something about it, as outcomes.
fn outcomes<'a>(pairs: impl Iterator<Item = (&'a String, String)>) -> Vec<FanOutItemOutcome> {
    pairs
        .map(|(item, detail)| FanOutItemOutcome {
            item_id: ID(item.clone()),
            detail,
        })
        .collect()
}

impl From<&FanOutState> for FanOutProgress {
    fn from(f: &FanOutState) -> Self {
        use leviath_runtime::spec::graph::{WorkerFailure, WorkerSource};
        let (worker_kind, worker) = match &f.config.worker {
            WorkerSource::Blueprint(blueprint) => {
                (FanOutWorkerKind::Blueprint, blueprint.to_string())
            }
            WorkerSource::BlueprintFile(path) => {
                (FanOutWorkerKind::BlueprintFile, path.to_string())
            }
            WorkerSource::Stage(stage) => (FanOutWorkerKind::Stage, stage.to_string()),
            WorkerSource::Query(query) => (FanOutWorkerKind::Query, query.clone()),
        };
        Self {
            stage: f.stage.to_string(),
            worker_kind,
            worker,
            merge_stage: f.config.merge_stage.as_ref().map(ToString::to_string),
            on_worker_failure: match f.config.on_worker_failure {
                WorkerFailure::Continue => WorkerFailurePolicy::Continue,
                WorkerFailure::FailAll => WorkerFailurePolicy::FailAll,
            },
            max_workers: f.max_workers.map(saturating),
            queued: f
                .queued
                .iter()
                .map(|item| FanOutItemState {
                    id: ID(item.id.clone()),
                    inputs: entries(&item.inputs),
                })
                .collect(),
            active: outcomes(f.active.iter().map(|(i, run)| (i, run.to_string()))),
            done: outcomes(f.done.iter().map(|(i, s)| (i, s.clone()))),
            failed: outcomes(f.failed.iter().map(|(i, e)| (i, e.clone()))),
            paused: f.paused,
        }
    }
}

/// A message waiting in a run's inbox.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateMessage {
    /// Who sent it.
    pub(crate) sender: String,
    /// What it says.
    pub(crate) text: String,
    /// The region it goes to, when it names one.
    pub(crate) region: Option<String>,
}

impl From<&MessageState> for StateMessage {
    fn from(m: &MessageState) -> Self {
        Self {
            sender: m.from.clone(),
            text: m.text.clone(),
            region: m.region.clone(),
        }
    }
}

/// A question a run has open, waiting for an answer.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct OpenQuestion {
    /// The question.
    pub(crate) id: ID,
    /// What it asks.
    pub(crate) prompt: String,
    /// The options offered, when it offers any.
    pub(crate) options: Vec<String>,
}

impl From<&OpenInteraction> for OpenQuestion {
    fn from(i: &OpenInteraction) -> Self {
        Self {
            id: ID(i.id.clone()),
            prompt: i.prompt.clone(),
            options: i.options.clone(),
        }
    }
}

/// What a run has spent and done, in total.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateTotals {
    /// Tokens and money.
    pub(crate) spend: StateSpend,
    /// Tool calls made.
    pub(crate) tool_calls: BigInt,
}

impl From<&Totals> for StateTotals {
    fn from(t: &Totals) -> Self {
        Self {
            spend: StateSpend::from(&t.spend),
            tool_calls: big(t.tool_calls),
        }
    }
}

/// What a run noticed about itself on the way.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateFlags {
    /// Files it changed, up to a cap.
    pub(crate) modified_files: Vec<String>,
    /// How many files it changed in all.
    pub(crate) modified_file_count: i32,
    /// Whether it finished with an empty answer.
    pub(crate) empty_output: bool,
    /// Whether it finished without the tool that submits an answer.
    pub(crate) no_output_tools: bool,
    /// Searches run.
    pub(crate) searches_run: i32,
    /// Searches that found nothing.
    pub(crate) searches_empty: i32,
    /// Times a stage hit its iteration cap.
    pub(crate) max_iterations_hit: i32,
    /// Times a gate was forced open.
    pub(crate) gates_forced: i32,
    /// Required regions left unfilled when a stage gave up on them.
    pub(crate) required_regions_abandoned: Vec<String>,
    /// Whether its working directory disappeared.
    pub(crate) workspace_lost: bool,
    /// Whether it produced an answer.
    pub(crate) produced_output: bool,
    /// Times an answer was forced out of it.
    pub(crate) output_forced: i32,
    /// Fan-out splits that fell back to fewer workers.
    pub(crate) splits_degraded: i32,
    /// Scripts that would not run.
    pub(crate) broken_scripts: Vec<String>,
}

impl From<&Flags> for StateFlags {
    fn from(f: &Flags) -> Self {
        Self {
            modified_files: f.modified_files.clone(),
            modified_file_count: saturating(f.modified_file_count),
            empty_output: f.empty_output,
            no_output_tools: f.no_output_tools,
            searches_run: saturating(f.searches_run),
            searches_empty: saturating(f.searches_empty),
            max_iterations_hit: saturating(f.max_iterations_hit),
            gates_forced: saturating(f.gates_forced),
            required_regions_abandoned: f.required_regions_abandoned.clone(),
            workspace_lost: f.workspace_lost,
            produced_output: f.produced_output,
            output_forced: saturating(f.output_forced),
            splits_degraded: saturating(f.splits_degraded),
            broken_scripts: f.broken_scripts.clone(),
        }
    }
}

/// A run's final answer, once it has one.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateAnswer {
    /// Its size in bytes. The answer itself is in the file the run's `files`
    /// name, and `Run.finalOutput` reads it.
    pub(crate) bytes: BigInt,
    /// The format it was asked for in.
    pub(crate) format: Option<String>,
    /// The stage that submitted it.
    pub(crate) stage: String,
    /// When.
    pub(crate) submitted_at: Timestamp,
    /// Whether the answer was cut short.
    pub(crate) truncated: bool,
}

impl From<&FinalOutputState> for StateAnswer {
    fn from(o: &FinalOutputState) -> Self {
        Self {
            bytes: super::big(o.bytes),
            format: o.format.clone(),
            stage: o.stage.to_string(),
            submitted_at: Timestamp(o.submitted_at),
            truncated: o.truncated,
        }
    }
}

/// Why a run moved from one stage to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum TransitionReason {
    /// The edge's condition held.
    Condition,
    /// A gate let it through.
    Gate,
    /// The model chose the edge.
    ModelChoice,
    /// The model named no edge it could take, so the run took the stage's
    /// first edge.
    Fallback,
    /// Something outside the run forced it.
    Forced,
    /// A fan-out worker finished.
    Worker,
    /// A routing script chose the edge.
    Router,
}

impl From<CoreReason> for TransitionReason {
    fn from(reason: CoreReason) -> Self {
        match reason {
            CoreReason::Condition => Self::Condition,
            CoreReason::Gate => Self::Gate,
            CoreReason::ModelChoice => Self::ModelChoice,
            CoreReason::Fallback => Self::Fallback,
            CoreReason::Forced => Self::Forced,
            CoreReason::Worker => Self::Worker,
            CoreReason::Router => Self::Router,
        }
    }
}

/// One move from a stage to the next.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StageTransition {
    /// The stage it left.
    pub(crate) from_stage: String,
    /// The stage it entered.
    pub(crate) to_stage: String,
    /// The edge it took, when it took a named one.
    pub(crate) edge: Option<String>,
    /// Why.
    pub(crate) reason: TransitionReason,
    /// The visit to `to` it began.
    pub(crate) visit: String,
}

impl From<&TransitionRecord> for StageTransition {
    fn from(t: &TransitionRecord) -> Self {
        Self {
            from_stage: t.from.to_string(),
            to_stage: t.to.to_string(),
            edge: t.edge.as_ref().map(ToString::to_string),
            reason: TransitionReason::from(t.reason),
            visit: t.visit.clone(),
        }
    }
}

/// A run's status, and the error a failed one carries.
pub(crate) fn status_of(status: &CoreStatus) -> (RunStateStatus, Option<String>) {
    match status {
        CoreStatus::Idle => (RunStateStatus::Idle, None),
        CoreStatus::Active => (RunStateStatus::Active, None),
        CoreStatus::Waiting => (RunStateStatus::Waiting, None),
        CoreStatus::Paused => (RunStateStatus::Paused, None),
        CoreStatus::Complete => (RunStateStatus::Complete, None),
        CoreStatus::Error(message) => (RunStateStatus::Error, Some(message.clone())),
        CoreStatus::Cancelled => (RunStateStatus::Cancelled, None),
    }
}

/// The visit counts of a run, stage by stage.
pub(crate) fn visit_counts(
    visits: &std::collections::BTreeMap<leviath_runtime::spec::names::StageName, u32>,
) -> Vec<StageVisitCount> {
    visits
        .iter()
        .map(|(stage, n)| StageVisitCount {
            stage: stage.to_string(),
            visits: saturating(*n),
        })
        .collect()
}

/// A run at one step: everything about it that has to survive a restart.
///
/// Read live from the daemon while it holds the run, and from the run file's
/// checkpoints and deltas otherwise, so both are the same shape.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RunState {
    /// The step this is the state after. The state a run starts in is step 0.
    pub(crate) seq: i32,
    /// Where the run is in its life.
    pub(crate) status: RunStateStatus,
    /// What went wrong, when `status` is `ERROR`.
    pub(crate) error: Option<String>,
    /// The stage, visit and turn.
    pub(crate) cursor: StateCursor,
    /// What the engine is doing with it.
    pub(crate) phase: RunPhase,
    /// Whether it takes messages now.
    pub(crate) accepts_messages: bool,
    /// How many times each stage was entered.
    pub(crate) visits: Vec<StageVisitCount>,
    /// The counters for the stage it is in.
    pub(crate) progress: StageProgress,
    /// One line per stage it has reached.
    pub(crate) ledger: Vec<LedgerStage>,
    /// Its context window: regions and the entries in them.
    pub(crate) context: ContextState,
    /// The batch of tool calls it has out.
    pub(crate) pending: Option<PendingToolBatch>,
    /// The fan-out it is running.
    pub(crate) fan_out: Option<FanOutProgress>,
    /// Messages waiting to be delivered.
    pub(crate) inbox: Vec<StateMessage>,
    /// Questions waiting for an answer.
    pub(crate) questions: Vec<OpenQuestion>,
    /// What it has spent and done.
    pub(crate) totals: StateTotals,
    /// Its working time.
    pub(crate) clock: StateClock,
    /// What it noticed about itself.
    pub(crate) flags: StateFlags,
    /// The runs it started.
    pub(crate) children: Vec<ID>,
    /// Its title, once titled.
    pub(crate) title: Option<String>,
    /// Its answer, once it has one.
    pub(crate) answer: Option<StateAnswer>,
    /// Why it is waiting, while it waits.
    pub(crate) wait_reason: Option<String>,
    /// The last move it made between stages.
    pub(crate) last_transition: Option<StageTransition>,
    /// Where it is among its stage's checkpoints.
    pub(crate) checkpoint: CheckpointProgress,
    /// Why it is held rather than running: each thing it names that this
    /// machine no longer has, or that changed since it started. Null when it
    /// is not held.
    pub(crate) held: Option<Vec<crate::commands::serve::graphql::mutation::spawn::SpawnIssue>>,
    /// The answer, logs and audits it keeps beside its run file.
    pub(crate) files: super::files::StateFiles,
    /// The stored parts it keeps beside its run file.
    pub(crate) blobs: Vec<super::files::StateBlob>,
    /// What a person approved for it beyond the calls they were asked about.
    pub(crate) grants: StateGrants,
    /// The bytes it has written, against its write ceilings.
    pub(crate) written_bytes: BigInt,
    /// When it last made progress, when its record keeps that apart from
    /// its last step (a run converted from an earlier release). Null when
    /// its last step is its progress.
    pub(crate) last_progress_at: Option<Timestamp>,
}

/// The approvals a person granted a run beyond the call they were asked
/// about, by the keys a later call is matched on: a command, or a path.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateGrants {
    /// Keys allowed for the rest of the run.
    pub(crate) run: Vec<String>,
    /// Keys allowed while the run stays in the stage they were granted in.
    pub(crate) stage: Vec<String>,
    /// The stage those were granted in, by its position in the graph from 0.
    /// Null when nothing was granted for a stage.
    pub(crate) stage_index: Option<i32>,
    /// Tools a person cleared at the taint gate for the rest of the run.
    pub(crate) cleared: Vec<String>,
}

impl From<&leviath_runtime::state::Grants> for StateGrants {
    fn from(g: &leviath_runtime::state::Grants) -> Self {
        Self {
            run: g.run.clone(),
            stage: g.stage.clone(),
            stage_index: g.stage_index.map(saturating),
            cleared: g.cleared.clone(),
        }
    }
}

/// The issues a held run is held for, as the schema types them.
pub(crate) fn held_issues(
    held: Option<&leviath_runtime::spec::issues::SpawnIssues>,
) -> Option<Vec<crate::commands::serve::graphql::mutation::spawn::SpawnIssue>> {
    held.map(|issues| {
        issues
            .iter()
            .map(crate::commands::serve::graphql::mutation::spawn::SpawnIssue::from)
            .collect()
    })
}

impl From<&CoreState> for RunState {
    fn from(s: &CoreState) -> Self {
        let (status, error) = status_of(&s.status);
        Self {
            seq: i32::try_from(s.seq).unwrap_or(i32::MAX),
            status,
            error,
            cursor: StateCursor::from(&s.cursor),
            phase: RunPhase::from(&s.phase),
            accepts_messages: s.accepts_messages,
            visits: visit_counts(&s.visits),
            progress: StageProgress::from(&s.progress),
            ledger: s.ledger.iter().map(LedgerStage::from).collect(),
            context: ContextState::from(&s.context),
            pending: s.pending.as_ref().map(PendingToolBatch::from),
            fan_out: s.fan_out.as_ref().map(FanOutProgress::from),
            inbox: s.inbox.iter().map(StateMessage::from).collect(),
            questions: s.interactions.iter().map(OpenQuestion::from).collect(),
            totals: StateTotals::from(&s.totals),
            clock: StateClock::from(&s.clock),
            flags: StateFlags::from(&s.flags),
            children: s.children.iter().map(|id| ID(id.to_string())).collect(),
            title: s.title.clone(),
            answer: s.final_output.as_ref().map(StateAnswer::from),
            wait_reason: s
                .wait_reason
                .as_ref()
                .map(|w| leviath_core::run_meta::WaitReason::from(w).to_string()),
            last_transition: s.last_transition.as_ref().map(StageTransition::from),
            checkpoint: CheckpointProgress::from(&s.point),
            held: held_issues(s.held.as_ref()),
            files: super::files::StateFiles::from(&s.files),
            blobs: s.blobs.iter().map(super::files::StateBlob::from).collect(),
            grants: StateGrants::from(&s.grants),
            written_bytes: big(s.written),
            last_progress_at: s.last_progress_at.map(Timestamp),
        }
    }
}
