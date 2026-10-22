//! A run's segments and whether it can be continued, read off its file.

use leviath_runtime::runfile::{RunFileReader, RunFileWriter};
use leviath_runtime::state::segment::{SegmentEnd, kept_answer_path};
use leviath_runtime::state::{ContinuedState, FileRef, RunEvent, RunStatus as State};

use super::*;
use crate::commands::serve::testutil::with_home;
use crate::runstate::create_run;
use crate::test_fixtures::fixtures::run_meta;

/// Record on run `run_id`'s file that it finished, then was continued, its
/// first answer kept at `kept` (written there when `answer` is given).
fn continued(run_id: &str, answer: Option<&str>) {
    let path = run_file::path(run_id);
    let dir = crate::runstate::run_dir(run_id);
    let mut writer = RunFileWriter::open(&path, Default::default()).unwrap();
    let mut done = writer.state().clone();
    done.status = State::Complete;
    writer.record(done, 10, Vec::new()).unwrap();
    let done = writer.state().clone();
    let kept = FileRef::whole(kept_answer_path(1), answer.unwrap_or("x").as_bytes());
    if let Some(answer) = answer {
        let at = dir.join("segments").join("1");
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join(leviath_core::FINAL_OUTPUT_FILE), answer).unwrap();
    }
    let mut again = done.clone();
    again.status = State::Active;
    again.totals.tool_calls += 1;
    let event = ContinuedState {
        segment: 2,
        stage: done.cursor.stage.clone(),
        forked_from: None,
        previous: Some(SegmentEnd::of(&done, Some(kept))),
        regions: Vec::new(),
    };
    writer
        .record(again, 20, vec![RunEvent::Continued(Box::new(event))])
        .unwrap();
}

#[tokio::test]
async fn each_segment_is_listed_with_its_answer() {
    with_home(|_| async {
        assert!(segments("nope").unwrap().is_empty());
        create_run(&run_meta("r-1")).unwrap();
        let read = segments("r-1").unwrap();
        assert_eq!(read.len(), 1);
        assert!(read[0].answer.is_none());

        continued("r-1", Some("the first answer"));
        let read = segments("r-1").unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].answer.as_deref(), Some("the first answer"));
        assert!(read[1].segment.current);

        // An answer whose file is gone reads as none.
        create_run(&run_meta("r-2")).unwrap();
        continued("r-2", None);
        assert!(segments("r-2").unwrap()[0].answer.is_none());
    })
    .await;
}

#[tokio::test]
async fn a_run_file_that_does_not_read_lists_nothing() {
    with_home(|_| async {
        create_run(&run_meta("r-1")).unwrap();
        std::fs::write(run_file::path("r-1"), b"junk").unwrap();
        assert!(segments("r-1").is_err());
        // One whose steps do not read.
        create_run(&run_meta("r-2")).unwrap();
        let path = run_file::path("r-2");
        let next = RunFileReader::read(&path).unwrap().last_seq() + 1;
        let garbled = leviath_runtime::runfile::codec::encode(
            leviath_runtime::runfile::codec::FrameKind::Delta,
            &(next, [0xffu8; 3]),
        )
        .unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(garbled);
        std::fs::write(&path, bytes).unwrap();
        assert!(segments("r-2").is_err());
    })
    .await;
}

#[tokio::test]
async fn only_a_finished_root_run_with_a_file_can_be_continued() {
    with_home(|_| async {
        let mut meta = run_meta("r-1");
        assert_eq!(continuable(&meta), Some(false), "a run still going");
        for status in [RunStatus::Complete, RunStatus::Error, RunStatus::Cancelled] {
            meta.status = status;
            assert_eq!(continuable(&meta), None, "no file to say");
        }
        create_run(&meta).unwrap();
        assert_eq!(continuable(&meta), Some(true));
        meta.parent_run_id = Some("p-1".into());
        assert_eq!(continuable(&meta), Some(false), "a sub-agent");
    })
    .await;
}
