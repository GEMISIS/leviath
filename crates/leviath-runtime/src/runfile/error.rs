//! What can be wrong with a run file, and which one.

use std::fmt;
use std::path::{Path, PathBuf};

use super::codec::CodecError;
use crate::spec::names::Digest;

/// A run file that could not be read or written, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFileError {
    /// The file.
    pub path: PathBuf,
    /// What is wrong with it.
    pub kind: RunFileErrorKind,
}

/// What is wrong with a run file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFileErrorKind {
    /// The operating system refused a read or a write.
    Io(String),
    /// The bytes are not a readable run file, or a frame in it is not.
    Codec(CodecError),
    /// The file has no spec frame first.
    NoSpec,
    /// The file has no state checkpoint to start from.
    NoState,
    /// A delta does not follow the step before it.
    SeqGap {
        /// The step the file should have had next.
        expected: u64,
        /// The step it has.
        found: u64,
    },
    /// A step was asked for that the file does not reach.
    NoSuchStep {
        /// The step asked for.
        seq: u64,
        /// The last step the file holds.
        last: u64,
    },
    /// A frame names code the file does not hold.
    MissingCode(Digest),
    /// The run names a stored part whose file is not in its `blobs/`
    /// directory.
    MissingBlob(Digest),
    /// A file the run names beside its run file does not read as named.
    Beside(crate::state::files::FileRefError),
}

impl RunFileError {
    /// `kind` for the file at `path`.
    pub fn new(path: &Path, kind: RunFileErrorKind) -> Self {
        Self {
            path: path.to_path_buf(),
            kind,
        }
    }

    /// An I/O failure on the file at `path`.
    pub fn io(path: &Path, error: &std::io::Error) -> Self {
        Self::new(path, RunFileErrorKind::Io(error.to_string()))
    }

    /// A codec failure on the file at `path`.
    pub fn codec(path: &Path, error: CodecError) -> Self {
        Self::new(path, RunFileErrorKind::Codec(error))
    }
}

impl fmt::Display for RunFileErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot be read or written: {e}"),
            Self::Codec(e) => write!(f, "{e}"),
            Self::NoSpec => f.write_str("does not start with the run's spec"),
            Self::NoState => f.write_str("holds no state checkpoint to start from"),
            Self::SeqGap { expected, found } => {
                write!(f, "skips from step {} to step {found}", expected - 1)
            }
            Self::NoSuchStep { seq, last } => {
                write!(f, "has no step {seq}; its last step is {last}")
            }
            Self::MissingCode(d) => write!(f, "does not hold the code {d} its spec names"),
            Self::MissingBlob(d) => write!(
                f,
                "names the stored part {d}, and blobs/{d} beside it is missing"
            ),
            Self::Beside(e) => write!(f, "names a file beside it that does not read: {e}"),
        }
    }
}

impl fmt::Display for RunFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run file {}: {}", self.path.display(), self.kind)
    }
}

impl std::error::Error for RunFileError {}
