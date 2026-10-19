//! Run files in binary layout 3, whose fan-outs named one worker, upgraded in
//! place.
//!
//! `fixtures/layout-3/run.lvr` was written by a layout-3 build (87296d6e)
//! against the mock provider: a parent whose `split` stage fanned out forty
//! items to its `w` stage, two at a time, and then merged. Its spec holds the
//! fan-out stage, its deltas every change of the fan-out in progress, and a
//! checkpoint taken while it ran holds one too. `fixtures/layout-3.json` is
//! what that build read back from the file: the spec, every delta, and the
//! first and latest state.

use std::path::PathBuf;

use leviath_legacy_runs::{ConvertError, needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec::{self, FrameKind};
use leviath_runtime::spec::graph::{EVERY_ITEM, StageMode, WorkerSource};
use leviath_runtime::state::RunState;
use serde_json::{Value, json};

use crate::common::fixtures_dir;

/// The fixture's run file, alone in a run directory of its own.
struct Layout3 {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Layout3 {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("runs")
            .join("fanprobe-1792002932-b74c1e3249b7");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixtures_dir().join("layout-3/run.lvr"), dir.join("run.lvr")).unwrap();
        Self { _tmp: tmp, dir }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("run.lvr")
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.file()).unwrap()
    }
}

/// What the layout-3 build read back from the fixture.
fn expected() -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join("layout-3.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// `value` as JSON, through its text: an `f32` reads back as the number it
/// was written as (`0.7`), as `lev run show --json` printed it, where
/// `serde_json::to_value` widens it to the `f64` nearest its bits.
fn json_of<T: serde::Serialize>(value: T) -> Value {
    serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap()
}

/// The one worker an upgraded fan-out of the fixture has, as this build
/// shows it.
fn the_worker() -> Value {
    json!([{
        "name": "w",
        "stage": "w",
        "blueprint": null,
        "query": null,
        "use_for": EVERY_ITEM,
    }])
}

/// A fan-out's settings as the layout-3 build showed them, the way this build
/// shows the same: the stage it belongs to and its one worker in place of
/// `worker`.
fn config_as_this_build_shows(config: &mut Value, stage: &str) {
    let config = config.as_object_mut().unwrap();
    assert_eq!(config.remove("worker").unwrap(), json!({"stage": "w"}));
    config.insert("stage".into(), stage.into());
    config.insert("workers_available".into(), the_worker());
}

/// A fan-out in progress as the layout-3 build showed it, the way this build
/// shows it: every item, queued or running, with its worker.
fn fan_out_as_this_build_shows(fan_out: &mut Value) {
    if fan_out.is_null() {
        return;
    }
    let stage = fan_out["stage"].as_str().unwrap().to_string();
    config_as_this_build_shows(&mut fan_out["config"], &stage);
    for item in fan_out["queued"].as_array_mut().unwrap() {
        item["worker"] = "w".into();
    }
    for active in fan_out["active"].as_array_mut().unwrap() {
        let run = active[1].take();
        *active = json!([active[0].take(), "w", run]);
    }
}

fn state_as_this_build_shows(mut state: Value) -> Value {
    fan_out_as_this_build_shows(&mut state["fan_out"]);
    state
}

/// The fixture's layout-3 run reads back as it did once upgraded, each
/// fan-out a fan-out of its stage with its one worker, which takes every
/// item: the spec's, every delta's and the checkpoints'.
#[test]
fn a_layout_3_fan_out_reads_back_with_its_one_worker_once_upgraded() {
    let run = Layout3::new();
    let before = run.bytes();
    let expected = expected();
    let refused = RunFileReader::read(&run.file()).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&run.dir));

    let report = upgrade(&run.dir).unwrap();
    assert!(!needs_upgrade(&run.dir));
    assert_eq!(report.original, run.dir.join("legacy").join("run.v3.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(report.cut, 0);

    let reader = RunFileReader::read(&run.file()).unwrap();
    let spec = reader.spec();
    let StageMode::FanOut(fan) = &spec.graph.stage("split").unwrap().mode else {
        panic!("split is a fan-out stage");
    };
    assert_eq!(fan.stage.as_str(), "split");
    let worker = fan.sole_worker().expect("one worker");
    assert_eq!(worker.name.as_str(), "w");
    assert_eq!(worker.runs, WorkerSource::Stage("w".parse().unwrap()));

    let mut want = expected["spec"].clone();
    crate::common::as_layout_6_reads(&mut want);
    for stage in want["graph"]["stages"].as_array_mut().unwrap() {
        let name = stage["name"].as_str().unwrap().to_string();
        if let Some(fan_out) = stage["mode"].get_mut("fan_out") {
            config_as_this_build_shows(fan_out, &name);
        }
    }
    crate::common::with_no_write_paths(&mut want);
    assert_eq!(json_of(spec), want);

    let last = reader.last_seq();
    let mut deltas = expected["deltas"].clone();
    let mut fan_outs = 0;
    for delta in deltas.as_array_mut().unwrap() {
        for change in delta["changes"].as_array_mut().unwrap() {
            if let Some(fan_out) = change.get_mut("FanOut") {
                fan_out_as_this_build_shows(fan_out);
                fan_outs += 1;
            }
        }
    }
    assert!(fan_outs > 2, "the fixture changes its fan-out as it runs");
    assert_eq!(json_of(reader.deltas(0, last).unwrap()), deltas);
    assert_eq!(
        json_of(reader.state_at(0).unwrap()),
        state_as_this_build_shows(expected["first"].clone())
    );
    assert_eq!(
        json_of(reader.latest_state().unwrap()),
        state_as_this_build_shows(expected["latest"].clone())
    );

    // A checkpoint taken while the fan-out ran was read in layout 3's shape
    // too: its queued and running items now name their worker.
    let after = run.bytes();
    let scan = codec::scan(&after);
    assert_eq!(scan.end, after.len());
    let running: Vec<RunState> = scan
        .frames
        .iter()
        .filter(|f| f.kind == FrameKind::State)
        .map(|f| f.decode::<RunState>(&after).unwrap())
        .filter(|s| s.fan_out.is_some())
        .collect();
    let live = running.first().expect("a checkpoint mid fan-out");
    let fan_out = live.fan_out.as_ref().unwrap();
    assert!(!fan_out.queued.is_empty());
    assert!(fan_out.queued.iter().all(|i| i.worker.as_str() == "w"));
}

/// A file upgraded once is in this build's layout: upgrading it again
/// changes nothing, and the original is kept once.
#[test]
fn upgrading_a_layout_3_file_twice_changes_nothing() {
    let run = Layout3::new();
    upgrade(&run.dir).unwrap();
    let once = run.bytes();
    let err = upgrade(&run.dir).unwrap_err();
    assert!(
        matches!(&err, ConvertError::NotOldLayout { path } if *path == run.file()),
        "{err}"
    );
    assert_eq!(run.bytes(), once);
    let kept: Vec<_> = std::fs::read_dir(run.dir.join("legacy"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(kept, ["run.v3.lvr"]);
}

/// A layout-3 file with a state that does not read is refused, and left
/// exactly as it was.
#[test]
fn a_layout_3_file_whose_state_does_not_read_is_refused() {
    let run = Layout3::new();
    let fixture = run.bytes();
    let spec = {
        let frames = codec::scan(&fixture).frames;
        fixture[..frames[0].end()].to_vec()
    };
    let not_a_state = codec::encode(FrameKind::State, &"not a state".to_string()).unwrap();
    let bad = [spec, not_a_state].concat();
    std::fs::write(run.file(), &bad).unwrap();
    let err = upgrade(&run.dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
    assert_eq!(run.bytes(), bad);
    assert!(!run.dir.join("legacy").exists());
}

/// So is one whose step does not read: a frame that checks out but holds
/// nothing a step could be.
#[test]
fn a_layout_3_file_whose_step_does_not_read_is_refused() {
    let run = Layout3::new();
    let fixture = run.bytes();
    let spec = {
        let frames = codec::scan(&fixture).frames;
        fixture[..frames[0].end()].to_vec()
    };
    let not_a_step = codec::encode(FrameKind::Delta, &()).unwrap();
    let bad = [spec, not_a_step].concat();
    std::fs::write(run.file(), &bad).unwrap();
    let err = upgrade(&run.dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
    assert_eq!(run.bytes(), bad);
    assert!(!run.dir.join("legacy").exists());
}
