//! What the records the world collects as a run goes become in its run file.
//!
//! Each [`RunRecord`] something sends as a run goes is folded, in the world,
//! into the [`RunEvent`]s of the run's next step. A model call is recorded
//! twice, by the worker that made it and by the system that billed it, and
//! [`Answered`] pairs the two so the step holds one `Inference` per call.

use crate::runfile::record::RunRecord;
use leviath_core::JsonDoc;

use crate::spec::names::{ModelId, ModelRef, ProviderName};
use crate::state::context::ToolCallState;
use crate::state::journal::{ContextCommitState, SettledState};
use crate::state::{RunEvent, Spend, ToolResultState};

/// What pairs a model attempt that answered with the usage record that
/// bills it. The attempt is recorded by the lane that read the answer and
/// the usage by the trip that made the call, so the two land in either
/// order and in different ticks; the call is one `Inference` either way.
///
/// A usage record that names its attempt needs nothing from the attempt's
/// own record, which is then not kept waiting. One that names none (a run
/// converted from an old journal) is billed to the latest attempt on its
/// model that answered.
#[derive(Debug, Default)]
pub struct Answered {
    /// Attempts that answered, waiting for a usage record that names none.
    waiting: Vec<AnsweredAttempt>,
    /// Attempts already billed by a usage record that named them, whose own
    /// record has yet to land.
    billed: Vec<String>,
}

/// One attempt that answered: who answered, and how it said it stopped.
#[derive(Debug)]
struct AnsweredAttempt {
    provider: String,
    model: String,
    id: String,
    finish_reason: Option<String>,
}

/// A model reference from a journal's provider and model strings, when the
/// model is a valid id.
fn model_ref(provider: &str, model: &str) -> Option<ModelRef> {
    Some(ModelRef {
        provider: ProviderName::new(provider).ok(),
        model: ModelId::new(model).ok()?,
    })
}

/// The id a call's cost is recorded under when the model it names is not one
/// an id can hold: nothing, or something with spaces in it.
const UNNAMED_MODEL: &str = "unnamed-model";

/// The model a usage record was billed to. A call that was billed is always
/// recorded, so a model string no id can hold is written the nearest way one
/// can: its whitespace and control characters as underscores, cut to an id's
/// length, and [`UNNAMED_MODEL`] where that leaves nothing.
pub(crate) fn billed_model(provider: &str, model: &str) -> ModelRef {
    model_ref(provider, model).unwrap_or_else(|| {
        let spelled: String = model
            .chars()
            .map(|c| match c.is_whitespace() || c.is_control() {
                true => '_',
                false => c,
            })
            .take(64)
            .collect();
        ModelRef {
            provider: ProviderName::new(provider).ok(),
            model: ModelId::new(spelled)
                .unwrap_or_else(|_| ModelId::new(UNNAMED_MODEL).expect("a valid id")),
        }
    })
}

/// The events a journal record becomes in a run file's step, read on its
/// own: a usage record is billed to no attempt.
pub fn journal_events(record: &RunRecord) -> Vec<RunEvent> {
    journal_events_with(record, &mut Answered::default())
}

/// The events a journal record becomes in a run file's step. `answered`
/// carries the attempts that answered from one record to the next, for the
/// usage record that bills them.
pub fn journal_events_with(record: &RunRecord, answered: &mut Answered) -> Vec<RunEvent> {
    let mut events = Vec::new();
    push_events(&mut events, answered, record);
    events
}

/// Add the events `record` describes to `events`.
pub(crate) fn push_events(events: &mut Vec<RunEvent>, answered: &mut Answered, record: &RunRecord) {
    use super::recorded;
    match record {
        RunRecord::InferenceAttempt(a) => {
            push_attempt(events, answered, a);
            events.push(RunEvent::Attempt(Box::new(recorded::attempt(a))));
        }
        RunRecord::Transition(taken) => events.push(RunEvent::Transition(taken.clone())),
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
            // result settled before dispatch) ends there, as its result says.
            for c in calls {
                if let Some(result) = &c.result {
                    push_done(events, &c.id, &c.execution_id, result, None);
                }
            }
        }
        RunRecord::ToolCallsResent {
            calls,
            requested_by,
            ..
        } => events.extend(
            calls
                .iter()
                .map(|(call_id, execution_id)| RunEvent::Dispatched {
                    call_id: call_id.clone(),
                    execution_id: execution_id.clone(),
                    requested_by: requested_by.clone(),
                }),
        ),
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
            cause: (*cause).into(),
            execution_id: (!execution_id.is_empty()).then(|| execution_id.clone()),
            revision_before: revision_before.clone(),
            revision_after: revision_after.clone(),
            regions: regions.iter().map(recorded::region_commit).collect(),
        }))),
        RunRecord::InferenceUsage {
            kind,
            stage,
            iteration,
            provider,
            model,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
            cost,
            ending,
            attempt,
            finish_reason,
            provider_call_id,
            ..
        } => {
            use leviath_providers::CallCost;
            let used = billed_model(provider, model);
            // A record that names its attempt stands on its own. One that
            // names none is paired with the attempt its lane recorded.
            let answering = match attempt.is_empty() {
                true => answered.take(provider, model),
                false => {
                    answered.billed(attempt);
                    None
                }
            };
            let mut spend = Spend {
                prompt_tokens: *prompt_tokens as u64,
                completion_tokens: *completion_tokens as u64,
                cached_tokens: *cached_tokens as u64,
                cache_write_tokens: *cache_write_tokens as u64,
                priced_usd: cost.usd().unwrap_or(0.0),
                reported_calls: u32::from(matches!(cost, CallCost::Reported(_))),
                computed_calls: u32::from(matches!(cost, CallCost::Computed(_))),
                unknown: Default::default(),
            };
            if let Some(reason) = cost.unknown() {
                spend.unknown.add(reason);
            }
            events.push(RunEvent::Inference {
                attempt: match &answering {
                    Some(a) => a.id.clone(),
                    None => attempt.clone(),
                },
                model: used,
                spend,
                finish_reason: finish_reason
                    .clone()
                    .or(answering.and_then(|a| a.finish_reason)),
                kind: (*kind).into(),
                stage: crate::spec::names::StageName::new(stage).ok(),
                iteration: u32::try_from(*iteration).unwrap_or(u32::MAX),
                ending: *ending,
                provider_call_id: provider_call_id.clone(),
            });
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
        RunRecord::Continued(continued) => events.push(RunEvent::Continued(continued.clone())),
    }
}

/// How a call ended, read off its result.
///
/// Every refusal and failure a call can end in starts with a marker the
/// model reads too: `[error]` it ran and failed (and the stand-in a restart
/// gives a call it found running ends unobserved), `[blocked]` and
/// `[unavailable]` a gate or the stage refused it, and `[denied]` a
/// permission rule refused it, or a person did when the refusal came from an
/// approval prompt. A result with no marker is a call that ran and answered.
pub fn outcome_of(result: &str) -> leviath_core::execution::ToolOutcome {
    use leviath_core::execution::ToolOutcome;
    if result.starts_with(crate::restore::INTERRUPTED_TOOL_RESULT) {
        ToolOutcome::Indeterminate
    } else if result.starts_with("[error]") {
        ToolOutcome::Failed
    } else if result.starts_with("[denied] User declined") {
        ToolOutcome::Denied
    } else if result.starts_with("[denied]") {
        match result.contains("approval prompt") {
            true => ToolOutcome::Denied,
            false => ToolOutcome::Blocked,
        }
    } else if result.starts_with("[blocked]") || result.starts_with("[unavailable]") {
        ToolOutcome::Blocked
    } else {
        ToolOutcome::Succeeded
    }
}

/// A finished call: its result, and how it ended with the parts it carried.
/// A record that does not say how the call ended is read off its result.
fn push_done(
    events: &mut Vec<RunEvent>,
    call_id: &str,
    execution_id: &str,
    result: &leviath_core::region::EntryContent,
    outcome: Option<leviath_core::execution::ToolOutcome>,
) {
    use leviath_core::execution::ToolOutcome;
    let outcome = outcome.unwrap_or(outcome_of(result.as_str()));
    // A refusal is not an error of the tool's: only a call that ran and
    // failed, or one nobody saw end, is.
    let is_error = matches!(outcome, ToolOutcome::Failed | ToolOutcome::Indeterminate);
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
        outcome: Some(super::recorded::outcome(outcome)),
        parts: result
            .stored()
            .filter_map(|part| part.name.clone())
            .collect(),
    });
}

/// A model attempt: one that answered waits in `answered` for the usage
/// record that bills it, and one that failed is a line saying so.
fn push_attempt(
    events: &mut Vec<RunEvent>,
    answered: &mut Answered,
    a: &crate::runfile::record::AttemptRecord,
) {
    use crate::runfile::record::AttemptOutcome;
    match &a.outcome {
        // An attempt a usage record already billed by name waits for nothing.
        AttemptOutcome::Succeeded if answered.was_billed(&a.id) => {}
        AttemptOutcome::Succeeded => answered.waiting.push(AnsweredAttempt {
            provider: a.provider.clone(),
            model: a.model.clone(),
            id: a.id.clone(),
            finish_reason: (!a.finish_reason.is_empty()).then(|| a.finish_reason.clone()),
        }),
        AttemptOutcome::Failed { kind, .. } => events.push(RunEvent::Log(format!(
            "model call {} on {}/{} failed: {kind}",
            a.id, a.provider, a.model
        ))),
    }
}

impl Answered {
    /// Whether nothing is waiting to be paired.
    pub(crate) fn is_empty(&self) -> bool {
        self.waiting.is_empty() && self.billed.is_empty()
    }

    /// The latest attempt on `provider`/`model` that answered, taken out:
    /// the one a usage record on that model that names no attempt bills.
    fn take(&mut self, provider: &str, model: &str) -> Option<AnsweredAttempt> {
        let at = self
            .waiting
            .iter()
            .rposition(|a| a.provider == provider && a.model == model)?;
        Some(self.waiting.remove(at))
    }

    /// A usage record billed the attempt `id` by name: its own record, if it
    /// is waiting, waits no longer, and if it has yet to land it is
    /// remembered so that it does not start to.
    fn billed(&mut self, id: &str) {
        match self.waiting.iter().position(|a| a.id == id) {
            Some(at) => {
                self.waiting.remove(at);
            }
            None => self.billed.push(id.to_string()),
        }
    }

    /// Whether a usage record already billed the attempt `id` by name. It
    /// is forgotten once asked: an attempt's record lands once.
    fn was_billed(&mut self, id: &str) -> bool {
        match self.billed.iter().position(|billed| billed == id) {
            Some(at) => {
                self.billed.remove(at);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
