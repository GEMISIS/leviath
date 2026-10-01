//! What a run asked a person, read whole.
//!
//! The run file is the only record of this: the hub hands an answer to the
//! caller that was waiting and forgets it, so without the file an approved
//! tool call is indistinguishable from one no policy ever stopped, and a run
//! that paused for somebody looks exactly like one that never asked.
//!
//! A question shows up twice in a run file. The step that asked it adds it to
//! the run's open questions, with its prompt and its options, and the step
//! that settled it records an answer event carrying the settlement. This pairs
//! the two by id. A question's kind is not stored, so it is read off what it
//! carried: a settlement that grants or refuses is a tool approval, a question
//! with options is a choice, and anything else took text.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use leviath_core::interaction::{InteractionKind, Settlement};
use leviath_core::run_archive::InteractionRecord;
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
    run_file::walk(run_id, &reader, &mut |step| {
        for question in &step.after.interactions {
            asked.entry(question.id.clone()).or_insert_with(|| Asked {
                question: question.clone(),
                stage: step.after.cursor.stage.to_string(),
                at: step.delta.at,
            });
        }
        for event in &step.delta.events {
            let RunEvent::Answered { id, answer } = event else {
                continue;
            };
            records.push(record(
                id,
                answer,
                asked.remove(id),
                step.cursor.stage.as_str(),
                step.delta.at,
            ));
        }
        ControlFlow::Continue(())
    })?;
    Ok(records)
}

/// The record of the question `id`, settled by `answer` at `at`.
///
/// An answer that is not a recorded settlement is what a person typed, kept as
/// their text.
fn record(id: &str, answer: &str, asked: Option<Asked>, stage: &str, at: i64) -> InteractionRecord {
    let settlement =
        serde_json::from_str::<Settlement>(answer).unwrap_or_else(|_| Settlement::Answered {
            approved: None,
            scope: None,
            choice: None,
            text: Some(answer.to_string()),
            feedback: None,
        });
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
