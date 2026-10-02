//! The files a run keeps beside its run file, as the run file names them.

use async_graphql::SimpleObject;
use leviath_graphql_derive::mirror;
use leviath_runtime::state::{BlobFile, FileRef, RunFiles, StageFiles as CoreStageFiles};

use super::super::super::scalars::BigInt;
use super::{big, saturating};

/// One file beside a run's file.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateFile {
    /// Where it is, relative to the run's directory.
    pub(crate) path: String,
    /// Its size in bytes when the step that names it was written. A log only
    /// grows, so it may be longer by now.
    pub(crate) bytes: BigInt,
    /// The sha256 of its contents, for a file written whole.
    pub(crate) sha256: Option<String>,
}

impl From<&FileRef> for StateFile {
    fn from(f: &FileRef) -> Self {
        Self {
            path: f.path.clone(),
            bytes: big(f.bytes),
            sha256: f.sha256.as_ref().map(ToString::to_string),
        }
    }
}

/// One stage's files beside its run's file.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StageFileSet {
    /// The stage's position in the run's graph.
    pub(crate) index: i32,
    /// What the model wrote in the stage.
    pub(crate) output: Option<StateFile>,
    /// The stage's tool activity and events.
    pub(crate) logs: Option<StateFile>,
    /// The taint gate's decisions in the stage.
    pub(crate) taint_audit: Option<StateFile>,
}

impl From<&CoreStageFiles> for StageFileSet {
    fn from(s: &CoreStageFiles) -> Self {
        Self {
            index: saturating(s.index),
            output: s.output.as_ref().map(StateFile::from),
            logs: s.logs.as_ref().map(StateFile::from),
            taint_audit: s.taint_audit.as_ref().map(StateFile::from),
        }
    }
}

/// The answer, logs and audits a run keeps beside its run file.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateFiles {
    /// The file holding the answer, once it has one.
    pub(crate) final_output: Option<StateFile>,
    /// Each stage's files, in stage order.
    pub(crate) stages: Vec<StageFileSet>,
}

impl From<&RunFiles> for StateFiles {
    fn from(f: &RunFiles) -> Self {
        Self {
            final_output: f.final_output.as_ref().map(StateFile::from),
            stages: f.stages.iter().map(StageFileSet::from).collect(),
        }
    }
}

/// A stored part a run holds, in `blobs/` beside its run file.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateBlob {
    /// The sha256 of its bytes, which is also its file's name.
    pub(crate) sha256: String,
    /// Its mime type.
    pub(crate) mime_type: String,
    /// Its size in bytes.
    pub(crate) size: BigInt,
    /// Its file name, when it had one.
    pub(crate) name: Option<String>,
    /// The region it first appeared in.
    pub(crate) region: Option<String>,
    /// The tool whose result carried it, when one did.
    pub(crate) tool: Option<String>,
}

impl From<&BlobFile> for StateBlob {
    fn from(b: &BlobFile) -> Self {
        Self {
            sha256: b.digest.to_string(),
            mime_type: b.mime_type.clone(),
            size: big(b.size),
            name: b.name.clone(),
            region: b.region.as_ref().map(ToString::to_string),
            tool: b.tool.clone(),
        }
    }
}
