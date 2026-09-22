//! The `answerInteraction` field, and the typed answer it takes: exactly one
//! of a choice, some text, an approval or a denial, whichever kind the open ask
//! was.
//!
//! The request names the interaction once and the answer says only what the
//! answer is, so a feedback line with an approval, or a scope on a denial, is
//! not a combination the schema can express. Nothing here has to refuse a
//! contradiction at run time, because none can be written down.

use async_graphql::{Context, Enum, ID, InputObject, OneofObject, SimpleObject};

use super::super::super::core::error::ServeError;
use super::super::super::core::spawn as spawn_core;
use super::super::super::types::AppState;
use super::super::error::{IntoGraphql, graphql_error};
use super::super::types::interaction::ApprovalScope;

/// Approve the call an ask is about.
#[derive(Debug, InputObject)]
pub(crate) struct ApproveWrite {
    /// How long the approval lasts. The narrowest is the default: an approval
    /// nobody asked to widen covers the one call it was given for.
    #[graphql(default_with = "ApprovalScope::Once")]
    pub(crate) scope: ApprovalScope,
}

/// Refuse the call an ask is about.
#[derive(Debug, InputObject)]
pub(crate) struct DenyWrite {
    /// What to tell the model instead. It reads this as part of the tool
    /// result, so a denial can redirect rather than only refuse.
    pub(crate) feedback: Option<String>,
}

/// The answer to one pending ask.
///
/// Exactly one field, and which one the request's own kind decides: a choice
/// for a multiple-choice ask, text for a free-text or edit-text one, an
/// approval or a denial for a confirm or a tool approval.
#[derive(Debug, OneofObject)]
pub(crate) enum InteractionAnswerWrite {
    /// For a multiple-choice ask: which option, zero-based into the request's
    /// own list.
    Choice(i32),
    /// For a free-text or edit-text ask: the words, or the edited document.
    Text(String),
    /// For a confirm or a tool approval: let it go ahead.
    Approve(ApproveWrite),
    /// For a confirm or a tool approval: refuse it.
    Deny(DenyWrite),
}

/// Which ask to answer, and what to answer it with.
#[derive(Debug, InputObject)]
pub(crate) struct AnswerInteractionRequest {
    /// The open request being answered.
    pub(crate) interaction_id: ID,
    /// Exactly one answer, of the kind the request takes.
    pub(crate) answer: InteractionAnswerWrite,
}

/// Whether an answer was the one that settled the ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum AnswerOutcome {
    /// The daemon took this answer.
    Accepted,
    /// Nothing open carries that id any more: it was answered already, or it
    /// expired. Two people clicking one prompt is ordinary, so this is an
    /// outcome rather than a failure.
    AlreadySettled,
}

/// How answering an ask landed.
#[derive(Debug, SimpleObject)]
pub(crate) struct AnswerInteractionResult {
    /// The request that was answered.
    pub(crate) interaction_id: ID,
    /// Whether this answer is the one that settled it.
    pub(crate) outcome: AnswerOutcome,
}

impl AnswerInteractionRequest {
    /// Turn the request into the response the daemon takes.
    fn into_response(self) -> Result<leviath_core::interaction::InteractionResponse, ServeError> {
        use leviath_core::interaction::{ApprovalScope as CoreScope, InteractionResponse};
        let request_id = self.interaction_id.to_string();
        match self.answer {
            InteractionAnswerWrite::Choice(index) => {
                let index = usize::try_from(index).map_err(|_| {
                    ServeError::BadRequest("`choice` cannot be negative".to_string())
                })?;
                Ok(InteractionResponse::choice(request_id, index))
            }
            InteractionAnswerWrite::Text(value) => Ok(InteractionResponse::text(request_id, value)),
            InteractionAnswerWrite::Approve(approve) => Ok(InteractionResponse::approval(
                request_id,
                true,
                CoreScope::from(approve.scope),
            )),
            InteractionAnswerWrite::Deny(deny) => {
                // A denial covers the call it was asked about and nothing
                // else: there is no such thing as denying every later call of
                // a tool for a stage, so no scope is offered or sent.
                let mut response =
                    InteractionResponse::approval(request_id, false, CoreScope::Once);
                response.feedback = deny.feedback;
                Ok(response)
            }
        }
    }
}

/// Answer a pending ask.
///
/// The first answer wins. A second answer to the same request is not an error
/// on the client's part: two people clicking one prompt is ordinary, and it
/// reads as `ALREADY_SETTLED` rather than as a failure.
pub(crate) async fn answer_interaction(
    ctx: &Context<'_>,
    request: AnswerInteractionRequest,
) -> async_graphql::Result<AnswerInteractionResult> {
    let state = ctx.data_unchecked::<AppState>();
    let response = request.into_response().gql()?;
    let interaction_id = ID::from(response.request_id.clone());
    match spawn_core::answer_interaction(state, response).await {
        Ok(()) => Ok(AnswerInteractionResult {
            interaction_id,
            outcome: AnswerOutcome::Accepted,
        }),
        // Nothing open under that id: answered already, or expired. The other
        // failures are the daemon's and stay failures.
        Err(ServeError::NotFound(_)) => Ok(AnswerInteractionResult {
            interaction_id,
            outcome: AnswerOutcome::AlreadySettled,
        }),
        Err(other) => Err(graphql_error(&other)),
    }
}
