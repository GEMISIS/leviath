//! A run file read as a history: the window at each step, every tool
//! execution, and every change to the window with its cause.
//!
//! These are what a reader makes of the steps, not what the file stores. The
//! CLI's history, executions and context-change readers build them by walking
//! a run's deltas, and the API, the dashboard and `lev context` show them.

use serde::{Deserialize, Serialize};

use leviath_core::ContextCause;
use leviath_core::execution::ToolOutcome;
use leviath_core::run_meta::{ContextSnapshot, RunMeta};

use super::record::RegionCommit;

/// A run's context window at one step, with the run's summary as it stood
/// then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunPoint {
    /// The run's summary at this point.
    pub meta: RunMeta,
    /// The full context window at this point.
    pub context: ContextSnapshot,
    /// Unix seconds this point was recorded.
    pub at: i64,
}

/// One attempt to execute one tool call.
///
/// An attempt, not a call: the same call reissued after a failure is a second
/// execution with its own id, and telling them apart is the point.
#[derive(Debug)]
pub struct Execution {
    /// The execution id minted at dispatch. The provider's call id where the
    /// run recorded none.
    pub id: String,
    /// The provider's own call id. Correlation only: a provider may reuse one.
    pub call_id: String,
    /// The tool name, as the model called it.
    pub tool: String,
    /// The arguments as the model sent them, verbatim. Kept as text because that
    /// is what was recorded, and because a model may send something the tool's
    /// own schema would refuse.
    pub arguments: String,
    /// The stage the batch was dispatched in.
    pub stage_index: usize,
    /// The stage-local iteration that produced the batch.
    pub iteration: usize,
    /// The stay in that stage it was dispatched during. Empty where the run
    /// recorded no visit.
    pub visit_id: String,
    /// The provider attempt whose answer asked for it. Empty where the run
    /// recorded no attempt.
    pub requested_by: String,
    /// The files it produced. Empty for every execution that produced none.
    pub artifacts: Vec<leviath_core::output::Artifact>,
    /// When it was dispatched, in unix seconds.
    pub dispatched_at: i64,
    /// The step that dispatched it.
    pub position: u64,
    /// When it ended, in unix seconds. `None` while the call is still running,
    /// and also on a run that died before it learned how it ended.
    pub ended_at: Option<i64>,
    /// The step its result came back in, for a caller that wants the bytes.
    pub result_position: Option<u64>,
    /// How it ended, where the run says so. `None` covers three different
    /// situations, which a reader must not flatten: still running, ended with
    /// no outcome recorded, or ended in a way only the result text describes.
    pub outcome: Option<ToolOutcome>,
}

impl Execution {
    /// Whether this attempt is still, as far as the run file knows, in flight.
    ///
    /// True of a call that is genuinely running, and of one whose daemon died
    /// without recording anything. A resume records the second as indeterminate,
    /// so a run nobody is resuming is where this stays true forever.
    pub fn unfinished(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// One region's part in a committed transaction, as a reader gets it.
///
/// The fields a [`RegionCommit`] carries, plus the two a reader would
/// otherwise compute for itself. The `Option`s are what a change noted one
/// region at a time does not carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionTransition {
    /// The region this part of the transaction touched.
    pub region: String,
    /// The digest of its contents before the change. `None` when the change
    /// carries no digest.
    pub digest_before: Option<String>,
    /// The digest of its contents afterwards. `None` when the change carries
    /// no digest.
    pub digest_after: Option<String>,
    /// What it held before, in tokens. `None` when the change carries only the
    /// delta.
    pub tokens_before: Option<usize>,
    /// What it held afterwards. `None` on the same changes.
    pub tokens_after: Option<usize>,
    /// How its token count moved; negative when it shrank.
    pub token_delta: i64,
    /// How many entries it held before. `None` when the change carries only
    /// the counts it moved.
    pub entries_before: Option<usize>,
    /// How many it held afterwards. `None` on the same changes.
    pub entries_after: Option<usize>,
    /// How many entries the change itself pushed.
    pub entries_added: usize,
    /// How many left, the eviction the change triggered included.
    pub entries_removed: usize,
}

impl From<RegionCommit> for RegionTransition {
    fn from(commit: RegionCommit) -> Self {
        Self {
            region: commit.region,
            digest_before: Some(commit.digest_before),
            digest_after: Some(commit.digest_after),
            tokens_before: Some(commit.tokens_before),
            tokens_after: Some(commit.tokens_after),
            token_delta: commit.tokens_after as i64 - commit.tokens_before as i64,
            entries_before: Some(commit.entries_before),
            entries_after: Some(commit.entries_after),
            entries_added: commit.entries_added,
            // What the region lost: everything it held plus everything the
            // change pushed, less what it ended with. Saturating, so a record
            // whose arithmetic does not close reports no removal rather than an
            // enormous one.
            entries_removed: (commit.entries_before + commit.entries_added)
                .saturating_sub(commit.entries_after),
        }
    }
}

/// One committed change to a run's context window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextChangeRecord {
    /// What made the change.
    pub cause: ContextCause,
    /// The window's revision before the transaction. `None` when the change
    /// names no revision.
    pub revision_before: Option<String>,
    /// The window's revision after it. `None` on the same changes.
    pub revision_after: Option<String>,
    /// The tool execution that committed it, where the runtime knew one. `None`
    /// where nothing recorded an execution.
    pub execution_id: Option<String>,
    /// Every region the transaction touched.
    pub regions: Vec<RegionTransition>,
    /// Unix seconds when it committed.
    pub at: i64,
}

/// One change with the step it landed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedChange {
    /// The step's sequence number. It only climbs within a run and never
    /// changes.
    pub position: u64,
    /// The change itself.
    pub record: ContextChangeRecord,
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
