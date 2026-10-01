//! Forcing a run that nothing drives any more to a terminal state on disk.
//! Split out of `runstate.rs` for size.

use super::*;

/// The outcome of forcing a run to a terminal state on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForceCancelOutcome {
    /// The run was live on disk and is now recorded terminal.
    Terminated,
    /// The run was already finished; nothing was written.
    AlreadyTerminal,
    /// No run directory with that id exists.
    NoSuchRun,
    /// The directory exists but the cancel could not be recorded in its run
    /// file.
    WriteFailed,
}

impl ForceCancelOutcome {
    /// Whether the id named a run at all - i.e. whether the cancel had a target,
    /// regardless of whether it needed to write anything.
    pub(crate) fn found_run(&self) -> bool {
        !matches!(self, Self::NoSuchRun)
    }
}

/// Force a run's on-disk metadata to `Cancelled`, in the runs dir resolved from
/// the environment. See [`force_cancel_in`].
pub(crate) fn force_cancel(run_id: &str) -> ForceCancelOutcome {
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
/// The cancel is recorded as the last step of the run's file. A directory
/// whose run file is missing or will not open cannot record one.
pub(crate) fn force_cancel_in(run_dir: &Path, now: i64) -> ForceCancelOutcome {
    use leviath_runtime::state::{PipelinePhase, RunStatus as State};
    if !run_dir.is_dir() {
        return ForceCancelOutcome::NoSuchRun;
    }
    let path = run_file::path_in(run_dir);
    let recorded = leviath_runtime::runfile::RunFileWriter::open(&path, Default::default())
        .and_then(|mut writer| {
            let mut next = writer.state().clone();
            if matches!(
                next.status,
                State::Complete | State::Error(_) | State::Cancelled
            ) {
                return Ok(ForceCancelOutcome::AlreadyTerminal);
            }
            next.status = State::Cancelled;
            next.phase = PipelinePhase::Done;
            writer
                .record(next, now, Vec::new())
                .map(|_| ForceCancelOutcome::Terminated)
        });
    recorded.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not record a cancel in a run's file");
        ForceCancelOutcome::WriteFailed
    })
}
