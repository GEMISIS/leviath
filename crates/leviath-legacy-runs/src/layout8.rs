//! The state and delta frames of a run file in binary layout 8, which builds
//! before a run accounted for every call it sent wrote.
//!
//! Layout 8 and this build's layout differ in what they keep about a model
//! call's cost:
//!
//! - a spend counts the calls whose cost is unknown by reason, where layout 8
//!   kept one count of unpriced calls and a flag for a cost its record never
//!   named;
//! - a billed call says how it ended and carries the provider's own id for
//!   it, and a failed attempt says whether its request went out;
//! - a state lists the calls it has sent that are not yet accounted for.
//!
//! A spend is held in a run's totals, in each stage record and each visit,
//! in each billed call and in how a segment ended, so the state, the delta
//! and the events that hold one are read here as layout 8 wrote them. Every
//! other type is the runtime's own, which did not change, and so is the spec:
//! a layout-8 spec frame is read as this build's.
//!
//! Layouts 2 to 7 kept the same spend and the same events, so they read
//! their states and deltas through this module too, each with the stage
//! record (`R`) and the fan-out in progress (`F`) it wrote.
//!
//! What an upgraded file says of its calls is what layout 8 knew. A call
//! counted unpriced had no reported cost and no rates, so it is unknown for
//! want of a price. A cost flagged unknown with no call counted was never
//! recorded. Every billed call was answered, since layout 8 recorded no
//! other, and no call is listed as out.

use std::collections::BTreeMap;

use leviath_core::cost::{CostUnknown, UnknownCalls};
use leviath_runtime::spec::names::{ModelRef, RunId, StageName};
use leviath_runtime::state::context::ToolCallState;
use leviath_runtime::state::journal::{
    self, ArtifactState, CallEnding, CallKind, ContextCommitState, ContextNoteState,
    ModelInputState, RequestDigestState, RetryState, SettledState, ToolOutcomeState,
};
use leviath_runtime::state::{
    self as state, BlobFile, Clock, ContextDiff, ContextState, Cursor, FinalOutputState, Flags,
    Grants, MessageState, OpenInteraction, PendingBatch, PipelinePhase, PointProgress,
    ReadPathCounts, RunFiles, RunStatus, StageProgress, StageStatus, ToolResultState,
    TransitionRecord, WaitState,
};
use serde::Deserialize;

/// The fingerprint in the header of every layout-8 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0x50, 0x7d, 0xeb, 0x68, 0xad, 0x34, 0x87, 0x52, 0x26, 0x94, 0x31, 0x12, 0xda, 0x90, 0x3b, 0xed,
    0x80, 0x61, 0x0e, 0x25, 0xcb, 0x45, 0xa6, 0x40, 0x30, 0x09, 0x2f, 0x67, 0xe3, 0xf6, 0x06, 0xff,
];

/// Tokens and cost, with layouts 2 to 8's fields.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct Spend {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cached_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) priced_usd: f64,
    pub(crate) reported_calls: u32,
    pub(crate) computed_calls: u32,
    pub(crate) unpriced_calls: u32,
    pub(crate) cost_unknown: bool,
}

impl From<Spend> for state::Spend {
    /// The same spend, its unpriced calls unknown for want of a price, and a
    /// cost its record never named counted as one call recorded without one.
    fn from(s: Spend) -> Self {
        let mut unknown = UnknownCalls::default();
        unknown.add_many(CostUnknown::NoPrice, s.unpriced_calls);
        if s.cost_unknown && s.unpriced_calls == 0 {
            unknown.add(CostUnknown::NotRecorded);
        }
        Self {
            prompt_tokens: s.prompt_tokens,
            completion_tokens: s.completion_tokens,
            cached_tokens: s.cached_tokens,
            cache_write_tokens: s.cache_write_tokens,
            priced_usd: s.priced_usd,
            reported_calls: s.reported_calls,
            computed_calls: s.computed_calls,
            unknown,
        }
    }
}

/// A run's totals, holding layout 8's spend.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct Totals {
    pub(crate) spend: Spend,
    pub(crate) tool_calls: u64,
}

impl From<Totals> for state::Totals {
    fn from(t: Totals) -> Self {
        Self {
            spend: t.spend.into(),
            tool_calls: t.tool_calls,
        }
    }
}

/// One visit to a stage, holding layout 8's spend.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct VisitRecord {
    pub(crate) id: String,
    pub(crate) entered_at: i64,
    pub(crate) left_at: Option<i64>,
    pub(crate) spend: Spend,
    pub(crate) clock: Clock,
}

impl From<VisitRecord> for state::VisitRecord {
    fn from(v: VisitRecord) -> Self {
        Self {
            id: v.id,
            entered_at: v.entered_at,
            left_at: v.left_at,
            spend: v.spend.into(),
            clock: v.clock,
        }
    }
}

/// One stage's record, with layout 8's fields.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct StageRecord {
    pub(crate) stage: StageName,
    pub(crate) status: StageStatus,
    pub(crate) entered: bool,
    pub(crate) spend: Spend,
    pub(crate) models: Vec<ModelRef>,
    pub(crate) visits: Vec<VisitRecord>,
    pub(crate) region_tokens: BTreeMap<String, u64>,
    pub(crate) first_call_prompt_tokens: Option<u64>,
    pub(crate) runaway_warned: bool,
    pub(crate) output_cap_raised: bool,
    pub(crate) resubmissions: u32,
    pub(crate) started_at: Option<i64>,
    pub(crate) ended_at: Option<i64>,
    pub(crate) clock: Clock,
}

impl From<StageRecord> for state::StageRecord {
    fn from(r: StageRecord) -> Self {
        Self {
            stage: r.stage,
            status: r.status,
            entered: r.entered,
            spend: r.spend.into(),
            models: r.models,
            visits: r.visits.into_iter().map(Into::into).collect(),
            region_tokens: r.region_tokens,
            first_call_prompt_tokens: r.first_call_prompt_tokens,
            runaway_warned: r.runaway_warned,
            output_cap_raised: r.output_cap_raised,
            resubmissions: r.resubmissions,
            started_at: r.started_at,
            ended_at: r.ended_at,
            clock: r.clock,
        }
    }
}

/// How a model call ended, as layouts 2 to 8 wrote it: a failure does not
/// say whether its request went out.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) enum AttemptOutcomeState {
    Succeeded,
    Failed {
        kind: String,
        transient: bool,
        capacity: bool,
        next: RetryState,
    },
}

impl From<AttemptOutcomeState> for journal::AttemptOutcomeState {
    /// The same ending. Whether a failed request went out was not kept, so
    /// it reads as not known.
    fn from(o: AttemptOutcomeState) -> Self {
        match o {
            AttemptOutcomeState::Succeeded => Self::Succeeded,
            AttemptOutcomeState::Failed {
                kind,
                transient,
                capacity,
                next,
            } => Self::Failed {
                kind,
                transient,
                capacity,
                next,
                sent: None,
            },
        }
    }
}

/// One trip to a provider, holding layout 8's ending.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct AttemptState {
    pub(crate) id: String,
    pub(crate) number: u32,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) outcome: AttemptOutcomeState,
    pub(crate) finish_reason: Option<String>,
    pub(crate) stopped_for: Option<String>,
    pub(crate) duration_ms: u64,
    pub(crate) backoff_ms: u64,
    pub(crate) digest: RequestDigestState,
    pub(crate) model_input: Option<ModelInputState>,
}

impl From<AttemptState> for journal::AttemptState {
    fn from(a: AttemptState) -> Self {
        Self {
            id: a.id,
            number: a.number,
            provider: a.provider,
            model: a.model,
            outcome: a.outcome.into(),
            finish_reason: a.finish_reason,
            stopped_for: a.stopped_for,
            duration_ms: a.duration_ms,
            backoff_ms: a.backoff_ms,
            digest: a.digest,
            model_input: a.model_input,
        }
    }
}

/// How a segment ended, holding layout 8's spend.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct SegmentEnd {
    pub(crate) status: RunStatus,
    pub(crate) stage: StageName,
    pub(crate) spend: Spend,
    pub(crate) tool_calls: u64,
    pub(crate) final_output: Option<FinalOutputState>,
    pub(crate) file: Option<state::FileRef>,
    pub(crate) seq: u64,
}

/// A segment starting, holding layout 8's ending of the one before.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct ContinuedState {
    pub(crate) segment: u32,
    pub(crate) stage: StageName,
    pub(crate) forked_from: Option<RunId>,
    pub(crate) previous: Option<SegmentEnd>,
    pub(crate) regions: Vec<(String, leviath_runtime::spec::graph::OnContinue)>,
}

impl From<ContinuedState> for state::ContinuedState {
    fn from(c: ContinuedState) -> Self {
        Self {
            segment: c.segment,
            stage: c.stage,
            forked_from: c.forked_from,
            previous: c.previous.map(|end| state::SegmentEnd {
                status: end.status,
                stage: end.stage,
                spend: end.spend.into(),
                tool_calls: end.tool_calls,
                final_output: end.final_output,
                file: end.file,
                seq: end.seq,
            }),
            regions: c.regions,
        }
    }
}

/// Something that happened during a step, in layouts 2 to 8's order of
/// variants.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) enum RunEvent {
    Inference {
        attempt: String,
        model: ModelRef,
        spend: Spend,
        finish_reason: Option<String>,
        kind: CallKind,
        stage: Option<StageName>,
        iteration: u32,
    },
    Transition(TransitionRecord),
    Failover {
        from: ModelRef,
        to: ModelRef,
        reason: String,
    },
    ToolStarted(ToolCallState),
    ToolFinished {
        call_id: String,
        result: ToolResultState,
        millis: u64,
    },
    Answered {
        id: String,
        answer: String,
    },
    Message(MessageState),
    Log(String),
    Attempt(Box<AttemptState>),
    Dispatched {
        call_id: String,
        execution_id: String,
        requested_by: String,
    },
    Completed {
        call_id: String,
        execution_id: String,
        outcome: Option<ToolOutcomeState>,
        parts: Vec<String>,
    },
    Artifacts {
        execution_id: String,
        artifacts: Vec<ArtifactState>,
    },
    Settled(Box<SettledState>),
    ContextCommitted(Box<ContextCommitState>),
    ContextNoted(ContextNoteState),
    Continued(Box<ContinuedState>),
}

impl From<RunEvent> for state::RunEvent {
    fn from(e: RunEvent) -> Self {
        use state::RunEvent as E;
        match e {
            // Layout 8 recorded a call only when it answered, and kept no
            // id of the provider's for it.
            RunEvent::Inference {
                attempt,
                model,
                spend,
                finish_reason,
                kind,
                stage,
                iteration,
            } => E::Inference {
                attempt,
                model,
                spend: spend.into(),
                finish_reason,
                kind,
                stage,
                iteration,
                ending: CallEnding::Answered,
                provider_call_id: None,
            },
            RunEvent::Transition(v) => E::Transition(v),
            RunEvent::Failover { from, to, reason } => E::Failover { from, to, reason },
            RunEvent::ToolStarted(v) => E::ToolStarted(v),
            RunEvent::ToolFinished {
                call_id,
                result,
                millis,
            } => E::ToolFinished {
                call_id,
                result,
                millis,
            },
            RunEvent::Answered { id, answer } => E::Answered { id, answer },
            RunEvent::Message(v) => E::Message(v),
            RunEvent::Log(v) => E::Log(v),
            RunEvent::Attempt(v) => E::Attempt(Box::new((*v).into())),
            RunEvent::Dispatched {
                call_id,
                execution_id,
                requested_by,
            } => E::Dispatched {
                call_id,
                execution_id,
                requested_by,
            },
            RunEvent::Completed {
                call_id,
                execution_id,
                outcome,
                parts,
            } => E::Completed {
                call_id,
                execution_id,
                outcome,
                parts,
            },
            RunEvent::Artifacts {
                execution_id,
                artifacts,
            } => E::Artifacts {
                execution_id,
                artifacts,
            },
            RunEvent::Settled(v) => E::Settled(v),
            RunEvent::ContextCommitted(v) => E::ContextCommitted(v),
            RunEvent::ContextNoted(v) => E::ContextNoted(v),
            RunEvent::Continued(v) => E::Continued(Box::new((*v).into())),
        }
    }
}

/// A run's state, with its fields in layouts 2 to 8's order, holding a
/// fan-out in progress as `F` and its stage records as `R`.
#[derive(Debug, Deserialize)]
pub(crate) struct RunState<F = state::FanOutState, R = StageRecord> {
    seq: u64,
    status: RunStatus,
    cursor: Cursor,
    phase: PipelinePhase,
    accepts_messages: bool,
    visits: BTreeMap<StageName, u32>,
    progress: StageProgress,
    ledger: Vec<R>,
    context: ContextState,
    pending: Option<PendingBatch>,
    fan_out: Option<F>,
    inbox: Vec<MessageState>,
    interactions: Vec<OpenInteraction>,
    totals: Totals,
    clock: Clock,
    flags: Flags,
    children: Vec<RunId>,
    title: Option<String>,
    final_output: Option<FinalOutputState>,
    wait_reason: Option<WaitState>,
    last_transition: Option<TransitionRecord>,
    point: PointProgress,
    title_error: Option<String>,
    read_paths: Option<ReadPathCounts>,
    held: Option<leviath_runtime::spec::issues::SpawnIssues>,
    files: RunFiles,
    blobs: Vec<BlobFile>,
    grants: Grants,
    written: u64,
    last_progress_at: Option<i64>,
    remote_jobs: BTreeMap<String, String>,
}

impl<F: Into<state::FanOutState>, R: Into<state::StageRecord>> From<RunState<F, R>>
    for state::RunState
{
    /// The same state, with no call listed as out: layout 8 kept no such
    /// list.
    fn from(s: RunState<F, R>) -> Self {
        Self {
            seq: s.seq,
            status: s.status,
            cursor: s.cursor,
            phase: s.phase,
            accepts_messages: s.accepts_messages,
            visits: s.visits,
            progress: s.progress,
            ledger: s.ledger.into_iter().map(Into::into).collect(),
            context: s.context,
            pending: s.pending,
            fan_out: s.fan_out.map(Into::into),
            inbox: s.inbox,
            interactions: s.interactions,
            totals: s.totals.into(),
            clock: s.clock,
            flags: s.flags,
            children: s.children,
            title: s.title,
            final_output: s.final_output,
            wait_reason: s.wait_reason,
            last_transition: s.last_transition,
            point: s.point,
            title_error: s.title_error,
            read_paths: s.read_paths,
            held: s.held,
            files: s.files,
            blobs: s.blobs,
            grants: s.grants,
            written: s.written,
            last_progress_at: s.last_progress_at,
            remote_jobs: s.remote_jobs,
            open_calls: Vec::new(),
        }
    }
}

/// One changed field of a state, in layouts 2 to 8's order of variants,
/// holding a fan-out in progress as `F` and a stage record as `R`.
#[derive(Debug, Deserialize)]
pub(crate) enum Change<F = state::FanOutState, R = StageRecord> {
    Status(RunStatus),
    Cursor(Cursor),
    Phase(PipelinePhase),
    AcceptsMessages(bool),
    Visits(BTreeMap<StageName, u32>),
    Progress(StageProgress),
    LedgerRecord(u32, R),
    LedgerTruncate(u32),
    Context(ContextDiff),
    Pending(Option<PendingBatch>),
    FanOut(Option<F>),
    Inbox(Vec<MessageState>),
    Interactions(Vec<OpenInteraction>),
    Totals(Totals),
    Clock(Clock),
    Flags(Flags),
    Children(Vec<RunId>),
    Title(Option<String>),
    FinalOutput(Option<FinalOutputState>),
    WaitReason(Option<WaitState>),
    LastTransition(Option<TransitionRecord>),
    TitleError(Option<String>),
    ReadPaths(Option<ReadPathCounts>),
    Point(PointProgress),
    Held(Option<leviath_runtime::spec::issues::SpawnIssues>),
    Files(RunFiles),
    Blobs(Vec<BlobFile>),
    Grants(Grants),
    Written(u64),
    LastProgressAt(Option<i64>),
    RemoteJobs(BTreeMap<String, String>),
}

impl<F: Into<state::FanOutState>, R: Into<state::StageRecord>> From<Change<F, R>>
    for state::Change
{
    fn from(c: Change<F, R>) -> Self {
        use state::Change as C;
        match c {
            Change::Status(v) => C::Status(v),
            Change::Cursor(v) => C::Cursor(v),
            Change::Phase(v) => C::Phase(v),
            Change::AcceptsMessages(v) => C::AcceptsMessages(v),
            Change::Visits(v) => C::Visits(v),
            Change::Progress(v) => C::Progress(v),
            Change::LedgerRecord(at, v) => C::LedgerRecord(at, v.into()),
            Change::LedgerTruncate(v) => C::LedgerTruncate(v),
            Change::Context(v) => C::Context(v),
            Change::Pending(v) => C::Pending(v),
            Change::FanOut(v) => C::FanOut(v.map(Into::into)),
            Change::Inbox(v) => C::Inbox(v),
            Change::Interactions(v) => C::Interactions(v),
            Change::Totals(v) => C::Totals(v.into()),
            Change::Clock(v) => C::Clock(v),
            Change::Flags(v) => C::Flags(v),
            Change::Children(v) => C::Children(v),
            Change::Title(v) => C::Title(v),
            Change::FinalOutput(v) => C::FinalOutput(v),
            Change::WaitReason(v) => C::WaitReason(v),
            Change::LastTransition(v) => C::LastTransition(v),
            Change::TitleError(v) => C::TitleError(v),
            Change::ReadPaths(v) => C::ReadPaths(v),
            Change::Point(v) => C::Point(v),
            Change::Held(v) => C::Held(v),
            Change::Files(v) => C::Files(v),
            Change::Blobs(v) => C::Blobs(v),
            Change::Grants(v) => C::Grants(v),
            Change::Written(v) => C::Written(v),
            Change::LastProgressAt(v) => C::LastProgressAt(v),
            Change::RemoteJobs(v) => C::RemoteJobs(v),
        }
    }
}

/// One step of a run's history, in layouts 2 to 8's shape, holding a fan-out
/// in progress as `F` and a stage record as `R`.
#[derive(Debug, Deserialize)]
pub(crate) struct StateDelta<F = state::FanOutState, R = StageRecord> {
    seq: u64,
    at: i64,
    changes: Vec<Change<F, R>>,
    events: Vec<RunEvent>,
}

impl<F: Into<state::FanOutState>, R: Into<state::StageRecord>> From<StateDelta<F, R>>
    for state::StateDelta
{
    fn from(d: StateDelta<F, R>) -> Self {
        Self {
            seq: d.seq,
            at: d.at,
            changes: d.changes.into_iter().map(Into::into).collect(),
            events: d.events.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
#[path = "layout8_tests.rs"]
pub(crate) mod tests;
