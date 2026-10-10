//! A run directory's run file, as the CLI and the dashboard read it.
//!
//! A run is one file, `run.lvr`: its spec, the state it started in, a step per
//! change and a checkpoint every so often. The listings, the context and stage
//! views, the history the dashboard steps through and the edges its explorer
//! lights all start here.
//!
//! The file is read from its bytes and never opened for writing. A run the
//! daemon is still appending to can end in a half-written frame; the reader
//! drops that tail from what it holds, and opening the file the way the daemon
//! does would cut it off the file underneath the writer.
//!
//! What a run is now (its record, its context window, its stage ledger, its
//! answer) is read from the two ends of the file, its spec and its last
//! checkpoint, with [`RunFileTail`]: listings read that for every run they
//! show. Only a reader of the steps themselves opens the whole file.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use leviath_core::run_meta::{ContextSnapshot, StageRecord};
use leviath_runtime::runfile::history::RunPoint;
use leviath_runtime::runfile::{RunFileReader, RunFileTail};
use leviath_runtime::secret_store::SecretStore;
use leviath_runtime::spec::launch::Secret;
use leviath_runtime::spec::names::SecretRef;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::RunState;

/// Where the run file of the run in `dir` is.
pub(crate) fn path_in(dir: &Path) -> PathBuf {
    dir.join(leviath_core::files::RUN_FILE)
}

/// What a run's webhook is signed with: `None` for a webhook with no
/// secret, the secret as the store beside the runs holds it, or the
/// reference the spec names it by when the store no longer holds it.
pub(crate) type CallbackSecret = Option<Result<Secret, SecretRef>>;

/// The secret the webhook of the run in `dir`, resolved to `spec`, is signed
/// with. The spec only names it; the secret is read from the store.
pub(crate) fn callback_secret(dir: &Path, spec: &RunSpec) -> CallbackSecret {
    let reference = spec.delivery.callback_secret()?;
    let held = SecretStore::of_run_dir(dir).read(reference);
    Some(held.ok_or_else(|| reference.clone()))
}

/// Whether `dir` holds a run file, told from its first few bytes alone: a
/// directory from an earlier release holds an older file under the same
/// name, and reading the whole of it to find that out is what a listing of a
/// thousand such runs cannot afford. A run file in an earlier binary layout
/// passes too, and is refused by its fingerprint when it is read.
pub(crate) fn is_run_file(dir: &Path) -> bool {
    use std::io::Read;
    let magic = leviath_runtime::runfile::codec::MAGIC;
    let mut head = vec![0u8; magic.len()];
    std::fs::File::open(path_in(dir))
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == magic)
}

/// The run file in `dir`, read from its bytes.
pub(crate) fn open_in(dir: &Path) -> anyhow::Result<RunFileReader> {
    Ok(RunFileReader::read(&path_in(dir))?)
}

/// The run in `dir` as of its last step, read from the two ends of its file.
pub(crate) fn tail_in(dir: &Path) -> anyhow::Result<RunFileTail> {
    Ok(RunFileTail::read(&path_in(dir))?)
}

/// The spec of the run in `dir`, read from the front of its file.
pub(crate) fn spec_in(dir: &Path) -> anyhow::Result<RunSpec> {
    Ok(leviath_runtime::runfile::read_spec(&path_in(dir))?)
}

/// The run file in `dir` and the state of its last step, or `None` when the
/// directory has no run file this build reads.
#[cfg(test)]
pub(crate) fn latest_in(dir: &Path) -> Option<(RunFileReader, RunState)> {
    let reader = open_in(dir).ok()?;
    let state = reader.latest_state().ok()?;
    Some((reader, state))
}

/// The run's context window as of its last step. `None` for a run that never
/// held one.
pub(crate) fn context_in(dir: &Path) -> Option<ContextSnapshot> {
    let tail = tail_in(dir).ok()?;
    if tail.state.context.regions.is_empty() {
        return None;
    }
    Some(leviath_runtime::runfile::context_snapshot(
        &tail.spec,
        &tail.state,
    ))
}

/// The run's per-stage ledger as of its last step.
pub(crate) fn stages_in(dir: &Path) -> Option<Vec<StageRecord>> {
    let tail = tail_in(dir).ok()?;
    Some(leviath_runtime::runfile::stage_records(
        &tail.spec,
        &tail.state,
    ))
}

/// What a run's file says about how it got where it is: the window at every
/// step that changed it, and every edge it took.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RunHistory {
    /// The window as the run started, then after each step that changed it,
    /// oldest first.
    pub(crate) points: Vec<RunPoint>,
    /// Each edge the run took, as `(from, to)`, in the order it took them.
    /// `None` when nothing says which edge a move followed.
    pub(crate) transitions: Option<Vec<(String, String)>>,
}

/// One point of a run's history.
fn point(spec: &RunSpec, state: &RunState, at: i64) -> RunPoint {
    RunPoint {
        meta: leviath_runtime::runfile::summary_of(spec, state, at),
        context: leviath_runtime::runfile::context_snapshot(spec, state),
        at,
    }
}

/// The history of the run in `dir`, read off its run file: the run replayed
/// from the state it started in, one step at a time. `None` when the
/// directory has no run file this build reads, or one of its steps does not
/// decode.
pub(crate) fn history_in(dir: &Path) -> Option<RunHistory> {
    let mut points = Vec::new();
    let transitions = walk_history_in(dir, |point| points.push(point))?;
    Some(RunHistory {
        points,
        transitions: Some(transitions),
    })
}

/// [`history_in`] a point at a time: each is handed to `each` as the walk
/// reaches it, and is the caller's to keep or drop. A reader that shows the
/// points one by one then never holds them all, and every point holds the
/// whole window, which on a long run is hundreds of copies of the largest
/// thing it records. Returns the edges the run took; `None`, with nothing
/// handed over, where [`history_in`] answers `None`.
pub(crate) fn walk_history_in(
    dir: &Path,
    mut each: impl FnMut(RunPoint),
) -> Option<Vec<(String, String)>> {
    let reader = open_in(dir).ok()?;
    let spec = reader.spec();
    let mut state = reader.state_at(0).ok()?;
    let deltas = reader.deltas(state.seq + 1, reader.last_seq()).ok()?;
    each(point(spec, &state, spec.created_at));
    let mut transitions = Vec::new();
    for delta in deltas {
        for taken in delta.transitions() {
            transitions.push((taken.from.to_string(), taken.to.to_string()));
        }
        let _ = leviath_runtime::runfile::history::step_points(&delta, &mut state, &mut |at| {
            each(point(spec, at, delta.at));
            ControlFlow::Continue(())
        });
    }
    Some(transitions)
}

#[cfg(test)]
#[path = "run_file_reads_tests.rs"]
pub(crate) mod tests;
