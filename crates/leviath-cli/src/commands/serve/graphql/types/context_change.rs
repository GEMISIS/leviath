//! What changed a run's context window, transaction by transaction.
//!
//! The half a snapshot cannot carry. `contextHistory` rebuilds the window turn
//! by turn, and a region that lost the plan it was holding looks identical there
//! whether a compaction summarised it away, a stage-edge transform cleared it,
//! or the model called `context_delete` on it - three answers with a bug in
//! three different places. These records name the path through the runtime that
//! moved the window, recorded as it moved.
//!
//! One record per committed transaction rather than per region, because that is
//! what happens: a compaction summarises one region and empties another, a stage
//! edge clears four, a resume rebuilds every region there is. Each carries the
//! window's revision either side, so a change is anchored to exact content
//! rather than to a moment.

use async_graphql::{Enum, ID, SimpleObject};
use leviath_core::run_archive::IndexedChange;
use leviath_graphql_derive::mirror;

use super::super::connection::{
    Connection, Paged, PositionQuery, Total, position_order, position_page,
};
use super::super::error::IntoGraphql;
use super::super::filter::MatchCx;
use super::super::paging::page::page;
use super::super::scalars::{BigInt, Cursor, Timestamp};
use crate::commands::serve::blocking::blocking;
use crate::commands::serve::core::context_changes;
use crate::commands::serve::cursor;

/// What changed a region of a context window.
///
/// A cause names a path through the runtime rather than a shape of edit: two
/// paths that both append to the conversation stay two causes, because which of
/// them ran is the question being asked. A write whose path has no cause of its
/// own records nothing at all rather than borrowing the nearest neighbour, so
/// this vocabulary never mislabels a change.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ContextCause {
    /// A region seeded from the blueprint or from caller input, at spawn or on
    /// entry to a stage that declares its own layout.
    Seed,
    /// A message delivered into the run while it runs, landing in the region
    /// that accepts messages.
    Message,
    /// The model's own reply, recorded as the assistant turn it was.
    ModelReply,
    /// A tool's answer landing in a region: the conversation by default,
    /// wherever the stage's `tool_results` sends it, or the region a tool writes
    /// on purpose.
    ToolResult,
    /// A part the model produced, kept somewhere other than the conversation:
    /// routed by the stage's `output_routing`, attached by a mime tool, or
    /// emitted as an artifact.
    ProducedPart,
    /// A compacting region summarising itself: the summary landing in its
    /// history region, and the source region being emptied behind it.
    Compaction,
    /// A stage-edge transform carrying, summarising or clearing a region as the
    /// run moves between stages, or as a child is seeded from its parent.
    Transform,
    /// A `context_*` or `todo_*` tool the model called: a write, an append, a
    /// release, a checklist item.
    ContextTool,
    /// A region's own `on_write` or `on_overflow` script, or a stage hook,
    /// writing on the region's behalf.
    Hook,
    /// A fan-out worker: the sources its window is seeded with, and the report it
    /// hands back to its parent.
    FanOut,
    /// An interaction point: the document it publishes for review, and what a
    /// person's answer puts in the conversation.
    Interaction,
    /// A resume rebuilding the window from the journal, region by region, before
    /// the run carries on.
    Resume,
    /// The runtime's own bookkeeping: a system nudge, a watchdog note, the record
    /// a transition choice leaves behind.
    Framework,
}

impl From<leviath_core::ContextCause> for ContextCause {
    fn from(cause: leviath_core::ContextCause) -> Self {
        use leviath_core::ContextCause as Core;
        match cause {
            Core::Seed => Self::Seed,
            Core::Message => Self::Message,
            Core::ModelReply => Self::ModelReply,
            Core::ToolResult => Self::ToolResult,
            Core::ProducedPart => Self::ProducedPart,
            Core::Compaction => Self::Compaction,
            Core::Transform => Self::Transform,
            Core::ContextTool => Self::ContextTool,
            Core::Hook => Self::Hook,
            Core::FanOut => Self::FanOut,
            Core::Interaction => Self::Interaction,
            Core::Resume => Self::Resume,
            Core::Framework => Self::Framework,
        }
    }
}

/// What one committed change did to one region.
///
/// No content: the window recorded on the same tick already holds the text, so
/// repeating it here would double the journal to say nothing new. The digests are
/// what tell you whether you need to go and read it - `digestBefore` equal to
/// `digestAfter` means this region ended the transaction holding exactly what it
/// started with.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionTransition {
    /// The region this part of the transaction touched, by the name it carries
    /// in the run's window.
    ///
    /// A name rather than a `Region`, because the window is not the manifest.
    /// The runtime carries `conversation`, `tool_results`, `final_output` and
    /// `stage_instructions` whether a blueprint declares them or not, and those
    /// are the regions that move most, so a manifest lookup would answer nothing
    /// for the common case. Join on `contextHistory`'s `ContextRegion.name` for
    /// what the region held, and on `blueprint { regions { name } }` for what
    /// the author declared.
    pub(crate) region: String,
    /// A content address of what it held before the change. Compare it with
    /// `digestAfter`, and with the same region in another transaction; nothing
    /// else is promised about the value.
    ///
    /// Null on a change a build that recorded one region at a time wrote, which
    /// digested nothing.
    pub(crate) digest_before: Option<String>,
    /// A content address of what it held afterwards. Null on the same changes.
    pub(crate) digest_after: Option<String>,
    /// What it held before, in tokens. Null on a change that recorded only how
    /// the count moved.
    pub(crate) tokens_before: Option<BigInt>,
    /// What it held afterwards. Null on the same changes.
    pub(crate) tokens_after: Option<BigInt>,
    /// How its token count moved. Negative when the region shrank.
    pub(crate) token_delta: BigInt,
    /// How many entries it held before. Null on a change that recorded only the
    /// counts it moved.
    pub(crate) entries_before: Option<i32>,
    /// How many it held afterwards. Null on the same changes.
    pub(crate) entries_after: Option<i32>,
    /// Entries the change itself pushed. Not derivable from the counts either
    /// side: a write into a full sliding region leaves the count where it was.
    pub(crate) entries_added: i32,
    /// Entries that left, including any eviction the change itself triggered.
    pub(crate) entries_removed: i32,
}

/// One committed transaction against a run's context window.
///
/// A transaction, not a write: a compaction summarises one region and empties
/// another, a stage edge clears four, a resume rebuilds every region there is.
/// All of those are one change here, with `regions` carrying each region it
/// touched - so a region that lost its plan can be read beside whatever moved
/// with it.
///
/// `revisionBefore` and `revisionAfter` anchor it to exact content rather than to
/// a moment: they are the revisions of `contextSnapshot`, so a transaction says
/// which window it started from and which one it produced.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextChange {
    /// What made the change.
    pub(crate) cause: ContextCause,
    /// The window's revision before the transaction, as `contextSnapshot` takes
    /// it. Null on a change a build that named no window wrote.
    pub(crate) revision_before: Option<String>,
    /// The window's revision after it. Null on the same changes.
    pub(crate) revision_after: Option<String>,
    /// The tool execution that committed it, by `ToolExecution.id`.
    ///
    /// Null means the journal recorded no execution for this change, which is
    /// every change made outside a tool call: the model's own reply landing in
    /// the conversation, a compaction, a stage-edge transform, a resume, a
    /// framework nudge. It is also null throughout a journal written by a build
    /// that did not record the connection.
    ///
    /// A call the dispatcher answers without the tool lane - a `context_*` tool,
    /// a refusal, a gate denial - is journaled as a dispatch like any other, so
    /// an id here names an execution `executions` lists.
    pub(crate) execution_id: Option<ID>,
    /// Every region the transaction touched, in the order the write path named
    /// them.
    pub(crate) regions: Vec<RegionTransition>,
    /// Where in the run's journal the record that carries this change sits.
    ///
    /// A byte offset. It only climbs within a run and never changes, so it orders
    /// changes and names one for as long as the run exists.
    pub(crate) journal_position: BigInt,
    /// When the transaction committed.
    pub(crate) at: Timestamp,
}

impl From<IndexedChange> for ContextChange {
    fn from(indexed: IndexedChange) -> Self {
        let record = indexed.record;
        Self {
            cause: ContextCause::from(record.cause),
            revision_before: record.revision_before,
            revision_after: record.revision_after,
            execution_id: record.execution_id.map(ID::from),
            regions: record
                .regions
                .into_iter()
                .map(RegionTransition::from)
                .collect(),
            journal_position: BigInt(i64::try_from(indexed.position).unwrap_or(i64::MAX)),
            at: Timestamp(record.at),
        }
    }
}

impl From<leviath_core::run_archive::RegionTransition> for RegionTransition {
    fn from(region: leviath_core::run_archive::RegionTransition) -> Self {
        Self {
            region: region.region,
            digest_before: region.digest_before,
            digest_after: region.digest_after,
            tokens_before: region.tokens_before.map(tokens),
            tokens_after: region.tokens_after.map(tokens),
            token_delta: BigInt(region.token_delta),
            entries_before: region.entries_before.map(count),
            entries_after: region.entries_after.map(count),
            entries_added: count(region.entries_added),
            entries_removed: count(region.entries_removed),
        }
    }
}

/// A token count as the 64 bits `BigInt` carries, saturating rather than
/// wrapping.
fn tokens(value: usize) -> BigInt {
    BigInt(i64::try_from(value).unwrap_or(i64::MAX))
}

impl Paged for ContextChange {
    const NAME: &'static str = "ContextChange";
}

position_order!(
    ContextChangeOrder,
    ContextChangeOrderField,
    JournalPosition,
    "The one sort key `contextChanges` may be ordered by.",
    "Where the record that carries this change sits in the run's journal."
);

/// Narrow a journal counter to the 32 bits GraphQL's `Int` carries.
///
/// Entry counts, which a region reaches the thousands of at most. Saturating
/// rather than wrapping: an implausible ceiling reads as wrong, where a wrapped
/// small number reads as fine.
fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Read one page of a run's context changes.
///
/// Shared by the field on a run and by anything else that grows one later, the
/// same way `interactions` is.
///
/// The whole journal is already read to answer this, so a file-backed filter
/// is confirmed across every change once, up front - though today no field on
/// `ContextChange` reads a second file.
pub(crate) async fn context_changes(
    run_id: String,
    filter: Option<ContextChangeFilter>,
    order_by: Option<Vec<ContextChangeOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<ContextChange>> {
    let limit = page(
        first,
        context_changes::CONTEXT_CHANGES_MAX_LIMIT,
        "the context changes page cap",
    )
    .gql()?;
    let filter = filter.unwrap_or_default();
    let rendered = super::super::paging::digest::canonical(&filter).gql()?;
    let digest = cursor::filter_digest(&["context_changes", &run_id, rendered.as_str()]);
    let descending = order_by
        .unwrap_or_default()
        .first()
        .is_some_and(|term| term.direction.descending());

    let for_read = run_id.clone();
    let changes = blocking(move || context_changes::read(&for_read))
        .await
        .gql()?;
    let items: Vec<ContextChange> = changes.into_iter().map(ContextChange::from).collect();
    let cx = MatchCx::at(leviath_core::duration::now_secs());
    let walked = position_page(
        items,
        &filter,
        &cx,
        PositionQuery {
            digest: &digest,
            after: after.as_ref().map(|token| token.0.as_str()),
            descending,
            limit,
        },
    )
    .await
    .gql()?;
    Ok(Connection::plain(
        walked.items,
        walked.cursor,
        Total::known(walked.total),
    ))
}

#[cfg(test)]
#[path = "context_change_tests.rs"]
mod tests;
