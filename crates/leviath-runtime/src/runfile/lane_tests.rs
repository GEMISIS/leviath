use std::path::PathBuf;

use super::*;
use crate::runfile::reader::RunFileReader;
use crate::runfile::reader_tests::{image, initial, scripted_run, spec};
use crate::spec::names::Digest;
use crate::state::ToolResultState;
use crate::state::context::{PartBody, ToolCallState};
use crate::state::files::FileRef;
use leviath_core::JsonDoc;

/// A step that records `state`.
fn step(run_id: &str, state: RunState) -> RunFileStep {
    RunFileStep {
        run_id: run_id.into(),
        now: Some(RunNow {
            spec: Arc::new(spec()),
            state,
        }),
        at: 7,
        events: Vec::new(),
        acks: Vec::new(),
    }
}

/// Something that happened, as a step's event.
fn log(line: &str) -> RunEvent {
    RunEvent::Log(line.to_string())
}

fn run_dir(runs: &Path, run_id: &str) -> PathBuf {
    let dir = runs.join(run_id);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn file(runs: &Path, run_id: &str) -> RunFileReader {
    RunFileReader::open(&runs.join(run_id).join(leviath_core::files::RUN_FILE)).unwrap()
}

#[tokio::test]
async fn a_run_gets_a_file_on_its_first_step_and_events_on_its_next() {
    let runs = tempfile::tempdir().unwrap();
    run_dir(runs.path(), "r1");
    let mut lane = RunFileLane::new("machine", "world");
    let states = scripted_run(3);
    // Events for a run with no file land nowhere.
    let early = RunFileStep::events("r1", 7, vec![log("early")]);
    assert_eq!(lane.record(runs.path(), early).await.unwrap(), None);
    assert_eq!(
        lane.record(runs.path(), step("r1", states[0].clone()))
            .await
            .unwrap(),
        None
    );
    assert_eq!(lane.writers.len(), 1);
    let mut next = step("r1", states[1].clone());
    next.events = vec![log("happened")];
    assert_eq!(lane.record(runs.path(), next).await.unwrap(), Some(1));
    let r = file(runs.path(), "r1");
    let delta = &r.deltas(1, 1).unwrap()[0];
    assert_eq!(delta.events, vec![log("happened")]);
    assert_eq!(r.owners().unwrap()[0].machine_id, "machine");
    assert_eq!(r.latest_state().unwrap().seq, 1);
    // A step with nothing in it writes nothing, and leaves a file open or
    // closed as it was.
    let nothing = RunFileStep::events("r2", 8, Vec::new());
    assert_eq!(lane.record(runs.path(), nothing).await.unwrap(), None);
    let empty = RunFileStep::events("r1", 8, Vec::new());
    assert_eq!(lane.record(runs.path(), empty).await.unwrap(), None);
    assert_eq!(lane.writers.len(), 1);
    // Events alone go on the state the file last recorded.
    let more = RunFileStep::events("r1", 8, vec![log("later")]);
    assert_eq!(lane.record(runs.path(), more).await.unwrap(), Some(2));
    let r = file(runs.path(), "r1");
    assert_eq!(r.deltas(2, 2).unwrap()[0].changes, Vec::new());
    // A deleted run is forgotten.
    lane.forget("r1");
    assert!(lane.writers.is_empty());
}

#[tokio::test]
async fn a_finished_run_closes_its_file_and_a_new_lane_carries_one_on() {
    let runs = tempfile::tempdir().unwrap();
    run_dir(runs.path(), "r1");
    let states = scripted_run(2);
    let mut lane = RunFileLane::new("m", "first");
    lane.record(runs.path(), step("r1", states[0].clone()))
        .await
        .unwrap();
    lane.record(runs.path(), step("r1", states[1].clone()))
        .await
        .unwrap();
    // A restarted daemon opens the file where it was left.
    let mut lane = RunFileLane::new("m", "second");
    let last = states.last().unwrap().clone();
    assert_eq!(
        lane.record(runs.path(), step("r1", last)).await.unwrap(),
        Some(2)
    );
    assert!(lane.writers.is_empty());
    let r = file(runs.path(), "r1");
    let owners: Vec<String> = r
        .owners()
        .unwrap()
        .into_iter()
        .map(|o| o.world_id)
        .collect();
    assert_eq!(owners, vec!["first", "second"]);
    assert_eq!(r.last_checkpoint().0, 2);
    // What happens after the run's last change of state still reaches its
    // file, which is opened again for it and closed again after.
    let late = RunFileStep::events("r1", 9, vec![log("too late?")]);
    assert_eq!(lane.record(runs.path(), late).await.unwrap(), Some(3));
    assert!(lane.writers.is_empty() && lane.noted().is_empty());
    let r = file(runs.path(), "r1");
    assert_eq!(r.deltas(3, 3).unwrap()[0].events.len(), 1);
}

/// A step's state is written as the world hands it: the lane names the files
/// it wrote and nothing else. A call the events say finished is not put in
/// the batch by the lane; the world's state says what is done.
#[tokio::test]
async fn a_step_records_the_state_it_is_handed() {
    let runs = tempfile::tempdir().unwrap();
    run_dir(runs.path(), "r1");
    let mut busy = initial();
    busy.pending = Some(crate::state::PendingBatch {
        calls: vec![ToolCallState {
            id: "c1".into(),
            name: "shell".into(),
            args: JsonDoc::default(),
            thought_signature: None,
        }],
        done: Default::default(),
    });
    let mut lane = RunFileLane::new("m", "w");
    lane.record(runs.path(), step("r1", busy.clone()))
        .await
        .unwrap();
    let finished = RunEvent::ToolFinished {
        call_id: "c1".into(),
        result: ToolResultState {
            text: "ran".into(),
            is_error: false,
        },
        millis: 0,
    };
    let landed = RunFileStep::events("r1", 8, vec![finished.clone()]);
    lane.record(runs.path(), landed).await.unwrap();
    let state = file(runs.path(), "r1").latest_state().unwrap();
    assert!(state.pending.unwrap().done.is_empty());
    // The world's state, holding the call as done, is what says it is.
    let mut done = busy;
    done.pending.as_mut().unwrap().done.insert(
        "c1".into(),
        ToolResultState {
            text: "ran".into(),
            is_error: false,
        },
    );
    lane.record(runs.path(), step("r1", done)).await.unwrap();
    let state = file(runs.path(), "r1").latest_state().unwrap();
    assert_eq!(state.pending.unwrap().done["c1"].text, "ran");
}

/// A stored part is named in the run file, by its digest and where it came
/// from, and its bytes are never copied in: a reader reads them from
/// `blobs/` beside the file, and a part whose file is missing is refused by
/// name.
#[tokio::test]
async fn stored_parts_are_named_and_never_copied_in() {
    let runs = tempfile::tempdir().unwrap();
    let dir = run_dir(runs.path(), "r1");
    let blobs = dir.join(leviath_core::files::BLOBS_DIR);
    std::fs::create_dir_all(&blobs).unwrap();
    let big = vec![7u8; 256 * 1024];
    std::fs::write(blobs.join(Digest::of(&big).as_str()), &big).unwrap();
    let mut s = initial();
    let mut shown =
        crate::runfile::reader_tests::entry("look", crate::state::context::EntryKind::Text);
    shown.parts = vec![image(&big), image(&big), image(b"gone"), inline_part()];
    s.context.regions[1].entries.push(shown);
    crate::state::files::note_blobs(&mut s.blobs, &s.context);
    let mut lane = RunFileLane::new("m", "w");
    lane.record(runs.path(), step("r1", s)).await.unwrap();
    let r = file(runs.path(), "r1");
    assert!(r.len() < 64 * 1024, "the file holds {} bytes", r.len());
    let named: Vec<Digest> = r
        .latest_state()
        .unwrap()
        .blobs
        .into_iter()
        .map(|b| b.digest)
        .collect();
    assert_eq!(named, vec![Digest::of(&big), Digest::of(b"gone")]);
    assert_eq!(r.blob(&Digest::of(&big)).unwrap(), big);
    let missing = r.blob(&Digest::of(b"gone")).unwrap_err();
    assert_eq!(
        missing.kind,
        crate::runfile::RunFileErrorKind::MissingBlob(Digest::of(b"gone"))
    );
    assert!(missing.to_string().contains("is missing"), "{missing}");
}

/// The files the lane writes beside a run file are named by the run's next
/// step, each on top of what the step before named; a finished run's file is
/// opened again for them, and a live run's waits for its next step.
#[tokio::test]
async fn files_written_beside_a_run_are_named_by_its_next_step() {
    let runs = tempfile::tempdir().unwrap();
    run_dir(runs.path(), "r1");
    let states = scripted_run(2);
    let mut lane = RunFileLane::new("m", "w");
    lane.wrote("r1", RunFiles::default());
    assert!(lane.written.is_empty());
    lane.record(runs.path(), step("r1", states[0].clone()))
        .await
        .unwrap();
    let mut named = RunFiles::default();
    named.set_stage_file(0, StageFile::Logs, FileRef::log("stages/0/logs.log", 4));
    lane.wrote("r1", named);
    // A live run's files wait for its next step.
    assert!(lane.noted().is_empty());
    assert_eq!(lane.unnamed(), ["r1"]);
    let mut more = RunFiles {
        final_output: Some(FileRef::whole("final_output", b"answer")),
        ..RunFiles::default()
    };
    more.set_stage_file(0, StageFile::Logs, FileRef::log("stages/0/logs.log", 9));
    more.set_stage_file(0, StageFile::Output, FileRef::log("stages/0/output.log", 2));
    more.set_stage_file(
        0,
        StageFile::TaintAudit,
        FileRef::whole("stages/0/taint_audit.json", b"[]"),
    );
    lane.wrote("r1", more);
    let mut done = states[1].clone();
    done.status = RunStatus::Complete;
    lane.record(runs.path(), step("r1", done)).await.unwrap();
    let files = file(runs.path(), "r1").latest_state().unwrap().files;
    assert_eq!(files.final_output.as_ref().unwrap().bytes, 6);
    assert_eq!(files.stage_file(0, StageFile::Logs).unwrap().bytes, 9);
    assert_eq!(files.stage_file(0, StageFile::Output).unwrap().bytes, 2);
    assert!(files.stage_file(0, StageFile::TaintAudit).is_some());
    // The run finished, so its file is closed: a log line after that is
    // named by a step of its own, on top of what was named before.
    assert!(lane.writers.is_empty());
    let mut late = RunFiles::default();
    late.set_stage_file(0, StageFile::Logs, FileRef::log("stages/0/logs.log", 12));
    lane.wrote("r1", late);
    assert_eq!(lane.noted(), ["r1"]);
    let named = RunFileStep::events("r1", 9, Vec::new());
    assert_eq!(lane.record(runs.path(), named).await.unwrap(), Some(2));
    let files = file(runs.path(), "r1").latest_state().unwrap().files;
    assert_eq!(files.stage_file(0, StageFile::Logs).unwrap().bytes, 12);
    assert_eq!(files.final_output.unwrap().bytes, 6);
    // A deleted run's files are forgotten with it.
    let gone = RunFiles {
        final_output: Some(FileRef::whole("final_output", b"x")),
        ..RunFiles::default()
    };
    lane.wrote("r1", gone);
    lane.forget("r1");
    assert!(lane.written.is_empty());
}

fn inline_part() -> crate::state::context::PartState {
    crate::state::context::PartState {
        mime_type: "text/plain".into(),
        body: PartBody::Inline("hi".into()),
        name: None,
        deliver: None,
    }
}

#[tokio::test]
async fn a_step_that_cannot_be_written_is_an_error_and_closes_the_file() {
    let runs = tempfile::tempdir().unwrap();
    let mut lane = RunFileLane::new("m", "w");
    // No run directory to make the file in.
    assert!(
        lane.record(runs.path(), step("nowhere", initial()))
            .await
            .is_err()
    );
    assert!(lane.writers.is_empty());
    // A file that is not a run file.
    let dir = run_dir(runs.path(), "junk");
    std::fs::write(dir.join(leviath_core::files::RUN_FILE), b"junk").unwrap();
    assert!(
        lane.record(runs.path(), step("junk", initial()))
            .await
            .is_err()
    );
    // A file whose writes fail.
    run_dir(runs.path(), "r1");
    lane.record(runs.path(), step("r1", initial()))
        .await
        .unwrap();
    crate::runfile::writer::break_writes(lane.writers.get_mut("r1").unwrap());
    let changed = RunState {
        title: Some("t".into()),
        ..initial()
    };
    assert!(lane.record(runs.path(), step("r1", changed)).await.is_err());
    assert!(lane.writers.is_empty());
}
