//! Run files in binary layout 6, which knew no write paths, upgraded in
//! place.
//!
//! `fixtures/layout-6/run.lvr` was written by a layout-6 build (bfcb0632)
//! against the mock provider: a one-stage run that wrote `docs/a.md` with
//! `write_file` and finished. `fixtures/layout-6.json` is what that build read
//! back from the file: the spec, every delta, and the first and latest state.

use leviath_legacy_runs::{needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec;
use serde_json::Value;

use crate::common::fixtures_dir;

/// `value` as JSON, through its text, as `lev run show --json` prints it.
fn json_of<T: serde::Serialize>(value: T) -> Value {
    serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap()
}

/// The fixture's layout-6 run reads back as it did once upgraded, with no
/// write paths anywhere: not on its graph, not on its stage, and none
/// inherited. Every state and delta reads as it did, each stage record with
/// no resubmissions counted.
#[test]
fn a_layout_6_run_reads_back_with_no_write_paths_once_upgraded() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join("runs")
        .join("probe5-1792508424-19166f7cbca2");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("run.lvr");
    std::fs::copy(fixtures_dir().join("layout-6/run.lvr"), &file).unwrap();
    let before = std::fs::read(&file).unwrap();
    let expected: Value = crate::common::as_layout_8_reads(
        serde_json::from_str(
            &std::fs::read_to_string(fixtures_dir().join("layout-6.json")).unwrap(),
        )
        .unwrap(),
    );
    let refused = RunFileReader::read(&file).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&dir));

    let report = upgrade(&dir).unwrap();
    assert!(!needs_upgrade(&dir));
    assert_eq!(report.original, dir.join("legacy").join("run.v6.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(report.cut, 0);

    let reader = RunFileReader::read(&file).unwrap();
    let spec = reader.spec();
    assert!(spec.graph.write_paths.is_empty());
    assert!(spec.launch.write_paths.is_empty());
    assert!(spec.write_layers(0).is_empty());
    let mut want = expected["spec"].clone();
    crate::common::with_no_write_paths(&mut want);
    assert_eq!(json_of(spec), want);
    let last = reader.last_seq();
    assert_eq!(json_of(reader.deltas(0, last).unwrap()), expected["deltas"]);
    assert_eq!(json_of(reader.state_at(0).unwrap()), expected["first"]);
    assert_eq!(json_of(reader.latest_state().unwrap()), expected["latest"]);

    // Every frame the old file held is still there, in order: the steps and
    // states are written again with each stage record's count, and read back
    // as above.
    let after = std::fs::read(&file).unwrap();
    let kinds = |bytes: &[u8]| -> Vec<codec::FrameKind> {
        codec::scan(bytes).frames.iter().map(|f| f.kind).collect()
    };
    assert_eq!(kinds(&after), kinds(&before));
}
