//! A run file read from its two ends, for a reader that wants where a run is
//! now rather than how it got there.
//!
//! The spec is the first frame, and the run's current state is its last
//! checkpoint with the few steps after it applied, both of which the file
//! can be read for without walking it: every frame repeats its length after
//! its checksum, so the last checkpoint is found walking back from the end.
//! A listing of a thousand runs reads this much of each, where opening each
//! with a [`RunFileReader`] would read and index every frame of every file.
//! The file is read the same way: its first bytes for the spec, and its last
//! for the checkpoint, reaching further back only when the checkpoint is
//! further back than that.
//!
//! Any file this cannot read from its ends (a torn tail, a step out of
//! place, no checkpoint at all) is read whole instead, by the
//! [`RunFileReader`], so either way the answer and any error are that
//! reader's.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use super::codec::{self, FrameKind, FrameRef};
use super::error::RunFileError;
use super::reader::RunFileReader;
use crate::spec::run_spec::RunSpec;
use crate::state::{RunState, StateDelta};

/// How much of a file is read from its front for the spec: more than any
/// spec frame a blueprint has come to. A bigger one is read with the whole
/// file.
const HEAD_READ: u64 = 64 * 1024;

/// How much of a file is read from its end first for the last checkpoint and
/// the steps after it. Four times as much again each time that is not
/// enough, until the file is read whole.
const TAIL_READ: u64 = 1024 * 1024;

/// A run as its file stands: its spec, its state as of its last step, and
/// when that step was taken.
#[derive(Debug, Clone, PartialEq)]
pub struct RunFileTail {
    /// The run's spec.
    pub spec: RunSpec,
    /// The run's state as of its last step.
    pub state: RunState,
    /// When the run last moved, in unix seconds: when its last step was
    /// taken, or when it was resolved for one that has taken none.
    pub updated_at: i64,
}

impl RunFileTail {
    /// Read the run file at `path` without writing to it.
    pub fn read(path: &Path) -> Result<Self, RunFileError> {
        Self::read_from_end(path, TAIL_READ)
    }

    /// [`read`](Self::read), reading `window` bytes from the end of the file
    /// first.
    ///
    /// The front of the file and its last `window` bytes are read into one
    /// buffer with a stretch of zeroes between them. No frame ends in zeroes
    /// and checks out, so a walk back from the end either settles on what
    /// was read or runs into the zeroes, and a bigger window is read.
    fn read_from_end(path: &Path, mut window: u64) -> Result<Self, RunFileError> {
        let mut file = std::fs::File::open(path).map_err(|e| RunFileError::io(path, &e))?;
        let len = file.metadata().map_or(0, |meta| meta.len());
        let mut head = Vec::new();
        read_at(&mut file, 0, HEAD_READ, &mut head);
        codec::check_header(&head, super::fingerprint())
            .map_err(|e| RunFileError::codec(path, e))?;
        let front = head.len() as u64;
        loop {
            let base = len.saturating_sub(window);
            if base <= front {
                read_at(&mut file, front, u64::MAX, &mut head);
                return Self::from_whole(path, head);
            }
            let mut bytes = head.clone();
            bytes.resize(head.len() + codec::FRAME_OVERHEAD, 0);
            read_at(&mut file, base, u64::MAX, &mut bytes);
            if let Some(tail) = from_ends(&bytes) {
                return Ok(tail);
            }
            window = window.saturating_mul(4);
        }
    }

    /// The run in the whole of a file, `bytes`, whose header has been
    /// checked. `path` names it in errors.
    fn from_whole(path: &Path, bytes: Vec<u8>) -> Result<Self, RunFileError> {
        match from_ends(&bytes) {
            Some(tail) => Ok(tail),
            None => Self::of(&RunFileReader::from_bytes(path, bytes)?),
        }
    }

    /// The run `reader` holds, as of its last step.
    pub fn of(reader: &RunFileReader) -> Result<Self, RunFileError> {
        let spec = reader.spec();
        let state = reader.latest_state()?;
        // The step decoded a moment ago, as part of the state.
        let updated_at = reader
            .deltas(state.seq, state.seq)
            .ok()
            .and_then(|deltas| deltas.last().map(|delta| delta.at))
            .unwrap_or(spec.created_at);
        Ok(Self {
            spec: spec.clone(),
            state,
            updated_at,
        })
    }
}

/// The run in `bytes`, read from the spec at the front and the last
/// checkpoint and the steps after it at the back. `None` for anything those
/// do not settle on their own.
fn from_ends(bytes: &[u8]) -> Option<RunFileTail> {
    let first = codec::frame_at(bytes, codec::HEADER_LEN)
        .ok()
        .filter(|frame| frame.kind == FrameKind::Spec)?;
    let spec: RunSpec = first.decode(bytes).ok()?;
    // Walk back from the end to the last checkpoint, keeping the steps
    // after it. Code, blobs and owner changes do not change the state.
    let mut after = Vec::new();
    let mut at = bytes.len();
    let checkpoint = loop {
        if at <= first.end() {
            return None;
        }
        let frame = codec::frame_before(bytes, at).ok()?;
        at = frame.offset;
        match frame.kind {
            FrameKind::State => break frame,
            FrameKind::Delta => after.push(frame),
            _ => {}
        }
    };
    let mut state: RunState = checkpoint.decode(bytes).ok()?;
    let mut updated_at = None;
    for frame in after.iter().rev() {
        let delta: StateDelta = frame.decode(bytes).ok()?;
        if delta.seq != state.seq + 1 {
            return None;
        }
        delta.apply(&mut state);
        updated_at = Some(delta.at);
    }
    let updated_at = match updated_at {
        Some(at) => at,
        None => checkpoint_taken_at(bytes, &spec, &state, &checkpoint)?,
    };
    Some(RunFileTail {
        spec,
        state,
        updated_at,
    })
}

/// When the step a checkpoint with no steps after it was taken. The writer
/// puts a checkpoint straight after the step it follows; a run that has taken
/// no step yet moved last when it was resolved.
fn checkpoint_taken_at(
    bytes: &[u8],
    spec: &RunSpec,
    state: &RunState,
    checkpoint: &FrameRef,
) -> Option<i64> {
    if state.seq == 0 {
        return Some(spec.created_at);
    }
    let step = codec::frame_before(bytes, checkpoint.offset)
        .ok()
        .filter(|frame| frame.kind == FrameKind::Delta)?;
    let delta: StateDelta = step.decode(bytes).ok()?;
    (delta.seq == state.seq).then_some(delta.at)
}

/// Append up to `len` bytes of `file` from `at` to `out`. A read that fails
/// appends less, which reads as a file cut short there.
fn read_at(file: &mut std::fs::File, at: u64, len: u64, out: &mut Vec<u8>) {
    let _ = file
        .seek(SeekFrom::Start(at))
        .and_then(|_| file.take(len).read_to_end(out));
}

/// The spec of the run file at `path`, read from the front of the file
/// alone where its first frame is small enough.
pub fn read_spec(path: &Path) -> Result<RunSpec, RunFileError> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(HEAD_READ).read_to_end(&mut head))
        .map_err(|e| RunFileError::io(path, &e))?;
    codec::check_header(&head, super::fingerprint()).map_err(|e| RunFileError::codec(path, e))?;
    let spec = codec::frame_at(&head, codec::HEADER_LEN)
        .ok()
        .filter(|frame| frame.kind == FrameKind::Spec)
        .and_then(|frame| frame.decode(&head).ok());
    match spec {
        Some(spec) => Ok(spec),
        None => Ok(RunFileReader::read(path)?.spec().clone()),
    }
}

#[cfg(test)]
#[path = "tail_tests.rs"]
mod tests;
