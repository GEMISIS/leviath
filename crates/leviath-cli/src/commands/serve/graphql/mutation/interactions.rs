//! The `answerInteraction` field, and the typed answer it takes: exactly one
//! of an option's word, a choice, some text, an approval or a denial,
//! whichever kind the open ask was.
//!
//! The request names the interaction once and the answer says only what the
//! answer is, so a feedback line with an approval, or a scope on a denial, is
//! not a combination the schema can express. Whether the answer fits the ask
//! (text for a choice, an option past the end of the list) depends on the ask,
//! which the schema cannot see; the daemon checks that, and a refusal comes
//! back as `BAD_USER_INPUT` with the ask still open.

use async_graphql::{Context, Enum, ID, InputObject, OneofObject, SimpleObject};

use super::super::super::core::error::ServeError;
use super::super::super::core::spawn as spawn_core;
use super::super::super::types::AppState;
use super::super::error::graphql_error;
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

/// One of the options an ask lists, named by its word.
#[derive(Debug, InputObject)]
pub(crate) struct AnswerOptionWrite {
    /// The option's `id` from the ask's `answerOptions`: `allow`,
    /// `allow-stage`, `allow-run`, `deny`, `yes`, `no`, or a choice's own
    /// word. The option's `number` is taken as well.
    pub(crate) id: ID,
    /// On a deny, what to tell the model instead. Refused beside any other
    /// option.
    pub(crate) feedback: Option<String>,
}

/// The answer to one pending ask.
///
/// Exactly one field, and which one the request's own kind decides: an
/// option's word for any ask that lists options, a choice for a
/// multiple-choice ask, text for a free-text or edit-text one, an approval or
/// a denial for a confirm or a tool approval.
#[derive(Debug, OneofObject)]
pub(crate) enum InteractionAnswerWrite {
    /// For any ask that lists options: the option, by the word its
    /// `answerOptions` gives it. Read against the open ask, so a choice's
    /// word means the option it was made from wherever that now sits.
    #[graphql(name = "option")]
    Named(AnswerOptionWrite),
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
    /// Nothing open carries that id any more, and a run on this machine is
    /// the one that asked it: it was answered already, or it expired. Two
    /// people clicking one prompt is ordinary, so this is an outcome rather
    /// than a failure. An id no run here asked is an error coded `NOT_FOUND`.
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

/// The response an option's word names, read against the ask open under
/// `request_id`. Nothing open under it is `NotFound`, which the caller reads
/// as already settled; a question a held run asked is `Held`.
async fn option_response(
    state: &AppState,
    request_id: &str,
    named: AnswerOptionWrite,
) -> Result<leviath_core::interaction::InteractionResponse, ServeError> {
    let ask = spawn_core::open_request(state, request_id).await?;
    leviath_core::interaction::answer_with_option(&ask, &named.id, named.feedback.as_deref())
        .map_err(ServeError::BadRequest)
}

impl AnswerInteractionRequest {
    /// Turn the request into the response the daemon takes.
    async fn into_response(
        self,
        state: &AppState,
    ) -> Result<leviath_core::interaction::InteractionResponse, ServeError> {
        use leviath_core::interaction::{ApprovalScope as CoreScope, InteractionResponse};
        let request_id = self.interaction_id.to_string();
        match self.answer {
            InteractionAnswerWrite::Named(named) => {
                option_response(state, &request_id, named).await
            }
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
/// reads as `ALREADY_SETTLED` rather than as a failure. A question a held run
/// asked is an error coded `RUN_HELD`, saying what to put back: nothing can
/// answer it until the run is back, and then it reopens under a new id. An id
/// that no run on this machine asked is an error coded `NOT_FOUND`, the miss
/// REST answers 404 to.
pub(crate) async fn answer_interaction(
    ctx: &Context<'_>,
    request: AnswerInteractionRequest,
) -> async_graphql::Result<AnswerInteractionResult> {
    let state = ctx.data_unchecked::<AppState>();
    let interaction_id = request.interaction_id.clone();
    let answered = match request.into_response(state).await {
        Ok(response) => spawn_core::answer_interaction(state, response).await,
        Err(refused) => Err(refused),
    };
    match answered {
        Ok(()) => Ok(AnswerInteractionResult {
            interaction_id,
            outcome: AnswerOutcome::Accepted,
        }),
        // Nothing open under that id, and a run here asked it: answered
        // already, or expired. The other failures, a held run's question among
        // them, stay failures.
        Err(ServeError::NotFound(_)) if asked_here(&interaction_id) => {
            Ok(AnswerInteractionResult {
                interaction_id,
                outcome: AnswerOutcome::AlreadySettled,
            })
        }
        Err(ServeError::NotFound(_)) => Err(graphql_error(&ServeError::NotFound(format!(
            "No interaction '{}': no run on this machine asked it",
            interaction_id.as_str()
        )))),
        Err(other) => Err(graphql_error(&other)),
    }
}

/// Whether a run on this machine could have asked the question `id` names.
///
/// An id is `<run>-<kind>-<n>`, and a run id has dashes of its own, so each
/// dash is tried as the end of the run's part. The run's directory is the
/// test rather than its record of what it settled: an answer is written to
/// the run file a tick after the hub hands it over, and a second click inside
/// that tick is still a second click.
fn asked_here(id: &str) -> bool {
    id.match_indices('-')
        .any(|(at, _)| crate::runstate::run_dir(id.get(..at).unwrap_or_default()).is_dir())
}
