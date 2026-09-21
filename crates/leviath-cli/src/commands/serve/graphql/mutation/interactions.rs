//! The `answerInteraction` field, and the typed answer it takes: exactly one
//! of a choice, some text, or an approval, whichever kind the open ask was.

use async_graphql::{Context, InputObject, OneofObject, SimpleObject};

use super::super::super::core::error::ServeError;
use super::super::super::core::spawn as spawn_core;
use super::super::super::types::AppState;
use super::super::error::{IntoGraphql, graphql_error};

/// Answer a multiple-choice ask.
#[derive(InputObject)]
pub(crate) struct AnswerChoiceInput {
    /// The open request being answered.
    pub(crate) request_id: String,
    /// Which option, zero-based into the request's own list.
    pub(crate) choice_index: i32,
}

/// Answer a free-text or edit-text ask.
#[derive(InputObject)]
pub(crate) struct AnswerTextInput {
    /// The open request being answered.
    pub(crate) request_id: String,
    /// The words, or the edited document.
    pub(crate) value: String,
}

/// How long a tool approval lasts once it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum ApprovalScope {
    /// This call only.
    Once,
    /// Every call of this tool for the rest of the stage.
    Stage,
    /// Every call of this tool for the rest of the run.
    Session,
}

impl From<ApprovalScope> for leviath_core::interaction::ApprovalScope {
    fn from(scope: ApprovalScope) -> Self {
        use leviath_core::interaction::ApprovalScope as Core;
        match scope {
            ApprovalScope::Once => Core::Once,
            ApprovalScope::Stage => Core::Stage,
            ApprovalScope::Session => Core::Run,
        }
    }
}

/// Answer a confirm or a tool approval.
#[derive(InputObject)]
pub(crate) struct AnswerApprovalInput {
    /// The open request being answered.
    pub(crate) request_id: String,
    /// Whether the call is approved.
    pub(crate) approved: bool,
    /// How long the approval lasts. Meaningless on a denial.
    pub(crate) scope: Option<ApprovalScope>,
    /// What to tell the model instead, on a denial. It reads this as part of
    /// the tool result, so a denial can redirect rather than only refuse.
    /// Refused beside an approval, where there is nothing to redirect.
    pub(crate) feedback: Option<String>,
}

/// The answer to one pending ask.
///
/// Exactly one variant, and which one the request's own kind decides: a choice
/// for a multiple-choice ask, text for a free-text or edit-text one, an
/// approval for a confirm or a tool approval. One variant at a time is the
/// schema's own rule, so there is no combination to get wrong.
#[derive(OneofObject)]
pub(crate) enum AnswerInteractionInput {
    /// For a multiple-choice ask.
    Choice(AnswerChoiceInput),
    /// For a free-text or edit-text ask.
    Text(AnswerTextInput),
    /// For a confirm or a tool approval.
    Approval(AnswerApprovalInput),
}

impl AnswerInteractionInput {
    /// Turn the answer into what the daemon takes, refusing the one
    /// combination that reads as a mistake.
    pub(super) fn into_response(
        self,
    ) -> Result<leviath_core::interaction::InteractionResponse, ServeError> {
        use leviath_core::interaction::{ApprovalScope as CoreScope, InteractionResponse};
        match self {
            Self::Choice(choice) => {
                let index = usize::try_from(choice.choice_index).map_err(|_| {
                    ServeError::BadRequest("`choiceIndex` cannot be negative".to_string())
                })?;
                Ok(InteractionResponse::choice(choice.request_id, index))
            }
            Self::Text(text) => Ok(InteractionResponse::text(text.request_id, text.value)),
            Self::Approval(approval) => {
                if approval.approved && approval.feedback.is_some() {
                    return Err(ServeError::BadRequest(
                        "`feedback` goes with a denial: it is what the model reads instead of \
                         the call, and there is nothing to redirect on an approval"
                            .to_string(),
                    ));
                }
                // The scope only means anything on an approval, and `ONCE` is
                // what a request that does not say wants: the narrowest.
                let scope = approval
                    .scope
                    .map(CoreScope::from)
                    .unwrap_or(CoreScope::Once);
                let mut response =
                    InteractionResponse::approval(approval.request_id, approval.approved, scope);
                response.feedback = approval.feedback;
                Ok(response)
            }
        }
    }
}

/// How answering an ask landed.
#[derive(Debug, SimpleObject)]
pub(crate) struct InteractionPayload {
    /// The request that was answered.
    pub(crate) request_id: String,
    /// True when the daemon took the answer. False when no open request
    /// carries that id any more: it was answered already, or it expired.
    pub(crate) accepted: bool,
}

/// Answer a pending ask.
///
/// The first answer wins. A second answer to the same request is not an
/// error on the client's part: two people clicking one prompt is ordinary,
/// and it reads as `accepted: false` rather than as a failure.
pub(crate) async fn answer_interaction(
    ctx: &Context<'_>,
    input: AnswerInteractionInput,
) -> async_graphql::Result<InteractionPayload> {
    let state = ctx.data_unchecked::<AppState>();
    let response = input.into_response().gql()?;
    let request_id = response.request_id.clone();
    match spawn_core::answer_interaction(state, response).await {
        Ok(()) => Ok(InteractionPayload {
            request_id,
            accepted: true,
        }),
        // Nothing open under that id: answered already, or expired. The
        // other failures are the daemon's and stay failures.
        Err(ServeError::NotFound(_)) => Ok(InteractionPayload {
            request_id,
            accepted: false,
        }),
        Err(other) => Err(graphql_error(&other)),
    }
}
