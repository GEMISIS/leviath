//! Shared helpers: a copy of a fixture run directory to convert, edits to
//! it, and a reader for the run file it becomes.

use std::path::{Path, PathBuf};

use leviath_core::run_meta::RunMeta;
use leviath_legacy_runs::journal::{self, JournalRecord};
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
pub fn env() -> ConvertEnv<'static> {
    ConvertEnv {
        agents_dir: Some(fixtures_dir().join("agents")),
        stages: None,
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
        // Under a runs directory of its own, so the secret store beside
        // the runs is the test's too.
        let dir = tmp.path().join("runs").join(name);
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

    pub fn records(&self) -> Vec<JournalRecord> {
        let bytes = std::fs::read(self.path("run.lvr")).unwrap();
        journal::read(&bytes).unwrap()
    }

    pub fn set_records(&self, records: &[JournalRecord]) {
        let mut out = journal::MAGIC.to_vec();
        out.extend(1u16.to_be_bytes());
        for r in records {
            let payload = serde_json::to_vec(r).unwrap();
            out.extend((payload.len() as u64).to_be_bytes());
            out.extend(payload);
        }
        std::fs::write(self.path("run.lvr"), out).unwrap();
    }

    /// Change the journal.
    pub fn journal(&self, edit: impl FnOnce(&mut Vec<JournalRecord>)) {
        let mut records = self.records();
        edit(&mut records);
        self.set_records(&records);
    }

    /// Append records written as JSON.
    pub fn append(&self, records: serde_json::Value) {
        let more: Vec<JournalRecord> = serde_json::from_value(records).unwrap();
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
                    JournalRecord::Header { meta, .. }
                    | JournalRecord::Progress { meta, .. }
                    | JournalRecord::Checkpoint { meta, .. } => edit(meta),
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

    /// Sign the run's webhook with `secret`, as an earlier release wrote it
    /// into `meta.json`.
    pub fn sign(&self, secret: &str) {
        self.json("meta.json", |v| v["callback_secret"] = secret.into());
    }

    /// Write `secret` into every journal record that carries the run's
    /// metadata, as an earlier release's journal held it.
    pub fn sign_journal(&self, secret: &str) {
        let frames: Vec<Vec<u8>> = raw_frames(&std::fs::read(self.path("run.lvr")).unwrap())
            .into_iter()
            .map(|frame| {
                let mut v: serde_json::Value = serde_json::from_slice(&frame).unwrap();
                for (_, record) in v.as_object_mut().unwrap() {
                    if let Some(meta) = record.get_mut("meta") {
                        meta["callback_secret"] = secret.into();
                    }
                }
                serde_json::to_vec(&v).unwrap()
            })
            .collect();
        std::fs::write(self.path("run.lvr"), journal_bytes(&frames)).unwrap();
    }

    /// Every file under the run's directory that holds `needle`: read raw,
    /// and a run file also frame by frame once decompressed.
    pub fn files_holding(&self, needle: &str) -> Vec<PathBuf> {
        let mut found = Vec::new();
        walk(&self.dir, &mut |path| {
            let bytes = std::fs::read(path).unwrap();
            let mut held = contains(&bytes, needle);
            if codec::check_header(&bytes, leviath_runtime::runfile::fingerprint()).is_ok() {
                let (frames, _) = codec::frames(&bytes);
                held |= frames.iter().any(|f| {
                    let plain = zstd::stream::decode_all(&bytes[f.body..f.body + f.len]).unwrap();
                    contains(&plain, needle)
                });
            }
            if held {
                found.push(path.to_path_buf());
            }
        });
        found
    }

    /// The secret store beside the runs this run is converted among.
    pub fn store(&self) -> leviath_runtime::secret_store::SecretStore {
        leviath_runtime::secret_store::SecretStore::of_run_dir(&self.dir)
    }

    /// Whether `needle` is anywhere in the converted run file once every
    /// frame is decompressed.
    pub fn file_holds(&self, needle: &str) -> bool {
        let bytes = std::fs::read(self.path("run.lvr")).unwrap();
        let (frames, _) = codec::frames(&bytes);
        frames.iter().any(|f| {
            let plain = zstd::stream::decode_all(&bytes[f.body..f.body + f.len]).unwrap();
            plain.windows(needle.len()).any(|w| w == needle.as_bytes())
        })
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
    pub states: Vec<RunState>,
    pub deltas: Vec<StateDelta>,
    /// The last state.
    pub last: RunState,
}

impl RunFile {
    pub fn read(path: &Path) -> Self {
        let bytes = std::fs::read(path).unwrap();
        codec::check_header(&bytes, leviath_runtime::runfile::fingerprint()).unwrap();
        let (frames, end) = codec::frames(&bytes);
        assert_eq!(end, bytes.len());
        assert_eq!(frames[0].kind, FrameKind::Spec);
        let (mut code, mut states, mut deltas) = (Vec::new(), Vec::new(), Vec::new());
        for f in &frames[1..] {
            match f.kind {
                FrameKind::Code => code.push(f.decode(&bytes).unwrap()),
                FrameKind::State => states.push(f.decode(&bytes).unwrap()),
                FrameKind::Delta => deltas.push(f.decode(&bytes).unwrap()),
                other => panic!("unexpected frame {other:?}"),
            }
        }
        let last: RunState = states.last().cloned().unwrap();
        Self {
            spec: frames[0].decode(&bytes).unwrap(),
            code,
            states,
            deltas,
            last,
        }
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

/// Whether `needle` is in `bytes`.
pub fn contains(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn walk(dir: &Path, each: &mut dyn FnMut(&Path)) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => walk(&path, each),
            false => each(&path),
        }
    }
}

/// The payload of each whole frame of an old journal.
pub fn raw_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut rest = &bytes[journal::MAGIC.len() + 2..];
    let mut out = Vec::new();
    while rest.len() >= 8 {
        let len = u64::from_be_bytes(rest[..8].try_into().unwrap()) as usize;
        if rest.len() < 8 + len {
            break;
        }
        out.push(rest[8..8 + len].to_vec());
        rest = &rest[8 + len..];
    }
    out
}

/// An old journal holding `frames`.
pub fn journal_bytes(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = journal::MAGIC.to_vec();
    out.extend(1u16.to_be_bytes());
    for f in frames {
        out.extend((f.len() as u64).to_be_bytes());
        out.extend(f);
    }
    out
}
