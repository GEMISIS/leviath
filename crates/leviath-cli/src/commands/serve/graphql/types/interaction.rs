//! What a run asked a person, and what came back.
//!
//! The only record of it: the hub hands an answer to whichever caller was
//! waiting and forgets it, so without the journal an approved tool call looks
//! exactly like one no policy ever stopped, and a run that paused for somebody
//! looks exactly like one that never asked. Read from the journal for the same
//! reason `executions` is: a context window shows what a run is doing now, not
//! what it stopped and asked about along the way.

use async_graphql::{Enum, Object, SimpleObject};
use leviath_core::run_archive::InteractionRecord;

use super::super::error::IntoGraphql;
use super::super::events::InteractionKind;
use super::super::scalars::{Cursor, Timestamp};
use crate::commands::serve::blocking::blocking;
use crate::commands::serve::core::interactions;

/// How far an approval that settled an ask reached.
///
/// The REST journal and the answer routes write this scope's widest value as
/// `session`; this field always spells it `RUN`, so a client reading GraphQL
/// never has to know the REST name for the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SettledApprovalScope {
    /// Just the one call it was asked about.
    Once,
    /// Every later call it covered, until the run left the stage it was asked
    /// in.
    Stage,
    /// Every later call it covered, for the rest of the run.
    Run,
}

impl From<leviath_core::interaction::ApprovalScope> for SettledApprovalScope {
    fn from(scope: leviath_core::interaction::ApprovalScope) -> Self {
        use leviath_core::interaction::ApprovalScope as Core;
        match scope {
            Core::Once => Self::Once,
            Core::Stage => Self::Stage,
            Core::Run => Self::Run,
        }
    }
}

/// How a question a run asked ended up.
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
#[derive(Debug, SimpleObject)]
pub(crate) struct Settlement {
    /// How it ended.
    pub(crate) outcome: SettlementOutcome,
    /// Whether a tool approval was granted. Null for every kind of ask but a
    /// tool approval that was answered.
    pub(crate) approved: Option<bool>,
    /// The scope an approval was given at: this call, this stage, or the rest
    /// of the run. Null where none was offered, or where nobody answered.
    pub(crate) scope: Option<SettledApprovalScope>,
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
                scope: scope.map(SettledApprovalScope::from),
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

/// The resolver state behind the `Interaction` type.
pub(crate) struct Interaction {
    /// What the journal recorded.
    pub(crate) record: InteractionRecord,
}

/// One question a run put to a person: a tool call to approve, a choice to
/// make, some text to write, or a document to edit.
///
/// The run waits on the ask, so an unanswered one is a run going nowhere until
/// somebody reads it. `settlement` says how it ended and what came back, and
/// `requestId` is the handle an answer is sent against, over this API or from
/// `lev respond`.
#[Object]
impl Interaction {
    /// The id the hub minted for this ask, which is what an answer arriving
    /// over the API or from `lev respond` carries.
    async fn request_id(&self) -> &str {
        &self.record.request_id
    }

    /// What kind of answer it took.
    async fn kind(&self) -> InteractionKind {
        InteractionKind::from(&self.record.kind)
    }

    /// The tool an approval was for. Null for every other kind.
    async fn tool(&self) -> Option<&str> {
        self.record.tool.as_deref()
    }

    /// The question as the person saw it.
    async fn prompt(&self) -> &str {
        &self.record.prompt
    }

    /// The stage the run was in when it asked.
    async fn stage(&self) -> &str {
        &self.record.stage
    }

    /// How it ended, and what the answer was.
    async fn settlement(&self) -> Settlement {
        Settlement::from(&self.record.settlement)
    }

    /// When the question was asked.
    async fn asked_at(&self) -> Timestamp {
        Timestamp(self.record.asked_at)
    }

    /// When it settled: answered, timed out, or cancelled.
    async fn settled_at(&self) -> Timestamp {
        Timestamp(self.record.at)
    }
}

/// One page of what a run asked a person.
#[derive(SimpleObject)]
pub(crate) struct InteractionConnection {
    /// The interactions on this page, in the order the run asked them.
    pub(crate) edges: Vec<InteractionEdge>,
    /// Where the next page starts.
    pub(crate) page_info: super::super::connection::PageInfo,
    /// How many the run's journal holds altogether.
    pub(crate) total: i32,
}

/// One interaction and its cursor.
#[derive(SimpleObject)]
pub(crate) struct InteractionEdge {
    /// Where this interaction sits among the run's own.
    pub(crate) cursor: Cursor,
    /// The interaction.
    pub(crate) node: Interaction,
}

/// Read one page of a run's interactions.
///
/// Shared by the field on a run and by anything else that grows one later, the
/// same way `executions::page` is.
pub(crate) async fn page(
    run_id: String,
    first: i32,
    after: Option<Cursor>,
) -> async_graphql::Result<InteractionConnection> {
    use crate::commands::serve::core::error::ServeError;
    let limit = usize::try_from(first)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| ServeError::BadRequest("`first` must be at least 1".to_string()))
        .gql()?;
    if limit > interactions::INTERACTIONS_MAX_LIMIT {
        return Err(ServeError::BadRequest(format!(
            "`first` may be at most {}, the interactions page cap",
            interactions::INTERACTIONS_MAX_LIMIT
        )))
        .gql();
    }
    let cursor = after.map(|cursor| cursor.0);
    let for_read = run_id.clone();
    let page = blocking(move || {
        let spec =
            interactions::InteractionsSpec::resolve(&for_read, Some(limit), cursor.as_deref())?;
        interactions::page(&for_read, &spec)
    })
    .await
    .gql()?;
    let total = i32::try_from(page.total).unwrap_or(i32::MAX);
    let end_cursor = page.next_cursor.clone().map(Cursor);
    Ok(InteractionConnection {
        edges: page
            .interactions
            .into_iter()
            .map(|indexed| InteractionEdge {
                // The index among the run's own interactions: stable for as
                // long as the run exists, and the only handle one needs since
                // nothing about an interaction is ever fetched separately.
                cursor: Cursor(indexed.index.to_string()),
                node: Interaction {
                    record: indexed.record,
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

#[cfg(test)]
#[path = "interaction_tests.rs"]
mod tests;
