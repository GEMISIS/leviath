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

    /// Whether a run in this state got there by this action.
    ///
    /// Only a cancel produces a terminal state. A run found `complete` after a
    /// cancel finished on its own; a run found `cancelled` after a cancel did
    /// what it was asked.
    fn produces(self, status: &leviath_core::run_meta::RunStatus) -> bool {
        use leviath_core::run_meta::RunStatus as Status;
        matches!((self, status), (Self::Cancel, Status::Cancelled))
    }
}

/// Which finishes count when the record is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counts {
    /// Any finish at all. What to ask before an act: a run that is over is
    /// over, whoever ended it.
    AnyFinish,
    /// Only a finish this action did not bring about. What to ask after the
    /// daemon has answered, where a cancel's own `cancelled` is the act
    /// landing rather than the run ending by itself.
    NotThisActs,
}

/// The finish the run's record shows right now, if it shows one that counts.
///
/// Read fresh at each call rather than handed down: the whole point of asking
/// twice is that the answer can have changed between the two.
///
/// Only when the record reads. A run the daemon just accepted may not have been
/// persisted yet, and the daemon is the authority on what is live, so an
/// unreadable record is not an answer here, it is a reason to go and ask.
fn finish_showing(
    id: &str,
    action: Action,
    counts: Counts,
) -> Option<leviath_core::run_meta::RunStatus> {
    let meta = runstate::read_meta(id).ok()?;
    if !is_terminal(&meta.status) {
        return None;
    }
    match counts == Counts::NotThisActs && action.produces(&meta.status) {
        true => None,
        false => Some(meta.status),
    }
}

/// What a run that is already over answers every act with.
fn already_over(id: &str, status: &leviath_core::run_meta::RunStatus, action: Action) -> String {
    format!(
        "Agent run '{id}' has finished ({}), so it cannot be {}",
        status.wire(),
        action.verb()
    )
}

/// Carry out a lifecycle action against one run.
///
/// A finished run is refused with a conflict rather than acted on. It is a
/// conflict and not a miss because the daemon answers one `ok: false` for every
/// reason it declines: reported as "not found", a pause against a finished run
/// reads as a wrong run id and sends whoever is debugging it looking for a run
/// that is sitting right there in the listing.
///
/// The record is read twice, once on each side of the control request, and each
/// read is only an answer when the record reads at all. A run that finishes in
/// between was live at the first read and over by the second, and the daemon's
/// cancel is unconditional once it arrives: a run it can no longer hold in its
/// world is forced onto `cancelled` on disk, which is a finished run's
/// `complete` overwritten. The second read is what turns that race into the
/// conflict it is, for both surfaces and for every act, and what a sweep then
/// reports as `ALREADY_FINISHED` instead of as a run it moved.
pub(crate) async fn act(state: &AppState, id: &str, action: Action) -> Result<(), ServeError> {
    if let Some(status) = finish_showing(id, action, Counts::AnyFinish) {
        return Err(ServeError::Conflict(already_over(id, &status, action)));
    }

    match state.control.request(&action.request(id.to_string())).await {
        Ok(ControlResponse::Ok { ok: true }) => {
            match finish_showing(id, action, Counts::NotThisActs) {
                Some(status) => Err(ServeError::Conflict(already_over(id, &status, action))),
                None => Ok(()),
            }
        }
        Ok(ControlResponse::Ok { ok: false }) => Err(ServeError::NotFound(action.refusal(id))),
        Ok(other) => Err(ServeError::unexpected_reply(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
