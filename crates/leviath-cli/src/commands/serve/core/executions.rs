//! What a run actually did, read whole.
//!
//! The run file is the only record of this, and it is read step by step rather
//! than from the run's last state: the point is what happened, including the
//! calls that failed or were refused, which the last state no longer shows.
//!
//! A call starts as an event in the step that dispatched it and ends as an
//! event in the step its result came back in. The dispatch names the
//! execution's own id and the model call that asked for it, the ending says
//! how it ended and which stored parts its result carried, and the files an
//! execution produced follow as an event of their own. A file written without
//! those (one converted from an older layout) names a call by the provider's
//! call id alone, so that id stands for the execution's, and an ending is read
//! off whether the result was an error.
//!
//! The payloads stay out of the listing on purpose. Every execution is a
//! handful of facts plus the arguments the model sent, and a result is fetched
//! for the one execution somebody opened, by the position the listing reported.
//! Otherwise a request for "what did this run do" would read every file body
//! the run ever produced.

use std::ops::ControlFlow;

use leviath_core::execution::ToolOutcome;
use leviath_core::run_archive::Execution;
use leviath_runtime::state::RunEvent;
use leviath_runtime::state::journal::{ArtifactState, ToolOutcomeState};

use super::error::ServeError;
use super::run_file;

/// Largest page of executions the GraphQL listing takes.
///
/// The same cap as the run listing: an execution is a handful of fields plus the
/// arguments the model sent, which is the same order of size as a run's summary.
pub(crate) const EXECUTIONS_MAX_LIMIT: usize = 200;

/// Most bytes of one result to serve inline.
///
/// A result can be a whole file, and a whole file belongs behind a byte route
/// rather than inside a JSON answer. What is served is the head, because the head
/// is what a person reads first and what a failure usually says.
pub(crate) const RESULT_MAX_BYTES: usize = 64 * 1024;

/// Every execution a run's file records, in dispatch order.
///
/// A run with no run file is not an error here: a run that never dispatched a
/// tool did nothing, and an empty list says so. A run id that names nothing is
/// the caller's problem to catch, which it does by reading the run first.
pub(crate) fn read(run_id: &str) -> Result<Vec<Execution>, ServeError> {
    let Some(reader) = run_file::open(run_id)? else {
        return Ok(Vec::new());
    };
    let stages = &reader.spec().graph.stages;
    let mut executions: Vec<Execution> = Vec::new();
    run_file::walk(run_id, &reader, &mut |step| {
        let cursor = &step.cursor;
        let requested_by = step
            .delta
            .events
            .iter()
            .find_map(|e| match e {
                RunEvent::Inference { attempt, .. } => Some(attempt.clone()),
                _ => None,
            })
            .unwrap_or_default();
        for event in &step.delta.events {
            match event {
                RunEvent::ToolStarted(call) => executions.push(Execution {
                    id: call.id.clone(),
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                    arguments: match call.args.value() {
                        serde_json::Value::String(text) => text.clone(),
                        _ => call.args.to_text(),
                    },
                    stage_index: stages
                        .iter()
                        .position(|s| s.name == cursor.stage)
                        .unwrap_or_default(),
                    iteration: cursor.iteration as usize,
                    visit_id: cursor.visit.clone(),
                    requested_by: requested_by.clone(),
                    artifacts: Vec::new(),
                    dispatched_at: step.delta.at,
                    position: step.delta.seq,
                    ended_at: None,
                    result_position: None,
                    outcome: None,
                }),
                RunEvent::ToolFinished {
                    call_id, result, ..
                } => {
                    let open = executions
                        .iter_mut()
                        .rev()
                        .find(|e| e.call_id == *call_id && e.ended_at.is_none());
                    if let Some(execution) = open {
                        execution.ended_at = Some(step.delta.at);
                        execution.result_position = Some(step.delta.seq);
                        execution.outcome = Some(match result.is_error {
                            true => ToolOutcome::Failed,
                            false => ToolOutcome::Succeeded,
                        });
                    }
                }
                RunEvent::Dispatched {
                    call_id,
                    execution_id,
                    requested_by,
                } => {
                    let open = executions
                        .iter_mut()
                        .rev()
                        .find(|e| e.call_id == *call_id && e.id == *call_id);
                    if let Some(execution) = open {
                        execution.id = execution_id.clone();
                        execution.requested_by = requested_by.clone();
                    }
                }
                RunEvent::Completed {
                    call_id,
                    execution_id,
                    outcome,
                    ..
                } => {
                    if let Some(execution) = executions
                        .iter_mut()
                        .rev()
                        .find(|e| e.id == *execution_id && e.call_id == *call_id)
                    {
                        execution.outcome = outcome.map(tool_outcome);
                    }
                }
                RunEvent::Artifacts {
                    execution_id,
                    artifacts,
                } => {
                    if let Some(execution) =
                        executions.iter_mut().rev().find(|e| e.id == *execution_id)
                    {
                        execution
                            .artifacts
                            .extend(artifacts.iter().filter_map(artifact));
                    }
                }
                _ => {}
            }
        }
        ControlFlow::Continue(())
    })?;
    Ok(executions)
}

/// How an execution ended, as the journal names it.
fn tool_outcome(outcome: ToolOutcomeState) -> ToolOutcome {
    match outcome {
        ToolOutcomeState::Succeeded => ToolOutcome::Succeeded,
        ToolOutcomeState::Failed => ToolOutcome::Failed,
        ToolOutcomeState::Blocked => ToolOutcome::Blocked,
        ToolOutcomeState::Denied => ToolOutcome::Denied,
        ToolOutcomeState::Indeterminate => ToolOutcome::Indeterminate,
    }
}

/// A file an execution produced, as the journal names it. One whose type no
/// longer reads as a mime type is left out rather than given one it never had.
fn artifact(a: &ArtifactState) -> Option<leviath_core::output::Artifact> {
    Some(leviath_core::output::Artifact {
        name: a.name.clone(),
        path: a.path.clone(),
        mime_type: leviath_core::mime::MimeType::parse(&a.mime_type).ok()?,
        size: a.size,
        sha256: a.sha256.clone(),
    })
}

/// One execution's result, as far as it fits.
///
/// The position is the step the listing reported the result at, and a position
/// that names no result answers `None` rather than erroring: a run a caller
/// read a moment ago can have been deleted since.
pub(crate) fn result(
    run_id: &str,
    position: u64,
    call_id: &str,
) -> Result<Option<ResultText>, ServeError> {
    let Some(reader) = run_file::open(run_id)? else {
        return Ok(None);
    };
    let steps = reader
        .deltas(position, position)
        .map_err(|e| run_file::unreadable(run_id, &e))?;
    let events: Vec<&RunEvent> = steps.iter().flat_map(|step| &step.events).collect();
    // The stored parts the result carried, by name, where its ending kept
    // them. A part with no name is referenced by its hash, which the run's
    // own parts listing carries.
    let parts: Vec<String> = events
        .iter()
        .find_map(|event| match event {
            RunEvent::Completed {
                call_id: done,
                parts,
                ..
            } if done == call_id => Some(parts.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let found = events
        .iter()
        .find_map(|event| match event {
            RunEvent::ToolFinished {
                call_id: done,
                result,
                ..
            } if done == call_id => Some(&result.text),
            _ => None,
        })
        .map(|text| ResultText {
            bytes: text.len(),
            text: leviath_core::text::truncate_at_boundary(text, RESULT_MAX_BYTES).to_string(),
            parts,
        });
    Ok(found)
}
/// One execution's result, cut to what is reasonable to send.
#[derive(Debug)]
pub(crate) struct ResultText {
    /// The text, up to the cap.
    pub(crate) text: String,
    /// How many bytes the whole result is, which is larger than `text` when it
    /// was cut.
    pub(crate) bytes: usize,
    /// The stored parts the result carried, by name. The bytes themselves are
    /// fetched from the run's parts, where they already live.
    pub(crate) parts: Vec<String>,
}

impl ResultText {
    /// Whether the text is only the head of the result.
    pub(crate) fn truncated(&self) -> bool {
        self.bytes > self.text.len()
    }
}

#[cfg(test)]
mod tests {
    use leviath_core::JsonDoc;
    use leviath_core::execution::ToolOutcome;
    use leviath_runtime::spec::names::ModelRef;
    use leviath_runtime::state::context::ToolCallState;
    use leviath_runtime::state::{RunEvent, ToolResultState};

    use super::super::run_file::tests::{garbage, recorded, step};
    use super::{RESULT_MAX_BYTES, read, result};

    fn started(id: &str, args: serde_json::Value) -> RunEvent {
        RunEvent::ToolStarted(ToolCallState {
            id: id.to_string(),
            name: "read_file".into(),
            args: JsonDoc::new(args),
            thought_signature: None,
        })
    }

    fn finished(id: &str, text: &str, is_error: bool) -> RunEvent {
        RunEvent::ToolFinished {
            call_id: id.to_string(),
            result: ToolResultState {
                text: text.to_string(),
                is_error,
            },
            millis: 3,
        }
    }

    #[tokio::test]
    async fn each_call_reads_back_with_where_and_how_it_ended() {
        crate::runstate::with_isolated_runs_dir_async("executions-read", |_d| async move {
            let run_id = recorded();
            step(
                &run_id,
                10,
                vec![
                    RunEvent::Inference {
                        attempt: "a1".into(),
                        model: ModelRef::parse("anthropic/m").unwrap(),
                        spend: Default::default(),
                        finish_reason: None,
                    },
                    started("c1", serde_json::json!({ "path": "a.rs" })),
                    started("c2", serde_json::json!("not json at all")),
                    started("c3", serde_json::json!({})),
                ],
                |s| s.cursor.iteration += 1,
            );
            let long = "x".repeat(RESULT_MAX_BYTES + 10);
            step(
                &run_id,
                20,
                vec![
                    finished("c1", "fn a() {}", false),
                    finished("c2", &long, true),
                    finished("ghost", "for a call never started", false),
                ],
                |s| s.cursor.iteration += 1,
            );

            let ran = read(&run_id).unwrap();
            assert_eq!(ran.len(), 3);
            let first = &ran[0];
            assert_eq!((first.id.as_str(), first.call_id.as_str()), ("c1", "c1"));
            assert_eq!(first.tool, "read_file");
            assert_eq!(first.arguments, r#"{"path":"a.rs"}"#);
            assert_eq!(first.requested_by, "a1");
            assert_eq!(first.position, 1);
            assert_eq!(first.dispatched_at, 10);
            assert_eq!(first.ended_at, Some(20));
            assert_eq!(first.result_position, Some(2));
            assert_eq!(first.outcome, Some(ToolOutcome::Succeeded));
            assert_eq!(ran[1].arguments, "not json at all");
            assert_eq!(ran[1].outcome, Some(ToolOutcome::Failed));
            assert!(ran[2].unfinished());

            let text = result(&run_id, 2, "c1").unwrap().unwrap();
            assert_eq!(text.text, "fn a() {}");
            assert!(!text.truncated());
            let cut = result(&run_id, 2, "c2").unwrap().unwrap();
            assert!(cut.truncated());
            assert_eq!(cut.bytes, long.len());
            assert!(result(&run_id, 2, "c3").unwrap().is_none());
            assert!(result(&run_id, 1, "c1").unwrap().is_none());
            assert!(result(&run_id, 99, "c1").unwrap().is_none());
        })
        .await;
    }

    /// A run with no file did nothing, and a result asked of it is nothing:
    /// the caller read a page a moment ago and the run has gone since.
    #[tokio::test]
    async fn a_run_with_no_file_did_nothing() {
        crate::runstate::with_isolated_runs_dir_async("executions-none", |_d| async move {
            assert!(read("ghost").unwrap().is_empty());
            assert!(result("ghost", 0, "c1").unwrap().is_none());
        })
        .await;
    }

    /// A file that cannot be read is reported rather than read as empty.
    #[tokio::test]
    async fn an_unreadable_file_is_reported() {
        crate::runstate::with_isolated_runs_dir_async("executions-corrupt", |_d| async move {
            garbage("broken", b"not a run file");
            assert_eq!(read("broken").unwrap_err().code(), "INTERNAL");
            assert_eq!(result("broken", 0, "c1").unwrap_err().code(), "INTERNAL");
            // A step that does not decode, asked for by its position.
            let run_id = recorded();
            super::super::run_file::tests::bad_step(&run_id, 1);
            assert_eq!(result(&run_id, 1, "c1").unwrap_err().code(), "INTERNAL");
            assert_eq!(read(&run_id).unwrap_err().code(), "INTERNAL");
        })
        .await;
    }
}
