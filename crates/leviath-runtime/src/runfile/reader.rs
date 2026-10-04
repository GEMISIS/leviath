//! Reading a run file: its spec, its code, its state at any step, and the
//! stored parts it names beside it.
//!
//! [`RunFileReader::open`] reads the whole file once and indexes its frames.
//! Nothing else is decoded until it is asked for, and the index is built by
//! reading only the first bytes of a frame's payload: every state and delta
//! starts with its `seq`, and every code frame with its digest.
//! Reading those first bytes still decompresses the frame's first block, so
//! the deltas, which are most of a long run's frames, are numbered from the
//! first and the last of them where that is unambiguous.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::codec::{self, CodecError, FrameKind, FrameRef};
use super::error::{RunFileError, RunFileErrorKind};
use super::frames::{CodeFrame, OwnerFrame};
use crate::spec::env::CodeFiles;
use crate::spec::names::Digest;
use crate::spec::run_spec::RunSpec;
use crate::state::{RunState, StateDelta};

/// The most payload bytes indexing reads from one frame: a digest's length
/// byte and its 64 hex digits, with room to spare.
const PEEK_BYTES: u64 = 80;

/// An open run file.
#[derive(Debug)]
pub struct RunFileReader {
    path: PathBuf,
    bytes: Vec<u8>,
    spec: RunSpec,
    code: BTreeMap<Digest, FrameRef>,
    owners: Vec<FrameRef>,
    states: Vec<(u64, FrameRef)>,
    deltas: Vec<(u64, FrameRef)>,
    cut: usize,
}

impl RunFileReader {
    /// Open the run file at `path`.
    ///
    /// A torn tail (a frame a crash left half written) is cut off the file
    /// and logged, the same as the journal before it: everything before it
    /// is whole and is kept.
    pub fn open(path: &Path) -> Result<Self, RunFileError> {
        let reader = Self::read(path)?;
        if reader.cut > 0 {
            let keep = reader.bytes.len() as u64;
            let cut = reader.cut;
            tracing::warn!(
                path = %path.display(),
                cut,
                keep,
                "run file ended in a torn frame; cutting it off"
            );
            truncate(path, keep)?;
        }
        Ok(reader)
    }

    /// Read the run file at `path` without writing to it: a torn tail is
    /// dropped from what is read, and left on the file for its writer.
    pub fn read(path: &Path) -> Result<Self, RunFileError> {
        Self::from_bytes(path, read_file(path)?)
    }

    /// Read a run file from bytes already in memory. `path` names it in
    /// errors. A torn tail is dropped from the bytes but nothing is written.
    pub fn from_bytes(path: &Path, mut bytes: Vec<u8>) -> Result<Self, RunFileError> {
        let err = |e: CodecError| RunFileError::codec(path, e);
        codec::check_header(&bytes, super::fingerprint()).map_err(err)?;
        let (frames, end) = codec::frames(&bytes);
        let cut = bytes.len() - end;
        bytes.truncate(end);
        let Some(first) = frames.first().filter(|f| f.kind == FrameKind::Spec) else {
            return Err(RunFileError::new(path, RunFileErrorKind::NoSpec));
        };
        let spec: RunSpec = first.decode(&bytes).map_err(err)?;
        let mut deltas = Vec::new();
        let mut reader = Self {
            path: path.to_path_buf(),
            spec,
            code: BTreeMap::new(),
            owners: Vec::new(),
            states: Vec::new(),
            deltas: Vec::new(),
            cut,
            bytes: Vec::new(),
        };
        for frame in frames.into_iter().skip(1) {
            match frame.kind {
                FrameKind::Code => {
                    let digest: Digest = peek(&bytes, &frame).map_err(err)?;
                    reader.code.insert(digest, frame);
                }
                FrameKind::State => {
                    let seq: u64 = peek(&bytes, &frame).map_err(err)?;
                    reader.states.push((seq, frame));
                }
                FrameKind::Delta => deltas.push(frame),
                FrameKind::Owner => reader.owners.push(frame),
                FrameKind::Spec => return Err(err(CodecError::Corrupt(frame.offset as u64))),
            }
        }
        reader.deltas = numbered(&bytes, deltas).map_err(err)?;
        reader.bytes = bytes;
        Ok(reader)
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many bytes of the file are whole frames.
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Whether the file holds no frames. Never true for a file that opened:
    /// a run file always has its spec.
    pub fn is_empty(&self) -> bool {
        self.bytes.len() <= codec::HEADER_LEN
    }

    /// How many bytes of torn tail were cut off when it was opened.
    pub fn cut_bytes(&self) -> usize {
        self.cut
    }

    /// The run's spec.
    pub fn spec(&self) -> &RunSpec {
        &self.spec
    }

    fn decode<T: DeserializeOwned>(&self, frame: &FrameRef) -> Result<T, RunFileError> {
        frame
            .decode(&self.bytes)
            .map_err(|e| RunFileError::codec(&self.path, e))
    }

    /// Some code the run uses, by digest.
    pub fn code(&self, digest: &Digest) -> Result<Option<Vec<u8>>, RunFileError> {
        match self.code.get(digest) {
            Some(frame) => Ok(Some(self.decode::<CodeFrame>(frame)?.bytes)),
            None => Ok(None),
        }
    }

    /// Every piece of code the spec names, read, for binding.
    pub fn code_files(&self) -> Result<CodeFiles, RunFileError> {
        let mut out = CodeFiles::new();
        for (_, digest) in &self.spec.code {
            let bytes = self.code(digest)?.ok_or_else(|| {
                RunFileError::new(&self.path, RunFileErrorKind::MissingCode(digest.clone()))
            })?;
            out.insert(digest.clone(), bytes);
        }
        Ok(out)
    }

    /// The digests of the code the file holds.
    pub fn code_digests(&self) -> impl Iterator<Item = &Digest> {
        self.code.keys()
    }

    /// The run's directory: where the file is, and the files it names.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new(""))
    }

    /// A stored part's bytes, by digest, from `blobs/` beside the file. A
    /// part whose file is not there is [`RunFileErrorKind::MissingBlob`].
    pub fn blob(&self, digest: &Digest) -> Result<Vec<u8>, RunFileError> {
        read_blob(self.dir(), digest).map_err(|kind| RunFileError::new(&self.path, kind))
    }

    /// Every change of owner, oldest first.
    pub fn owners(&self) -> Result<Vec<OwnerFrame>, RunFileError> {
        self.owners.iter().map(|f| self.decode(f)).collect()
    }

    /// The last step the file holds.
    pub fn last_seq(&self) -> u64 {
        let delta = self.deltas.last().map_or(0, |(s, _)| *s);
        let state = self.states.last().map_or(0, |(s, _)| *s);
        delta.max(state)
    }

    /// How many state checkpoints the file holds.
    pub fn checkpoints(&self) -> usize {
        self.states.len()
    }

    /// The step and the size in bytes of the last state checkpoint, or
    /// zeroes when the file holds none.
    pub fn last_checkpoint(&self) -> (u64, u64) {
        self.states.last().map_or((0, 0), |(s, f)| {
            (*s, (f.len + codec::FRAME_OVERHEAD) as u64)
        })
    }

    /// The run's state as of its last step.
    pub fn latest_state(&self) -> Result<RunState, RunFileError> {
        self.state_at(self.last_seq())
    }

    /// The run's state as of step `seq`: the last checkpoint at or before it,
    /// with the deltas after that checkpoint applied up to `seq`.
    pub fn state_at(&self, seq: u64) -> Result<RunState, RunFileError> {
        let last = self.last_seq();
        if seq > last {
            return Err(RunFileError::new(
                &self.path,
                RunFileErrorKind::NoSuchStep { seq, last },
            ));
        }
        let Some((_, base)) = self.states.iter().rev().find(|(s, _)| *s <= seq) else {
            return Err(RunFileError::new(&self.path, RunFileErrorKind::NoState));
        };
        let mut state: RunState = self.decode(base)?;
        let from = state.seq;
        for (_, frame) in self.deltas.iter().filter(|(s, _)| *s > from && *s <= seq) {
            let delta: StateDelta = self.decode(frame)?;
            if delta.seq != state.seq + 1 {
                return Err(RunFileError::new(
                    &self.path,
                    RunFileErrorKind::SeqGap {
                        expected: state.seq + 1,
                        found: delta.seq,
                    },
                ));
            }
            delta.apply(&mut state);
        }
        Ok(state)
    }

    /// The deltas for steps `from` to `to`, both included, in order.
    pub fn deltas(&self, from: u64, to: u64) -> Result<Vec<StateDelta>, RunFileError> {
        self.deltas
            .iter()
            .filter(|(s, _)| *s >= from && *s <= to)
            .map(|(_, f)| self.decode(f))
            .collect()
    }
}

/// Where the stored part `digest` of the run in `run_dir` is.
pub fn blob_path(run_dir: &Path, digest: &Digest) -> PathBuf {
    run_dir
        .join(leviath_core::files::BLOBS_DIR)
        .join(digest.as_str())
}

/// The bytes of the stored part `digest` of the run in `run_dir`. A file
/// that is not there is [`RunFileErrorKind::MissingBlob`], naming the part;
/// one that does not read is [`RunFileErrorKind::Io`].
pub fn read_blob(run_dir: &Path, digest: &Digest) -> Result<Vec<u8>, RunFileErrorKind> {
    std::fs::read(blob_path(run_dir, digest)).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => RunFileErrorKind::MissingBlob(digest.clone()),
        _ => RunFileErrorKind::Io(e.to_string()),
    })
}

/// Each delta frame with its step.
///
/// The writer appends one delta per step, in order, so when the first and
/// the last delta's steps are exactly as far apart as there are deltas
/// between them, every step in between is known without decompressing its
/// frame. Any other file (a gap, a step out of place) has every delta's step
/// read from the delta.
fn numbered(bytes: &[u8], frames: Vec<FrameRef>) -> Result<Vec<(u64, FrameRef)>, CodecError> {
    let (Some(first), Some(last)) = (frames.first(), frames.last()) else {
        return Ok(Vec::new());
    };
    let from: u64 = peek(bytes, first)?;
    let to: u64 = peek(bytes, last)?;
    if to.checked_sub(from) == Some(frames.len() as u64 - 1) {
        return Ok((from..).zip(frames).collect());
    }
    frames
        .into_iter()
        .map(|frame| Ok((peek(bytes, &frame)?, frame)))
        .collect()
}

/// Decode the value a frame's payload starts with, reading no more of the
/// payload than that takes.
fn peek<T: DeserializeOwned>(bytes: &[u8], frame: &FrameRef) -> Result<T, CodecError> {
    let body = bytes.get(frame.body..frame.body + frame.len).unwrap_or(&[]);
    let mut head = Vec::new();
    zstd::stream::read::Decoder::with_buffer(body)
        .and_then(|d| d.take(PEEK_BYTES).read_to_end(&mut head))
        .map_err(decode_error)?;
    postcard::take_from_bytes(&head)
        .map(|(value, _)| value)
        .map_err(decode_error)
}

fn decode_error(e: impl std::fmt::Display) -> CodecError {
    CodecError::Decode(e.to_string())
}

/// The bytes of the run file at `path`. A file that does not start with
/// [`codec::MAGIC`] is read no further than its header, which is all it
/// takes to refuse it: a directory of runs can hold files of another format
/// as big as any run's.
pub(super) fn read_file(path: &Path) -> Result<Vec<u8>, RunFileError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut file| {
            file.by_ref()
                .take(codec::HEADER_LEN as u64)
                .read_to_end(&mut bytes)
                .and_then(|_| match bytes.starts_with(codec::MAGIC) {
                    true => file.read_to_end(&mut bytes),
                    false => Ok(0),
                })
        })
        .map_err(|e| RunFileError::io(path, &e))?;
    Ok(bytes)
}

/// Cut the file at `path` to `len` bytes.
pub(super) fn truncate(path: &Path, len: u64) -> Result<(), RunFileError> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_len(len))
        .map_err(|e| RunFileError::io(path, &e))
}
