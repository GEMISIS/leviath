//! How a run's state changes, one step at a time.
//!
//! A [`StateDelta`] records the fields that changed between two states and
//! the events that happened in between. [`StateDelta::between`] computes one
//! from two states, and [`StateDelta::apply`] replays it, so any state in a
//! run's history is its starting state with its deltas applied in order.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::context::{ContextDiff, ToolCallState};
use super::journal::{
    ArtifactState, AttemptState, ContextCommitState, ContextNoteState, SettledState,
    ToolOutcomeState,
};
use super::*;
use crate::spec::names::{EdgeName, ModelRef};

/// One step of a run's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StateDelta {
    /// The state's `seq` after this delta is applied.
    pub seq: u64,
    /// When the step was recorded, in unix seconds.
    pub at: i64,
    /// The fields that changed.
    pub changes: Vec<Change>,
    /// What happened during the step that the state itself does not keep.
    pub events: Vec<RunEvent>,
}

/// One field of [`RunState`] that changed, with its new value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Change {
    /// `status`.
    Status(RunStatus),
    /// `cursor`.
    Cursor(Cursor),
    /// `phase`.
    Phase(PipelinePhase),
    /// `accepts_messages`.
    AcceptsMessages(bool),
    /// `visits`.
    Visits(BTreeMap<StageName, u32>),
    /// `progress`.
    Progress(StageProgress),
    /// `ledger`, one record at its index (appended when the index is the
    /// ledger's length).
    LedgerRecord(u32, StageRecord),
    /// `ledger`, cut to this many records.
    LedgerTruncate(u32),
    /// `context`.
    Context(ContextDiff),
    /// `pending`.
    Pending(Option<PendingBatch>),
    /// `fan_out`.
    FanOut(Option<FanOutState>),
    /// `inbox`.
    Inbox(Vec<MessageState>),
    /// `interactions`.
    Interactions(Vec<OpenInteraction>),
    /// `totals`.
    Totals(Totals),
    /// `clock`.
    Clock(Clock),
    /// `flags`.
    Flags(Flags),
    /// `children`.
    Children(Vec<RunId>),
    /// `title`.
    Title(Option<String>),
    /// `final_output`.
    FinalOutput(Option<FinalOutputState>),
    /// `wait_reason`.
    WaitReason(Option<WaitState>),
    /// `last_transition`.
    LastTransition(Option<TransitionRecord>),
}

/// An edge a run took, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TransitionRecord {
    /// The stage it left.
    pub from: StageName,
    /// The stage it entered.
    pub to: StageName,
    /// The edge. `None` when no declared edge was taken (a forced move).
    pub edge: Option<EdgeName>,
    /// Why this edge.
    pub reason: TransitionReason,
    /// The visit of `to` it started.
    pub visit: String,
}

/// Why a run took an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum TransitionReason {
    /// The edge's condition held.
    Condition,
    /// A gate let the run through after holding it back.
    Gate,
    /// The model picked it.
    ModelChoice,
    /// A person or the host moved the run.
    Forced,
    /// A fan-out worker started in its stage.
    Worker,
    /// Code picked it.
    Router,
}

/// Something that happened during a step that the state does not keep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum RunEvent {
    /// A model request finished.
    Inference {
        /// The attempt's id.
        attempt: String,
        /// The model that answered.
        model: ModelRef,
        /// Its spend.
        spend: Spend,
        /// Why the model stopped, as the provider said it.
        finish_reason: Option<String>,
    },
    /// A request moved to another model.
    Failover {
        /// The model that failed.
        from: ModelRef,
        /// The model tried next.
        to: ModelRef,
        /// Why.
        reason: String,
    },
    /// A tool call started.
    ToolStarted(ToolCallState),
    /// A tool call finished.
    ToolFinished {
        /// The call.
        call_id: String,
        /// Its result.
        result: ToolResultState,
        /// How long it took, in milliseconds.
        millis: u64,
    },
    /// A person answered a question.
    Answered {
        /// The question's id.
        id: String,
        /// The answer.
        answer: String,
    },
    /// A message reached the run.
    Message(MessageState),
    /// A line for the run's log.
    Log(String),
    /// A model call, in full: how it ended, how long it took, what was sent,
    /// and the request itself when it was captured. Recorded for every call,
    /// beside the `Inference` that carries a successful one's spend.
    Attempt(Box<AttemptState>),
    /// A tool call was dispatched as one execution.
    Dispatched {
        /// The provider's id for the call.
        call_id: String,
        /// The execution's own id.
        execution_id: String,
        /// The model call that asked for it.
        requested_by: String,
    },
    /// An execution ended: how, and the stored parts its result carried.
    /// Recorded beside the `ToolFinished` that carries the result.
    Completed {
        /// The provider's id for the call.
        call_id: String,
        /// The execution's own id.
        execution_id: String,
        /// How it ended, when that was observed.
        outcome: Option<ToolOutcomeState>,
        /// The names of the stored parts its result carried.
        parts: Vec<String>,
    },
    /// An execution produced files.
    Artifacts {
        /// The execution.
        execution_id: String,
        /// The files.
        artifacts: Vec<ArtifactState>,
    },
    /// A question was settled, recorded whole beside the `Answered` that
    /// carries its answer.
    Settled(Box<SettledState>),
    /// The context changed, with its cause and the revisions on either side.
    ContextCommitted(Box<ContextCommitState>),
    /// One region changed, with its cause.
    ContextNoted(ContextNoteState),
}

macro_rules! diff_fields {
    ($prev:ident, $next:ident, $changes:ident; $($field:ident => $variant:ident),* $(,)?) => {
        $(
            if $prev.$field != $next.$field {
                $changes.push(Change::$variant($next.$field.clone()));
            }
        )*
    };
}

impl StateDelta {
    /// The delta from `prev` to `next`, stamped `at`, carrying `events`.
    pub fn between(prev: &RunState, next: &RunState, at: i64, events: Vec<RunEvent>) -> Self {
        let mut changes = Vec::new();
        diff_fields!(prev, next, changes;
            status => Status,
            cursor => Cursor,
            phase => Phase,
            accepts_messages => AcceptsMessages,
            visits => Visits,
            progress => Progress,
        );
        for (i, record) in next.ledger.iter().enumerate() {
            if prev.ledger.get(i) != Some(record) {
                changes.push(Change::LedgerRecord(i as u32, record.clone()));
            }
        }
        if next.ledger.len() < prev.ledger.len() {
            changes.push(Change::LedgerTruncate(next.ledger.len() as u32));
        }
        let context = ContextDiff::between(&prev.context, &next.context);
        if !context.is_empty() {
            changes.push(Change::Context(context));
        }
        diff_fields!(prev, next, changes;
            pending => Pending,
            fan_out => FanOut,
            inbox => Inbox,
            interactions => Interactions,
            totals => Totals,
            clock => Clock,
            flags => Flags,
            children => Children,
            title => Title,
            final_output => FinalOutput,
            wait_reason => WaitReason,
            last_transition => LastTransition,
        );
        Self {
            seq: prev.seq + 1,
            at,
            changes,
            events,
        }
    }

    /// Whether the step changed nothing and recorded nothing.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.events.is_empty()
    }

    /// Replay this delta onto the state it was taken from.
    pub fn apply(&self, state: &mut RunState) {
        for change in &self.changes {
            match change.clone() {
                Change::Status(v) => state.status = v,
                Change::Cursor(v) => state.cursor = v,
                Change::Phase(v) => state.phase = v,
                Change::AcceptsMessages(v) => state.accepts_messages = v,
                Change::Visits(v) => state.visits = v,
                Change::Progress(v) => state.progress = v,
                Change::LedgerRecord(i, record) => match state.ledger.get_mut(i as usize) {
                    Some(slot) => *slot = record,
                    None => state.ledger.push(record),
                },
                Change::LedgerTruncate(len) => state.ledger.truncate(len as usize),
                Change::Context(diff) => diff.apply(&mut state.context),
                Change::Pending(v) => state.pending = v,
                Change::FanOut(v) => state.fan_out = v,
                Change::Inbox(v) => state.inbox = v,
                Change::Interactions(v) => state.interactions = v,
                Change::Totals(v) => state.totals = v,
                Change::Clock(v) => state.clock = v,
                Change::Flags(v) => state.flags = v,
                Change::Children(v) => state.children = v,
                Change::Title(v) => state.title = v,
                Change::FinalOutput(v) => state.final_output = v,
                Change::WaitReason(v) => state.wait_reason = v,
                Change::LastTransition(v) => state.last_transition = v,
            }
        }
        state.seq = self.seq;
    }
}
