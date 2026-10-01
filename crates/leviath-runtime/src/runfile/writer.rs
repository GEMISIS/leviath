//! Writing a run file: the spec and its code once, then one delta per step
//! and a state checkpoint every so often.
//!
//! The writer holds the run's state as of its last step, so the next step is
//! a diff against it and a checkpoint is that state written whole. It is the
//! file's only writer: appends are ordered, and a failed append is cut back
//! off the file before the error is returned, so a later append never lands
//! behind a torn frame.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::codec::{self, FrameKind};
use super::error::{RunFileError, RunFileErrorKind};
use super::frames::{BlobFrame, CodeFrame, OwnerFrame};
use super::reader::RunFileReader;
use crate::spec::env::CodeFiles;
use crate::spec::names::Digest;
use crate::spec::run_spec::RunSpec;
use crate::state::{RunEvent, RunState, RunStatus, StateDelta};

/// When a writer puts a full state checkpoint in the file.
///
/// A checkpoint is what a reader starts from, so the deltas after the last
/// one are what every read of the current state replays. Two limits keep that
/// replay short: a count of deltas, and the bytes those deltas take measured
/// against the size of the checkpoint they would otherwise be replayed onto.
/// A run whose steps are small checkpoints rarely; a run whose steps rewrite
/// most of its context checkpoints about as often as that stops being cheaper.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckpointPolicy {
    /// Checkpoint after this many deltas.
    pub every: u32,
    /// Checkpoint once the deltas since the last checkpoint take more than
    /// this share of that checkpoint's bytes.
    pub size_ratio: f64,
}

impl Default for CheckpointPolicy {
    /// 64 deltas, or deltas twice the size of the last checkpoint. Measured
    /// on a scripted run of 120 tool-calling turns that compacts its context
    /// every ten: the file came to 67 KB against 46 KB for its deltas alone
    /// and 151 KB with a checkpoint at every step. Halving the ratio made it
    /// 83 KB, and doubling it 57 KB. A read of the current state replays at
    /// most 64 deltas, and never more delta bytes than two checkpoints.
    fn default() -> Self {
        Self {
            every: 64,
            size_ratio: 2.0,
        }
    }
}

/// The one writer of a run file.
#[derive(Debug)]
pub struct RunFileWriter {
    path: PathBuf,
    file: std::fs::File,
    len: u64,
    state: RunState,
    code: BTreeSet<Digest>,
    blobs: BTreeSet<Digest>,
    policy: CheckpointPolicy,
    since_checkpoint: u32,
    delta_bytes: u64,
    checkpoint_bytes: u64,
    owner: Option<OwnerFrame>,
}

/// Encode one frame. The payloads are this build's own types, which always
/// encode; only a frame over 4 GiB could not, and no state is that size.
fn frame<T: Serialize>(kind: FrameKind, payload: &T) -> Vec<u8> {
    codec::encode(kind, payload).expect("a run file frame always encodes")
}

impl RunFileWriter {
    /// Start a run file at `path`: the header, the spec, each piece of code
    /// once, and `initial` as its first checkpoint. The file is owner-only.
    /// An existing file at `path` is replaced.
    pub fn create(
        path: &Path,
        spec: &RunSpec,
        code: &CodeFiles,
        initial: &RunState,
        policy: CheckpointPolicy,
    ) -> Result<Self, RunFileError> {
        let mut buf = codec::header(super::fingerprint());
        buf.extend(frame(FrameKind::Spec, spec));
        for (digest, bytes) in code {
            buf.extend(frame(
                FrameKind::Code,
                &CodeFrame {
                    digest: digest.clone(),
                    bytes: bytes.clone(),
                },
            ));
        }
        let state = frame(FrameKind::State, initial);
        let checkpoint_bytes = state.len() as u64;
        buf.extend(state);
        let file = leviath_sys::write_private(path, &buf)
            .and_then(|()| leviath_sys::open_private_append(path))
            .map_err(|e| RunFileError::io(path, &e))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len: buf.len() as u64,
            state: initial.clone(),
            code: code.keys().cloned().collect(),
            blobs: BTreeSet::new(),
            policy,
            since_checkpoint: 0,
            delta_bytes: 0,
            checkpoint_bytes,
            owner: None,
        })
    }

    /// Carry on writing the run file at `path`, from its last step. A torn
    /// tail is cut off first.
    pub fn open(path: &Path, policy: CheckpointPolicy) -> Result<Self, RunFileError> {
        let reader = RunFileReader::open(path)?;
        let state = reader.latest_state()?;
        let file =
            leviath_sys::open_private_append(path).map_err(|e| RunFileError::io(path, &e))?;
        let checkpoint = reader.last_checkpoint();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len: reader.len(),
            since_checkpoint: (state.seq - checkpoint.0) as u32,
            checkpoint_bytes: checkpoint.1,
            delta_bytes: 0,
            state,
            code: reader.code_digests().cloned().collect(),
            blobs: reader.blob_digests().cloned().collect(),
            policy,
            owner: None,
        })
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many bytes the file holds.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file holds nothing. Never true: a created file has its
    /// header and spec.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The run's state as of the last step written.
    pub fn state(&self) -> &RunState {
        &self.state
    }

    /// The last step written.
    pub fn seq(&self) -> u64 {
        self.state.seq
    }

    /// Whether the file holds the blob `digest`.
    pub fn has_blob(&self, digest: &Digest) -> bool {
        self.blobs.contains(digest)
    }

    /// Whether the file holds the code `digest`.
    pub fn has_code(&self, digest: &Digest) -> bool {
        self.code.contains(digest)
    }

    /// Append `bytes`, synced to disk when `sync` is set.
    fn append(&mut self, bytes: &[u8], sync: bool) -> Result<(), RunFileError> {
        let written = self.file.write_all(bytes).and_then(|()| match sync {
            true => self.file.sync_data(),
            false => self.file.flush(),
        });
        match written {
            Ok(()) => {
                self.len += bytes.len() as u64;
                Ok(())
            }
            Err(e) => {
                // Cut back whatever part of the frame landed, so the next
                // append starts on a frame boundary.
                let _ = self.file.set_len(self.len);
                Err(RunFileError::io(&self.path, &e))
            }
        }
    }

    /// Store a part's bytes, once. `false` when the file already held them.
    pub fn add_blob(&mut self, digest: &Digest, bytes: &[u8]) -> Result<bool, RunFileError> {
        if self.blobs.contains(digest) {
            return Ok(false);
        }
        let payload = BlobFrame {
            digest: digest.clone(),
            bytes: bytes.to_vec(),
        };
        self.append(&frame(FrameKind::Blob, &payload), false)?;
        self.blobs.insert(digest.clone());
        Ok(true)
    }

    /// Store some code, once. `false` when the file already held it.
    pub fn add_code(&mut self, digest: &Digest, bytes: &[u8]) -> Result<bool, RunFileError> {
        if self.code.contains(digest) {
            return Ok(false);
        }
        let payload = CodeFrame {
            digest: digest.clone(),
            bytes: bytes.to_vec(),
        };
        self.append(&frame(FrameKind::Code, &payload), false)?;
        self.code.insert(digest.clone());
        Ok(true)
    }

    /// Append `delta`, with the pending owner change before it and, when
    /// `next` is given and the policy or a finished run asks for one, `next`
    /// as a checkpoint after it: one write, so a step and its checkpoint land
    /// or fail together.
    fn write_delta(
        &mut self,
        delta: &StateDelta,
        next: Option<&RunState>,
    ) -> Result<(), RunFileError> {
        let expected = self.state.seq + 1;
        if delta.seq != expected {
            return Err(RunFileError::new(
                &self.path,
                RunFileErrorKind::SeqGap {
                    expected,
                    found: delta.seq,
                },
            ));
        }
        let owner = self
            .owner
            .take()
            .map(|o| frame(FrameKind::Owner, &o))
            .unwrap_or_default();
        let delta_frame = frame(FrameKind::Delta, delta);
        let since = self.since_checkpoint + 1;
        let delta_bytes = self.delta_bytes + delta_frame.len() as u64;
        let checkpoint = next
            .filter(|s| is_finished(s) || self.due(since, delta_bytes))
            .map(|s| frame(FrameKind::State, s))
            .unwrap_or_default();
        let checkpointed = !checkpoint.is_empty();
        let checkpoint_bytes = checkpoint.len() as u64;
        self.append(&[owner, delta_frame, checkpoint].concat(), checkpointed)?;
        match checkpointed {
            true => {
                self.since_checkpoint = 0;
                self.delta_bytes = 0;
                self.checkpoint_bytes = checkpoint_bytes;
            }
            false => {
                self.since_checkpoint = since;
                self.delta_bytes = delta_bytes;
            }
        }
        Ok(())
    }

    /// Append one step. Its `seq` must be the step after the last one.
    pub fn append_delta(&mut self, delta: &StateDelta) -> Result<(), RunFileError> {
        self.write_delta(delta, None)?;
        delta.apply(&mut self.state);
        Ok(())
    }

    fn due(&self, since: u32, delta_bytes: u64) -> bool {
        let over_size = delta_bytes as f64 > self.policy.size_ratio * self.checkpoint_bytes as f64;
        since > 0 && (since >= self.policy.every || over_size)
    }

    /// Whether the policy asks for a checkpoint now.
    pub fn checkpoint_due(&self) -> bool {
        self.due(self.since_checkpoint, self.delta_bytes)
    }

    /// Write `state` whole, as the checkpoint readers start from, and hold it
    /// as the state the next step is taken against.
    ///
    /// Synced to disk: a checkpoint is what a resume reads, so it is the one
    /// write worth waiting for.
    pub fn checkpoint(&mut self, state: &RunState) -> Result<(), RunFileError> {
        let bytes = frame(FrameKind::State, state);
        self.append(&bytes, true)?;
        self.checkpoint_bytes = bytes.len() as u64;
        self.since_checkpoint = 0;
        self.delta_bytes = 0;
        self.state = state.clone();
        Ok(())
    }

    /// Record that a machine and daemon have taken the run over, in the same
    /// write as the next step. A process that opens a file and never changes
    /// the run leaves no trace in it.
    pub fn owner_on_next_step(&mut self, owner: OwnerFrame) {
        self.owner = Some(owner);
    }

    /// Record that a machine and daemon have taken the run over, now.
    pub fn set_owner(&mut self, owner: &OwnerFrame) -> Result<(), RunFileError> {
        self.append(&frame(FrameKind::Owner, owner), false)
    }

    /// Record the run reaching `next`: the delta from the last step, with
    /// `events`, and a checkpoint when the policy asks for one or the run has
    /// finished. Returns the step written, or `None` when nothing changed and
    /// nothing happened.
    pub fn record(
        &mut self,
        mut next: RunState,
        at: i64,
        events: Vec<RunEvent>,
    ) -> Result<Option<u64>, RunFileError> {
        let delta = StateDelta::between(&self.state, &next, at, events);
        if delta.is_empty() {
            return Ok(None);
        }
        next.seq = delta.seq;
        self.write_delta(&delta, Some(&next))?;
        self.state = next;
        Ok(Some(delta.seq))
    }
}

/// Whether a run has finished, which is when its file ends on a checkpoint.
fn is_finished(state: &RunState) -> bool {
    matches!(
        state.status,
        RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled
    )
}

/// Swap the writer's file for a read-only handle, so every append fails.
#[cfg(test)]
pub(crate) fn break_writes(writer: &mut RunFileWriter) {
    writer.file = std::fs::File::open(&writer.path).expect("the file exists");
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
