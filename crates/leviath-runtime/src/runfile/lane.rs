//! The persistence lane's side of the run file.
//!
//! The world decides every step: the persist system hands the lane a run's
//! state as [`inspect`] reads it, with the events it made of what happened
//! since the last step (or only those events, when nothing about the state
//! needs recording). The lane writes it off the ECS thread: it keeps one
//! [`RunFileWriter`] per live run, which diffs the state against the last
//! step it wrote and appends the delta.
//!
//! The one thing a step gets from the lane is the files beside the run file
//! that the lane itself wrote (the answer, the stage logs, the taint audit).
//! Whether a write landed and how long a log is after an append are facts
//! only the writer has, so the lane names those files in the step's
//! [`RunFiles`] as it writes them, on top of what its last step named.
//!
//! [`inspect`]: crate::state::inspect::inspect

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::error::RunFileError;
use super::frames::OwnerFrame;
use super::writer::{CheckpointPolicy, RunFileWriter};
use crate::spec::env::CodeFiles;
use crate::spec::run_spec::RunSpec;
use crate::state::files::{RunFiles, StageFile};
use crate::state::{RunEvent, RunState, RunStatus};

/// One step of a run, as the world decided it, to record in its run file.
#[derive(Debug)]
pub(crate) struct RunFileStep {
    /// The run.
    pub run_id: String,
    /// Its state now, with its spec for a run whose file does not exist yet.
    /// `None` when nothing about the state is to be recorded: the step is the
    /// last one's state with `events` on it.
    pub now: Option<RunNow>,
    /// When, in unix seconds.
    pub at: i64,
    /// What happened since the run's last step.
    pub events: Vec<RunEvent>,
    /// Who waits to hear where the step landed, fired once it is written or
    /// lost.
    pub acks: Vec<tokio::sync::oneshot::Sender<crate::persistence_bridge::Appended>>,
}

/// A run's state now, and the spec it was placed from.
#[derive(Debug)]
pub(crate) struct RunNow {
    /// Its spec, for a run whose file does not exist yet.
    pub spec: Arc<RunSpec>,
    /// Its state.
    pub state: RunState,
}

impl RunFileStep {
    /// A step that records `events` and nothing new about the state.
    pub(crate) fn events(run_id: &str, at: i64, events: Vec<RunEvent>) -> Self {
        Self {
            run_id: run_id.to_string(),
            now: None,
            at,
            events,
            acks: Vec::new(),
        }
    }
}

/// The run files the lane is writing.
pub(crate) struct RunFileLane {
    writers: HashMap<String, RunFileWriter>,
    /// Per run, the files written beside its run file since its last step.
    written: HashMap<String, RunFiles>,
    owner: OwnerFrame,
    policy: CheckpointPolicy,
}

impl RunFileLane {
    /// A lane that stamps `machine_id` and `world_id` on every file it takes
    /// over.
    pub(crate) fn new(machine_id: &str, world_id: &str) -> Self {
        Self {
            writers: HashMap::new(),
            written: HashMap::new(),
            owner: OwnerFrame {
                machine_id: machine_id.to_string(),
                world_id: world_id.to_string(),
                at: 0,
            },
            policy: CheckpointPolicy::default(),
        }
    }

    /// Close a run's file and drop what was written for it: the run was
    /// deleted.
    pub(crate) fn forget(&mut self, run_id: &str) {
        self.writers.remove(run_id);
        self.written.remove(run_id);
    }

    /// Note that the files `named` names were written beside `run_id`'s run
    /// file, for its next step to name.
    pub(crate) fn wrote(&mut self, run_id: &str, named: RunFiles) {
        if named == RunFiles::default() {
            return;
        }
        let slot = self.written.entry(run_id.to_string()).or_default();
        overlay(slot, named);
    }

    /// The runs with files written beside a closed run file (one that
    /// finished), which no step will come to name. A live run's files are
    /// named by its next step.
    pub(crate) fn noted(&self) -> Vec<String> {
        let mut runs: Vec<String> = self
            .written
            .keys()
            .filter(|run_id| !self.writers.contains_key(*run_id))
            .cloned()
            .collect();
        runs.sort();
        runs
    }

    /// The runs with files written beside them that no step has named yet.
    pub(crate) fn unnamed(&self) -> Vec<String> {
        let mut runs: Vec<String> = self.written.keys().cloned().collect();
        runs.sort();
        runs
    }

    /// Make every later write to `run_id`'s open file fail.
    #[cfg(test)]
    pub(crate) fn break_writes(&mut self, run_id: &str) {
        crate::runfile::writer::break_writes(self.writers.get_mut(run_id).expect("an open file"));
    }

    /// Record one step for its run, under `runs_dir`, with the files written
    /// beside its run file since its last step named.
    ///
    /// A step with a state opens the file (or makes it) on the run's first
    /// step in this process, and closes it once the run has finished. A step
    /// of events only goes on the file that is open, or the one on disk (a
    /// finished run's is closed); `None` when the run has no file, or when
    /// the step holds nothing to write. A step that cannot be written closes
    /// the file too, so the next step reopens it from what is on disk.
    pub(crate) async fn record(
        &mut self,
        runs_dir: &Path,
        step: RunFileStep,
    ) -> Result<Option<u64>, RunFileError> {
        let run_dir = runs_dir.join(&step.run_id);
        let slot = self.writers.remove(&step.run_id);
        let written = self.written.remove(&step.run_id).unwrap_or_default();
        if step.now.is_none() && step.events.is_empty() && written == RunFiles::default() {
            if let Some(writer) = slot {
                self.writers.insert(step.run_id, writer);
            }
            return Ok(None);
        }
        let owner = OwnerFrame {
            at: step.at,
            ..self.owner.clone()
        };
        let policy = self.policy;
        let run_id = step.run_id.clone();
        let job = move || write_step(slot, &run_dir, step, written, owner, policy);
        // Nothing on the blocking side panics: frames always encode and every
        // failure is a returned error.
        let (writer, written) = tokio::task::spawn_blocking(job)
            .await
            .expect("writing a run file step does not panic");
        if let Some(writer) = writer {
            self.writers.insert(run_id, writer);
        }
        written
    }
}

/// Write one step on a blocking thread. Returns the writer to keep, if any.
fn write_step(
    slot: Option<RunFileWriter>,
    run_dir: &Path,
    step: RunFileStep,
    written: RunFiles,
    owner: OwnerFrame,
    policy: CheckpointPolicy,
) -> (Option<RunFileWriter>, Result<Option<u64>, RunFileError>) {
    let path = run_dir.join(leviath_core::files::RUN_FILE);
    let opened = match (&step.now, slot) {
        (_, Some(writer)) => Ok(Some(writer)),
        (Some(now), None) => writer_for(&path, now, owner, policy).map(Some),
        (None, None) if path.is_file() => RunFileWriter::open(&path, policy).map(Some),
        (None, None) => Ok(None),
    };
    let result = opened.and_then(|writer| {
        let Some(mut writer) = writer else {
            return Ok((None, None));
        };
        let mut state = match step.now {
            Some(now) => now.state,
            None => writer.state().clone(),
        };
        state.files = writer.state().files.clone();
        overlay(&mut state.files, written);
        warn_missing_blobs(run_dir, writer.state(), &state);
        let finished = matches!(
            state.status,
            RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled
        );
        let seq = writer.record(state, step.at, step.events)?;
        Ok(((!finished).then_some(writer), seq))
    });
    match result {
        Ok((writer, seq)) => (writer, Ok(seq)),
        Err(e) => (None, Err(e)),
    }
}

/// The writer for a run whose file is not open: the file on disk, or a new
/// file started from the run's state now.
fn writer_for(
    path: &Path,
    now: &RunNow,
    owner: OwnerFrame,
    policy: CheckpointPolicy,
) -> Result<RunFileWriter, RunFileError> {
    let mut writer = match path.exists() {
        true => RunFileWriter::open(path, policy)?,
        false => {
            // The spawn path writes the file with the run's code before the
            // run starts; a run placed without one gets a file here, from its
            // spec and the state it is in now, and no code frames. The files
            // beside it are named by its first step, as every later one is.
            let initial = RunState {
                seq: 0,
                files: RunFiles::default(),
                blobs: Vec::new(),
                ..now.state.clone()
            };
            RunFileWriter::create(path, &now.spec, &CodeFiles::new(), &initial, policy)?
        }
    };
    writer.owner_on_next_step(owner);
    Ok(writer)
}

/// `named` on top of `files`: each file it names replaces the one `files`
/// named in its place.
fn overlay(files: &mut RunFiles, named: RunFiles) {
    if let Some(answer) = named.final_output {
        files.final_output = Some(answer);
    }
    for stage in named.stages {
        let which = [
            (StageFile::Output, stage.output),
            (StageFile::Logs, stage.logs),
            (StageFile::TaintAudit, stage.taint_audit),
        ];
        for (which, file) in which {
            if let Some(file) = file {
                files.set_stage_file(stage.index, which, file);
            }
        }
    }
}

/// Say, once per part, when a stored part a step names first has no file
/// under `blobs/`: a reader of the run is refused it by name.
fn warn_missing_blobs(run_dir: &Path, prior: &RunState, state: &RunState) {
    for blob in state.blobs.iter().skip(prior.blobs.len()) {
        let path = super::reader::blob_path(run_dir, &blob.digest);
        if !path.is_file() {
            let shown = path.display().to_string();
            tracing::warn!(path = %shown, "a stored part the run names has no file beside its run file");
        }
    }
}

#[cfg(test)]
#[path = "lane_tests.rs"]
mod tests;
