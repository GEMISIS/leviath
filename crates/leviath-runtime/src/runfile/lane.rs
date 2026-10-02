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

use crate::runfile::record::RunRecord;
use leviath_core::JsonDoc;

use super::error::RunFileError;
use super::frames::OwnerFrame;
use super::writer::{CheckpointPolicy, RunFileWriter};
use crate::spec::env::CodeFiles;
use crate::spec::names::{Digest, ModelId, ModelRef, ProviderName};
use crate::spec::run_spec::RunSpec;
use crate::state::context::{PartBody, ToolCallState};
use crate::state::journal::{ContextCommitState, SettledState};
use crate::state::{RunEvent, RunState, RunStatus, Spend, ToolResultState};

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

    /// Keep what `record` says happened, for the run's next step: the next
    /// state it records, or the [`flush`](Self::flush) that writes what is
    /// noted on its own.
    pub(crate) fn note(&mut self, run_id: &str, record: &RunRecord) {
        let buffered = self.events.entry(run_id.to_string()).or_default();
        push_events(buffered, record);
    }

    /// The runs with something noted that no step has written yet.
    pub(crate) fn noted(&self) -> Vec<String> {
        let mut runs: Vec<String> = self
            .events
            .iter()
            .filter(|(_, events)| !events.is_empty())
            .map(|(run_id, _)| run_id.clone())
            .collect();
        runs.sort();
        runs
    }

    /// Write what was noted for `run_id` since its last step as a step of its
    /// own, now, with the state as it last was: how a tool batch's record is
    /// on disk before the batch runs, and how what happened after a run's
    /// last change of state (a finished run's last lines) reaches its file.
    ///
    /// The file is the one open, or the one on disk under `runs_dir` (a
    /// finished run's is closed). `None`, with what was noted dropped, when
    /// the run has no file or nothing was noted. A step that cannot be
    /// written closes the file, as [`record`](Self::record) does.
    pub(crate) async fn flush(
        &mut self,
        runs_dir: &Path,
        run_id: &str,
    ) -> Result<Option<u64>, RunFileError> {
        let events = self.events.remove(run_id).unwrap_or_default();
        let slot = self.writers.remove(run_id);
        if events.is_empty() {
            if let Some(writer) = slot {
                self.writers.insert(run_id.to_string(), writer);
            }
            return Ok(None);
        }
        let path = runs_dir.join(run_id).join(leviath_core::files::RUN_FILE);
        if slot.is_none() && !path.is_file() {
            return Ok(None);
        }
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        let policy = self.policy;
        let job = move || {
            let opened = match slot {
                Some(writer) => Ok(writer),
                None => RunFileWriter::open(&path, policy),
            };
            opened.and_then(|mut writer| {
                let mut state = writer.state().clone();
                fold_finished(&mut state, &events, Started::Elsewhere);
                let finished = matches!(
                    state.status,
                    RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled
                );
                let seq = writer.record(state, at, events)?;
                Ok(((!finished).then_some(writer), seq))
            })
        };
        // Nothing on the blocking side panics: frames always encode and every
        // failure is a returned error.
        let written = tokio::task::spawn_blocking(job)
            .await
            .expect("writing a run file step does not panic");
        written.map(|(writer, seq)| {
            if let Some(writer) = writer {
                self.writers.insert(run_id.to_string(), writer);
            }
            seq
        })
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
    mut step: RunFileStep,
    events: Vec<RunEvent>,
    owner: OwnerFrame,
    policy: CheckpointPolicy,
) -> (Option<RunFileWriter>, Result<Option<u64>, RunFileError>) {
    let path = run_dir.join(leviath_core::files::RUN_FILE);
    let finished = matches!(
        step.state.status,
        RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled
    );
    fold_finished(&mut step.state, &events, Started::InThisState);
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

/// Where the state a step records stands against a batch the step's events
/// start.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Started {
    /// The state is the run's state now, so a batch the events start is the
    /// one it holds.
    InThisState,
    /// The state is the one last written, from before the events: a batch
    /// they start is not the one it holds.
    Elsewhere,
}

/// Put each call the step's events say finished into the batch `state` holds
/// as done, so the file holds a finished call as done the moment it lands
/// rather than when the whole batch ends. Only results that came back after
/// the last batch the events start count, and only for a call of the batch
/// in `state`. A result already there is kept.
fn fold_finished(state: &mut RunState, events: &[RunEvent], started: Started) {
    let Some(batch) = state.pending.as_mut() else {
        return;
    };
    let last_start = events
        .iter()
        .rposition(|e| matches!(e, RunEvent::ToolStarted(_)));
    if last_start.is_some() && started == Started::Elsewhere {
        return;
    }
    let after = last_start.map_or(0, |i| i + 1);
    for event in events.iter().skip(after) {
        if let RunEvent::ToolFinished {
            call_id, result, ..
        } = event
            && batch.calls.iter().any(|c| c.id == *call_id)
        {
            batch
                .done
                .entry(call_id.clone())
                .or_insert_with(|| result.clone());
        }
    }
}

/// A model reference from a journal's provider and model strings, when the
/// model is a valid id.
fn model_ref(provider: &str, model: &str) -> Option<ModelRef> {
    Some(ModelRef {
        provider: ProviderName::new(provider).ok(),
        model: ModelId::new(model).ok()?,
    })
}

/// The events a journal record becomes in a run file's step.
pub fn journal_events(record: &RunRecord) -> Vec<RunEvent> {
    let mut events = Vec::new();
    push_events(&mut events, record);
    events
}

/// Add the events `record` describes to `events`.
pub(crate) fn push_events(events: &mut Vec<RunEvent>, record: &RunRecord) {
    use super::recorded;
    match record {
        RunRecord::InferenceAttempt(a) => {
            push_attempt(events, a);
            events.push(RunEvent::Attempt(Box::new(recorded::attempt(a))));
        }
        RunRecord::ToolBatch {
            calls,
            requested_by,
            ..
        } => {
            events.extend(calls.iter().map(|c| {
                RunEvent::ToolStarted(ToolCallState {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    args: JsonDoc::parse(&c.arguments)
                        .unwrap_or(JsonDoc::new(c.arguments.clone().into())),
                    thought_signature: c.thought_signature.clone(),
                })
            }));
            events.extend(calls.iter().map(|c| RunEvent::Dispatched {
                call_id: c.id.clone(),
                execution_id: c.execution_id.clone(),
                requested_by: requested_by.clone(),
            }));
            // A call that came back in the batch record itself (a refusal, a
            // result settled before dispatch) ends there. One a restart found
            // still running ended unobserved: its effect may or may not have
            // landed.
            for c in calls {
                if let Some(result) = &c.result {
                    let outcome = result
                        .as_str()
                        .starts_with(crate::restore::INTERRUPTED_TOOL_RESULT)
                        .then_some(leviath_core::execution::ToolOutcome::Indeterminate);
                    push_done(events, &c.id, &c.execution_id, result, outcome);
                }
            }
        }
        RunRecord::ToolCallDone {
            call_id,
            execution_id,
            result,
            outcome,
            ..
        } => push_done(events, call_id, execution_id, result, *outcome),
        RunRecord::ArtifactsProduced {
            execution_id,
            artifacts,
            ..
        } => events.push(RunEvent::Artifacts {
            execution_id: execution_id.clone(),
            artifacts: artifacts.iter().map(recorded::artifact).collect(),
        }),
        RunRecord::Interaction {
            request_id,
            kind,
            tool,
            prompt,
            stage,
            settlement,
            asked_at,
            ..
        } => {
            let answer = serde_json::to_string(settlement).expect("a settlement is plain data");
            events.push(RunEvent::Answered {
                id: request_id.clone(),
                answer: answer.clone(),
            });
            events.push(RunEvent::Settled(Box::new(SettledState {
                id: request_id.clone(),
                kind: recorded::question_kind(kind),
                tool: tool.clone(),
                prompt: prompt.clone(),
                stage: stage.clone(),
                settlement: answer,
                asked_at: *asked_at,
            })));
        }
        RunRecord::ContextTransaction {
            revision_before,
            revision_after,
            cause,
            regions,
            execution_id,
            ..
        } => events.push(RunEvent::ContextCommitted(Box::new(ContextCommitState {
            cause: recorded::cause(*cause),
            execution_id: (!execution_id.is_empty()).then(|| execution_id.clone()),
            revision_before: revision_before.clone(),
            revision_after: revision_after.clone(),
            regions: regions.iter().map(recorded::region_commit).collect(),
        }))),
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
    }
}

/// A finished call: its result, and how it ended with the parts it carried.
fn push_done(
    events: &mut Vec<RunEvent>,
    call_id: &str,
    execution_id: &str,
    result: &leviath_core::region::EntryContent,
    outcome: Option<leviath_core::execution::ToolOutcome>,
) {
    let is_error = match outcome {
        Some(o) => o != leviath_core::execution::ToolOutcome::Succeeded,
        None => result.as_str().starts_with("[error]"),
    };
    events.push(RunEvent::ToolFinished {
        call_id: call_id.to_string(),
        result: ToolResultState {
            text: result.as_str().to_string(),
            is_error,
        },
        millis: 0,
    });
    events.push(RunEvent::Completed {
        call_id: call_id.to_string(),
        execution_id: execution_id.to_string(),
        outcome: outcome.map(super::recorded::outcome),
        parts: result
            .stored()
            .filter_map(|part| part.name.clone())
            .collect(),
    });
}

/// A model call's spend-bearing event: an answer, or a line saying it failed.
fn push_attempt(events: &mut Vec<RunEvent>, a: &crate::runfile::record::AttemptRecord) {
    use crate::runfile::record::AttemptOutcome;
    match &a.outcome {
        AttemptOutcome::Succeeded => {
            events.extend(
                model_ref(&a.provider, &a.model).map(|model| RunEvent::Inference {
                    attempt: a.id.clone(),
                    model,
                    spend: Spend::default(),
                    finish_reason: (!a.finish_reason.is_empty()).then(|| a.finish_reason.clone()),
                }),
            )
        }
        AttemptOutcome::Failed { kind, .. } => events.push(RunEvent::Log(format!(
            "model call {} on {}/{} failed: {kind}",
            a.id, a.provider, a.model
        ))),
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
