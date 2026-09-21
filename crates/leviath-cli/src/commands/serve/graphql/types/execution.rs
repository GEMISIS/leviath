//! What a run did: one attempt at one tool call, and how it ended.
//!
//! Read from the journal rather than from the run's current state, because the
//! question is what happened and not what is left. A call that was refused, one
//! that failed and was reissued, one that was cut off by a restart: all three are
//! here, and none of them is visible in a folded context window.

use async_graphql::{Context, Enum, Object, SimpleObject};
use leviath_core::run_archive::Execution;

use super::super::error::IntoGraphql;
use super::super::scalars::{BigInt, Timestamp};
use super::tool_calls::{ToolCall, tool_call};
use crate::commands::serve::blocking::blocking;
use crate::commands::serve::core::{context_changes, executions, inferences};

/// How one attempt to execute a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ToolOutcome {
    /// The tool ran and answered.
    Succeeded,
    /// The tool ran and failed.
    Failed,
    /// A gate refused it before it ran: the taint gate, a permission rule.
    Blocked,
    /// A person refused it.
    Denied,
    /// Nobody observed how it ended, and nobody ever will. A daemon that died
    /// between dispatch and completion leaves this, and the resume that carried
    /// the run on is what recorded it.
    Indeterminate,
}

impl From<leviath_core::execution::ToolOutcome> for ToolOutcome {
    fn from(outcome: leviath_core::execution::ToolOutcome) -> Self {
        use leviath_core::execution::ToolOutcome as Core;
        match outcome {
            Core::Succeeded => Self::Succeeded,
            Core::Failed => Self::Failed,
            Core::Blocked => Self::Blocked,
            Core::Denied => Self::Denied,
            Core::Indeterminate => Self::Indeterminate,
        }
    }
}

/// The resolver state behind the `ToolExecution` type.
pub(crate) struct ToolExecution {
    /// The run this belongs to, for the fields that read its journal again.
    pub(crate) run_id: String,
    /// What the journal recorded.
    pub(crate) record: Execution,
}

impl ToolExecution {
    /// The stay this belongs to, or nothing where the journal recorded none.
    ///
    /// The journal writes an unrecorded correlation as an empty string. Nothing
    /// would ever match one, and an empty id passed to a lookup would look like a
    /// question rather than the absence of one.
    fn visit_id(&self) -> Option<&str> {
        Some(self.record.visit_id.as_str()).filter(|id| !id.is_empty())
    }

    /// The attempt that asked for this call, or nothing where none was recorded.
    fn requested_by_id(&self) -> Option<String> {
        Some(self.record.requested_by.clone()).filter(|id| !id.is_empty())
    }

    /// This execution's own id, or nothing where none was minted.
    fn minted_id(&self) -> Option<String> {
        Some(self.record.id.clone()).filter(|id| !id.is_empty())
    }
}

/// One attempt to carry out one tool call: what was asked for, what the tool
/// did, and how it turned out.
///
/// An attempt, not a call: a call the model reissued is a second execution with
/// its own id. The provider's call id is on the call and may repeat, which is
/// exactly why these have ids of their own.
#[Object]
impl ToolExecution {
    /// This attempt's own id, minted when it was dispatched.
    ///
    /// Null in a journal written before executions had identity, where the
    /// provider's call id is the only handle there is. A client that needs a key
    /// for a list can use `journalPosition` and `callId` together, which every
    /// journal supports.
    async fn id(&self) -> Option<String> {
        self.minted_id()
    }

    /// The call this attempt was carrying out, typed by its tool.
    async fn call(&self) -> ToolCall {
        // The description is left out here. It would have to come from this
        // build's tool catalog, which describes the tool as it is now rather
        // than as it was when the call was made, and a run's history should not
        // quietly re-word itself after an upgrade.
        tool_call(&self.record.tool, None, &self.record.arguments)
    }

    /// The provider's own id for the call, kept as correlation.
    ///
    /// Not an identity: a provider may reuse one across a retry or a reissue. It
    /// is what matches this execution to what the provider says about it.
    async fn call_id(&self) -> &str {
        &self.record.call_id
    }

    /// How it ended.
    ///
    /// Null means one of three things, and a client must not flatten them: it is
    /// still running, it ended before this build recorded outcomes, or it ended
    /// in a way only the result text describes. `endedAt` tells the first apart
    /// from the other two.
    async fn outcome(&self) -> Option<ToolOutcome> {
        self.record.outcome.map(ToolOutcome::from)
    }

    /// The stage it was dispatched in, by index.
    ///
    /// Where it sat, not which stay it belonged to: a stage entered three times
    /// has one index and three visits. Correlate on `visit`.
    async fn stage_index(&self) -> i32 {
        i32::try_from(self.record.stage_index).unwrap_or(i32::MAX)
    }

    /// The stage-local iteration whose turn asked for it.
    ///
    /// The batch key within one stay: one batch per iteration. It restarts at
    /// every entry into a stage, so it names an execution only together with
    /// `visit`.
    async fn iteration(&self) -> i32 {
        i32::try_from(self.record.iteration).unwrap_or(i32::MAX)
    }

    /// The stay in a stage this execution belongs to.
    ///
    /// The correlation key for everything that happened during one stay, which is
    /// what `stageIndex` and `iteration` cannot be: a stage entered three times
    /// has one index, and the iteration restarts on every entry.
    ///
    /// Null means one of three things. The journal recorded no visit for this
    /// batch - either it was written by a build that did not, or the run had no
    /// stage ledger. Or the visit was past the ledger's per-stage cap of the
    /// earliest 128 stays, where the stay is real and its detail is not kept; the
    /// stage's own roll-ups in `stages` are the complete figures there.
    async fn visit(&self) -> async_graphql::Result<Option<super::run_detail::StageVisit>> {
        let Some(visit_id) = self.visit_id() else {
            return Ok(None);
        };
        let run_id = self.run_id.clone();
        let records = blocking(move || crate::runstate::read_stages_index(&run_id)).await;
        Ok(records
            .iter()
            .map(super::run_detail::StageRecord::from)
            .find_map(|stage| {
                stage
                    .visits
                    .into_iter()
                    .find(|visit| visit.id.as_deref() == Some(visit_id))
            }))
    }

    /// The provider attempt whose answer asked for this call.
    ///
    /// One trip to the provider, from `inferences`. It is the attempt that
    /// answered, which a client cannot work out for itself: a failover means the
    /// answer came from a different provider than the attempt before it went to.
    ///
    /// Null means the journal recorded no attempt for this batch, which is every
    /// batch in a journal written by a build that did not record the connection,
    /// and any batch no provider answer asked for.
    async fn requested_by(
        &self,
    ) -> async_graphql::Result<Option<super::inference::InferenceAttempt>> {
        let Some(attempt_id) = self.requested_by_id() else {
            return Ok(None);
        };
        let run_id = self.run_id.clone();
        let found = blocking(move || inferences::attempt(&run_id, &attempt_id))
            .await
            .gql()?;
        Ok(found.map(|attempt| super::inference::InferenceAttempt {
            record: attempt.record,
            failover: attempt.failover,
        }))
    }

    /// The context-window transactions this execution committed.
    ///
    /// Empty for an execution that committed none, which most are: a tool that
    /// reads a file changes nothing, and its answer landing in the conversation is
    /// committed by the batch rather than by the call. The `context_*` and `todo_*`
    /// tools are the ones that show up here, along with anything that wrote a part
    /// into a region of its own.
    ///
    /// Independent of `outcome`. A call that succeeded may have committed nothing,
    /// and a call that failed may have committed something before it failed, so
    /// neither field may be read off the other.
    async fn context_changes(
        &self,
    ) -> async_graphql::Result<Vec<super::context_change::ContextChange>> {
        let Some(id) = self.minted_id() else {
            return Ok(Vec::new());
        };
        let run_id = self.run_id.clone();
        let changes = blocking(move || context_changes::by_execution(&run_id, &id))
            .await
            .gql()?;
        Ok(changes
            .into_iter()
            .map(super::context_change::ContextChange::from)
            .collect())
    }

    /// The files this execution produced, as the journal recorded them when it
    /// produced them.
    ///
    /// Empty for every execution that produced none, which is all but a
    /// `submit_output` that named artifacts. Read from the journal rather than
    /// from the run's answer on purpose: the answer holds the *latest*
    /// submission's files and says nothing about which call made them, so a
    /// submission a later one replaced would be invisible.
    async fn produced_artifacts(&self, ctx: &Context<'_>) -> Vec<super::run_detail::Artifact> {
        let state = ctx.data_unchecked::<crate::commands::serve::AppState>();
        self.record
            .artifacts
            .iter()
            .map(|artifact| super::run_detail::artifact(state, &self.run_id, artifact))
            .collect()
    }

    /// When it was dispatched.
    async fn dispatched_at(&self) -> Timestamp {
        Timestamp(self.record.dispatched_at)
    }

    /// When it ended. Null while it is still running, and on an attempt whose
    /// ending was never recorded.
    async fn ended_at(&self) -> Option<Timestamp> {
        self.record.ended_at.map(Timestamp)
    }

    /// Where in the run's journal the record that dispatched it sits.
    ///
    /// A byte offset. It only climbs within a run and never changes, so it orders
    /// executions and names one for as long as the run exists.
    async fn journal_position(&self) -> BigInt {
        BigInt(i64::try_from(self.record.position).unwrap_or(i64::MAX))
    }

    /// What the tool answered, read from the journal on demand.
    ///
    /// Its own field rather than part of the execution, because a result can be a
    /// whole file and a page of executions must not carry every one of them. Null
    /// when the attempt has no recorded result: still running, or cut off.
    ///
    /// For an indeterminate outcome this is the stand-in the resume put in the
    /// window, not something the tool returned.
    async fn result(&self) -> async_graphql::Result<Option<ToolResult>> {
        let Some(position) = self.record.result_position else {
            return Ok(None);
        };
        let run_id = self.run_id.clone();
        let call_id = self.record.call_id.clone();
        let found = blocking(move || executions::result(&run_id, position, &call_id))
            .await
            .gql()?;
        Ok(found.map(|result| ToolResult {
            truncated: result.truncated(),
            text: result.text,
            bytes: BigInt(i64::try_from(result.bytes).unwrap_or(i64::MAX)),
            parts: result.parts,
        }))
    }
}

/// What a tool answered, as far as it fits in an answer.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolResult {
    /// The text, cut to the head where the whole thing is large.
    pub(crate) text: String,
    /// How many bytes the whole result is.
    pub(crate) bytes: BigInt,
    /// Whether `text` is only the head of it.
    pub(crate) truncated: bool,
    /// The stored parts it carried, by name. Their bytes are fetched from the
    /// run's own parts, where they already live.
    pub(crate) parts: Vec<String>,
}

/// One page of what a run did.
#[derive(SimpleObject)]
pub(crate) struct ToolExecutionConnection {
    /// The executions on this page, in dispatch order.
    pub(crate) edges: Vec<ToolExecutionEdge>,
    /// Where the next page starts.
    pub(crate) page_info: super::super::connection::PageInfo,
    /// How many the run's journal holds altogether.
    pub(crate) total: i32,
}

/// One execution and its cursor.
#[derive(SimpleObject)]
pub(crate) struct ToolExecutionEdge {
    /// Where to resume from after this one.
    pub(crate) cursor: super::super::scalars::Cursor,
    /// The execution.
    pub(crate) node: ToolExecution,
}

/// Read one page of a run's executions.
///
/// Shared by the field on a run and by anything else that grows one later, so
/// the page cap and the cursor rules are stated once.
pub(crate) async fn page(
    run_id: String,
    first: i32,
    after: Option<super::super::scalars::Cursor>,
) -> async_graphql::Result<ToolExecutionConnection> {
    use crate::commands::serve::core::error::ServeError;
    let limit = usize::try_from(first)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| ServeError::BadRequest("`first` must be at least 1".to_string()))
        .gql()?;
    if limit > executions::EXECUTIONS_MAX_LIMIT {
        return Err(ServeError::BadRequest(format!(
            "`first` may be at most {}, the executions page cap",
            executions::EXECUTIONS_MAX_LIMIT
        )))
        .gql();
    }
    let cursor = after.map(|cursor| cursor.0);
    let for_read = run_id.clone();
    let page = blocking(move || {
        let spec = executions::ExecutionsSpec::resolve(&for_read, Some(limit), cursor.as_deref())?;
        executions::page(&for_read, &spec)
    })
    .await
    .gql()?;
    let total = i32::try_from(page.total).unwrap_or(i32::MAX);
    let end_cursor = page.next_cursor.clone().map(super::super::scalars::Cursor);
    Ok(ToolExecutionConnection {
        edges: page
            .executions
            .into_iter()
            .map(|record| ToolExecutionEdge {
                // The position orders executions and never changes, so it is the
                // one thing on an execution worth pointing a cursor at.
                cursor: super::super::scalars::Cursor(record.position.to_string()),
                node: ToolExecution {
                    run_id: run_id.clone(),
                    record,
                },
            })
            .collect(),
        page_info: super::super::connection::PageInfo {
            end_cursor: end_cursor.clone(),
            has_next_page: end_cursor.is_some(),
        },
        total,
    })
}
