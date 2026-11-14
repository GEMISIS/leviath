//! Run files in binary layout 9, whose failed runs stated no kind, upgraded
//! in place.
//!
//! `fixtures/layout-9/run.lvr` is a run held mid-stage when its daemon was
//! killed, whose graph sets `routing = { retry_attempts = 2 }` and whose
//! `verify` stage sets `routing = { on_error = "fail" }`.
//!
//! `fixtures/layout-9-errored/run.lvr` was written by a layout-9 build: a run
//! of one image stage whose model answered three times with the word "done"
//! and no image, and failed for it.

use leviath_legacy_runs::{ConvertError, needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec::{self, FrameKind};
use leviath_runtime::state::RunStatus;

use crate::common::fixtures_dir;

/// The fixture `name`'s run file, alone in a run directory of its own.
fn placed(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runs").join("layout-9-run");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        fixtures_dir().join(name).join("run.lvr"),
        dir.join("run.lvr"),
    )
    .unwrap();
    (tmp, dir)
}

/// The fixture is refused by this build's reader until it is upgraded, and
/// reads once it is: its spec byte for byte, since layout 9 wrote it in this
/// build's shape, and its state as it was. The file as it was is kept beside
/// it.
#[test]
fn a_layout_9_run_reads_once_upgraded_and_its_original_is_kept() {
    let (_tmp, dir) = placed("layout-9");
    let file = dir.join("run.lvr");
    let before = std::fs::read(&file).unwrap();
    let refused = RunFileReader::read(&file).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&dir));

    let report = upgrade(&dir).unwrap();
    assert!(!needs_upgrade(&dir));
    assert_eq!(report.original, dir.join("legacy").join("run.v9.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(report.cut, 0);

    let reader = RunFileReader::read(&file).unwrap();
    assert_eq!(report.run_id, reader.spec().run_id);
    let latest = reader.latest_state().unwrap();
    assert!(
        !matches!(latest.status, RunStatus::Error { .. }),
        "the run was held, not failed"
    );

    let after = std::fs::read(&file).unwrap();
    let (old, new) = (codec::scan(&before).frames, codec::scan(&after).frames);
    let kinds = |frames: &[codec::FrameRef]| frames.iter().map(|f| f.kind).collect::<Vec<_>>();
    assert_eq!(kinds(&new), kinds(&old));
    assert_eq!(
        before[old[0].offset..old[0].end()],
        after[new[0].offset..new[0].end()],
        "the spec is copied as it was"
    );

    // Upgrading again is refused: the file is this build's now.
    let again = upgrade(&dir).unwrap_err();
    assert!(
        matches!(again, ConvertError::NotOldLayout { .. }),
        "{again}"
    );
}

/// A run that failed under layout 9 reads, once upgraded, as failed with the
/// sentence it had and no kind, in its state and in its summary.
#[test]
fn a_layout_9_failure_keeps_its_sentence_and_states_no_kind() {
    let (_tmp, dir) = placed("layout-9-errored");
    let file = dir.join("run.lvr");
    assert!(needs_upgrade(&dir));
    upgrade(&dir).unwrap();

    let reader = RunFileReader::read(&file).unwrap();
    let latest = reader.latest_state().unwrap();
    let RunStatus::Error { message, kind } = &latest.status else {
        panic!("the run failed: {:?}", latest.status);
    };
    assert!(
        message.starts_with(
            "[no-media] stage 'draw' makes image, and 3 replies in a row carried none"
        ),
        "{message}"
    );
    assert!(
        message.contains("text: \"done\" (4 characters)"),
        "{message}"
    );
    assert_eq!(kind, &None);

    let meta = leviath_runtime::runfile::summary(&reader).expect("a summary");
    assert_eq!(meta.error.as_deref(), Some(message.as_str()));
    assert_eq!(meta.error_kind, None);
}

/// A layout-9 file whose state does not read is refused, and left exactly as
/// it was.
#[test]
fn a_layout_9_file_whose_state_does_not_read_is_refused() {
    let (_tmp, dir) = placed("layout-9");
    let file = dir.join("run.lvr");
    let bytes = std::fs::read(&file).unwrap();
    let spec = codec::scan(&bytes).frames[0];
    let mut bad = bytes[..spec.end()].to_vec();
    bad.extend(codec::encode(FrameKind::State, &"not a state".to_string()).unwrap());
    std::fs::write(&file, &bad).unwrap();
    let err = upgrade(&dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), bad);
}

/// And so is one whose step does not read.
#[test]
fn a_layout_9_file_whose_step_does_not_read_is_refused() {
    let (_tmp, dir) = placed("layout-9");
    let file = dir.join("run.lvr");
    let mut bad = std::fs::read(&file).unwrap();
    bad.extend(codec::encode(FrameKind::Delta, &"not a step".to_string()).unwrap());
    std::fs::write(&file, &bad).unwrap();
    let err = upgrade(&dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), bad);
}

/// A step layout 9 wrote after the state reads as this build's once
/// upgraded: a failure it recorded keeps its sentence.
#[test]
fn a_layout_9_step_is_written_again_as_this_builds() {
    let (_tmp, dir) = placed("layout-9");
    let file = dir.join("run.lvr");
    let mut bytes = std::fs::read(&file).unwrap();
    // The step after the last one the fixture holds, which a copy of it
    // upgraded says.
    let seq = {
        let (_tmp, copy) = placed("layout-9");
        upgrade(&copy).unwrap();
        RunFileReader::read(&copy.join("run.lvr"))
            .unwrap()
            .last_seq()
            + 1
    };
    // A delta as layout 9 wrote one: its seq, its time, one change (variant
    // 0, the status, holding variant 5, a failure and its sentence), and no
    // event.
    let step = (
        seq,
        5i64,
        vec![(0u32, (5u32, "it broke"))],
        Vec::<u8>::new(),
    );
    bytes.extend(codec::encode(FrameKind::Delta, &step).unwrap());
    std::fs::write(&file, &bytes).unwrap();

    upgrade(&dir).unwrap();
    let reader = RunFileReader::read(&file).unwrap();
    assert_eq!(
        reader.latest_state().unwrap().status,
        RunStatus::Error {
            message: "it broke".into(),
            kind: None,
        }
    );
}
