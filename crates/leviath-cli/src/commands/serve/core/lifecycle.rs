//! Pause, resume and cancel: the three ways a person moves a run from
//! outside it.
//!
//! Only the daemon changes a run, so each of these is a control-socket
//! request. What lives here is everything around that: refusing an act
//! against a run that has already finished, and turning the daemon's reply
//! into one failure both surfaces render.

use leviath_runtime::control_socket::{ControlRequest, ControlResponse};

use super::super::types::AppState;
use super::error::ServeError;
use crate::runstate;

/// Whether a run in this state is finished, so nothing can move it.
///
/// `CompleteInteractive` is deliberately not one of these: the run finished
/// its required stages and still takes messages, so cancelling it is a
/// reasonable thing to ask for.
pub(crate) fn is_terminal(status: &leviath_core::run_meta::RunStatus) -> bool {
    use leviath_core::run_meta::RunStatus as Status;
    matches!(status, Status::Complete | Status::Error | Status::Cancelled)
}

/// What to ask the daemon to do with a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Park the run. It keeps its place and resumes where it stopped.
    Pause,
    /// Un-park a paused run.
    Resume,
    /// Stop the run, and its sub-agents with it.
    Cancel,
}

impl Action {
    /// The control request that carries out this action.
    fn request(self, run_id: String) -> ControlRequest {
        match self {
            Self::Pause => ControlRequest::Pause { run_id },
            Self::Resume => ControlRequest::Resume { run_id },
            Self::Cancel => ControlRequest::Cancel { run_id },
        }
    }

    /// What the daemon's "no" means for this action.
    ///
    /// The daemon answers one `ok: false` for every reason it declined, so
    /// this names the reasons rather than claiming the run does not exist.
    fn refusal(self, id: &str) -> String {
        match self {
            Self::Pause => format!("Agent run '{id}' not found or not pausable"),
            Self::Resume => format!("Agent run '{id}' not found or not paused"),
            Self::Cancel => format!("Agent run '{id}' not found"),
        }
    }

    /// The word this action goes in a conflict message as.
    fn verb(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Resume => "resumed",
            Self::Cancel => "cancelled",
        }
    }
}

/// Carry out a lifecycle action against one run.
///
/// A finished run is refused with a conflict rather than acted on. Before
/// this, a pause against a finished run went to the daemon, came back
/// `ok: false`, and was reported as "not found" - which reads as a wrong run
/// id and sends whoever is debugging it looking for a run that is sitting
/// right there in the listing.
///
/// The check is made against the run's own record, and only when that record
/// reads: a run the daemon just accepted may not have been persisted yet, and
/// the daemon is the authority on what is live. So an unreadable record is
/// not an answer here, it is a reason to go and ask.
pub(crate) async fn act(state: &AppState, id: &str, action: Action) -> Result<(), ServeError> {
    if let Ok(meta) = runstate::read_meta(id)
        && is_terminal(&meta.status)
    {
        return Err(ServeError::Conflict(format!(
            "Agent run '{id}' has finished ({}), so it cannot be {}",
            meta.status.wire(),
            action.verb()
        )));
    }

    match state.control.request(&action.request(id.to_string())).await {
        Ok(ControlResponse::Ok { ok: true }) => Ok(()),
        Ok(ControlResponse::Ok { ok: false }) => Err(ServeError::NotFound(action.refusal(id))),
        Ok(other) => Err(ServeError::unexpected_reply(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
