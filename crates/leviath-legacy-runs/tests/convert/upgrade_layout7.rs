//! Run files in binary layout 7, whose stage records counted no
//! resubmissions, upgraded in place.
//!
//! `fixtures/layout-7/run.lvr` was written by a layout-7 build (a7440b8f)
//! against a mock provider whose model calls `submit_output` on every turn: a
//! two-stage run, each stage an output stage that submitted until its
//! iteration cap. `fixtures/layout-7.json` is what that build's
//! `GET /api/runs/{id}/spec`, `/deltas` and `/state` answered for it.

use leviath_legacy_runs::{ConvertError, needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec::{self, FrameKind};
use serde_json::Value;

use crate::common::fixtures_dir;

/// The fixture's run file, alone in a run directory of its own.
fn placed() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join("runs")
        .join("i990fix-1792517508-0f8b2770b0b2");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixtures_dir().join("layout-7/run.lvr"), dir.join("run.lvr")).unwrap();
    (tmp, dir)
}

/// What the layout-7 build served for the fixture.
fn served() -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join("layout-7.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// `value` as JSON, through its text, as the layout-7 build served it.
fn json_of<T: serde::Serialize>(value: T) -> Value {
    serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap()
}

/// The fixture reads back as it did once upgraded, every stage record with no
/// resubmissions counted. The spec is copied byte for byte, since layout 7
/// wrote it in this build's shape; the steps and states are written again.
#[test]
fn a_layout_7_run_reads_back_with_no_resubmissions_once_upgraded() {
    let (_tmp, dir) = placed();
    let file = dir.join("run.lvr");
    let before = std::fs::read(&file).unwrap();
    let refused = RunFileReader::read(&file).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&dir));

    let report = upgrade(&dir).unwrap();
    assert!(!needs_upgrade(&dir));
    assert_eq!(report.original, dir.join("legacy").join("run.v7.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(report.cut, 0);

    let served = served();
    let expected = crate::common::as_layout_8_reads(served.clone());
    let reader = RunFileReader::read(&file).unwrap();
    assert_eq!(json_of(reader.spec()), expected["spec"]);
    assert_eq!(
        json_of(reader.deltas(0, reader.last_seq()).unwrap()),
        expected["deltas"]
    );
    let latest = reader.latest_state().unwrap();
    assert_eq!(json_of(&latest), expected["latest"]);
    let ledger: Vec<(&str, u32)> = latest
        .ledger
        .iter()
        .map(|r| (r.stage.as_str(), r.resubmissions))
        .collect();
    assert_eq!(ledger, [("draft", 0), ("final", 0)]);

    let after = std::fs::read(&file).unwrap();
    let (old, new) = (codec::scan(&before).frames, codec::scan(&after).frames);
    let kinds = |frames: &[codec::FrameRef]| frames.iter().map(|f| f.kind).collect::<Vec<_>>();
    assert_eq!(kinds(&new), kinds(&old));
    assert_eq!(
        before[old[0].offset..old[0].end()],
        after[new[0].offset..new[0].end()]
    );
}

/// A layout-7 file whose spec does not read is refused, and left exactly as
/// it was.
#[test]
fn a_layout_7_file_whose_spec_does_not_read_is_refused() {
    let (_tmp, dir) = placed();
    let file = dir.join("run.lvr");
    let mut head = [0u8; 32];
    head.copy_from_slice(&std::fs::read(&file).unwrap()[4..36]);
    let mut bad = codec::header(&head);
    bad.extend(codec::encode(FrameKind::Spec, &"not a spec".to_string()).unwrap());
    std::fs::write(&file, &bad).unwrap();
    let err = upgrade(&dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), bad);
}

/// A layout-7 file whose state does not read is refused too.
#[test]
fn a_layout_7_file_whose_state_does_not_read_is_refused() {
    let (_tmp, dir) = placed();
    let file = dir.join("run.lvr");
    let bytes = std::fs::read(&file).unwrap();
    let spec = codec::scan(&bytes).frames[0];
    let mut bad = bytes[..spec.end()].to_vec();
    bad.extend(codec::encode(FrameKind::State, &"not a state".to_string()).unwrap());
    std::fs::write(&file, &bad).unwrap();
    let err = upgrade(&dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
}
