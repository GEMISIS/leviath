//! The journal read as a list of executions: what the run tried to do, and how
//! each attempt ended.
//!
//! The records answer this between them rather than individually. A batch record
//! says what was dispatched; a completion record says how one of those calls
//! ended, and may sit thousands of records later or never arrive at all. Pairing
//! them is what turns a journal into a history, and doing it here means the API,
//! the console and anything else agree about what happened.
//!
//! What this deliberately does not carry is the payloads. A run's results hold
//! file bodies and command output, and a list of five thousand executions that
//! carried every one of them would be a read of hundreds of megabytes to answer
//! "what did this run do". Each execution instead names the position of the
//! record holding its result, and a caller that wants one seeks there.

use std::io::{self, Read, Seek, SeekFrom};

use super::{Frame, Frames, RunRecord, read_record};
use crate::execution::ToolOutcome;
use crate::region::EntryContent;

/// One attempt to execute one tool call, as the journal records it.
///
/// An attempt, not a call: the same call reissued after a failure is a second
/// execution with its own id, and telling them apart is the point.
#[derive(Debug)]
pub struct Execution {
    /// The execution id minted at dispatch. Empty in a journal written before
    /// executions had identity, where the provider's call id is all there is.
    pub id: String,
    /// The provider's own call id. Correlation only: a provider may reuse one.
    pub call_id: String,
    /// The tool name, as the model called it.
    pub tool: String,
    /// The arguments as the model sent them, verbatim. Kept as text because that
    /// is what was recorded, and because a model may send something the tool's
    /// own schema would refuse.
    pub arguments: String,
    /// The stage the batch was dispatched in.
    pub stage_index: usize,
    /// The stage-local iteration that produced the batch.
    pub iteration: usize,
    /// The stay in that stage it was dispatched during. Empty where the journal
    /// records no visit.
    pub visit_id: String,
    /// The provider attempt whose answer asked for it. Empty where the journal
    /// records no attempt.
    pub requested_by: String,
    /// The files it produced, as the journal recorded them when it produced
    /// them. Empty for every execution that produced none.
    pub artifacts: Vec<crate::output::Artifact>,
    /// When it was dispatched, in unix seconds.
    pub dispatched_at: i64,
    /// Where the batch record that dispatched it sits in the journal.
    pub position: u64,
    /// When it ended, in unix seconds. `None` while the call is still running,
    /// and also on a run that died before the journal learned how it ended.
    pub ended_at: Option<i64>,
    /// Where the record carrying its result sits, for a caller that wants the
    /// bytes. The dispatching batch record for a call the dispatcher resolved
    /// inline, its own completion record otherwise.
    pub result_position: Option<u64>,
    /// How it ended, where the journal says so. `None` covers three different
    /// situations, which a reader must not flatten: still running, ended before
    /// outcomes were recorded, or ended in a way only the result text describes.
    pub outcome: Option<ToolOutcome>,
}

impl Execution {
    /// Whether this attempt is still, as far as the journal knows, in flight.
    ///
    /// True of a call that is genuinely running, and of one whose daemon died
    /// without recording anything. A resume records the second as indeterminate,
    /// so a run nobody is resuming is where this stays true forever.
    pub fn unfinished(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// Read every execution the archive records, in dispatch order.
///
/// Streams the file one frame at a time: a long run's journal is walked holding
/// one record, not the whole parsed journal. A torn tail ends the walk with the
/// executions read so far, which is what a live run's journal looks like while
/// the lane is mid-append.
pub fn read_archive_executions(r: &mut dyn Read) -> io::Result<Vec<Execution>> {
    let (_, mut frames) = Frames::open(r)?;
    let mut executions: Vec<Execution> = Vec::new();
    while let Ok(Some((position, frame))) = frames.next_frame() {
        let Frame::Record(record) = frame else {
            continue;
        };
        match *record {
            RunRecord::ToolBatch {
                calls,
                at,
                stage_index,
                iteration,
                visit_id,
                requested_by,
                ..
            } => {
                for call in calls {
                    // A call the dispatcher resolved before the batch ever
                    // reached the tool lane: a context tool, a refusal, a gate
                    // denial. Its result is in this very record, and no
                    // completion record will follow.
                    let inline = call.result.is_some();
                    executions.push(Execution {
                        id: call.execution_id,
                        call_id: call.id,
                        tool: call.name,
                        arguments: call.arguments,
                        stage_index,
                        iteration,
                        visit_id: visit_id.clone(),
                        requested_by: requested_by.clone(),
                        artifacts: Vec::new(),
                        dispatched_at: at,
                        position,
                        ended_at: inline.then_some(at),
                        result_position: inline.then_some(position),
                        outcome: None,
                    });
                }
            }
            // Files one execution produced. Attached by execution id alone,
            // which is exact: nothing else in the journal claims to have made
            // them, and an id that matches nothing dispatched is dropped rather
            // than attached to the nearest call.
            RunRecord::ArtifactsProduced {
                execution_id,
                artifacts,
                ..
            } => {
                if let Some(execution) = executions
                    .iter_mut()
                    .find(|e| !e.id.is_empty() && e.id == execution_id)
                {
                    execution.artifacts.extend(artifacts);
                }
            }
            RunRecord::ToolCallDone {
                iteration,
                call_id,
                execution_id,
                outcome,
                at,
                ..
            } => {
                if let Some(execution) =
                    match_completion(&mut executions, &execution_id, &call_id, iteration)
                {
                    execution.ended_at = Some(at);
                    execution.result_position = Some(position);
                    execution.outcome = outcome;
                }
            }
            _ => {}
        }
    }
    Ok(executions)
}

/// The execution a completion record belongs to.
///
/// By execution id where both sides have one, which is exact. Otherwise by call
/// id within the same iteration, taking the most recent attempt still unfinished:
/// that is the best a journal written before executions had identity supports,
/// and it is why they now do.
fn match_completion<'e>(
    executions: &'e mut [Execution],
    execution_id: &str,
    call_id: &str,
    iteration: usize,
) -> Option<&'e mut Execution> {
    if !execution_id.is_empty() {
        return executions.iter_mut().find(|e| e.id == execution_id);
    }
    executions
        .iter_mut()
        .rev()
        .find(|e| e.call_id == call_id && e.iteration == iteration && e.unfinished())
}

/// A reader that can also move to a given offset.
///
/// A trait object rather than a type parameter, deliberately. A generic function
/// is compiled once per kind of reader it is used with, and a real file never
/// fails to seek while a test double exists to make it fail: neither copy would
/// ever exercise both paths, and the two together would still leave each copy
/// half covered.
pub trait SeekRead: Read + Seek {}

impl<T: Read + Seek> SeekRead for T {}

/// The result recorded at `position`, for the call `call_id`.
///
/// Both record kinds that can hold a result are read, because both do: a
/// completion record carries one call's result, and a batch record carries the
/// results of the calls its dispatcher resolved inline. `None` means the record
/// at that position holds no result for that call, which is what a stale
/// position looks like.
pub fn read_result_at(
    r: &mut dyn SeekRead,
    position: u64,
    call_id: &str,
) -> io::Result<Option<EntryContent>> {
    r.seek(SeekFrom::Start(position))?;
    let Some(record) = read_record(r)? else {
        return Ok(None);
    };
    Ok(result_of(record, call_id))
}

/// The result one record holds for `call_id`, if it holds one at all.
///
/// Its own function rather than a match inside the reader above, because that
/// reader is generic over where the bytes come from and everything inside it is
/// compiled once per kind of reader. Reading the record is the part that has to be
/// generic; deciding what it says is not.
fn result_of(record: RunRecord, call_id: &str) -> Option<EntryContent> {
    match record {
        // A position identifies one record, so a mismatched call id means a
        // caller pointing at a record that is not the one it thinks it is.
        RunRecord::ToolCallDone { call_id: done, .. } if done != call_id => None,
        RunRecord::ToolCallDone { result, .. } => Some(result),
        RunRecord::ToolBatch { calls, .. } => calls
            .into_iter()
            .find(|c| c.id == call_id)
            .and_then(|c| c.result),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_archive::{
        RUN_ARCHIVE_MAGIC, RUN_ARCHIVE_VERSION, RunRecord, ToolCallRecord, write_archive_start,
        write_record,
    };

    /// One dispatched call, pending unless `result` says otherwise.
    fn call(id: &str, execution_id: &str, result: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            execution_id: execution_id.to_string(),
            id: id.to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"ls"}"#.to_string(),
            result: result.map(Into::into),
            thought_signature: None,
        }
    }

    /// An archive holding `records`.
    fn archive(records: Vec<RunRecord>) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_archive_start(&mut bytes, RUN_ARCHIVE_VERSION).expect("a Vec takes the preamble");
        for record in &records {
            write_record(&mut bytes, record).expect("a Vec takes a record");
        }
        bytes
    }

    /// A batch and its completions read back as executions, each ended where its
    /// completion record says.
    #[test]
    fn a_batch_and_its_completions_read_as_executions() {
        let bytes = archive(vec![
            RunRecord::ToolBatch {
                calls: vec![call("c1", "x1", None), call("c2", "x2", None)],
                at: 10,
                stage_index: 2,
                iteration: 5,
                visit_id: String::new(),
                requested_by: String::new(),
                response: "doing two things".to_string(),
            },
            RunRecord::ToolCallDone {
                iteration: 5,
                call_id: "c2".to_string(),
                execution_id: "x2".to_string(),
                result: "second".into(),
                outcome: None,
                at: 12,
            },
            RunRecord::ToolCallDone {
                iteration: 5,
                call_id: "c1".to_string(),
                execution_id: "x1".to_string(),
                result: "first".into(),
                outcome: Some(ToolOutcome::Succeeded),
                at: 14,
            },
        ]);
        let executions =
            read_archive_executions(&mut bytes.as_slice()).expect("the archive reads back");

        // Dispatch order, not completion order: the second call finished first,
        // and the list still reads the way the model asked for them.
        assert_eq!(executions.len(), 2);
        assert_eq!(executions[0].id, "x1");
        assert_eq!(executions[0].call_id, "c1");
        assert_eq!(executions[0].tool, "shell");
        assert_eq!(executions[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(executions[0].stage_index, 2);
        assert_eq!(executions[0].iteration, 5);
        assert_eq!(executions[0].dispatched_at, 10);
        assert_eq!(executions[0].ended_at, Some(14));
        assert_eq!(executions[0].outcome, Some(ToolOutcome::Succeeded));
        assert_eq!(executions[1].id, "x2");
        assert_eq!(executions[1].ended_at, Some(12));
        // Both were dispatched by the same record, so both name its position.
        assert_eq!(executions[0].position, executions[1].position);
        assert!(!executions[0].unfinished());
    }

    /// A call with no completion record is unfinished, and says so rather than
    /// reading as one that ended.
    #[test]
    fn a_call_with_no_completion_stays_unfinished() {
        let bytes = archive(vec![RunRecord::ToolBatch {
            calls: vec![call("c1", "x1", None)],
            at: 10,
            stage_index: 0,
            iteration: 1,
            visit_id: String::new(),
            requested_by: String::new(),
            response: String::new(),
        }]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert_eq!(executions.len(), 1);
        assert!(executions[0].unfinished());
        assert_eq!(executions[0].ended_at, None);
        assert_eq!(executions[0].result_position, None);
        assert_eq!(executions[0].outcome, None);
    }

    /// A call the dispatcher resolved inline ends with its own batch record, and
    /// no completion record ever follows.
    #[test]
    fn an_inline_result_ends_with_the_batch_that_carried_it() {
        let bytes = archive(vec![RunRecord::ToolBatch {
            calls: vec![call("c1", "x1", Some("refused"))],
            at: 10,
            stage_index: 0,
            iteration: 1,
            visit_id: String::new(),
            requested_by: String::new(),
            response: String::new(),
        }]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert_eq!(executions[0].ended_at, Some(10));
        assert_eq!(
            executions[0].result_position,
            Some(executions[0].position),
            "its result is in the record that dispatched it"
        );
        // Seeking there reads the text back.
        let mut cursor = std::io::Cursor::new(bytes);
        let result = read_result_at(
            &mut cursor,
            executions[0].result_position.expect("a position"),
            "c1",
        )
        .expect("the record reads");
        assert_eq!(result.as_deref(), Some("refused"));
    }

    /// A completion is read back from the position its execution names.
    #[test]
    fn a_result_is_read_from_the_position_the_execution_names() {
        let bytes = archive(vec![
            RunRecord::ToolBatch {
                calls: vec![call("c1", "x1", None)],
                at: 10,
                stage_index: 0,
                iteration: 1,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::ToolCallDone {
                iteration: 1,
                call_id: "c1".to_string(),
                execution_id: "x1".to_string(),
                result: "the file body".into(),
                outcome: Some(ToolOutcome::Succeeded),
                at: 11,
            },
        ]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        let position = executions[0].result_position.expect("it ended");
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(
            read_result_at(&mut cursor, position, "c1")
                .expect("the record reads")
                .as_deref(),
            Some("the file body")
        );
        // A different call's id finds nothing there, rather than the wrong text.
        assert_eq!(
            read_result_at(&mut cursor, position, "c9").expect("the record reads"),
            None
        );
    }

    /// Two attempts at one call id are two executions, and each completion lands
    /// on its own.
    ///
    /// This is the whole reason executions have ids. With only the provider's
    /// call id to go on, the second attempt's result would overwrite the first
    /// one's and the retry would vanish from the history.
    #[test]
    fn two_attempts_at_one_call_id_stay_apart() {
        let bytes = archive(vec![
            RunRecord::ToolBatch {
                calls: vec![call("c1", "x1", None)],
                at: 10,
                stage_index: 0,
                iteration: 1,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::ToolCallDone {
                iteration: 1,
                call_id: "c1".to_string(),
                execution_id: "x1".to_string(),
                result: "timed out".into(),
                outcome: Some(ToolOutcome::Failed),
                at: 11,
            },
            RunRecord::ToolBatch {
                calls: vec![call("c1", "x2", None)],
                at: 12,
                stage_index: 0,
                iteration: 2,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::ToolCallDone {
                iteration: 2,
                call_id: "c1".to_string(),
                execution_id: "x2".to_string(),
                result: "done".into(),
                outcome: Some(ToolOutcome::Succeeded),
                at: 13,
            },
        ]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert_eq!(executions.len(), 2);
        assert_eq!(executions[0].outcome, Some(ToolOutcome::Failed));
        assert_eq!(executions[1].outcome, Some(ToolOutcome::Succeeded));
        assert_ne!(executions[0].position, executions[1].position);
    }

    /// An old journal has no execution ids, and its completions still pair up.
    ///
    /// The fallback is the call id within one iteration, which is what those
    /// journals were written against. It takes the most recent unfinished
    /// attempt, so a call id reused across iterations does not collect two
    /// endings.
    #[test]
    fn a_journal_with_no_execution_ids_still_pairs_up() {
        let bytes = archive(vec![
            RunRecord::ToolBatch {
                calls: vec![call("c1", "", None)],
                at: 10,
                stage_index: 0,
                iteration: 1,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::ToolBatch {
                calls: vec![call("c1", "", None)],
                at: 12,
                stage_index: 0,
                iteration: 2,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::ToolCallDone {
                iteration: 2,
                call_id: "c1".to_string(),
                execution_id: String::new(),
                result: "second".into(),
                outcome: None,
                at: 13,
            },
        ]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert_eq!(executions.len(), 2);
        assert!(
            executions[0].unfinished(),
            "the first iteration's call never got an ending"
        );
        assert_eq!(executions[1].ended_at, Some(13));
    }

    /// A completion for a call nothing dispatched is dropped.
    ///
    /// It cannot be attached to anything, and inventing an execution for it
    /// would put a call in the history that the model never made.
    #[test]
    fn a_completion_with_no_dispatch_is_dropped() {
        let bytes = archive(vec![RunRecord::ToolCallDone {
            iteration: 1,
            call_id: "ghost".to_string(),
            execution_id: "x9".to_string(),
            result: "from nowhere".into(),
            outcome: None,
            at: 11,
        }]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert!(executions.is_empty(), "{executions:?}");
    }

    /// One file, as a submission recorded it.
    fn artifact(name: &str) -> crate::output::Artifact {
        crate::output::Artifact {
            name: name.to_string(),
            path: format!("out/{name}"),
            mime_type: crate::mime::MimeType::parse("text/markdown").expect("a type"),
            size: 12,
            sha256: "beef".to_string(),
        }
    }

    /// The files an execution produced land on that execution, by its id alone.
    ///
    /// By id and nothing else, which is exact. An id matching nothing dispatched
    /// is dropped rather than attached to the nearest call: a file the journal
    /// cannot attribute is a file nobody made, and guessing which call made it is
    /// the mistake this record exists to prevent.
    #[test]
    fn the_files_an_execution_produced_land_on_it() {
        let bytes = archive(vec![
            RunRecord::ToolBatch {
                calls: vec![
                    call("c1", "x1", Some("recorded")),
                    call("c2", "x2", Some("ok")),
                ],
                at: 10,
                stage_index: 0,
                iteration: 1,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::ArtifactsProduced {
                execution_id: "x1".to_string(),
                artifacts: vec![artifact("report")],
                at: 11,
            },
            RunRecord::ArtifactsProduced {
                execution_id: "x1".to_string(),
                artifacts: vec![artifact("chart")],
                at: 12,
            },
            RunRecord::ArtifactsProduced {
                execution_id: "x-nothing-dispatched".to_string(),
                artifacts: vec![artifact("orphan")],
                at: 13,
            },
        ]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        let names: Vec<&str> = executions[0]
            .artifacts
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["report", "chart"],
            "both, in the order recorded"
        );
        assert!(
            executions[1].artifacts.is_empty(),
            "the other call produced nothing"
        );
    }

    /// An execution with no id of its own takes no artifacts, whatever a record
    /// names.
    ///
    /// A journal written before executions had identity records every call with
    /// an empty id, and an empty id matching an empty id would hand one call's
    /// files to every call in the run.
    #[test]
    fn an_unidentified_execution_takes_no_files() {
        let bytes = archive(vec![
            RunRecord::ToolBatch {
                calls: vec![call("c1", "", Some("recorded"))],
                at: 10,
                stage_index: 0,
                iteration: 1,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::ArtifactsProduced {
                execution_id: String::new(),
                artifacts: vec![artifact("report")],
                at: 11,
            },
        ]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert!(executions[0].artifacts.is_empty());
    }

    /// A file that is not an archive is refused, rather than read as one with no
    /// executions.
    #[test]
    fn something_that_is_not_an_archive_is_refused() {
        let mut bytes = b"not an archive at all".as_slice();
        assert!(read_archive_executions(&mut bytes).is_err());
    }

    /// A torn tail ends the walk with what came before it.
    #[test]
    fn a_torn_tail_keeps_the_executions_before_it() {
        let mut bytes = archive(vec![RunRecord::ToolBatch {
            calls: vec![call("c1", "x1", None)],
            at: 10,
            stage_index: 0,
            iteration: 1,
            visit_id: String::new(),
            requested_by: String::new(),
            response: String::new(),
        }]);
        // Half a length prefix, which is what a crash mid-append leaves.
        bytes.extend_from_slice(&[0, 0, 0]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert_eq!(executions.len(), 1);
    }

    /// A record kind this build does not understand is stepped over, and the
    /// positions of everything after it stay right.
    ///
    /// The length prefix is what makes that possible, and a position that drifted
    /// here would send every later seek into the middle of a frame.
    #[test]
    fn an_unreadable_frame_does_not_shift_later_positions() {
        let mut bytes = archive(vec![RunRecord::ToolBatch {
            calls: vec![call("c1", "x1", None)],
            at: 10,
            stage_index: 0,
            iteration: 1,
            visit_id: String::new(),
            requested_by: String::new(),
            response: String::new(),
        }]);
        // A well-formed frame whose payload is not a record this build knows.
        let payload = br#"{"FromALaterBuild":{"whatever":1}}"#;
        bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(payload);
        let tail = RunRecord::ToolCallDone {
            iteration: 1,
            call_id: "c1".to_string(),
            execution_id: "x1".to_string(),
            result: "after the unknown".into(),
            outcome: None,
            at: 15,
        };
        write_record(&mut bytes, &tail).expect("a Vec takes a record");

        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        let position = executions[0]
            .result_position
            .expect("the completion past the unknown frame still paired");
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(
            read_result_at(&mut cursor, position, "c1")
                .expect("the record reads")
                .as_deref(),
            Some("after the unknown")
        );
    }

    /// Records that are neither a dispatch nor a completion are passed over.
    ///
    /// Most of a journal is context and progress, and none of it says anything
    /// about an execution. Reading it as though it might would be the same
    /// mistake as folding a window to answer what a run tried.
    #[test]
    fn records_about_anything_else_are_passed_over() {
        let bytes = archive(vec![
            RunRecord::StatusChanged {
                status: crate::run_meta::RunStatus::Running,
                at: 1,
            },
            RunRecord::ToolBatch {
                calls: vec![call("c1", "x1", None)],
                at: 10,
                stage_index: 0,
                iteration: 1,
                visit_id: String::new(),
                requested_by: String::new(),
                response: String::new(),
            },
            RunRecord::StatusChanged {
                status: crate::run_meta::RunStatus::Complete,
                at: 20,
            },
        ]);
        let executions = read_archive_executions(&mut bytes.as_slice()).expect("it reads");
        assert_eq!(executions.len(), 1, "one dispatch, two status changes");
        assert_eq!(executions[0].call_id, "c1");
    }

    /// A reader that cannot seek, and a reader that cannot read.
    ///
    /// Both are what a file being replaced or truncated under a running server
    /// looks like. The failure has to come back as a failure: answering "no
    /// result" would read as an execution that produced nothing.
    struct Broken {
        /// Whether the seek is the part that fails. Reading always does, which is
        /// what the read stops at once the seek has worked.
        seek_fails: bool,
    }

    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("the file went away mid-read"))
        }
    }

    impl Seek for Broken {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            match self.seek_fails {
                true => Err(io::Error::other("the file went away mid-seek")),
                false => Ok(0),
            }
        }
    }

    /// A result read that cannot seek or cannot read fails rather than answering
    /// that there was no result.
    #[test]
    fn a_result_read_that_breaks_reports_the_failure() {
        let failed_seek = read_result_at(&mut Broken { seek_fails: true }, 10, "c1");
        assert!(failed_seek.is_err(), "a failed seek is not an empty answer");
        let failed_read = read_result_at(&mut Broken { seek_fails: false }, 10, "c1");
        assert!(failed_read.is_err(), "a failed read is not an empty answer");
    }

    /// A position pointing at a record that holds no results answers nothing.
    #[test]
    fn a_position_on_another_kind_of_record_holds_no_result() {
        let bytes = archive(vec![RunRecord::StatusChanged {
            status: crate::run_meta::RunStatus::Complete,
            at: 20,
        }]);
        let mut cursor = std::io::Cursor::new(bytes);
        // The first record sits right after the preamble.
        let position = RUN_ARCHIVE_MAGIC.len() as u64 + 2;
        assert_eq!(
            read_result_at(&mut cursor, position, "c1").expect("the record reads"),
            None
        );
    }

    /// A position past the end of the file answers nothing rather than erroring.
    #[test]
    fn a_position_past_the_end_answers_nothing() {
        let bytes = archive(Vec::new());
        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(
            read_result_at(&mut cursor, 4096, "c1").expect("a clean end of file"),
            None
        );
    }
}
