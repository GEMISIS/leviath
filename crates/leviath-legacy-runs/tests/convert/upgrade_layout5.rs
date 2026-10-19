//! Run files in binary layout 5, which kept neither a spawn's idempotency
//! key nor a graph's `routing`, upgraded in place.
//!
//! `fixtures/layout-5/run.lvr` was written by a layout-5 build (fb532a57)
//! against the mock provider: a run of a blueprint that sets `routing` on its
//! graph and its stage, spawned over `lev serve` with a signed webhook and two
//! labels, which ran to the end. Layout 5 wrote nothing for `routing`.
//! `fixtures/layout-5.json` is what that build's `GET /api/runs/{id}/spec`,
//! `/deltas` and `/state` answered for it.

use std::path::PathBuf;

use leviath_legacy_runs::{ConvertError, needs_upgrade, upgrade};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::runfile::codec::{self, FrameKind};
use serde_json::Value;

use crate::common::fixtures_dir;

/// The fixture's run file, alone in a run directory of its own.
struct Layout5 {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Layout5 {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("runs")
            .join("routed-1792428309-8e21fdb7950b");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(fixtures_dir().join("layout-5/run.lvr"), dir.join("run.lvr")).unwrap();
        Self { _tmp: tmp, dir }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("run.lvr")
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.file()).unwrap()
    }
}

/// What the layout-5 build read back from the fixture.
fn expected() -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join("layout-5.json")).unwrap();
    crate::common::as_layout_8_reads(serde_json::from_str(&text).unwrap())
}

/// `value` as JSON, through its text, as the layout-5 build served it.
fn json_of<T: serde::Serialize>(value: T) -> Value {
    serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap()
}

/// The fixture reads back as it did once upgraded: the same spec with its
/// webhook and labels, under no idempotency key and with no `routing`, which
/// layout 5 never recorded, and every step and state as they were, each
/// stage record with no resubmissions counted.
#[test]
fn a_layout_5_run_reads_back_with_no_key_and_no_routing_once_upgraded() {
    let run = Layout5::new();
    let before = run.bytes();
    let expected = expected();
    let refused = RunFileReader::read(&run.file()).unwrap_err().to_string();
    assert!(refused.contains("different run types"), "{refused}");
    assert!(needs_upgrade(&run.dir));

    let report = upgrade(&run.dir).unwrap();
    assert!(!needs_upgrade(&run.dir));
    assert_eq!(report.original, run.dir.join("legacy").join("run.v5.lvr"));
    assert_eq!(std::fs::read(&report.original).unwrap(), before);
    assert_eq!(report.cut, 0);

    let reader = RunFileReader::read(&run.file()).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.delivery.idempotency, None);
    assert_eq!(spec.delivery.metadata["ticket"], "959");
    assert!(spec.delivery.callback_secret().is_some());
    assert_eq!(spec.graph.routing, None);
    assert!(spec.graph.stages.iter().all(|s| s.routing.is_none()));
    let mut want = expected["spec"].clone();
    crate::common::as_layout_6_reads(&mut want);
    assert_eq!(json_of(spec), want);
    assert_eq!(
        json_of(reader.deltas(0, reader.last_seq()).unwrap()),
        expected["deltas"]
    );
    assert_eq!(json_of(reader.latest_state().unwrap()), expected["latest"]);

    assert_eq!(kinds(&run.bytes()), kinds(&before));
}

/// The kind of every frame in `bytes`, in order.
fn kinds(bytes: &[u8]) -> Vec<FrameKind> {
    codec::scan(bytes).frames.iter().map(|f| f.kind).collect()
}

/// A file upgraded once is in this build's layout: upgrading it again is
/// refused and changes nothing.
#[test]
fn upgrading_a_layout_5_file_twice_changes_nothing() {
    let run = Layout5::new();
    upgrade(&run.dir).unwrap();
    let once = run.bytes();
    let err = upgrade(&run.dir).unwrap_err();
    assert!(
        matches!(&err, ConvertError::NotOldLayout { path } if *path == run.file()),
        "{err}"
    );
    assert_eq!(run.bytes(), once);
}

/// A layout-5 file whose spec does not read is refused, and left exactly as
/// it was.
#[test]
fn a_layout_5_file_whose_spec_does_not_read_is_refused() {
    let run = Layout5::new();
    let mut head = [0u8; 32];
    head.copy_from_slice(&run.bytes()[4..36]);
    let mut bad = codec::header(&head);
    bad.extend(codec::encode(FrameKind::Spec, &"not a spec".to_string()).unwrap());
    std::fs::write(run.file(), &bad).unwrap();
    let err = upgrade(&run.dir).unwrap_err();
    assert!(matches!(err, ConvertError::Unreadable { .. }), "{err}");
    assert_eq!(run.bytes(), bad);
    assert!(!run.dir.join("legacy").exists());
}

/// A layout-5 run written by a later layout-5 build (dd98049e), parked in an
/// interactive stage with `idle_timeout_secs` waiting for its next message,
/// upgrades without losing any of it: the stage's interactive settings, the
/// phase and wait it parked in, and every step.
/// `fixtures/layout-5-parked.json` is what that build served for it.
#[test]
fn a_parked_interactive_layout_5_run_upgrades_without_loss() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp
        .path()
        .join("runs")
        .join("chatty-1792435373-147fdae882e9");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("run.lvr");
    std::fs::copy(fixtures_dir().join("layout-5-parked/run.lvr"), &file).unwrap();
    let before = std::fs::read(&file).unwrap();
    let text = std::fs::read_to_string(fixtures_dir().join("layout-5-parked.json")).unwrap();
    let expected = crate::common::as_layout_8_reads(serde_json::from_str(&text).unwrap());
    assert!(needs_upgrade(&dir));

    upgrade(&dir).unwrap();
    let reader = RunFileReader::read(&file).unwrap();
    let latest = reader.latest_state().unwrap();
    assert_eq!(
        json_of(&reader.spec().graph.stages[0].mode),
        serde_json::json!({"interactive": {"idle_timeout_secs": 900}})
    );
    assert_eq!(json_of(&latest.phase), "AwaitingMessage");
    let mut want = expected["spec"].clone();
    crate::common::as_layout_6_reads(&mut want);
    assert_eq!(json_of(reader.spec()), want);
    assert_eq!(
        json_of(reader.deltas(0, reader.last_seq()).unwrap()),
        expected["deltas"]
    );
    // The build served the live run's state, whose stage files its next
    // record had not written yet; everything the file holds is compared.
    let mut held = json_of(latest);
    let mut served = expected["latest"].clone();
    held["files"] = Value::Null;
    served["files"] = Value::Null;
    assert_eq!(held, served);
    assert_eq!(kinds(&std::fs::read(&file).unwrap()), kinds(&before));
}
