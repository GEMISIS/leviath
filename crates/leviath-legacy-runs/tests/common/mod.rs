//! Shared helpers: a copy of a fixture run directory to convert, edits to
//! it, and a reader for the run file it becomes.

use std::path::{Path, PathBuf};

use leviath_core::run_archive::{self, RunRecord};
use leviath_core::run_meta::RunMeta;
use leviath_legacy_runs::{ConvertEnv, ConvertError, ConvertReport, convert};
use leviath_runtime::runfile::codec::{self, FrameKind};
use leviath_runtime::spec::names::Digest;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::{RunState, StateDelta};

/// Every fixture run directory.
pub const FIXTURES: &[&str] = &[
    "real-finished",
    "finished",
    "errored",
    "fanout-parent",
    "interaction-worker",
    "mid-tool-batch",
];

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The installed agents the fixtures ran.
pub fn env() -> ConvertEnv {
    ConvertEnv {
        agents_dir: Some(fixtures_dir().join("agents")),
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&entry.path(), &target),
            false => {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
}

/// A fixture run directory, copied somewhere it can be changed.
pub struct Run {
    _tmp: tempfile::TempDir,
    pub dir: PathBuf,
}

impl Run {
    pub fn fixture(name: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(name);
        copy_dir(&fixtures_dir().join(name), &dir);
        Self { _tmp: tmp, dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn write(&self, name: &str, text: &str) {
        std::fs::write(self.path(name), text).unwrap();
    }

    pub fn remove(&self, name: &str) {
        std::fs::remove_file(self.path(name)).unwrap();
    }

    pub fn records(&self) -> Vec<RunRecord> {
        let bytes = std::fs::read(self.path("run.lvr")).unwrap();
        run_archive::read_archive(&mut bytes.as_slice()).unwrap().1
    }

    pub fn set_records(&self, records: &[RunRecord]) {
        let mut out = Vec::new();
        run_archive::write_archive_start(&mut out, 1).unwrap();
        for r in records {
            run_archive::write_record(&mut out, r).unwrap();
        }
        std::fs::write(self.path("run.lvr"), out).unwrap();
    }

    /// Change the journal.
    pub fn journal(&self, edit: impl FnOnce(&mut Vec<RunRecord>)) {
        let mut records = self.records();
        edit(&mut records);
        self.set_records(&records);
    }

    /// Append records written as JSON.
    pub fn append(&self, records: serde_json::Value) {
        let more: Vec<RunRecord> = serde_json::from_value(records).unwrap();
        self.journal(|r| r.extend(more));
    }

    /// Change the run's metadata everywhere it is recorded.
    pub fn meta(&self, edit: impl Fn(&mut RunMeta)) {
        let text = std::fs::read_to_string(self.path("meta.json")).unwrap();
        let mut meta: RunMeta = serde_json::from_str(&text).unwrap();
        edit(&mut meta);
        self.write("meta.json", &serde_json::to_string(&meta).unwrap());
        self.journal(|records| {
            for r in records.iter_mut() {
                match r {
                    RunRecord::Header { meta, .. }
                    | RunRecord::Progress { meta, .. }
                    | RunRecord::Checkpoint { meta, .. } => edit(meta),
                    _ => {}
                }
            }
        });
    }

    /// Change a JSON file.
    pub fn json(&self, name: &str, edit: impl FnOnce(&mut serde_json::Value)) {
        let text = std::fs::read_to_string(self.path(name)).unwrap();
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        edit(&mut v);
        self.write(name, &v.to_string());
    }

    pub fn convert(&self) -> Result<ConvertReport, ConvertError> {
        convert(&self.dir, &env())
    }

    pub fn converted(&self) -> (ConvertReport, RunFile) {
        let report = self.convert().unwrap();
        (report, RunFile::read(&self.path("run.lvr")))
    }
}

/// A run file, decoded frame by frame.
pub struct RunFile {
    pub spec: RunSpec,
    pub code: Vec<(Digest, Vec<u8>)>,
    pub blobs: Vec<(Digest, Vec<u8>)>,
    pub states: Vec<RunState>,
    pub deltas: Vec<StateDelta>,
    /// The last state, found walking backwards from the end.
    pub last: RunState,
}

impl RunFile {
    pub fn read(path: &Path) -> Self {
        let bytes = std::fs::read(path).unwrap();
        codec::check_header(&bytes, leviath_runtime::runfile::fingerprint()).unwrap();
        let (frames, end) = codec::frames(&bytes);
        assert_eq!(end, bytes.len());
        assert_eq!(frames[0].kind, FrameKind::Spec);
        let mut file = Self {
            spec: frames[0].decode(&bytes).unwrap(),
            code: Vec::new(),
            blobs: Vec::new(),
            states: Vec::new(),
            deltas: Vec::new(),
            last: codec::last_of(&bytes, end, FrameKind::State)
                .unwrap()
                .unwrap()
                .decode(&bytes)
                .unwrap(),
        };
        for f in &frames[1..] {
            match f.kind {
                FrameKind::Code => file.code.push(f.decode(&bytes).unwrap()),
                FrameKind::Blob => file.blobs.push(f.decode(&bytes).unwrap()),
                FrameKind::State => file.states.push(f.decode(&bytes).unwrap()),
                FrameKind::Delta => file.deltas.push(f.decode(&bytes).unwrap()),
                other => panic!("unexpected frame {other:?}"),
            }
        }
        file
    }

    /// The first state with every delta applied.
    pub fn fold(&self) -> RunState {
        let mut state = self.states[0].clone();
        for d in &self.deltas {
            d.apply(&mut state);
        }
        state
    }
}
