//! The doctor's check on whether the daemon can still write what its runs do.
//!
//! Every other check here asks whether work can start. This one asks whether
//! the work that has already happened was recorded, which nothing else answers:
//! a daemon whose journal has been refusing writes for an hour lists its runs,
//! serves its API and reports every lane as idle.

use super::Check;
use leviath_runtime::control_socket::ControlResponse;
use leviath_runtime::host::DaemonHealth;

/// The daemon's own health, out of whatever a `List` request came back with.
///
/// `None` for an unreachable daemon or any other reply, which is the honest
/// answer: nothing was learned, and the checks that need a daemon say so
/// themselves.
pub(super) fn reported_health(reply: std::io::Result<ControlResponse>) -> Option<DaemonHealth> {
    match reply {
        Ok(ControlResponse::List { health, .. }) => Some(*health),
        Ok(_) | Err(_) => None,
    }
}

/// Whether the daemon's persistence lane has lost anything.
///
/// `None` when no daemon was asked, so `--no-daemon`, `--offline` and a server
/// that never had a client report nothing rather than guessing. A daemon that
/// has lost a write fails the check: the counters are sticky, because a record
/// that was lost does not come back, and the fix is on the filesystem.
pub(super) fn journal_check(health: Option<&DaemonHealth>) -> Option<Check> {
    let journal = &health?.journal;
    let Some(complaint) = journal.complaint() else {
        return Some(Check::ok(
            "journal",
            format!("{} record(s) written, none lost", journal.appends_attempted),
        ));
    };
    Some(Check::fail(
        "journal",
        format!(
            "{complaint}. Runs whose journal fails are stopped; check the disk for space, \
             a read-only mount, or a permission on the runs directory"
        ),
    ))
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
