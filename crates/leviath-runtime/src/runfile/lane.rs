//! The persistence lane's side of the run file.
//!
//! The persist system hands the lane a run's state as [`inspect`] reads it,
//! and the lane does the rest off the ECS thread: it keeps one
//! [`RunFileWriter`] per live run, which diffs the state against the last
//! step and appends the delta. The journal records the lane already carries
//! (model calls, tool calls, answers, messages, failovers) become the
//! [`RunEvent`]s of the next delta.
//!
//! [`inspect`]: crate::state::inspect::inspect

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use leviath_core::JsonDoc;
use leviath_core::run_archive::RunRecord;

use super::error::RunFileError;
use super::frames::OwnerFrame;
use super::writer::{CheckpointPolicy, RunFileWriter};
use crate::spec::env::CodeFiles;
use crate::spec::names::{Digest, ModelId, ModelRef, ProviderName};
use crate::spec::run_spec::RunSpec;
use crate::state::context::{PartBody, ToolCallState};
use crate::state::{MessageState, RunEvent, RunState, RunStatus, Spend, ToolResultState};

/// One run's state, to record in its run file.
#[derive(Debug)]
pub(crate) struct RunFileStep {
    /// The run.
    pub run_id: String,
    /// Its spec, for a run whose file does not exist yet.
    pub spec: Arc<RunSpec>,
    /// Its state now.
    pub state: RunState,
    /// When, in unix seconds.
    pub at: i64,
}

/// The run files the lane is writing.
pub(crate) struct RunFileLane {
    writers: HashMap<String, RunFileWriter>,
    events: HashMap<String, Vec<RunEvent>>,
    owner: OwnerFrame,
    policy: CheckpointPolicy,
}

impl RunFileLane {
    /// A lane that stamps `machine_id` and `world_id` on every file it takes
    /// over.
    pub(crate) fn new(machine_id: &str, world_id: &str) -> Self {
        Self {
            writers: HashMap::new(),
            events: HashMap::new(),
            owner: OwnerFrame {
                machine_id: machine_id.to_string(),
                world_id: world_id.to_string(),
                at: 0,
            },
            policy: CheckpointPolicy::default(),
        }
    }

    /// Close a run's file and drop what was buffered for it: the run was
    /// deleted.
    pub(crate) fn forget(&mut self, run_id: &str) {
        self.writers.remove(run_id);
        self.events.remove(run_id);
    }

    /// Keep what `record` says happened, for the run's next delta. Only for a
    /// run whose file is open: a run without one has no delta to carry it.
    pub(crate) fn note(&mut self, run_id: &str, record: &RunRecord) {
        if !self.writers.contains_key(run_id) {
            return;
        }
        let buffered = self.events.entry(run_id.to_string()).or_default();
        push_events(buffered, record);
    }

    /// Write what was noted for `run_id` since its last step as a step of its
    /// own, now, with the state as it last was: how a tool batch's record is
    /// on disk before the batch runs. `None` when the run has no file open or
    /// nothing was noted. A step that cannot be written closes the file, as
    /// [`record`](Self::record) does.
    pub(crate) async fn flush(&mut self, run_id: &str) -> Result<Option<u64>, RunFileError> {
        let events = self.events.remove(run_id).unwrap_or_default();
        let Some(mut writer) = self.writers.remove(run_id) else {
            return Ok(None);
        };
        if events.is_empty() {
            self.writers.insert(run_id.to_string(), writer);
            return Ok(None);
        }
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let job = move || {
            let state = writer.state().clone();
            let written = writer.record(state, at, events);
            (writer, written)
        };
        // Nothing on the blocking side panics: frames always encode and every
        // failure is a returned error.
        let (writer, written) = tokio::task::spawn_blocking(job)
            .await
            .expect("writing a run file step does not panic");
        if written.is_ok() {
            self.writers.insert(run_id.to_string(), writer);
        }
        written
    }

    /// Make every later write to `run_id`'s open file fail.
    #[cfg(test)]
    pub(crate) fn break_writes(&mut self, run_id: &str) {
        crate::runfile::writer::break_writes(self.writers.get_mut(run_id).expect("an open file"));
    }

    /// Record one step for its run, under `runs_dir`.
    ///
    /// The file is opened (or made) on the run's first step in this process,
    /// and closed once the run has finished. A step that cannot be written
    /// closes it too, so the next step reopens it from what is on disk.
    pub(crate) async fn record(
        &mut self,
        runs_dir: &Path,
        step: RunFileStep,
    ) -> Result<Option<u64>, RunFileError> {
        let run_dir = runs_dir.join(&step.run_id);
        let slot = self.writers.remove(&step.run_id);
        let events = self.events.remove(&step.run_id).unwrap_or_default();
        let owner = OwnerFrame {
            at: step.at,
            ..self.owner.clone()
        };
        let policy = self.policy;
        let run_id = step.run_id.clone();
        let job = move || write_step(slot, &run_dir, step, events, owner, policy);
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
    events: Vec<RunEvent>,
    owner: OwnerFrame,
    policy: CheckpointPolicy,
) -> (Option<RunFileWriter>, Result<Option<u64>, RunFileError>) {
    let path = run_dir.join(leviath_core::files::RUN_FILE);
    let finished = matches!(
        step.state.status,
        RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled
    );
    let result = writer_for(slot, &path, &step, owner, policy).and_then(|mut writer| {
        store_new_blobs(&mut writer, run_dir, &step.state)?;
        let seq = writer.record(step.state, step.at, events)?;
        Ok((writer, seq))
    });
    match result {
        Ok((writer, seq)) => ((!finished).then_some(writer), Ok(seq)),
        Err(e) => (None, Err(e)),
    }
}

/// The writer for a step: the one already open, the file on disk, or a new
/// file started from the step's own state.
fn writer_for(
    slot: Option<RunFileWriter>,
    path: &Path,
    step: &RunFileStep,
    owner: OwnerFrame,
    policy: CheckpointPolicy,
) -> Result<RunFileWriter, RunFileError> {
    if let Some(writer) = slot {
        return Ok(writer);
    }
    let mut writer = match path.exists() {
        true => RunFileWriter::open(path, policy)?,
        false => {
            // The spawn path writes the file with the run's code before the
            // run starts; a run placed without one gets a file here, from its
            // spec and the state it is in now, and no code frames.
            let initial = RunState {
                seq: 0,
                ..step.state.clone()
            };
            RunFileWriter::create(path, &step.spec, &CodeFiles::new(), &initial, policy)?
        }
    };
    writer.owner_on_next_step(owner);
    Ok(writer)
}

/// Copy into the file every stored part the state names that it does not
/// hold yet, from the run's blob directory. A part whose bytes are not there
/// (a world keeping its blobs in memory) is left out and read as missing.
fn store_new_blobs(
    writer: &mut RunFileWriter,
    run_dir: &Path,
    state: &RunState,
) -> Result<(), RunFileError> {
    let blobs = blob_dir(run_dir);
    let stored = state
        .context
        .regions
        .iter()
        .flat_map(|r| &r.entries)
        .flat_map(|e| &e.parts)
        .filter_map(|p| match &p.body {
            PartBody::Stored(blob) => Some(&blob.digest),
            PartBody::Inline(_) => None,
        });
    let mut missing: Vec<&Digest> = stored.filter(|d| !writer.has_blob(d)).collect();
    missing.dedup();
    for digest in missing {
        if let Ok(bytes) = std::fs::read(blobs.join(digest.as_str())) {
            writer.add_blob(digest, &bytes)?;
        }
    }
    Ok(())
}

fn blob_dir(run_dir: &Path) -> PathBuf {
    run_dir.join(leviath_core::files::BLOBS_DIR)
}

/// A model reference from a journal's provider and model strings, when the
/// model is a valid id.
fn model_ref(provider: &str, model: &str) -> Option<ModelRef> {
    Some(ModelRef {
        provider: ProviderName::new(provider).ok(),
        model: ModelId::new(model).ok()?,
    })
}

/// Add the events `record` describes to `events`.
pub(crate) fn push_events(events: &mut Vec<RunEvent>, record: &RunRecord) {
    use leviath_core::run_archive::AttemptOutcome;
    match record {
        RunRecord::InferenceAttempt(a) => match &a.outcome {
            AttemptOutcome::Succeeded => events.extend(model_ref(&a.provider, &a.model).map(
                |model| RunEvent::Inference {
                    attempt: a.id.clone(),
                    model,
                    spend: Spend::default(),
                    finish_reason: (!a.finish_reason.is_empty()).then(|| a.finish_reason.clone()),
                },
            )),
            AttemptOutcome::Failed { kind, .. } => events.push(RunEvent::Log(format!(
                "model call {} on {}/{} failed: {kind}",
                a.id, a.provider, a.model
            ))),
        },
        RunRecord::InferenceUsage {
            provider,
            model,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
            cost_usd,
            cost_reported_by_provider,
            ..
        } => {
            let Some(used) = model_ref(provider, model) else {
                return;
            };
            let reported = *cost_reported_by_provider == Some(true);
            let spend = Spend {
                prompt_tokens: *prompt_tokens as u64,
                completion_tokens: *completion_tokens as u64,
                cached_tokens: *cached_tokens as u64,
                cache_write_tokens: *cache_write_tokens as u64,
                priced_usd: cost_usd.unwrap_or(0.0),
                reported_calls: u32::from(cost_usd.is_some() && reported),
                computed_calls: u32::from(cost_usd.is_some() && !reported),
                unpriced_calls: u32::from(cost_usd.is_none()),
            };
            add_spend(events, used, spend);
        }
        RunRecord::InferenceFailover(f) => {
            if let (Some(from), Some(to)) = (
                model_ref(&f.from_provider, &f.from_model),
                model_ref(&f.to_provider, &f.to_model),
            ) {
                events.push(RunEvent::Failover {
                    from,
                    to,
                    reason: f.reason.clone(),
                });
            }
        }
        RunRecord::ToolBatch { calls, .. } => {
            events.extend(calls.iter().map(|c| {
                RunEvent::ToolStarted(ToolCallState {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    args: JsonDoc::parse(&c.arguments)
                        .unwrap_or(JsonDoc::new(c.arguments.clone().into())),
                    thought_signature: c.thought_signature.clone(),
                })
            }));
        }
        RunRecord::ToolCallDone {
            call_id,
            result,
            outcome,
            ..
        } => {
            let is_error = match outcome {
                Some(o) => *o != leviath_core::execution::ToolOutcome::Succeeded,
                None => result.as_str().starts_with("[error]"),
            };
            events.push(RunEvent::ToolFinished {
                call_id: call_id.clone(),
                result: ToolResultState {
                    text: result.as_str().to_string(),
                    is_error,
                },
                millis: 0,
            });
        }
        RunRecord::Interaction {
            request_id,
            settlement,
            ..
        } => events.push(RunEvent::Answered {
            id: request_id.clone(),
            answer: serde_json::to_string(settlement).expect("a settlement is plain data"),
        }),
        RunRecord::Message { message, .. } => events.push(RunEvent::Message(MessageState {
            from: message.role.clone(),
            text: message.content.clone(),
            region: None,
        })),
        _ => {}
    }
}

/// Put a call's spend on the model call it belongs to: the latest one on the
/// same model with nothing spent yet, or a new one when there is none.
fn add_spend(events: &mut Vec<RunEvent>, used: ModelRef, spend: Spend) {
    let open = events.iter_mut().rev().find_map(|e| match e {
        RunEvent::Inference {
            model, spend: s, ..
        } if *model == used && *s == Spend::default() => Some(s),
        _ => None,
    });
    match open {
        Some(slot) => *slot = spend,
        None => events.push(RunEvent::Inference {
            attempt: String::new(),
            model: used,
            spend,
            finish_reason: None,
        }),
    }
}

#[cfg(test)]
#[path = "lane_tests.rs"]
mod tests;
