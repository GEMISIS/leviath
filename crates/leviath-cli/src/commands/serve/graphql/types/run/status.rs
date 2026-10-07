//! The lifecycle states a run moves through, as a filterable enum.

use async_graphql::Enum;
use leviath_graphql_derive::mirror;

/// The lifecycle states a run moves through.
///
/// One state per variant of the daemon's own `RunStatus`, so the two cannot
/// drift: the conversion below is exhaustive and a new daemon state will not
/// compile until it is named here.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RunStatus {
    /// Spawned but not yet running.
    Starting,
    /// Moving: inferring, calling tools, transitioning.
    Running,
    /// Parked: on a prompt somebody has to answer, or holding for children.
    WaitingInput,
    /// Paused by `lev pause`; resumes with `lev resume`.
    Paused,
    /// Finished with an answer or a terminal state.
    Complete,
    /// Every required stage finished but the run still accepts messages.
    CompleteInteractive,
    /// Unrecoverable failure; `Run.error` carries what went wrong.
    Error,
    /// Stopped from outside. Nothing went wrong; somebody decided.
    Cancelled,
    /// A state this build has no name for, which is what a newer daemon's new
    /// state looks like from here.
    ///
    /// Only ever reached through a live frame, where the status arrives as the
    /// daemon's own word rather than as a value this build chose. A run read
    /// from disk is parsed into one of the states above or not read at all.
    Unknown,
}

impl From<&leviath_core::run_meta::RunStatus> for RunStatus {
    fn from(status: &leviath_core::run_meta::RunStatus) -> Self {
        use leviath_core::run_meta::RunStatus as Daemon;
        match status {
            Daemon::Starting => Self::Starting,
            Daemon::Running => Self::Running,
            Daemon::WaitingInput => Self::WaitingInput,
            Daemon::Paused => Self::Paused,
            Daemon::Complete => Self::Complete,
            Daemon::CompleteInteractive => Self::CompleteInteractive,
            Daemon::Error => Self::Error,
            Daemon::Cancelled => Self::Cancelled,
        }
    }
}

impl RunStatus {
    /// The state one of the daemon's own words names.
    ///
    /// The live frames carry the word rather than a parsed state, and the
    /// daemon on the other end of the socket may be a newer build than this
    /// one. [`Unknown`](Self::Unknown) is what a word this build does not know
    /// becomes, so one new state does not cost a subscriber the whole frame.
    pub(crate) fn from_wire(word: &str) -> Self {
        use leviath_core::run_meta::RunStatus as Daemon;
        [
            Daemon::Starting,
            Daemon::Running,
            Daemon::WaitingInput,
            Daemon::Paused,
            Daemon::Complete,
            Daemon::CompleteInteractive,
            Daemon::Error,
            Daemon::Cancelled,
        ]
        .iter()
        .find(|status| status.wire() == word)
        .map_or(Self::Unknown, Self::from)
    }
}
