//! The files a run keeps beside its run file, as the run file names them.
//!
//! A run's large or readable data is not copied into `run.lvr`: its answer
//! (`final_output`), each stage's logs (`stages/<n>/output.log` and
//! `stages/<n>/logs.log`), the taint gate's audit (`stages/<n>/taint_audit.json`)
//! and every stored part (`blobs/<sha256>`) are files in the run's directory.
//! The run file records a reference to each: its path relative to the run's
//! directory, its size, and for a file written whole its digest. Every reader
//! finds a run's files through these references, with [`FileRef::path_in`],
//! and nowhere else.
//!
//! The persistence lane writes these files and the run file, so it is what
//! fills these references in: the state the world inspects carries none.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::spec::names::{Digest, RegionName};

/// The name of a stage's readable output log.
pub const OUTPUT_LOG: &str = "output.log";

/// The name of a stage's operational log.
pub const LOGS_LOG: &str = "logs.log";

/// The name of a stage's taint-gate audit.
pub const TAINT_AUDIT: &str = "taint_audit.json";

/// The directory holding the per-stage files, inside a run's directory.
pub const STAGES_DIR: &str = "stages";

/// One file beside the run file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileRef {
    /// Where it is, relative to the run's directory, with `/` between parts.
    pub path: String,
    /// Its size in bytes when the step that names it was written. A log is
    /// only appended to, so a log may be longer by then, never shorter.
    pub bytes: u64,
    /// The sha256 of its contents, for a file written whole.
    pub sha256: Option<Digest>,
}

/// Why a file a run file names cannot be read as it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRefError {
    /// The path leaves the run's directory.
    Outside(String),
    /// The file is not there, or does not read.
    Unreadable {
        /// The path the run file names.
        path: String,
        /// What the operating system said.
        error: String,
    },
    /// The file is not what the run file says it is.
    Changed {
        /// The path the run file names.
        path: String,
        /// What is wrong with it.
        why: String,
    },
}

impl std::fmt::Display for FileRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outside(path) => write!(
                f,
                "the run file names '{path}', which is outside the run's directory"
            ),
            Self::Unreadable { path, error } => write!(f, "{path} does not read: {error}"),
            Self::Changed { path, why } => {
                write!(f, "{path} is not the file the run file names: {why}")
            }
        }
    }
}

impl std::error::Error for FileRefError {}

impl FileRef {
    /// A file written whole: its size and digest from its bytes.
    pub fn whole(path: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            path: path.into(),
            bytes: bytes.len() as u64,
            sha256: Some(Digest::of(bytes)),
        }
    }

    /// A log `bytes` long.
    pub fn log(path: impl Into<String>, bytes: u64) -> Self {
        Self {
            path: path.into(),
            bytes,
            sha256: None,
        }
    }

    /// The file in the run directory `run_dir`. A path that is absolute or
    /// climbs out of the directory is refused: a run file can come from
    /// another machine, and what it names is read without asking.
    pub fn path_in(&self, run_dir: &Path) -> Result<PathBuf, FileRefError> {
        let inside = !self.path.is_empty()
            && self
                .path
                .split(['/', '\\'])
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && !self.path.contains(':');
        match inside {
            true => Ok(run_dir.join(&self.path)),
            false => Err(FileRefError::Outside(self.path.clone())),
        }
    }

    /// The file's bytes, checked against what the run file says: a file
    /// written whole must have the digest it was written with, and a log
    /// must be at least as long as it was.
    pub fn read(&self, run_dir: &Path) -> Result<Vec<u8>, FileRefError> {
        let bytes =
            std::fs::read(self.path_in(run_dir)?).map_err(|e| FileRefError::Unreadable {
                path: self.path.clone(),
                error: e.to_string(),
            })?;
        self.check(bytes.len() as u64, Some(&bytes))?;
        Ok(bytes)
    }

    /// Whether a file of `len` bytes, holding `bytes` when they were read,
    /// is the one this names.
    pub fn check(&self, len: u64, bytes: Option<&[u8]>) -> Result<(), FileRefError> {
        let changed = |why: String| FileRefError::Changed {
            path: self.path.clone(),
            why,
        };
        match (&self.sha256, bytes) {
            (Some(digest), Some(bytes)) if Digest::of(bytes) != *digest => Err(changed(format!(
                "its contents changed since the run wrote them ({} bytes, {len} now)",
                self.bytes
            ))),
            _ if len < self.bytes => Err(changed(format!(
                "it is {len} bytes, shorter than the {} the run wrote",
                self.bytes
            ))),
            _ => Ok(()),
        }
    }
}

/// The files beside a run's file, other than its stored parts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunFiles {
    /// The answer the run handed back, once it has one.
    pub final_output: Option<FileRef>,
    /// Each stage's files, by the stage's position in the graph, in order.
    pub stages: Vec<StageFiles>,
}

/// One stage's files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StageFiles {
    /// The stage's position in the run's graph.
    pub index: u32,
    /// What the model wrote, once it wrote anything.
    pub output: Option<FileRef>,
    /// The stage's tool activity and events.
    pub logs: Option<FileRef>,
    /// The taint gate's decisions, once it made one.
    pub taint_audit: Option<FileRef>,
}

/// Which of a stage's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageFile {
    /// `output.log`.
    Output,
    /// `logs.log`.
    Logs,
    /// `taint_audit.json`.
    TaintAudit,
}

impl StageFile {
    /// The file's name in the stage's directory.
    pub fn name(self) -> &'static str {
        match self {
            Self::Output => OUTPUT_LOG,
            Self::Logs => LOGS_LOG,
            Self::TaintAudit => TAINT_AUDIT,
        }
    }

    /// Where the file of stage `index` is, relative to the run's directory.
    pub fn path(self, index: u32) -> String {
        format!("{STAGES_DIR}/{index}/{}", self.name())
    }
}

impl StageFiles {
    /// The file `which`, when the stage has it.
    pub fn get(&self, which: StageFile) -> Option<&FileRef> {
        match which {
            StageFile::Output => self.output.as_ref(),
            StageFile::Logs => self.logs.as_ref(),
            StageFile::TaintAudit => self.taint_audit.as_ref(),
        }
    }

    fn slot(&mut self, which: StageFile) -> &mut Option<FileRef> {
        match which {
            StageFile::Output => &mut self.output,
            StageFile::Logs => &mut self.logs,
            StageFile::TaintAudit => &mut self.taint_audit,
        }
    }
}

impl RunFiles {
    /// The files of the stage at `index`, when it has any.
    pub fn stage(&self, index: u32) -> Option<&StageFiles> {
        self.stages.iter().find(|s| s.index == index)
    }

    /// The file `which` of the stage at `index`, when it has it.
    pub fn stage_file(&self, index: u32, which: StageFile) -> Option<&FileRef> {
        self.stage(index).and_then(|s| s.get(which))
    }

    /// Name `file` as the file `which` of the stage at `index`.
    pub fn set_stage_file(&mut self, index: u32, which: StageFile, file: FileRef) {
        let at = match self.stages.binary_search_by_key(&index, |s| s.index) {
            Ok(at) => at,
            Err(at) => {
                self.stages.insert(
                    at,
                    StageFiles {
                        index,
                        output: None,
                        logs: None,
                        taint_audit: None,
                    },
                );
                at
            }
        };
        *self.stages[at].slot(which) = Some(file);
    }
}

/// A stored part the run holds, in `blobs/<sha256>` beside its run file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobFile {
    /// The sha256 of its bytes, which is also its file's name.
    pub digest: Digest,
    /// Its mime type.
    pub mime_type: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its file name, when it had one.
    pub name: Option<String>,
    /// The region it first appeared in. `None` for a part no recorded
    /// context held.
    pub region: Option<RegionName>,
    /// The tool whose result carried it, when one did.
    pub tool: Option<String>,
}

impl BlobFile {
    /// Where its bytes are, relative to the run's directory.
    pub fn path(&self) -> String {
        format!("{}/{}", leviath_core::files::BLOBS_DIR, self.digest)
    }
}

/// Add to `blobs` every stored part `context` holds that it does not list
/// yet, in the order they appear. Returns how many were added.
pub fn note_blobs(blobs: &mut Vec<BlobFile>, context: &super::ContextState) -> usize {
    let before = blobs.len();
    for region in &context.regions {
        for entry in &region.entries {
            for part in &entry.parts {
                let super::context::PartBody::Stored(blob) = &part.body else {
                    continue;
                };
                if blobs.iter().any(|b| b.digest == blob.digest) {
                    continue;
                }
                blobs.push(BlobFile {
                    digest: blob.digest.clone(),
                    mime_type: part.mime_type.clone(),
                    size: blob.size,
                    name: part.name.clone(),
                    region: Some(region.name.clone()),
                    tool: match &entry.kind {
                        super::EntryKind::ToolResult { tool, .. } => Some(tool.clone()),
                        _ => None,
                    },
                });
            }
        }
    }
    blobs.len() - before
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
