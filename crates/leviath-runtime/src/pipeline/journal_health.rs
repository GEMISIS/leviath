//! Failing a run whose journal cannot be written.
//!
//! A run's journal (`run.lvr`) is the record of what it asked for, what it
//! attempted and what came back. Everything downstream reads it: `lev dash`, a
//! post-mortem, a resume after a restart. A run that keeps going while its
//! journal refuses writes is a run building a history that lies - the tool calls
//! happened, the answer was spent on, and nothing on disk says so.
//!
//! So the persistence lane names the runs whose journal it lost a write for, and
//! this fails them. The lane never waits for that to happen: it records the loss
//! and moves to its next message, and the failure travels as an ordinary
//! run-status change on the next tick, the same as any other run that ends.
//!
//! A run whose **directory is gone** never arrives here. Deleting a run makes
//! every later write for it a no-op on purpose, and the lane tells the two apart
//! before it records anything - see `may_write` in
//! [`persistence_bridge`](crate::persistence_bridge).

use super::*;
use crate::persist_stats::PersistLaneStats;

/// The persistence lane's health, shared with the lane itself.
///
/// A resource so the world can both report it (`lev ps`, `lev doctor`, the
/// GraphQL schema) and drain the runs it has named.
#[derive(Resource)]
pub(crate) struct PersistLaneHealth(pub Arc<PersistLaneStats>);

/// What `fail_runs_with_unwritable_journals` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type JournalFailureQuery = (
    Entity,
    &'static RunMetadata,
    &'static mut AgentState,
    Option<&'static mut StageIoBuffer>,
);

/// Fail every run the persistence lane could not write a journal record for.
///
/// Straight to a terminal `Error`, not through the stage's `error` edge: an
/// `error_recovery` stage is more work done on the same unwritable journal, and
/// what this run needs is to stop. The message names the file, because the fix is
/// nearly always on the filesystem - a full disk, a read-only mount, a
/// permission somebody changed - and nothing else in the run will say which path
/// to look at.
///
/// Runs the world no longer holds are dropped rather than kept: the lane names a
/// run once, and a run that has finished or been unloaded has nothing left to
/// fail.
///
/// Early in the tick, ahead of the systems that would drive the run further. The
/// lane records a loss after the tick that dispatched the write has ended, so
/// this is the first chance anybody has to act on it - and acting on it before
/// the collect and transition systems is what stops the run taking one more turn
/// it cannot record. A run that reached a terminal status in between keeps that
/// status: it is over, and turning a finished run into a failed one after the
/// fact would be its own kind of lie.
pub(crate) fn fail_runs_with_unwritable_journals(
    health: Res<PersistLaneHealth>,
    mut agents: Query<JournalFailureQuery>,
) {
    crate::tick_scope::clear();
    let lost = health.0.take_unwritable();
    if lost.is_empty() {
        return;
    }
    for (entity, md, mut state, buffer) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let Some(error) = lost.iter().find(|e| e.run_id == md.run_id) else {
            continue;
        };
        if super::is_terminal_status(&state.status) {
            continue;
        }
        let message = format!(
            "the run's journal could not be written: {} - {}. Stopping here rather than \
             going on with a history that cannot record what the run did",
            error.path, error.message
        );
        tracing::error!(
            run_id = %md.run_id,
            path = %error.path,
            error = %error.message,
            "failing a run whose journal cannot be written"
        );
        if let Some(mut buffer) = buffer {
            buffer.logs.push((0, format!("[journal] {message}")));
        }
        state.status = AgentStatus::Error { message };
    }
}

#[cfg(test)]
#[path = "journal_health_tests.rs"]
mod tests;
