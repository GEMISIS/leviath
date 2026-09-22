//! What a run asked a person, and what came back.
//!
//! One question, one type, whether it is still parked on a run or has already
//! settled: `settlement` is null for the first and set for the second, and a
//! field the journal does not keep once a question settles (`body`, `options`,
//! the typed `toolCall`) reads null or empty there instead of carrying a stale
//! copy. The only record of the settled half: the hub hands an answer to
//! whichever caller was waiting and forgets it, so without the journal an
//! approved tool call looks exactly like one no policy ever stopped, and a run
//! that paused for somebody looks exactly like one that never asked.

use async_graphql::{Enum, ID, Object, SimpleObject};
use leviath_core::run_archive::InteractionRecord;
use leviath_graphql_derive::mirror;

use super::super::connection::{
    Connection, Paged, PositionQuery, Total, position_order, position_page,
};
use super::super::error::IntoGraphql;
use super::super::filter::MatchCx;
use super::super::paging::page::page;
use super::super::scalars::{Cursor, Timestamp};
use crate::commands::serve::blocking::blocking;
use crate::commands::serve::core::error::ServeError;
use crate::commands::serve::core::interactions;
use crate::commands::serve::cursor;

/// What kind of answer a parked run is waiting for.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum InteractionKind {
    /// The person writes anything.
    FreeText,
    /// The person picks from options.
    MultipleChoice,
    /// The person confirms or denies.
    Confirm,
    /// The person approves or denies a tool call.
    ToolApproval,
    /// The person edits a document directly.
    EditText,
}

impl From<&leviath_core::interaction::InteractionKind> for InteractionKind {
    fn from(kind: &leviath_core::interaction::InteractionKind) -> Self {
        use leviath_core::interaction::InteractionKind as Core;
        match kind {
            Core::FreeText => Self::FreeText,
            Core::MultipleChoice => Self::MultipleChoice,
            Core::Confirm => Self::Confirm,
            Core::ToolApproval => Self::ToolApproval,
            Core::EditText => Self::EditText,
        }
    }
}

/// How far an approval that settled an ask reached.
///
/// The REST journal and the answer routes write this scope's widest value as
/// `session`; this field always spells it `RUN`, so a client reading GraphQL
/// never has to know the REST name for the same thing.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ApprovalScope {
    /// Just the one call it was asked about.
    Once,
    /// Every later call it covered, until the run left the stage it was asked
    /// in.
    Stage,
    /// Every later call it covered, for the rest of the run.
    Run,
}

impl From<leviath_core::interaction::ApprovalScope> for ApprovalScope {
    fn from(scope: leviath_core::interaction::ApprovalScope) -> Self {
        use leviath_core::interaction::ApprovalScope as Core;
        match scope {
            Core::Once => Self::Once,
            Core::Stage => Self::Stage,
            Core::Run => Self::Run,
        }
    }
}

impl From<ApprovalScope> for leviath_core::interaction::ApprovalScope {
    fn from(scope: ApprovalScope) -> Self {
        match scope {
            ApprovalScope::Once => Self::Once,
            ApprovalScope::Stage => Self::Stage,
            ApprovalScope::Run => Self::Run,
        }
    }
}

/// How a question a run asked ended up.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SettlementOutcome {
    /// A person answered it.
    Answered,
    /// Nobody answered before the run's interaction timeout ran out, so the
    /// hub answered for them.
    TimedOut,
    /// The request was withdrawn: the run was cancelled, or the agent that
    /// asked it went away.
    Cancelled,
    /// It never opened, because a request was already open under the same id,
    /// and the daemon keeps the one somebody may already be reading.
    ///
    /// Not a denial: the run was handed the neutral answer, which a tool
    /// approval and a taint gate both read as not-approved. Its own outcome
    /// because it means two runs minted one id, which is a fault in this
    /// server rather than a decision about the call.
    Refused,
}

/// How a question this run asked ended up, and what the answer was.
///
/// `approved`, `scope`, `choice`, `text` and `feedback` are null unless
/// `outcome` is `ANSWERED`. Nobody answered a `TIMED_OUT`, `CANCELLED` or
/// `REFUSED` ask, so there is nothing for any of them to carry.
#[mirror]
#[derive(Debug, Clone, SimpleObject)]
pub(crate) struct Settlement {
    /// How it ended.
    pub(crate) outcome: SettlementOutcome,
    /// Whether a tool approval was granted. Null for every kind of ask but a
    /// tool approval that was answered.
    pub(crate) approved: Option<bool>,
    /// The scope an approval was given at: this call, this stage, or the rest
    /// of the run. Null where none was offered, or where nobody answered.
    pub(crate) scope: Option<ApprovalScope>,
    /// Which option was picked, for a question that offered a list.
    pub(crate) choice: Option<i32>,
    /// What was typed, for a question that took text. An edited document
    /// comes back here too, exactly as it was left.
    pub(crate) text: Option<String>,
    /// What the person told the model to do instead, on a denial.
    pub(crate) feedback: Option<String>,
}

impl From<&leviath_core::interaction::Settlement> for Settlement {
    fn from(settlement: &leviath_core::interaction::Settlement) -> Self {
        use leviath_core::interaction::Settlement as Core;
        // Every field but `outcome` stays null on the arms that carry no
        // answer: a client switching on `outcome` first never needs to check
        // whether an unrelated field is meaningful before reading it.
        match settlement {
            Core::Answered {
                approved,
                scope,
                choice,
                text,
                feedback,
            } => Self {
                outcome: SettlementOutcome::Answered,
                approved: *approved,
                scope: scope.map(ApprovalScope::from),
                choice: choice.map(|c| i32::try_from(c).unwrap_or(i32::MAX)),
                text: text.clone(),
                feedback: feedback.clone(),
            },
            Core::TimedOut => Self {
                outcome: SettlementOutcome::TimedOut,
                approved: None,
                scope: None,
                choice: None,
                text: None,
                feedback: None,
            },
            Core::Cancelled => Self {
                outcome: SettlementOutcome::Cancelled,
                approved: None,
                scope: None,
                choice: None,
                text: None,
                feedback: None,
            },
            Core::Refused => Self {
                outcome: SettlementOutcome::Refused,
                approved: None,
                scope: None,
                choice: None,
                text: None,
                feedback: None,
            },
        }
    }
}

/// The resolver state behind `InteractionOutput`: one question, open or
/// settled.
#[derive(Debug)]
pub(crate) struct Interaction {
    /// The run this question belongs to.
    pub(crate) run_id: String,
    /// The request's id, which is what an answer names it by whether it is
    /// still open or already settled.
    pub(crate) id: String,
    /// What kind of answer it takes, or took.
    pub(crate) kind: InteractionKind,
    /// The question itself.
    pub(crate) prompt: String,
    /// A longer document shown alongside the prompt. Null once settled: the
    /// journal does not keep it.
    pub(crate) body: Option<String>,
    /// The choices offered, for a multiple-choice ask. Empty once settled.
    pub(crate) options: Vec<String>,
    /// The call awaiting approval, typed. Null once settled: the journal keeps
    /// the tool's name, not its arguments.
    pub(crate) tool_call: Option<Box<super::tool_calls::ToolCall>>,
    /// The tool an approval was for, or is for. Set either way.
    pub(crate) tool_name: Option<String>,
    /// The stage the run is, or was, in.
    pub(crate) stage_name: String,
    /// Whether the run holds until this is answered. Unknown for a settled
    /// ask, where it reads `false`: the journal does not keep it.
    pub(crate) is_required: bool,
    /// When it was asked. Null while it is open: the daemon stamps a time only
    /// once a request is journaled.
    pub(crate) asked_at: Option<Timestamp>,
    /// How it ended, and what the answer was. Null while it is open.
    pub(crate) settlement: Option<Settlement>,
    /// When it settled. Null while it is open.
    pub(crate) settled_at: Option<Timestamp>,
}

/// The GraphQL name every listing and relation names this type by. The Rust
/// name is `Interaction`, because `#[mirror]` adds the `Output` suffix itself;
/// this alias exists so a constructor can be written the way the rest of this
/// schema's cross-file seams are - `InteractionOutput::open(run_id, request)`
/// - without a second type to keep in step with the first.
pub(crate) type InteractionOutput = Interaction;

impl Interaction {
    /// One ask still open, parked on a run.
    pub(crate) fn open(
        run_id: String,
        request: leviath_core::interaction::InteractionRequest,
    ) -> Self {
        let tool_call = request.tool_name.clone().map(|tool| {
            Box::new(super::tool_calls::from_value(
                &tool,
                None,
                // A request that names a tool and no arguments is a call with
                // none, which is what an empty object says.
                request
                    .tool_arguments
                    .clone()
                    .unwrap_or_else(|| serde_json::Value::Object(Default::default())),
            ))
        });
        Self {
            run_id,
            id: request.id,
            kind: (&request.kind).into(),
            prompt: request.prompt,
            body: request.body,
            options: request.options,
            tool_call,
            tool_name: request.tool_name,
            stage_name: request.stage_name,
            is_required: request.required,
            asked_at: None,
            settlement: None,
            settled_at: None,
        }
    }

    /// One ask that has settled, read from the run's journal.
    fn settled(run_id: String, record: InteractionRecord) -> Self {
        Self {
            run_id,
            id: record.request_id,
            kind: (&record.kind).into(),
            prompt: record.prompt,
            body: None,
            options: Vec::new(),
            tool_call: None,
            tool_name: record.tool,
            stage_name: record.stage,
            is_required: false,
            asked_at: Some(Timestamp(record.asked_at)),
            settlement: Some(Settlement::from(&record.settlement)),
            settled_at: Some(Timestamp(record.at)),
        }
    }
}

/// One question a run put to a person: a tool call to approve, a choice to
/// make, some text to write, or a document to edit.
///
/// The run waits on the ask, so an unanswered one is a run going nowhere until
/// somebody reads it. `settlement` is null while it is open and set once it
/// has settled, which is also when `body`, `options` and the typed `toolCall`
/// stop being kept: the journal records what was asked as a name and a stage,
/// not the whole document offered alongside it.
#[mirror]
#[Object]
impl Interaction {
    /// The id an answer is sent against: the id it was minted under while
    /// open, and the id the journal recorded it under once it settled.
    async fn id(&self) -> ID {
        ID(self.id.clone())
    }

    /// The run this question belongs to.
    #[filter(io)]
    async fn run(&self) -> async_graphql::Result<super::run::Run> {
        let now = leviath_core::duration::now_secs();
        let run_id = self.run_id.clone();
        let for_error = run_id.clone();
        let meta = blocking(move || crate::runstate::read_meta(&run_id))
            .await
            .map_err(|_| ServeError::NotFound(format!("Run '{for_error}' not found")))
            .gql()?;
        Ok(super::run::Run {
            meta: std::sync::Arc::new(meta),
            now,
        })
    }

    /// What kind of answer it takes, or took.
    async fn kind(&self) -> InteractionKind {
        self.kind
    }

    /// The question, as the person saw it or would see it.
    async fn prompt(&self) -> &str {
        &self.prompt
    }

    /// A longer document shown alongside the prompt. Null once the question
    /// has settled.
    async fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    /// The choices, for a multiple-choice ask. Empty once settled.
    async fn options(&self) -> &[String] {
        &self.options
    }

    /// The call awaiting approval, typed. Null for every other kind of ask,
    /// and null once settled: the journal keeps the tool's name, not its
    /// arguments.
    // Unfiltered: a call is an interface over a type per tool, and there is no
    // one comparator shape that spans them.
    #[filter(skip)]
    async fn tool_call(&self) -> Option<&super::tool_calls::ToolCall> {
        self.tool_call.as_deref()
    }

    /// The tool an approval was for, whether it is still open or has settled.
    /// Null for every other kind of ask.
    async fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    /// The stage the run is, or was, in when it asked.
    async fn stage_name(&self) -> &str {
        &self.stage_name
    }

    /// Whether the run holds until this is answered. Unknown for a settled
    /// ask, where the journal keeps no record of it and this reads `false`.
    async fn is_required(&self) -> bool {
        self.is_required
    }

    /// When it was asked. Null while it is open: a request carries no time
    /// until the daemon journals it.
    async fn asked_at(&self) -> Option<Timestamp> {
        self.asked_at
    }

    /// How it ended, and what the answer was. Null while it is open.
    async fn settlement(&self) -> Option<Settlement> {
        self.settlement.clone()
    }

    /// When it settled: answered, timed out, or cancelled. Null while it is
    /// open.
    async fn settled_at(&self) -> Option<Timestamp> {
        self.settled_at
    }
}

impl Paged for Interaction {
    const NAME: &'static str = "Interaction";
}

position_order!(
    InteractionOrder,
    InteractionOrderField,
    Sequence,
    "The one sort key `interactions` and `openInteractions` may be ordered by.",
    "Where this sits among the run's own asks, or among every open one, in the \
     order the listing read them."
);

/// Read one page of what a run asked a person and has since settled.
///
/// Shared by the field on a run and by anything else that grows one later, the
/// same way `executions` is. `openInteraction` and `openInteractions` are the
/// asks that have not reached this listing yet: a question is written down
/// only once it settles.
///
/// The whole journal is already read to answer this, so a file-backed filter
/// (`run`) is confirmed across every interaction once, up front.
pub(crate) async fn interactions(
    run_id: String,
    filter: Option<InteractionFilter>,
    order_by: Option<Vec<InteractionOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Interaction>> {
    let limit = page(
        first,
        interactions::INTERACTIONS_MAX_LIMIT,
        "the interactions page cap",
    )
    .gql()?;
    let filter = filter.unwrap_or_default();
    let rendered = super::super::paging::digest::canonical(&filter).gql()?;
    let digest = cursor::filter_digest(&["interactions", &run_id, rendered.as_str()]);
    let descending = order_by
        .unwrap_or_default()
        .first()
        .is_some_and(|term| term.direction.descending());

    let for_read = run_id.clone();
    let records = blocking(move || interactions::read(&for_read))
        .await
        .gql()?;
    let items: Vec<Interaction> = records
        .into_iter()
        .map(|record| Interaction::settled(run_id.clone(), record))
        .collect();
    walk(items, &filter, &digest, after, descending, limit).await
}

/// Read one page of every open ask across every run: the approval inbox.
///
/// Shared by the root field and by nothing else, since this is the one
/// listing this schema reads from the daemon's own memory rather than from a
/// file.
pub(crate) async fn open(
    open: Vec<(String, leviath_core::interaction::InteractionRequest)>,
    filter: Option<InteractionFilter>,
    order_by: Option<Vec<InteractionOrder>>,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<Connection<Interaction>> {
    let limit = page(
        first,
        interactions::INTERACTIONS_MAX_LIMIT,
        "the open interactions page cap",
    )
    .gql()?;
    let filter = filter.unwrap_or_default();
    let rendered = super::super::paging::digest::canonical(&filter).gql()?;
    let digest = cursor::filter_digest(&["interactions", "open", rendered.as_str()]);
    let descending = order_by
        .unwrap_or_default()
        .first()
        .is_some_and(|term| term.direction.descending());

    let items: Vec<Interaction> = open
        .into_iter()
        .map(|(run_id, request)| Interaction::open(run_id, request))
        .collect();
    walk(items, &filter, &digest, after, descending, limit).await
}

/// The walk every interaction listing shares, once it has its items.
async fn walk(
    items: Vec<Interaction>,
    filter: &InteractionFilter,
    digest: &str,
    after: Option<Cursor>,
    descending: bool,
    limit: usize,
) -> async_graphql::Result<Connection<Interaction>> {
    let cx = MatchCx::at(leviath_core::duration::now_secs());
    let walked = position_page(
        items,
        filter,
        &cx,
        PositionQuery {
            digest,
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
#[path = "interaction_tests.rs"]
mod tests;
