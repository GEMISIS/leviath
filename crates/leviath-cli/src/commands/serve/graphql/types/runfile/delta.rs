//! `StateDelta`: one step of a run, as its run file records it.
//!
//! A step is the parts of the state that changed, each carrying its new value,
//! and what happened on the way that the state alone does not show: a model
//! call, a tool finishing, a message. Applying every step from the start in
//! order gives the state at any point, which is what `Run.state(at:)` does.

use async_graphql::{ID, SimpleObject, Union};
use leviath_graphql_derive::mirror;
use leviath_runtime::state::journal::CallKind;
use leviath_runtime::state::{Change, RunEvent as CoreEvent, StateDelta as CoreDelta};

use super::super::super::scalars::{BigInt, Timestamp};
use super::big;
use super::context::{ContextDiff, StateToolCall};
use super::journal::{
    ArtifactsStep, AttemptStep, CompletedStep, ContextCommittedStep, ContextNotedStep,
    DispatchedStep, ProducedFile, SettledStep, outcome_of,
};
use super::state::{
    CheckpointProgress, FanOutProgress, LedgerStage, OpenQuestion, PendingToolBatch, RunPhase,
    RunStateStatus, StageProgress, StageTransition, StageVisitCount, StateAnswer, StateClock,
    StateCursor, StateFlags, StateMessage, StateSpend, StateTotals, status_of, visit_counts,
};

/// Write one change member: a single-field object named for what changed.
macro_rules! change_member {
    ($(#[$doc:meta])* $name:ident { $(#[$fdoc:meta])* $field:ident: $ty:ty }) => {
        $(#[$doc])*
        #[mirror(no_filter)]
        #[derive(Debug, SimpleObject)]
        pub(crate) struct $name {
            $(#[$fdoc])*
            pub(crate) $field: $ty,
        }
    };
}

/// The new status, and the error a failed run carries.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StatusChange {
    /// Where the run is in its life now.
    pub(crate) status: RunStateStatus,
    /// What went wrong, when `status` is `ERROR`.
    pub(crate) error: Option<String>,
}

change_member!(
    /// The run's stage, visit or turn moved.
    CursorChange {
        /// Where it is now.
        cursor: StateCursor
    }
);
change_member!(
    /// The engine moved the run to another phase.
    PhaseChange {
        /// The phase now.
        phase: RunPhase
    }
);
change_member!(
    /// The run started or stopped taking messages.
    AcceptsMessagesChange {
        /// Whether it takes them now.
        accepts_messages: bool
    }
);
change_member!(
    /// A stage was entered again.
    VisitsChange {
        /// Every stage's count now.
        visits: Vec<StageVisitCount>
    }
);
change_member!(
    /// The current stage's counters moved.
    ProgressChange {
        /// The counters now.
        progress: StageProgress
    }
);

/// One line of the ledger was written.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct LedgerRecordChange {
    /// The line's position, from 0.
    pub(crate) index: i32,
    /// The line now.
    pub(crate) record: LedgerStage,
}

change_member!(
    /// The ledger was cut short.
    LedgerTruncateChange {
        /// How many lines it keeps.
        length: i32
    }
);
change_member!(
    /// The context window changed.
    ContextDiffChange {
        /// What changed in it.
        diff: ContextDiff
    }
);
change_member!(
    /// A batch of tool calls went out, a result came in, or the batch ended.
    PendingChange {
        /// The batch now. Null once it ended.
        pending: Option<PendingToolBatch>
    }
);
change_member!(
    /// A fan-out started, moved or ended.
    FanOutChange {
        /// The fan-out now. Null once it ended.
        fan_out: Option<FanOutProgress>
    }
);
change_member!(
    /// A message arrived or was delivered.
    InboxChange {
        /// The waiting messages now.
        inbox: Vec<StateMessage>
    }
);
change_member!(
    /// A question opened or was answered.
    QuestionsChange {
        /// The open questions now.
        questions: Vec<OpenQuestion>
    }
);
change_member!(
    /// The run spent something or called a tool.
    TotalsChange {
        /// The totals now.
        totals: StateTotals
    }
);
change_member!(
    /// The working clock started, stopped or banked time.
    ClockChange {
        /// The clock now.
        clock: StateClock
    }
);
change_member!(
    /// The run noticed something about itself.
    FlagsChange {
        /// The flags now.
        flags: StateFlags
    }
);
change_member!(
    /// The run started a child run.
    ChildrenChange {
        /// Its children now.
        children: Vec<ID>
    }
);
change_member!(
    /// The run was titled or renamed.
    TitleChange {
        /// The title now.
        title: Option<String>
    }
);
change_member!(
    /// The run was held because this machine could not take it back, or it
    /// came back.
    HeldChange {
        /// What it is held for now. Null when it is no longer held.
        held: Option<Vec<crate::commands::serve::graphql::mutation::spawn::SpawnIssue>>
    }
);
change_member!(
    /// Titling the run gave up, or a title came after all.
    TitleErrorChange {
        /// Why the run has no title now. Null when nothing went wrong.
        title_error: Option<String>
    }
);
/// The run's read-path grants were fixed.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ReadPathsChange {
    /// How many `read_paths` its blueprint declares. Null when it declares
    /// none.
    pub(crate) declared: Option<i32>,
    /// How many of them this machine grants. Null when it declares none.
    pub(crate) granted: Option<i32>,
}
change_member!(
    /// The run submitted its answer.
    AnswerChange {
        /// The answer now.
        answer: Option<StateAnswer>
    }
);
change_member!(
    /// The run started or stopped waiting.
    WaitReasonChange {
        /// Why it waits now. Null when it no longer does.
        wait_reason: Option<String>
    }
);
change_member!(
    /// The run moved between stages.
    TransitionChange {
        /// The move.
        transition: Option<StageTransition>
    }
);

change_member!(
    /// The run moved among its stage's checkpoints, or put one to a person.
    CheckpointChange {
        /// Where it is among them now.
        checkpoint: CheckpointProgress
    }
);

change_member!(
    /// The run's answer, logs or audits beside its run file changed.
    FilesChange {
        /// The files it names now.
        files: super::files::StateFiles
    }
);
change_member!(
    /// The run stored a part beside its run file.
    BlobsChange {
        /// Every stored part it names now.
        blobs: Vec<super::files::StateBlob>
    }
);

change_member!(
    /// A person approved a call for the run or its stage, or a stage grant
    /// ended.
    GrantsChange {
        /// The grants now.
        grants: super::state::StateGrants
    }
);
change_member!(
    /// The run wrote something.
    WrittenChange {
        /// The bytes it has written now.
        written_bytes: BigInt
    }
);

/// One part of a run's state that a step changed, with its new value.
#[derive(Debug, Union)]
pub(crate) enum StateChange {
    /// The status.
    Status(StatusChange),
    /// The stage, visit or turn.
    Cursor(CursorChange),
    /// The engine phase.
    Phase(PhaseChange),
    /// Whether messages are taken.
    AcceptsMessages(AcceptsMessagesChange),
    /// The visit counts.
    Visits(VisitsChange),
    /// The current stage's counters.
    Progress(ProgressChange),
    /// One ledger line.
    LedgerRecord(LedgerRecordChange),
    /// The ledger's length.
    LedgerTruncate(LedgerTruncateChange),
    /// The context window.
    Context(ContextDiffChange),
    /// The pending tool batch.
    Pending(PendingChange),
    /// The fan-out.
    FanOut(FanOutChange),
    /// The inbox.
    Inbox(InboxChange),
    /// The open questions.
    Questions(QuestionsChange),
    /// The totals.
    Totals(TotalsChange),
    /// The working clock.
    Clock(ClockChange),
    /// The flags.
    Flags(FlagsChange),
    /// The children.
    Children(ChildrenChange),
    /// The title.
    Title(TitleChange),
    /// The answer.
    Answer(AnswerChange),
    /// The wait reason.
    WaitReason(WaitReasonChange),
    /// The last move between stages.
    Transition(TransitionChange),
    /// Why the run has no title.
    TitleError(TitleErrorChange),
    /// The run's read-path grants.
    ReadPaths(ReadPathsChange),
    /// Where the run is among its stage's checkpoints.
    Checkpoint(CheckpointChange),
    /// Whether the run is held, and for what.
    Held(HeldChange),
    /// The files beside the run file.
    Files(FilesChange),
    /// The stored parts beside the run file.
    Blobs(BlobsChange),
    /// What a person approved beyond the calls they were asked about.
    Grants(GrantsChange),
    /// What the run has written.
    Written(WrittenChange),
}

impl From<&Change> for StateChange {
    fn from(change: &Change) -> Self {
        match change {
            Change::Status(status) => {
                let (status, error) = status_of(status);
                Self::Status(StatusChange { status, error })
            }
            Change::Cursor(cursor) => Self::Cursor(CursorChange {
                cursor: StateCursor::from(cursor),
            }),
            Change::Phase(phase) => Self::Phase(PhaseChange {
                phase: RunPhase::from(phase),
            }),
            Change::AcceptsMessages(accepts) => Self::AcceptsMessages(AcceptsMessagesChange {
                accepts_messages: *accepts,
            }),
            Change::Visits(visits) => Self::Visits(VisitsChange {
                visits: visit_counts(visits),
            }),
            Change::Progress(progress) => Self::Progress(ProgressChange {
                progress: StageProgress::from(progress),
            }),
            Change::LedgerRecord(index, record) => Self::LedgerRecord(LedgerRecordChange {
                index: super::saturating(*index),
                record: LedgerStage::from(record),
            }),
            Change::LedgerTruncate(length) => Self::LedgerTruncate(LedgerTruncateChange {
                length: super::saturating(*length),
            }),
            Change::Context(diff) => Self::Context(ContextDiffChange {
                diff: ContextDiff::from(diff),
            }),
            Change::Pending(pending) => Self::Pending(PendingChange {
                pending: pending.as_ref().map(PendingToolBatch::from),
            }),
            Change::FanOut(fan_out) => Self::FanOut(FanOutChange {
                fan_out: fan_out.as_ref().map(FanOutProgress::from),
            }),
            Change::Inbox(inbox) => Self::Inbox(InboxChange {
                inbox: inbox.iter().map(StateMessage::from).collect(),
            }),
            Change::Interactions(open) => Self::Questions(QuestionsChange {
                questions: open.iter().map(OpenQuestion::from).collect(),
            }),
            Change::Totals(totals) => Self::Totals(TotalsChange {
                totals: StateTotals::from(totals),
            }),
            Change::Clock(clock) => Self::Clock(ClockChange {
                clock: StateClock::from(clock),
            }),
            Change::Flags(flags) => Self::Flags(FlagsChange {
                flags: StateFlags::from(flags),
            }),
            Change::Children(children) => Self::Children(ChildrenChange {
                children: children.iter().map(|id| ID(id.to_string())).collect(),
            }),
            Change::Title(title) => Self::Title(TitleChange {
                title: title.clone(),
            }),
            Change::FinalOutput(answer) => Self::Answer(AnswerChange {
                answer: answer.as_ref().map(StateAnswer::from),
            }),
            Change::WaitReason(reason) => Self::WaitReason(WaitReasonChange {
                wait_reason: reason
                    .as_ref()
                    .map(|w| leviath_core::run_meta::WaitReason::from(w).to_string()),
            }),
            Change::LastTransition(transition) => Self::Transition(TransitionChange {
                transition: transition.as_ref().map(StageTransition::from),
            }),
            Change::TitleError(error) => Self::TitleError(TitleErrorChange {
                title_error: error.clone(),
            }),
            Change::ReadPaths(counts) => Self::ReadPaths(ReadPathsChange {
                declared: counts.map(|c| super::saturating(c.declared)),
                granted: counts.map(|c| super::saturating(c.granted)),
            }),
            Change::Point(point) => Self::Checkpoint(CheckpointChange {
                checkpoint: CheckpointProgress::from(point),
            }),
            Change::Held(held) => Self::Held(HeldChange {
                held: super::state::held_issues(held.as_ref()),
            }),
            Change::Files(files) => Self::Files(FilesChange {
                files: super::files::StateFiles::from(files),
            }),
            Change::Blobs(blobs) => Self::Blobs(BlobsChange {
                blobs: blobs.iter().map(super::files::StateBlob::from).collect(),
            }),
            Change::Grants(grants) => Self::Grants(GrantsChange {
                grants: super::state::StateGrants::from(grants),
            }),
            Change::Written(written) => Self::Written(WrittenChange {
                written_bytes: big(*written),
            }),
        }
    }
}

/// Which kind of model call an inference was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum InferenceCallKind {
    /// A turn of the stage's own work.
    Stage,
    /// A summary of a region.
    Compaction,
    /// The call that named the run.
    Title,
    /// A call asking the model which edge to take.
    Routing,
}

impl From<CallKind> for InferenceCallKind {
    fn from(kind: CallKind) -> Self {
        match kind {
            CallKind::Stage => Self::Stage,
            CallKind::Compaction => Self::Compaction,
            CallKind::Title => Self::Title,
            CallKind::Routing => Self::Routing,
        }
    }
}

/// A model call finished and was billed. One per call.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct InferenceStep {
    /// The id of the attempt that answered. Empty when none was recorded.
    pub(crate) attempt: String,
    /// The model that answered, as `provider/model`.
    pub(crate) model: String,
    /// What it cost.
    pub(crate) spend: StateSpend,
    /// Why the model stopped, in the provider's words.
    pub(crate) finish_reason: Option<String>,
    /// Which kind of call it was.
    pub(crate) kind: InferenceCallKind,
    /// The stage it was made for. Null for the title call, which no stage
    /// owns.
    pub(crate) stage: Option<String>,
    /// The iteration it was made in.
    pub(crate) iteration: i32,
}

/// The run took an edge. One per edge taken, so a step that moved the run
/// twice lists both moves.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct TransitionStep {
    /// The move.
    pub(crate) transition: StageTransition,
}

/// A model call moved to a fallback model.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FailoverStep {
    /// The model that failed.
    pub(crate) from_model: String,
    /// The model tried next.
    pub(crate) to_model: String,
    /// Why.
    pub(crate) reason: String,
}

/// A tool call started.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolStartedStep {
    /// The call.
    pub(crate) call: StateToolCall,
}

/// A tool call finished.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolFinishedStep {
    /// The provider's id for the call.
    pub(crate) call_id: String,
    /// The result, as the model reads it.
    pub(crate) text: String,
    /// Whether the call failed.
    pub(crate) is_error: bool,
    /// How long it took.
    pub(crate) millis: BigInt,
}

/// A question was answered.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct AnsweredStep {
    /// The question.
    pub(crate) question_id: ID,
    /// The answer.
    pub(crate) answer: String,
}

/// A message was delivered.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct MessageStep {
    /// The message.
    pub(crate) message: StateMessage,
}

/// A line the run logged.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct LogStep {
    /// The line.
    pub(crate) line: String,
}

/// Something that happened during a step that the state alone does not show.
#[derive(Debug, Union)]
pub(crate) enum StepEvent {
    /// A model call finished.
    Inference(InferenceStep),
    /// A model call moved to a fallback.
    Failover(FailoverStep),
    /// A tool call started.
    ToolStarted(ToolStartedStep),
    /// A tool call finished.
    ToolFinished(ToolFinishedStep),
    /// A question was answered.
    Answered(AnsweredStep),
    /// A message was delivered.
    Message(MessageStep),
    /// A line was logged.
    Log(LogStep),
    /// A model call, in full.
    Attempt(Box<AttemptStep>),
    /// A tool call was dispatched as one execution.
    Dispatched(DispatchedStep),
    /// An execution ended.
    Completed(CompletedStep),
    /// An execution produced files.
    Artifacts(ArtifactsStep),
    /// A question was settled, whole.
    Settled(SettledStep),
    /// The context changed, with its cause and revisions.
    ContextCommitted(ContextCommittedStep),
    /// One region changed, with its cause.
    ContextNoted(ContextNotedStep),
    /// The run took an edge.
    Transition(TransitionStep),
}

impl From<&CoreEvent> for StepEvent {
    fn from(event: &CoreEvent) -> Self {
        match event {
            CoreEvent::Inference {
                attempt,
                model,
                spend,
                finish_reason,
                kind,
                stage,
                iteration,
            } => Self::Inference(InferenceStep {
                attempt: attempt.clone(),
                model: model.to_string(),
                spend: StateSpend::from(spend),
                finish_reason: finish_reason.clone(),
                kind: InferenceCallKind::from(*kind),
                stage: stage.as_ref().map(ToString::to_string),
                iteration: super::saturating(*iteration),
            }),
            CoreEvent::Transition(taken) => Self::Transition(TransitionStep {
                transition: StageTransition::from(taken),
            }),
            CoreEvent::Failover { from, to, reason } => Self::Failover(FailoverStep {
                from_model: from.to_string(),
                to_model: to.to_string(),
                reason: reason.clone(),
            }),
            CoreEvent::ToolStarted(call) => Self::ToolStarted(ToolStartedStep {
                call: StateToolCall::from(call),
            }),
            CoreEvent::ToolFinished {
                call_id,
                result,
                millis,
            } => Self::ToolFinished(ToolFinishedStep {
                call_id: call_id.clone(),
                text: result.text.clone(),
                is_error: result.is_error,
                millis: big(*millis),
            }),
            CoreEvent::Answered { id, answer } => Self::Answered(AnsweredStep {
                question_id: ID(id.clone()),
                answer: answer.clone(),
            }),
            CoreEvent::Message(message) => Self::Message(MessageStep {
                message: StateMessage::from(message),
            }),
            CoreEvent::Log(line) => Self::Log(LogStep { line: line.clone() }),
            CoreEvent::Attempt(a) => Self::Attempt(Box::new(AttemptStep::from(&**a))),
            CoreEvent::Dispatched {
                call_id,
                execution_id,
                requested_by,
            } => Self::Dispatched(DispatchedStep {
                call_id: call_id.clone(),
                execution_id: ID(execution_id.clone()),
                requested_by: requested_by.clone(),
            }),
            CoreEvent::Completed {
                call_id,
                execution_id,
                outcome,
                parts,
            } => Self::Completed(CompletedStep {
                call_id: call_id.clone(),
                execution_id: ID(execution_id.clone()),
                outcome: outcome.map(outcome_of),
                parts: parts.clone(),
            }),
            CoreEvent::Artifacts {
                execution_id,
                artifacts,
            } => Self::Artifacts(ArtifactsStep {
                producer_id: ID(execution_id.clone()),
                files: artifacts
                    .iter()
                    .map(|a| ProducedFile {
                        name: a.name.clone(),
                        path: a.path.clone(),
                        mime_type: a.mime_type.clone(),
                        size: big(a.size),
                        sha256: a.sha256.clone(),
                    })
                    .collect(),
            }),
            CoreEvent::Settled(settled) => Self::Settled(SettledStep::from(&**settled)),
            CoreEvent::ContextCommitted(commit) => {
                Self::ContextCommitted(ContextCommittedStep::from(&**commit))
            }
            CoreEvent::ContextNoted(note) => Self::ContextNoted(ContextNotedStep::from(note)),
        }
    }
}

/// One step of a run: what changed, and what happened.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateDelta {
    /// The step. The first step a run takes is 1.
    pub(crate) seq: i32,
    /// When it was taken.
    pub(crate) at: Timestamp,
    /// The parts of the state it changed, each with its new value.
    pub(crate) changes: Vec<StateChange>,
    /// What happened during it.
    pub(crate) events: Vec<StepEvent>,
}

impl From<&CoreDelta> for StateDelta {
    fn from(d: &CoreDelta) -> Self {
        Self {
            seq: i32::try_from(d.seq).unwrap_or(i32::MAX),
            at: Timestamp(d.at),
            changes: d.changes.iter().map(StateChange::from).collect(),
            events: d.events.iter().map(StepEvent::from).collect(),
        }
    }
}
