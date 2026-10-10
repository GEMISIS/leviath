//! Run files in binary layout 2, which alpha builds wrote, upgraded in place.
//!
//! `fixtures/layout-2/run.lvr` was written by a layout-2 build (5b443ecd):
//! its converter turned the `finished` fixture, under a two-stage blueprint
//! whose stages a machine looked up (each with a reply cap and two
//! fallbacks), into a run file, and its writer then carried the run on for
//! four steps under a new owner. `fixtures/layout-2.json` is what that build
//! read back from the file: the spec, the code, the owners, the first and the
//! latest state, and every delta.

use std::path::PathBuf;

use leviath_legacy_runs::{ConvertError, needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec::{self, FrameKind};
use leviath_runtime::spec::run_spec::RunSpec;
use serde_json::{Value, json};

use crate::common::{Run, fixtures_dir};

/// The fixture's run file, alone in a run directory of its own.
struct Layout2 {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Layout2 {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("runs")
            .join("probe-1790000000-0123456789ab");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixtures_dir().join("layout-2/run.lvr"), dir.join("run.lvr")).unwrap();
        Self { _tmp: tmp, dir }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("run.lvr")
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.file()).unwrap()
    }

    fn set_bytes(&self, bytes: &[u8]) {
        std::fs::write(self.file(), bytes).unwrap();
    }

    fn kept(&self) -> PathBuf {
        self.dir.join("legacy").join("run.v2.lvr")
    }
}

/// What the layout-2 build read back from the fixture.
fn expected() -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join("layout-2.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// A stage plan as the layout-2 build showed it, the way this build shows
/// the same stage: its provider, model, window and fallbacks as one model,
/// and its cap as the stage's reply cap.
fn as_this_build_shows(mut stage: Value) -> Value {
    let plan = stage.as_object_mut().unwrap();
    let mut take = |key: &str| plan.remove(key).unwrap();
    let model = json!({
        "provider": take("provider"),
        "id": take("model"),
        "context_window": take("context_window"),
        "fallbacks": take("fallbacks"),
    });
    let cap = take("max_output_tokens");
    plan.insert("model".into(), model);
    plan.insert("reply_cap".into(), cap);
    stage
}

fn json_of<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn a_layout_2_run_file_reads_back_as_it_did_once_upgraded() {
    let run = Layout2::new();
    let before = run.bytes();
    let expected = expected();
    assert_eq!(hex(&before[4..codec::HEADER_LEN]), expected["fingerprint"]);
    // This build refuses the file as it stands, by the build that wrote it.
    let refused = RunFileReader::read(&run.file()).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&run.dir));

    let report = upgrade(&run.dir).unwrap();
    assert!(!needs_upgrade(&run.dir));
    assert_eq!(report.run_file, run.file());
    assert_eq!(report.original, run.kept());
    assert_eq!(report.cut, 0);
    assert_eq!(std::fs::read(&report.original).unwrap(), before);

    let reader = RunFileReader::read(&run.file()).unwrap();
    let spec = reader.spec();
    assert_eq!(report.run_id, spec.run_id);
    let mut want = expected["spec"].clone();
    let stages: Vec<Value> = want["stages"]
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(as_this_build_shows)
        .collect();
    want["stages"] = Value::Array(stages);
    assert_eq!(json_of(spec), want);
    let main = spec.stage("main").unwrap();
    assert_eq!(main.reply_cap, Some(200));
    assert_eq!(main.model.provider.as_str(), "openai");
    assert_eq!(main.model.context_window, 200_000);
    let fallbacks: Vec<String> = main.model.fallbacks.iter().map(|m| m.to_string()).collect();
    assert_eq!(
        fallbacks,
        ["anthropic/claude-fallback", "ollama/llama-fallback"]
    );
    let review = spec.stage("review").unwrap();
    assert_eq!(review.reply_cap, Some(8_000));
    assert_eq!(review.model.id.as_str(), "gpt-review");

    // Everything after the spec reads as it did.
    let last = reader.last_seq();
    assert_eq!(json_of(last), expected["last_seq"]);
    assert_eq!(json_of(reader.checkpoints()), expected["checkpoints"]);
    assert_eq!(json_of(reader.state_at(0).unwrap()), expected["first"]);
    assert_eq!(json_of(reader.latest_state().unwrap()), expected["latest"]);
    assert_eq!(json_of(reader.deltas(0, last).unwrap()), expected["deltas"]);
    assert_eq!(json_of(reader.owners().unwrap()), expected["owners"]);
    let code: Vec<(String, String)> = reader
        .code_files()
        .unwrap()
        .into_iter()
        .map(|(d, b)| (d.to_string(), String::from_utf8(b).unwrap()))
        .collect();
    assert_eq!(json_of(code), expected["code"]);

    // Byte for byte: every frame but the spec is the one the old file held.
    let after = run.bytes();
    let (old, _) = codec::frames(&before);
    let (new, end) = codec::frames(&after);
    assert_eq!(end, after.len());
    assert_eq!(old.len(), new.len());
    assert_eq!(new[0].kind, FrameKind::Spec);
    for (o, n) in old.iter().zip(&new).skip(1) {
        assert_eq!(o.kind, n.kind);
        assert_eq!(before[o.offset..o.end()], after[n.offset..n.end()]);
    }
}

/// A file upgraded once is in this build's layout, and upgrading it again
/// changes nothing; so is a file this build wrote.
#[test]
fn upgrading_twice_or_a_file_this_build_wrote_changes_nothing() {
    let run = Layout2::new();
    upgrade(&run.dir).unwrap();
    let once = run.bytes();
    let err = upgrade(&run.dir).unwrap_err();
    assert!(
        matches!(&err, ConvertError::NotLayout2 { path } if *path == run.file()),
        "{err}"
    );
    assert!(err.to_string().contains("nothing to upgrade"), "{err}");
    assert_eq!(run.bytes(), once);
    let kept: Vec<_> = std::fs::read_dir(run.dir.join("legacy"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(kept, ["run.v2.lvr"]);

    let converted = Run::fixture("finished");
    converted.converted();
    let written = std::fs::read(converted.path("run.lvr")).unwrap();
    assert!(!needs_upgrade(&converted.dir));
    assert!(matches!(
        upgrade(&converted.dir),
        Err(ConvertError::NotLayout2 { .. })
    ));
    assert_eq!(std::fs::read(converted.path("run.lvr")).unwrap(), written);
    assert!(!converted.path("legacy/run.v2.lvr").exists());
}

/// A directory with no run file, or with an old journal, or a file too short
/// to have a header, holds nothing to upgrade.
#[test]
fn a_directory_without_a_layout_2_file_has_nothing_to_upgrade() {
    let old = Run::fixture("finished");
    let empty = tempfile::tempdir().unwrap();
    let short = Layout2::new();
    short.set_bytes(&short.bytes()[..codec::HEADER_LEN - 1]);
    for dir in [old.dir.as_path(), empty.path(), short.dir.as_path()] {
        assert!(!needs_upgrade(dir));
        let err = upgrade(dir).unwrap_err();
        assert!(matches!(err, ConvertError::NotLayout2 { .. }), "{err}");
    }
    assert!(old.path("run.lvr").is_file());
    assert!(!old.path("legacy").exists());
}

/// A layout-2 file whose spec does not read, or that does not start with
/// one, is refused, and left exactly as it was with nothing written beside
/// it.
#[test]
fn a_layout_2_file_that_does_not_read_is_refused_and_left_as_it_was() {
    let run = Layout2::new();
    let fixture = run.bytes();
    let header = &fixture[..codec::HEADER_LEN];
    let not_a_spec = codec::encode(FrameKind::Spec, &"not a spec".to_string()).unwrap();
    let state_first = {
        let (frames, _) = codec::frames(&fixture);
        let state = frames.iter().find(|f| f.kind == FrameKind::State).unwrap();
        fixture[state.offset..state.end()].to_vec()
    };
    for (body, why) in [
        (not_a_spec, "does not decode"),
        (state_first, "does not start with the run's spec"),
        (Vec::new(), "does not start with the run's spec"),
    ] {
        let bad = [header, &body].concat();
        run.set_bytes(&bad);
        assert!(needs_upgrade(&run.dir));
        let err = upgrade(&run.dir).unwrap_err();
        assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
        assert!(err.to_string().contains(why), "{err}");
        assert_eq!(run.bytes(), bad);
        assert!(!run.dir.join("legacy").exists());
    }
}

/// A frame torn by a crash at the end of the file is left out of the
/// upgraded file, and kept in the original.
#[test]
fn a_torn_last_frame_is_left_out_and_kept_in_the_original() {
    let run = Layout2::new();
    let whole = run.bytes();
    let torn = [whole.as_slice(), &[FrameKind::Delta as u8, 9, 0]].concat();
    run.set_bytes(&torn);
    let report = upgrade(&run.dir).unwrap();
    assert_eq!(report.cut, 3);
    assert_eq!(std::fs::read(&report.original).unwrap(), torn);
    let reader = RunFileReader::read(&run.file()).unwrap();
    assert_eq!(reader.cut_bytes(), 0);
    assert_eq!(
        json_of(reader.latest_state().unwrap()),
        expected()["latest"]
    );
}

/// Every spec frame in the file is upgraded, not only the first.
#[test]
fn every_spec_frame_is_upgraded() {
    let run = Layout2::new();
    let fixture = run.bytes();
    let (frames, _) = codec::frames(&fixture);
    let spec = &fixture[frames[0].offset..frames[0].end()];
    run.set_bytes(&[fixture.as_slice(), spec].concat());
    upgrade(&run.dir).unwrap();
    let after = run.bytes();
    let (frames, _) = codec::frames(&after);
    let specs: Vec<RunSpec> = frames
        .iter()
        .filter(|f| f.kind == FrameKind::Spec)
        .map(|f| f.decode(&after).unwrap())
        .collect();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0], specs[1]);
}

/// The file kept as it was is never written over. One an earlier upgrade
/// kept with the same bytes (a start that stopped between keeping the old
/// file and writing the new) is that copy; one with other bytes keeps its
/// name, and this one takes the next.
#[test]
fn a_file_kept_before_is_never_written_over() {
    let same = Layout2::new();
    std::fs::create_dir_all(same.dir.join("legacy")).unwrap();
    std::fs::copy(same.file(), same.kept()).unwrap();
    assert_eq!(upgrade(&same.dir).unwrap().original, same.kept());
    assert_eq!(
        std::fs::read_dir(same.dir.join("legacy")).unwrap().count(),
        1
    );

    let other = Layout2::new();
    let legacy = other.dir.join("legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(other.kept(), b"an earlier file").unwrap();
    std::fs::write(legacy.join("run.v2.1.lvr"), b"and another").unwrap();
    let before = other.bytes();
    let report = upgrade(&other.dir).unwrap();
    assert_eq!(report.original, legacy.join("run.v2.2.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(std::fs::read(other.kept()).unwrap(), b"an earlier file");
    assert_eq!(
        std::fs::read(legacy.join("run.v2.1.lvr")).unwrap(),
        b"and another"
    );
}

/// A run whose old file cannot be kept is not upgraded: its file is left as
/// it was.
#[test]
fn a_run_whose_old_file_cannot_be_kept_is_left_as_it_was() {
    let run = Layout2::new();
    std::fs::write(run.dir.join("legacy"), b"a file where the directory goes").unwrap();
    let before = run.bytes();
    let err = upgrade(&run.dir).unwrap_err();
    assert!(
        matches!(&err, ConvertError::Io { path, .. } if *path == run.dir),
        "{err}"
    );
    assert_eq!(run.bytes(), before);
    assert!(needs_upgrade(&run.dir));
}
