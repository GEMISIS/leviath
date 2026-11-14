//! Why a run failed, in a form a caller can branch on.

use serde::{Deserialize, Serialize};

/// What ended a run in an error, beside the sentence that says so.
///
/// The sentence is for a person. A caller that wants to say a failure in its
/// own words, or to do something different about one, matches this instead:
/// the wording of a sentence is free to improve, and a label is not.
///
/// One variant per place a run can fail, not per provider error: a provider
/// call that failed is [`ProviderFailure`](Self::ProviderFailure) whatever the
/// provider said, and what the provider said is on the call's own record.
///
/// Carried as its [`label`](Self::label) wherever it is written down (the run
/// file, a run's record, the API), so a label this build does not know reads
/// as no kind at all rather than failing the read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RunFailureKind {
    /// A stage that makes an image, a video or audio was answered without any,
    /// as many times as a media model is asked.
    NoMedia,
    /// A stage that makes media of one type was answered with another.
    WrongMedia,
    /// The model's tool call was cut off by its output cap every time it was
    /// asked again.
    CutOffToolCall,
    /// A provider call failed and nothing was left to retry or reroute to.
    ProviderFailure,
    /// The prompt does not fit the model's context window.
    ContextOverflow,
    /// The run's working directory is gone.
    WorkspaceLost,
    /// The run stopped making progress and could not be moved.
    Wedged,
    /// The call that picks the next stage failed.
    RoutingFailed,
    /// The answer that picks the next stage named none of the choices.
    RoutingUnmatched,
    /// The stage the run was moving to could not be entered.
    StageEntryFailed,
    /// A fan-out stage that fails with any of its workers had one fail.
    WorkersFailed,
    /// A stage ran out of turns or was stopped for repeating itself, and the
    /// gate on its way out was not satisfied.
    GateUnsatisfied,
    /// A stage ended with no transition it could take.
    DeadEnd,
    /// The run reached its end without the final output it requires.
    FinalOutputMissing,
    /// A hook failed, or refused the run.
    Hook,
    /// A checkpoint that needed a person's answer got none.
    CheckpointUnanswered,
    /// The run's file could not be written.
    JournalUnwritable,
    /// The runtime itself failed while working on the run.
    InternalError,
    /// The run's file could not be read.
    RunFileUnreadable,
    /// The run was refused before it started.
    StartRefused,
}

impl RunFailureKind {
    /// Every kind, for the places that read one back from its label.
    pub const ALL: [RunFailureKind; 20] = [
        RunFailureKind::NoMedia,
        RunFailureKind::WrongMedia,
        RunFailureKind::CutOffToolCall,
        RunFailureKind::ProviderFailure,
        RunFailureKind::ContextOverflow,
        RunFailureKind::WorkspaceLost,
        RunFailureKind::Wedged,
        RunFailureKind::RoutingFailed,
        RunFailureKind::RoutingUnmatched,
        RunFailureKind::StageEntryFailed,
        RunFailureKind::WorkersFailed,
        RunFailureKind::GateUnsatisfied,
        RunFailureKind::DeadEnd,
        RunFailureKind::FinalOutputMissing,
        RunFailureKind::Hook,
        RunFailureKind::CheckpointUnanswered,
        RunFailureKind::JournalUnwritable,
        RunFailureKind::InternalError,
        RunFailureKind::RunFileUnreadable,
        RunFailureKind::StartRefused,
    ];

    /// The short, stable word for this kind: what the run file, a run's record
    /// and the API carry.
    pub fn label(self) -> &'static str {
        match self {
            RunFailureKind::NoMedia => "no-media",
            RunFailureKind::WrongMedia => "wrong-media",
            RunFailureKind::CutOffToolCall => "cut-off-tool-call",
            RunFailureKind::ProviderFailure => "provider-failure",
            RunFailureKind::ContextOverflow => "context-overflow",
            RunFailureKind::WorkspaceLost => "workspace-lost",
            RunFailureKind::Wedged => "wedged",
            RunFailureKind::RoutingFailed => "routing-failed",
            RunFailureKind::RoutingUnmatched => "routing-unmatched",
            RunFailureKind::StageEntryFailed => "stage-entry-failed",
            RunFailureKind::WorkersFailed => "workers-failed",
            RunFailureKind::GateUnsatisfied => "gate-unsatisfied",
            RunFailureKind::DeadEnd => "dead-end",
            RunFailureKind::FinalOutputMissing => "final-output-missing",
            RunFailureKind::Hook => "hook",
            RunFailureKind::CheckpointUnanswered => "checkpoint-unanswered",
            RunFailureKind::JournalUnwritable => "journal-unwritable",
            RunFailureKind::InternalError => "internal-error",
            RunFailureKind::RunFileUnreadable => "run-file-unreadable",
            RunFailureKind::StartRefused => "start-refused",
        }
    }

    /// The kind a [`label`](Self::label) came from, or `None` for a word this
    /// build does not know.
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.label() == label)
    }
}

impl std::fmt::Display for RunFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod tests;
