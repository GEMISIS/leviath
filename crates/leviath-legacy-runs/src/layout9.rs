//! The state and delta frames of a run file in binary layout 9, which builds
//! before a failed run stated its kind wrote.
//!
//! Layout 9 and this build's layout differ in one thing: a failed run's
//! status held the sentence that says what went wrong and nothing else, where
//! this build's also holds which kind of failure it is.
//!
//! A status is held in a state, in the change that replaces it, and in how a
//! segment ended, so the state, the delta and the events that hold one are
//! read here as layout 9 wrote them. Every other type is the runtime's own,
//! which did not change, and so is the spec: a layout-9 spec frame is read as
//! this build's.
//!
//! Layouts 2 to 8 kept the same status, so they read theirs through
//! [`RunStatus`] too.
//!
//! What an upgraded file says of a failure is what layout 9 knew: its
//! sentence, and no kind.

use std::collections::BTreeMap;

use leviath_runtime::spec::names::{ModelRef, RunId, StageName};
use leviath_runtime::state::context::ToolCallState;
use leviath_runtime::state::journal::{
    ArtifactState, AttemptState, CallEnding, CallKind, ContextCommitState, ContextNoteState,
    OpenCallState, SettledState, ToolOutcomeState,
};
use leviath_runtime::state::{
    self as state, BlobFile, Clock, ContextDiff, ContextState, Cursor, FanOutState,
    FinalOutputState, Flags, Grants, MessageState, OpenInteraction, PendingBatch, PipelinePhase,
    PointProgress, ReadPathCounts, RunFiles, Spend, StageProgress, StageRecord, ToolResultState,
    Totals, TransitionRecord, WaitState,
};
use serde::Deserialize;

/// The fingerprint in the header of every layout-9 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0xb3, 0x92, 0xf9, 0x03, 0x95, 0xf7, 0xe6, 0x5b, 0x4d, 0xb2, 0x5e, 0xa7, 0x59, 0xa9, 0x9c, 0x0b,
    0x08, 0x08, 0x84, 0xd2, 0x58, 0x92, 0x82, 0xe9, 0x4a, 0x06, 0x86, 0x3f, 0x5a, 0xbd, 0x5f, 0x7d,
];

/// How a run is doing, as layouts 2 to 9 wrote it: a failure holds its
/// sentence alone.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) enum RunStatus {
    Idle,
    Active,
    Waiting,
    Paused,
    Complete,
    Error(String),
    Cancelled,
}

impl From<RunStatus> for state::RunStatus {
    /// The same status. A failure keeps its sentence and states no kind,
    /// since none was written.
    fn from(s: RunStatus) -> Self {
        match s {
            RunStatus::Idle => Self::Idle,
            RunStatus::Active => Self::Active,
            RunStatus::Waiting => Self::Waiting,
            RunStatus::Paused => Self::Paused,
            RunStatus::Complete => Self::Complete,
            RunStatus::Error(message) => Self::Error {
                message,
                kind: None,
            },
            RunStatus::Cancelled => Self::Cancelled,
        }
    }
}

/// How a segment ended, holding layout 9's status.
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

impl From<SegmentEnd> for state::SegmentEnd {
    fn from(end: SegmentEnd) -> Self {
        Self {
            status: end.status.into(),
            stage: end.stage,
            spend: end.spend,
            tool_calls: end.tool_calls,
            final_output: end.final_output,
            file: end.file,
            seq: end.seq,
        }
    }
}

/// A segment starting, holding layout 9's ending of the one before.
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
            previous: c.previous.map(Into::into),
            regions: c.regions,
        }
    }
}

/// Something that happened during a step, in layout 9's order of variants,
/// which is this build's.
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
        ending: CallEnding,
        provider_call_id: Option<String>,
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
            RunEvent::Inference {
                attempt,
                model,
                spend,
                finish_reason,
                kind,
                stage,
                iteration,
                ending,
                provider_call_id,
            } => E::Inference {
                attempt,
                model,
                spend,
                finish_reason,
                kind,
                stage,
                iteration,
                ending,
                provider_call_id,
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
            RunEvent::Attempt(v) => E::Attempt(v),
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

/// A run's state, with its fields in layout 9's order, which is this
/// build's.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct RunState {
    pub(crate) seq: u64,
    pub(crate) status: RunStatus,
    pub(crate) cursor: Cursor,
    pub(crate) phase: PipelinePhase,
    pub(crate) accepts_messages: bool,
    pub(crate) visits: BTreeMap<StageName, u32>,
    pub(crate) progress: StageProgress,
    pub(crate) ledger: Vec<StageRecord>,
    pub(crate) context: ContextState,
    pub(crate) pending: Option<PendingBatch>,
    pub(crate) fan_out: Option<FanOutState>,
    pub(crate) inbox: Vec<MessageState>,
    pub(crate) interactions: Vec<OpenInteraction>,
    pub(crate) totals: Totals,
    pub(crate) clock: Clock,
    pub(crate) flags: Flags,
    pub(crate) children: Vec<RunId>,
    pub(crate) title: Option<String>,
    pub(crate) final_output: Option<FinalOutputState>,
    pub(crate) wait_reason: Option<WaitState>,
    pub(crate) last_transition: Option<TransitionRecord>,
    pub(crate) point: PointProgress,
    pub(crate) title_error: Option<String>,
    pub(crate) read_paths: Option<ReadPathCounts>,
    pub(crate) held: Option<leviath_runtime::spec::issues::SpawnIssues>,
    pub(crate) files: RunFiles,
    pub(crate) blobs: Vec<BlobFile>,
    pub(crate) grants: Grants,
    pub(crate) written: u64,
    pub(crate) last_progress_at: Option<i64>,
    pub(crate) remote_jobs: BTreeMap<String, String>,
    pub(crate) open_calls: Vec<OpenCallState>,
}

impl From<RunState> for state::RunState {
    fn from(s: RunState) -> Self {
        Self {
            seq: s.seq,
            status: s.status.into(),
            cursor: s.cursor,
            phase: s.phase,
            accepts_messages: s.accepts_messages,
            visits: s.visits,
            progress: s.progress,
            ledger: s.ledger,
            context: s.context,
            pending: s.pending,
            fan_out: s.fan_out,
            inbox: s.inbox,
            interactions: s.interactions,
            totals: s.totals,
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
            open_calls: s.open_calls,
        }
    }
}

/// One changed field of a state, in layout 9's order of variants, which is
/// this build's.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) enum Change {
    Status(RunStatus),
    Cursor(Cursor),
    Phase(PipelinePhase),
    AcceptsMessages(bool),
    Visits(BTreeMap<StageName, u32>),
    Progress(StageProgress),
    LedgerRecord(u32, StageRecord),
    LedgerTruncate(u32),
    Context(ContextDiff),
    Pending(Option<PendingBatch>),
    FanOut(Option<FanOutState>),
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
    OpenCalls(Vec<OpenCallState>),
}

impl From<Change> for state::Change {
    fn from(c: Change) -> Self {
        use state::Change as C;
        match c {
            Change::Status(v) => C::Status(v.into()),
            Change::Cursor(v) => C::Cursor(v),
            Change::Phase(v) => C::Phase(v),
            Change::AcceptsMessages(v) => C::AcceptsMessages(v),
            Change::Visits(v) => C::Visits(v),
            Change::Progress(v) => C::Progress(v),
            Change::LedgerRecord(at, v) => C::LedgerRecord(at, v),
            Change::LedgerTruncate(v) => C::LedgerTruncate(v),
            Change::Context(v) => C::Context(v),
            Change::Pending(v) => C::Pending(v),
            Change::FanOut(v) => C::FanOut(v),
            Change::Inbox(v) => C::Inbox(v),
            Change::Interactions(v) => C::Interactions(v),
            Change::Totals(v) => C::Totals(v),
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
            Change::OpenCalls(v) => C::OpenCalls(v),
        }
    }
}

/// One step of a run's history, in layout 9's shape.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct StateDelta {
    pub(crate) seq: u64,
    pub(crate) at: i64,
    pub(crate) changes: Vec<Change>,
    pub(crate) events: Vec<RunEvent>,
}

impl From<StateDelta> for state::StateDelta {
    fn from(d: StateDelta) -> Self {
        Self {
            seq: d.seq,
            at: d.at,
            changes: d.changes.into_iter().map(Into::into).collect(),
            events: d.events.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
#[path = "layout9_tests.rs"]
mod tests;
