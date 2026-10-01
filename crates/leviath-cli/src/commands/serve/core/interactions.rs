//! What a run asked a person, read whole.
//!
//! The run file is the only record of this: the hub hands an answer to the
//! caller that was waiting and forgets it, so without the file an approved
//! tool call is indistinguishable from one no policy ever stopped, and a run
//! that paused for somebody looks exactly like one that never asked.
//!
//! The step that settled a question records it whole: its kind, the tool an
//! approval was for, the prompt, the stage it was asked in, when, and how it
//! settled. A file written without that (one converted from an older layout)
//! has the question twice instead: the step that asked it added it to the
//! run's open questions, and the step that settled it records an answer event
//! carrying the settlement. That pairing is by id, and the kind is read off
//! what the question carried: a settlement that grants or refuses is a tool
//! approval, a question with options is a choice, and anything else took text.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use leviath_core::interaction::{InteractionKind, Settlement};
use leviath_core::run_archive::InteractionRecord;
use leviath_runtime::state::journal::{QuestionKind, SettledState};
use leviath_runtime::state::{OpenInteraction, RunEvent};

use super::error::ServeError;
use super::run_file;

/// Largest page of interactions the GraphQL listing takes.
///
/// The same cap as the run listing and the executions listing: an interaction
/// is a handful of fields, none of them a file body.
pub(crate) const INTERACTIONS_MAX_LIMIT: usize = 200;

/// A question as it was asked: the step's time and stage, and what it said.
struct Asked {
    question: OpenInteraction,
    stage: String,
    at: i64,
}

/// Every question a run's file records it settling, in the order they
/// settled.
///
/// A run with no run file is not an error here: a run that never asked anybody
/// anything did nothing of the kind, and an empty list says so.
pub(crate) fn read(run_id: &str) -> Result<Vec<InteractionRecord>, ServeError> {
    let Some(reader) = run_file::open(run_id)? else {
        return Ok(Vec::new());
    };
    let mut asked: BTreeMap<String, Asked> = BTreeMap::new();
    let mut records = Vec::new();
    let mut whole = Vec::new();
    run_file::walk(run_id, &reader, &mut |step| {
        for question in &step.after.interactions {
            asked.entry(question.id.clone()).or_insert_with(|| Asked {
                question: question.clone(),
                stage: step.after.cursor.stage.to_string(),
                at: step.delta.at,
            });
        }
        for event in &step.delta.events {
            match event {
                RunEvent::Settled(settled) => whole.push(kept(settled, step.delta.at)),
                RunEvent::Answered { id, answer } => records.push(record(
                    id,
                    answer,
                    asked.remove(id),
                    step.cursor.stage.as_str(),
                    step.delta.at,
                )),
                _ => {}
            }
        }
        ControlFlow::Continue(())
    })?;
    Ok(match whole.is_empty() {
        true => records,
        false => whole,
    })
}

/// A question the run file kept whole, as the journal's record of it.
fn kept(settled: &SettledState, at: i64) -> InteractionRecord {
    InteractionRecord {
        request_id: settled.id.clone(),
        kind: match settled.kind {
            QuestionKind::FreeText => InteractionKind::FreeText,
            QuestionKind::MultipleChoice => InteractionKind::MultipleChoice,
            QuestionKind::Confirm => InteractionKind::Confirm,
            QuestionKind::ToolApproval => InteractionKind::ToolApproval,
            QuestionKind::EditText => InteractionKind::EditText,
        },
        tool: settled.tool.clone(),
        prompt: settled.prompt.clone(),
        stage: settled.stage.clone(),
        settlement: settlement_of(&settled.settlement),
        asked_at: settled.asked_at,
        at,
    }
}

/// A recorded settlement, or what a person typed, kept as their text.
fn settlement_of(answer: &str) -> Settlement {
    serde_json::from_str::<Settlement>(answer).unwrap_or_else(|_| Settlement::Answered {
        approved: None,
        scope: None,
        choice: None,
        text: Some(answer.to_string()),
        feedback: None,
    })
}

/// The record of the question `id`, settled by `answer` at `at`.
///
/// An answer that is not a recorded settlement is what a person typed, kept as
/// their text.
fn record(id: &str, answer: &str, asked: Option<Asked>, stage: &str, at: i64) -> InteractionRecord {
    let settlement = settlement_of(answer);
    let (prompt, options, stage, asked_at) = match asked {
        Some(a) => (a.question.prompt, a.question.options, a.stage, a.at),
        None => (String::new(), Vec::new(), stage.to_string(), at),
    };
    let approval = matches!(
        settlement,
        Settlement::Answered {
            approved: Some(_),
            ..
        }
    );
    let kind = match (approval, options.is_empty()) {
        (true, _) => InteractionKind::ToolApproval,
        (false, false) => InteractionKind::MultipleChoice,
        (false, true) => InteractionKind::FreeText,
    };
    InteractionRecord {
        request_id: id.to_string(),
        kind,
        tool: None,
        prompt,
        stage,
        settlement,
        asked_at,
        at,
    }
}

#[cfg(test)]
#[path = "interactions_tests.rs"]
mod tests;
