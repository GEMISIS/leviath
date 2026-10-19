//! Run files in binary layout 4, whose compaction named one model, upgraded
//! in place.
//!
//! `fixtures/layout-4/run.lvr` was written by a layout-4 build (00bc2493)
//! against the mock provider: a two-stage run whose graph summarizes on
//! `openai/gpt-mock`, which compacted twice on the way. `fixtures/layout-4.json`
//! is what that build read back from the file: the spec, every delta, and the
//! first and latest state.

use std::path::PathBuf;

use leviath_legacy_runs::{needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec;
use serde_json::{Value, json};

use crate::common::fixtures_dir;

/// The fixture's run file, alone in a run directory of its own.
struct Layout4 {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Layout4 {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("runs")
            .join("cpinned-1792366371-f4ca717ddd56");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixtures_dir().join("layout-4/run.lvr"), dir.join("run.lvr")).unwrap();
        Self { _tmp: tmp, dir }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("run.lvr")
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.file()).unwrap()
    }
}

/// `value` as JSON, through its text, as `lev run show --json` prints it.
fn json_of<T: serde::Serialize>(value: T) -> Value {
    serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap()
}

/// The fixture's layout-4 run reads back as it did once upgraded: its one
/// compaction model is the graph's `model`, with no `models`, and every
/// state and delta is unchanged.
#[test]
fn a_layout_4_run_reads_back_with_its_one_compaction_model_once_upgraded() {
    let run = Layout4::new();
    let before = run.bytes();
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_dir().join("layout-4.json")).unwrap(),
    )
    .unwrap();
    let refused = RunFileReader::read(&run.file()).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&run.dir));

    let report = upgrade(&run.dir).unwrap();
    assert!(!needs_upgrade(&run.dir));
    assert_eq!(report.original, run.dir.join("legacy").join("run.v4.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(report.cut, 0);

    let reader = RunFileReader::read(&run.file()).unwrap();
    let compaction = reader.spec().graph.compaction.as_ref().unwrap();
    assert_eq!(
        compaction
            .named()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["openai/gpt-mock"]
    );
    let mut want = expected["spec"].clone();
    want["graph"]["compaction"]["models"] = json!([]);
    crate::common::as_layout_6_reads(&mut want);
    assert_eq!(json_of(reader.spec()), want);
    let last = reader.last_seq();
    assert_eq!(json_of(reader.deltas(0, last).unwrap()), expected["deltas"]);
    assert_eq!(json_of(reader.state_at(0).unwrap()), expected["first"]);
    assert_eq!(json_of(reader.latest_state().unwrap()), expected["latest"]);

    // Every frame after the spec is the one the old file held.
    let after = run.bytes();
    let old = codec::scan(&before).frames;
    let new = codec::scan(&after).frames;
    assert_eq!(old.len(), new.len());
    for (o, n) in old.iter().zip(&new).skip(1) {
        assert_eq!(before[o.offset..o.end()], after[n.offset..n.end()]);
    }
}
