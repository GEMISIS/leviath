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

/// Model attempts that answered, waiting for the usage record that bills
/// them. An attempt is recorded by the worker that made the call, and its
/// usage by the system that reads the answer, so the two can land in
/// different ticks; the call becomes one `Inference` when its usage lands.
#[derive(Debug, Default)]
pub struct Answered(Vec<AnsweredAttempt>);

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
            cost_usd,
            cost_reported_by_provider,
            ..
        } => {
            let Some(used) = model_ref(provider, model) else {
                return;
            };
            let attempt = answered.take(provider, model);
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
            events.push(RunEvent::Inference {
                attempt: attempt.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
                model: used,
                spend,
                finish_reason: attempt.and_then(|a| a.finish_reason),
                kind: (*kind).into(),
                stage: crate::spec::names::StageName::new(stage).ok(),
                iteration: u32::try_from(*iteration).unwrap_or(u32::MAX),
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

/// A model attempt: one that answered waits in `answered` for the usage
/// record that bills it, and one that failed is a line saying so.
fn push_attempt(
    events: &mut Vec<RunEvent>,
    answered: &mut Answered,
    a: &crate::runfile::record::AttemptRecord,
) {
    use crate::runfile::record::AttemptOutcome;
    match &a.outcome {
        AttemptOutcome::Succeeded => answered.0.push(AnsweredAttempt {
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
    /// Whether no attempt is waiting for its bill.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The latest attempt on `provider`/`model` that answered, taken out:
    /// the one a usage record on that model bills.
    fn take(&mut self, provider: &str, model: &str) -> Option<AnsweredAttempt> {
        let at = self
            .0
            .iter()
            .rposition(|a| a.provider == provider && a.model == model)?;
        Some(self.0.remove(at))
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
