use super::*;
use crate::runfile::reader_tests::{scripted_run, write_run};
use crate::state::segment::{SegmentEnd, kept_answer_path};

fn stage(name: &str) -> StageName {
    StageName::new(name).unwrap()
}

#[test]
fn a_run_never_continued_has_one_segment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let states = scripted_run(2);
    write_run(&path, &states, Default::default());
    let reader = RunFileReader::read(&path).unwrap();
    let segments = segments(&reader).unwrap();
    assert_eq!(segments.len(), 1);
    let only = &segments[0];
    assert_eq!(only.index, 1);
    assert!(only.current);
    assert_eq!(only.started_seq, 0);
    assert_eq!(only.started_at, reader.spec().created_at);
    assert_eq!(only.last_seq, reader.last_seq());
    assert_eq!(only.stage, states[0].cursor.stage);
    assert_eq!(only.forked_from, None);
}

/// A run continued once has two: the first as it ended, with what it spent
/// itself and its answer kept beside the file, and the one it is in now,
/// with only what was spent since.
#[test]
fn a_continued_run_has_a_segment_for_each_stretch_of_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let states = scripted_run(1);
    let mut writer = write_run(&path, &states, Default::default());
    let mut done = writer.state().clone();
    done.status = RunStatus::Complete;
    done.totals.spend.priced_usd = 2.0;
    done.totals.tool_calls = 4;
    writer.record(done.clone(), 20, Vec::new()).unwrap();
    let done = writer.state().clone();
    let kept = FileRef::whole(kept_answer_path(1), b"first");
    let mut again = done.clone();
    again.status = RunStatus::Active;
    again.totals.spend.priced_usd = 2.5;
    again.totals.tool_calls = 6;
    let continued = crate::state::ContinuedState {
        segment: 2,
        stage: stage("plan"),
        forked_from: None,
        previous: Some(SegmentEnd::of(&done, Some(kept.clone()))),
        regions: Vec::new(),
    };
    writer
        .record(
            again,
            30,
            vec![crate::state::RunEvent::Continued(Box::new(continued))],
        )
        .unwrap();
    let reader = RunFileReader::read(&path).unwrap();
    let segments = segments(&reader).unwrap();
    assert_eq!(segments.len(), 2);
    let (first, second) = (&segments[0], &segments[1]);
    assert_eq!(first.status, RunStatus::Complete);
    assert_eq!(first.spend.priced_usd, 2.0);
    assert_eq!(first.tool_calls, 4);
    assert_eq!(first.file, Some(kept));
    assert_eq!(first.last_seq, done.seq);
    assert!(!first.current);
    assert_eq!(second.index, 2);
    assert_eq!(second.started_seq, done.seq + 1);
    assert_eq!(second.started_at, 30);
    assert_eq!(second.status, RunStatus::Active);
    assert_eq!(second.spend.priced_usd, 0.5);
    assert_eq!(second.tool_calls, 2);
    assert!(second.current);
}

/// A fork's history starts with the context it carried over, which is no
/// segment of its own.
#[test]
fn a_fork_starts_at_its_first_continuation() {
    let first = scripted_run(1).remove(0);
    let mut now = first.clone();
    now.totals.spend.priced_usd = 0.25;
    let starts = vec![(
        1,
        9,
        crate::state::ContinuedState {
            segment: 1,
            stage: stage("plan"),
            forked_from: Some(RunId::new("src-1").unwrap()),
            previous: None,
            regions: Vec::new(),
        },
    )];
    let segments = build(5, &first, &now, starts);
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].index, 1);
    assert_eq!(segments[0].started_seq, 1);
    assert_eq!(segments[0].forked_from, Some(RunId::new("src-1").unwrap()));
    assert_eq!(segments[0].spend.priced_usd, 0.25);
}

/// A run file holding `frames` after its header and spec, read.
fn file_of(frames: &[Vec<u8>]) -> (tempfile::TempDir, RunFileReader) {
    use crate::runfile::codec::{FrameKind, encode, header};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let mut bytes = header(crate::runfile::fingerprint());
    bytes.extend(encode(FrameKind::Spec, &crate::runfile::reader_tests::spec()).unwrap());
    for frame in frames {
        bytes.extend(frame);
    }
    std::fs::write(&path, bytes).unwrap();
    let reader = RunFileReader::read(&path).unwrap();
    (dir, reader)
}

/// A file whose steps, first state or last checkpoint do not read has no
/// segments to list: each read that fails says so.
#[test]
fn a_file_that_does_not_read_lists_no_segments() {
    use crate::runfile::codec::{FrameKind, encode};
    use crate::runfile::reader_tests::zstd_frame;
    let states = scripted_run(1);
    let state = |s: &RunState| encode(FrameKind::State, s).unwrap();
    let delta = encode(
        FrameKind::Delta,
        &crate::state::StateDelta::between(&states[0], &states[1], 1, Vec::new()),
    )
    .unwrap();
    // A step whose bytes are not a step.
    let garbled = zstd_frame(FrameKind::Delta, &[1, 0xff, 0xff, 0xff]);
    let (_dir, reader) = file_of(&[state(&states[0]), garbled]);
    assert!(segments(&reader).is_err());
    // No state to start from.
    let (_dir, reader) = file_of(std::slice::from_ref(&delta));
    assert!(segments(&reader).is_err());
    // A last checkpoint whose bytes are not a state.
    let broken = zstd_frame(FrameKind::State, &[1, 0xff, 0xff, 0xff]);
    let (_dir, reader) = file_of(&[state(&states[0]), delta, broken]);
    assert!(segments(&reader).is_err());
}
