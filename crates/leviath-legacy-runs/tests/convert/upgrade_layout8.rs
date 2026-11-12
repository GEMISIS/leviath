//! Run files in binary layout 8, which accounted only for the model calls
//! that answered, upgraded in place.
//!
//! `fixtures/layout-8/run.lvr` was written by a layout-8 build: a run held
//! mid-stage when its daemon was killed, whose graph sets
//! `routing = { retry_attempts = 2 }` and whose `verify` stage sets
//! `routing = { on_error = "fail" }`.

use leviath_legacy_runs::{ConvertError, needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec::{self, FrameKind};

use crate::common::fixtures_dir;

/// The fixture's run file, alone in a run directory of its own.
fn placed() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runs").join("layout-8-run");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixtures_dir().join("layout-8/run.lvr"), dir.join("run.lvr")).unwrap();
    (tmp, dir)
}

/// The fixture is refused by this build's reader until it is upgraded, and
/// reads once it is: its spec byte for byte, since layout 8 wrote it in this
/// build's shape, and its state with the cost of every call it knew of and
/// no call listed as out. The file as it was is kept beside it.
#[test]
fn a_layout_8_run_reads_once_upgraded_and_its_original_is_kept() {
    let (_tmp, dir) = placed();
    let file = dir.join("run.lvr");
    let before = std::fs::read(&file).unwrap();
    let refused = RunFileReader::read(&file).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&dir));

    let report = upgrade(&dir).unwrap();
    assert!(!needs_upgrade(&dir));
    assert_eq!(report.original, dir.join("legacy").join("run.v8.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(report.cut, 0);

    let reader = RunFileReader::read(&file).unwrap();
    assert_eq!(report.run_id, reader.spec().run_id);
    let latest = reader.latest_state().unwrap();
    assert!(
        latest.open_calls.is_empty(),
        "layout 8 listed no call as out"
    );
    assert!(
        latest.totals.spend.is_exact(),
        "nothing it knew of is unknown"
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

/// A layout-8 file whose state does not read is refused, and left exactly as
/// it was.
#[test]
fn a_layout_8_file_whose_state_does_not_read_is_refused() {
    let (_tmp, dir) = placed();
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
fn a_layout_8_file_whose_step_does_not_read_is_refused() {
    let (_tmp, dir) = placed();
    let file = dir.join("run.lvr");
    let mut bad = std::fs::read(&file).unwrap();
    bad.extend(codec::encode(FrameKind::Delta, &"not a step".to_string()).unwrap());
    std::fs::write(&file, &bad).unwrap();
    let err = upgrade(&dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), bad);
}

/// A step layout 8 wrote after the state reads as this build's once
/// upgraded: a call it billed was answered, and what it spent is kept.
#[test]
fn a_layout_8_step_is_written_again_as_this_builds() {
    let (_tmp, dir) = placed();
    let file = dir.join("run.lvr");
    let mut bytes = std::fs::read(&file).unwrap();
    let seq = {
        let scan = codec::scan(&bytes);
        scan.frames.len() as u64
    };
    // A delta as layout 8 wrote one: its seq, its time, no change, and one
    // log line (variant 7 of its events).
    let step = (seq, 5i64, Vec::<u8>::new(), vec![(7u32, "kept")]);
    bytes.extend(codec::encode(FrameKind::Delta, &step).unwrap());
    std::fs::write(&file, &bytes).unwrap();

    upgrade(&dir).unwrap();
    let reader = RunFileReader::read(&file).unwrap();
    let steps = reader.deltas(0, reader.last_seq()).unwrap();
    assert!(steps.iter().any(|d| {
        d.events
            .iter()
            .any(|e| matches!(e, leviath_runtime::state::RunEvent::Log(line) if line == "kept"))
    }));
}
