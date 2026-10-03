//! The old journal: an LVR1 `run.lvr`, read and folded.
//!
//! An old run kept its history as an append-only list of JSON records: a
//! header with the run's metadata, then every model call, tool batch, answer,
//! status change and context-window snapshot or diff, in order. The records
//! the running world still sends (model calls, attempts, failovers, tool
//! batches, answers, window transactions) are the payload types of
//! [`leviath_runtime::runfile::record`]; the ones that only ever lived in an
//! old journal are here.
//!
//! [`read`] reads the file, and [`fold`] walks the records to what the
//! conversion needs from them as a whole: the metadata the run was last
//! written with, its context window then, and a tool batch it was still
//! running.

use serde::{Deserialize, Serialize};

use leviath_core::region::EntryContent;
use leviath_core::run_meta::{
    ContextSnapshot, RegionEntrySnapshot, RegionSnapshot, RunMeta, RunStatus,
};
use leviath_runtime::runfile::record::{
    AttemptRecord, FailoverRecord, InferenceKind, RegionCommit, ToolCallRecord,
};

mod read;

pub use read::{MAGIC, read};

/// Who owned the run when its journal was started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    /// The run's id.
    pub run_id: String,
    /// The machine that owned it.
    pub machine_id: String,
    /// The world, or daemon instance, that owned it.
    pub world_id: String,
    /// Unix seconds when the journal was created.
    pub created_at: i64,
}

/// A conversation message as an old journal recorded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageRecord {
    /// `"user"`, `"assistant"`, `"tool"` or `"system"`.
    pub role: String,
    /// The message text.
    pub content: String,
}

/// What one model call sent, as an old journal recorded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceRequestRecord {
    /// The model the request targeted.
    pub model: String,
    /// System-block texts, in order.
    pub system: Vec<String>,
    /// The conversation messages sent.
    pub messages: Vec<MessageRecord>,
    /// The tool names offered to the model.
    pub tool_names: Vec<String>,
    /// The temperature used.
    pub temperature: f32,
    /// The max output tokens requested.
    pub max_tokens: usize,
}

/// What one model call answered, as an old journal recorded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferenceResponseRecord {
    /// The assistant's text.
    pub content: String,
    /// Any tool calls the model requested.
    pub tool_calls: Vec<ToolCallRecord>,
    /// Prompt tokens billed.
    pub prompt_tokens: usize,
    /// Completion tokens billed.
    pub completion_tokens: usize,
    /// Tokens read from provider cache.
    pub cached_tokens: usize,
    /// Tokens written to provider cache.
    pub cache_write_tokens: usize,
}

/// One region's change within a [`ContextDelta`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RegionDelta {
    /// A new region, or one rewritten other than by appending, in full.
    Set(RegionSnapshot),
    /// Entries appended to an existing region.
    Append {
        /// The region name.
        name: String,
        /// The entries appended after the ones already there.
        entries: Vec<RegionEntrySnapshot>,
        /// The region's new token count.
        current_tokens: usize,
    },
    /// An existing region emptied of entries.
    Clear {
        /// The region name.
        name: String,
    },
    /// A region that went away.
    Remove {
        /// The region name.
        name: String,
    },
}

/// The change to a context window since the snapshot before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextDelta {
    /// The window's stage name at this point.
    pub stage_name: String,
    /// The window's total token count at this point.
    pub total_tokens: usize,
    /// The window's max token budget at this point.
    pub max_tokens: usize,
    /// Per-region changes.
    pub regions: Vec<RegionDelta>,
}

/// One record of an old journal, by the name its JSON carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JournalRecord {
    /// The run's identity and metadata. Always the first record.
    Header {
        /// Who owned the run.
        identity: RunIdentity,
        /// The run's metadata when the journal was started.
        meta: Box<RunMeta>,
    },
    /// The run moved to another machine or world.
    OwnershipChanged {
        /// The new owning machine.
        machine_id: String,
        /// The new owning world.
        world_id: String,
        /// Unix seconds.
        at: i64,
    },
    /// One model call, request and answer.
    Inference {
        /// The stage the run was in.
        stage: String,
        /// The stage-local iteration.
        iteration: usize,
        /// What was sent.
        request: InferenceRequestRecord,
        /// What came back.
        response: InferenceResponseRecord,
        /// Unix seconds.
        at: i64,
    },
    /// What one model call cost.
    InferenceUsage {
        /// Which kind of call it was; stage work when the record names none.
        #[serde(default)]
        kind: InferenceKind,
        /// The stage the run was in, empty for the title call.
        stage: String,
        /// The stage-local iteration.
        iteration: usize,
        /// The provider that served the call.
        provider: String,
        /// The model the call targeted.
        model: String,
        /// Prompt tokens billed.
        prompt_tokens: usize,
        /// Completion tokens billed.
        completion_tokens: usize,
        /// Tokens read from provider cache.
        cached_tokens: usize,
        /// Tokens written to provider cache.
        cache_write_tokens: usize,
        /// What the call cost in USD, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        /// Whether the cost is the provider's own figure.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_reported_by_provider: Option<bool>,
        /// Unix seconds.
        at: i64,
    },
    /// A batch of tool calls, written when it was dispatched.
    ToolBatch {
        /// The calls, with the results the dispatcher settled inline.
        calls: Vec<ToolCallRecord>,
        /// Unix seconds.
        at: i64,
        /// The stage index the batch was dispatched in.
        #[serde(default)]
        stage_index: usize,
        /// The stage-local iteration that produced the batch.
        #[serde(default)]
        iteration: usize,
        /// The stay in that stage it was dispatched during.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        visit_id: String,
        /// The attempt whose answer asked for these calls.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        requested_by: String,
        /// The assistant text of the turn that issued the calls.
        #[serde(default)]
        response: String,
    },
    /// One call of the pending batch finished.
    ToolCallDone {
        /// The iteration of the batch it belongs to.
        iteration: usize,
        /// The provider's call id.
        call_id: String,
        /// The attempt this completes.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        execution_id: String,
        /// The result.
        result: EntryContent,
        /// How it ended, where the journal said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<leviath_core::execution::ToolOutcome>,
        /// Unix seconds.
        at: i64,
    },
    /// Files one execution produced.
    ArtifactsProduced {
        /// The execution that produced them.
        execution_id: String,
        /// The files.
        artifacts: Vec<leviath_core::output::Artifact>,
        /// Unix seconds.
        at: i64,
    },
    /// A question put to a person, and how it ended.
    Interaction {
        /// The request id.
        request_id: String,
        /// What was asked for.
        kind: leviath_core::interaction::InteractionKind,
        /// The tool an approval was for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        /// The question as the person saw it.
        prompt: String,
        /// The stage the run was in.
        stage: String,
        /// How it ended.
        settlement: leviath_core::interaction::Settlement,
        /// Unix seconds when it was asked.
        asked_at: i64,
        /// Unix seconds when it settled.
        at: i64,
    },
    /// One trip to a provider.
    InferenceAttempt(AttemptRecord),
    /// One provider judged unusable, and the model tried instead.
    InferenceFailover(FailoverRecord),
    /// A full context-window snapshot.
    ContextCheckpoint {
        /// The window.
        snapshot: ContextSnapshot,
        /// Unix seconds.
        at: i64,
    },
    /// Why one region changed.
    ContextChange {
        /// The region.
        region: String,
        /// What changed it.
        cause: leviath_core::ContextCause,
        /// Entries the change added.
        entries_added: usize,
        /// Entries it removed.
        entries_removed: usize,
        /// How the region's token count moved.
        token_delta: i64,
        /// Unix seconds.
        at: i64,
    },
    /// One committed change to the window, over every region it touched.
    ContextTransaction {
        /// The window's revision before.
        revision_before: String,
        /// The window's revision after.
        revision_after: String,
        /// What made the change.
        cause: leviath_core::ContextCause,
        /// Every region it touched.
        regions: Vec<RegionCommit>,
        /// The execution that committed it.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        execution_id: String,
        /// Unix seconds.
        at: i64,
    },
    /// The window's change since the snapshot or diff before it.
    ContextDiff {
        /// The change.
        delta: ContextDelta,
        /// Unix seconds.
        at: i64,
    },
    /// An inbound message.
    Message {
        /// The message.
        message: MessageRecord,
        /// Unix seconds.
        at: i64,
    },
    /// The run's status changed.
    StatusChanged {
        /// The new status.
        status: RunStatus,
        /// Unix seconds.
        at: i64,
    },
    /// The run's metadata and its whole window.
    Checkpoint {
        /// The metadata.
        meta: Box<RunMeta>,
        /// The window.
        context: ContextSnapshot,
        /// Unix seconds.
        at: i64,
    },
    /// The run's metadata and the window's change since the record before.
    Progress {
        /// The metadata.
        meta: Box<RunMeta>,
        /// The window's change.
        delta: ContextDelta,
        /// Unix seconds.
        at: i64,
    },
}

/// Apply `delta` to `base` in place. A change to a region `base` does not
/// have is skipped, so a malformed diff never stops a fold.
pub fn apply_delta(base: &mut ContextSnapshot, delta: &ContextDelta) {
    base.stage_name = delta.stage_name.clone();
    base.total_tokens = delta.total_tokens;
    base.max_tokens = delta.max_tokens;
    for region_delta in &delta.regions {
        match region_delta {
            RegionDelta::Set(snapshot) => {
                match base.regions.iter_mut().find(|r| r.name == snapshot.name) {
                    Some(existing) => *existing = snapshot.clone(),
                    None => base.regions.push(snapshot.clone()),
                }
            }
            RegionDelta::Append {
                name,
                entries,
                current_tokens,
            } => {
                if let Some(region) = base.regions.iter_mut().find(|r| &r.name == name) {
                    region.entries.extend(entries.iter().cloned());
                    region.current_tokens = *current_tokens;
                }
            }
            RegionDelta::Clear { name } => {
                if let Some(region) = base.regions.iter_mut().find(|r| &r.name == name) {
                    region.entries.clear();
                    region.current_tokens = 0;
                }
            }
            RegionDelta::Remove { name } => {
                base.regions.retain(|r| &r.name != name);
            }
        }
    }
}

/// A tool batch that was dispatched and whose results never reached the
/// window: the run stopped mid-batch. `calls` carry every result recorded
/// before it stopped; a call still at `result: None` never finished.
///
/// Only a batch with work in the tool lane counts. One the dispatcher settled
/// entirely by itself (context tools, refusals, gate denials) is issued again
/// instead, because replaying it would put `context_write: ok` over a region
/// the write never reached.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingToolBatch {
    /// The stage-local iteration that produced the batch.
    pub iteration: usize,
    /// The calls, with every recorded result merged in.
    pub calls: Vec<ToolCallRecord>,
}

/// What a whole journal says about the run as it last stood.
#[derive(Debug, Clone, PartialEq)]
pub struct Folded {
    /// The metadata the run was last written with.
    pub meta: RunMeta,
    /// Its context window then.
    pub context: ContextSnapshot,
    /// A tool batch it was still running, if any.
    pub pending_batch: Option<PendingToolBatch>,
}

/// Whether `context` already holds the assistant turn of `batch`: the batch
/// finished and its turn landed before the run stopped. Matched by the first
/// call id, which is unique per batch.
fn context_contains_batch(context: &ContextSnapshot, batch: &PendingToolBatch) -> bool {
    let first_id = batch.calls.first().map(|c| c.id.as_str());
    context.regions.iter().any(|region| {
        region.entries.iter().any(|entry| match &entry.kind {
            leviath_core::region::EntryKind::AssistantTurn { tool_calls } => {
                tool_calls.iter().any(|tc| Some(tc.id.as_str()) == first_id)
            }
            _ => false,
        })
    })
}

/// Fold a journal to how the run last stood. `None` when the records do not
/// start with a [`JournalRecord::Header`].
pub fn fold(records: &[JournalRecord]) -> Option<Folded> {
    let mut iter = records.iter();
    let Some(JournalRecord::Header { meta, .. }) = iter.next() else {
        return None;
    };
    let mut folded = Folded {
        meta: (**meta).clone(),
        context: ContextSnapshot {
            stage_name: String::new(),
            total_tokens: 0,
            max_tokens: 0,
            regions: Vec::new(),
        },
        pending_batch: None,
    };
    for record in iter {
        match record {
            JournalRecord::Header { meta, .. } => folded.meta = (**meta).clone(),
            JournalRecord::ToolBatch {
                calls, iteration, ..
            } => {
                // Only the newest batch can still be in flight, and only one
                // with a call the dispatcher did not settle itself.
                folded.pending_batch =
                    calls
                        .iter()
                        .any(|call| call.result.is_none())
                        .then(|| PendingToolBatch {
                            iteration: *iteration,
                            calls: calls.clone(),
                        });
            }
            JournalRecord::ToolCallDone {
                iteration,
                call_id,
                result,
                ..
            } => {
                // A record for a batch a later one replaced is ignored.
                let call = folded
                    .pending_batch
                    .as_mut()
                    .filter(|b| b.iteration == *iteration)
                    .and_then(|b| b.calls.iter_mut().find(|c| c.id == *call_id));
                if let Some(call) = call {
                    call.result = Some(result.clone());
                }
            }
            JournalRecord::ContextCheckpoint { snapshot, .. } => folded.context = snapshot.clone(),
            JournalRecord::ContextDiff { delta, .. } => apply_delta(&mut folded.context, delta),
            JournalRecord::StatusChanged { status, .. } => folded.meta.status = status.clone(),
            JournalRecord::Checkpoint { meta, context, .. } => {
                folded.meta = (**meta).clone();
                folded.context = context.clone();
            }
            JournalRecord::Progress { meta, delta, .. } => {
                folded.meta = (**meta).clone();
                apply_delta(&mut folded.context, delta);
            }
            _ => {}
        }
    }
    // A batch is only pending if it was never applied: a later model call
    // moved the iteration on, or its turn is already in the window.
    let applied = folded.pending_batch.as_ref().is_some_and(|batch| {
        folded.meta.iteration != batch.iteration || context_contains_batch(&folded.context, batch)
    });
    if applied {
        folded.pending_batch = None;
    }
    Some(folded)
}

/// When the first record that held the run's window was written: what every
/// earlier release listed as the first point of its context history.
pub(crate) fn first_point_at(records: &[JournalRecord]) -> Option<i64> {
    records.iter().find_map(|r| match r {
        JournalRecord::ContextCheckpoint { at, .. }
        | JournalRecord::ContextDiff { at, .. }
        | JournalRecord::Progress { at, .. }
        | JournalRecord::Checkpoint { at, .. } => Some(*at),
        _ => None,
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
