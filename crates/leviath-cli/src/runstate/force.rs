//! Forcing a run that nothing drives any more to a terminal state on disk.
//! Split out of `runstate.rs` for size.

use super::*;

/// The outcome of forcing a run to a terminal state on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceCancelOutcome {
    /// The run was live on disk and is now recorded terminal.
    Terminated,
    /// The run was already finished; nothing was written.
    AlreadyTerminal,
    /// No run directory with that id exists.
    NoSuchRun,
    /// The directory exists but its metadata could not be rewritten.
    WriteFailed,
}

impl ForceCancelOutcome {
    /// Whether the id named a run at all - i.e. whether the cancel had a target,
    /// regardless of whether it needed to write anything.
    pub fn found_run(&self) -> bool {
        !matches!(self, Self::NoSuchRun)
    }
}

/// Force a run's on-disk metadata to `Cancelled`, in the runs dir resolved from
/// the environment. See [`force_cancel_in`].
pub fn force_cancel(run_id: &str) -> ForceCancelOutcome {
    force_cancel_in(&run_dir(run_id), leviath_core::duration::now_secs())
}

/// Force the run in `run_dir` to `Cancelled`, stamping `updated_at` with `now`.
///
/// This is the floor under every kill path: it needs nothing but the filesystem,
/// so it works for a run the daemon can't rebuild (blueprint deleted, metadata
/// corrupt, died mid-spawn) and for a run whose daemon is gone entirely. Both
/// the daemon's force-terminator seam and `lev cancel --force` route here so
/// there is one definition of "terminated on disk".
///
/// A directory whose `meta.json` is missing or unparseable still gets a minimal
/// `Cancelled` record written: such a run is otherwise skipped by `list_runs`,
/// which makes it invisible *and* permanent.
pub fn force_cancel_in(run_dir: &Path, now: i64) -> ForceCancelOutcome {
    force_terminal_in(run_dir, RunStatus::Cancelled, None, now)
}

/// Force the run in `run_dir` to `Error` with `message`, stamping `updated_at`.
///
/// For the spawn that never became a run. The spawner stakes out the run
/// directory and writes a `Starting` placeholder *before* building the agent, so
/// a spawn that fails leaves something to diagnose - but `Starting` is not
/// terminal, so that placeholder went on claiming the run was alive for ever,
/// showing up in `lev ps` and the dashboard with nothing behind it (issue #190).
/// Recording the failure where the placeholder is turns it into an answer.
pub fn force_error_in(run_dir: &Path, message: &str, now: i64) -> ForceCancelOutcome {
    force_terminal_in(run_dir, RunStatus::Error, Some(message.to_string()), now)
}

/// Rewrite the run in `run_dir` to a terminal `status`, attaching `error` when
/// there is something to say. Shared by [`force_cancel_in`] and
/// [`force_error_in`] so "terminated on disk" has one implementation.
fn force_terminal_in(
    run_dir: &Path,
    status: RunStatus,
    error: Option<String>,
    now: i64,
) -> ForceCancelOutcome {
    if !run_dir.is_dir() {
        return ForceCancelOutcome::NoSuchRun;
    }
    let run_id = run_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let terminated = match read_meta_from(run_dir) {
        Ok(meta) if is_terminal_status(&meta.status) => return ForceCancelOutcome::AlreadyTerminal,
        Ok(meta) => RunMeta {
            status,
            updated_at: now,
            // Keep whatever the run had already recorded when there is nothing
            // new to say (the cancel path).
            error: error.clone().or(meta.error),
            ..meta
        },
        // Unreadable metadata: synthesize just enough to record the outcome. The
        // run id is the directory name, which is the one field always recoverable.
        Err(_) => RunMeta {
            status,
            updated_at: now,
            error: Some(
                error
                    .clone()
                    .unwrap_or_else(|| "run metadata was unreadable; cancelled".to_string()),
            ),
            ..RunMeta::new(
                run_id.clone(),
                run_id,
                String::new(),
                String::new(),
                None,
                String::new(),
                0,
            )
        },
    };
    match write_meta_to(run_dir, &terminated) {
        Ok(()) => ForceCancelOutcome::Terminated,
        Err(e) => {
            // Formatted outside the macro: a method call inside a `%field` is
            // only evaluated when a subscriber visits the value, so it would go
            // unexercised under the tests' no-op subscriber.
            let path = run_dir.display().to_string();
            tracing::warn!(
                run_dir = %path,
                error = %e,
                "could not force a run to a terminal state on disk"
            );
            ForceCancelOutcome::WriteFailed
        }
    }
}
