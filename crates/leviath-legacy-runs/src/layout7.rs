//! The state and delta frames of a run file in binary layout 7, which builds
//! before a stage record counted its resubmissions wrote.
//!
//! Layout 7 and this build's layout differ in one record: a stage's, which
//! this build gives a `resubmissions` count. The state holds the stage
//! records in its ledger, and a delta holds one in a ledger change, so both
//! are read here as layout 7 wrote them. Every other type is the runtime's
//! own, which did not change, and so is the spec: a layout-7 spec frame is
//! read as this build's.
//!
//! Layouts 2 to 7 kept the same stage record, so they read their states and
//! deltas through this module too. Layouts 2 and 3 also kept a fan-out in
//! progress in a shape of their own (see [`crate::layout3`]), which is why the
//! state and the change take the fan-out they hold as a parameter.
//!
//! No build counted resubmissions before, so an upgraded stage has none.

use std::collections::BTreeMap;

use leviath_runtime::spec::names::{ModelRef, RunId, StageName};
use leviath_runtime::state::{
    self as state, BlobFile, Clock, ContextDiff, ContextState, Cursor, FinalOutputState, Flags,
    Grants, MessageState, OpenInteraction, PendingBatch, PipelinePhase, PointProgress,
    ReadPathCounts, RunEvent, RunFiles, RunStatus, Spend, StageProgress, StageStatus, Totals,
    TransitionRecord, VisitRecord, WaitState,
};
use serde::Deserialize;

/// The fingerprint in the header of every layout-7 run file.
pub(crate) const FINGERPRINT: [u8; 32] = [
    0xbd, 0x94, 0xbd, 0x2a, 0x40, 0xcd, 0x42, 0xad, 0xbc, 0xf8, 0xbc, 0x88, 0x05, 0x69, 0x45, 0x3f,
    0xa2, 0x45, 0x47, 0x55, 0xf5, 0x5c, 0x05, 0x02, 0x84, 0x66, 0xac, 0x52, 0x7d, 0x62, 0x2d, 0x69,
];

/// One stage's record, with layouts 2 to 7's fields.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(crate) struct StageRecord {
    stage: StageName,
    status: StageStatus,
    entered: bool,
    spend: Spend,
    models: Vec<ModelRef>,
    visits: Vec<VisitRecord>,
    region_tokens: BTreeMap<String, u64>,
    first_call_prompt_tokens: Option<u64>,
    runaway_warned: bool,
    output_cap_raised: bool,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    clock: Clock,
}

impl From<StageRecord> for state::StageRecord {
    /// The same record, with no resubmissions counted.
    fn from(r: StageRecord) -> Self {
        Self {
            stage: r.stage,
            status: r.status,
            entered: r.entered,
            spend: r.spend,
            models: r.models,
            visits: r.visits,
            region_tokens: r.region_tokens,
            first_call_prompt_tokens: r.first_call_prompt_tokens,
            runaway_warned: r.runaway_warned,
            output_cap_raised: r.output_cap_raised,
            resubmissions: 0,
            started_at: r.started_at,
            ended_at: r.ended_at,
            clock: r.clock,
        }
    }
}

/// A run's state, with its fields in layouts 2 to 7's order, holding a
/// fan-out in progress as `F`.
#[derive(Debug, Deserialize)]
pub(crate) struct RunState<F = state::FanOutState> {
    seq: u64,
    status: RunStatus,
    cursor: Cursor,
    phase: PipelinePhase,
    accepts_messages: bool,
    visits: BTreeMap<StageName, u32>,
    progress: StageProgress,
    ledger: Vec<StageRecord>,
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

impl<F: Into<state::FanOutState>> From<RunState<F>> for state::RunState {
    fn from(s: RunState<F>) -> Self {
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
        }
    }
}

/// One changed field of a state, in layouts 2 to 7's order of variants,
/// holding a fan-out in progress as `F`.
#[derive(Debug, Deserialize)]
pub(crate) enum Change<F = state::FanOutState> {
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

impl<F: Into<state::FanOutState>> From<Change<F>> for state::Change {
    fn from(c: Change<F>) -> Self {
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
        }
    }
}

/// One step of a run's history, in layouts 2 to 7's shape, holding a fan-out
/// in progress as `F`.
#[derive(Debug, Deserialize)]
pub(crate) struct StateDelta<F = state::FanOutState> {
    seq: u64,
    at: i64,
    changes: Vec<Change<F>>,
    events: Vec<RunEvent>,
}

impl<F: Into<state::FanOutState>> From<StateDelta<F>> for state::StateDelta {
    fn from(d: StateDelta<F>) -> Self {
        Self {
            seq: d.seq,
            at: d.at,
            changes: d.changes.into_iter().map(Into::into).collect(),
            events: d.events,
        }
    }
}

#[cfg(test)]
#[path = "layout7_tests.rs"]
mod tests;
