//! The file-reading half of a run: what it touched, and one window of one file.
//!
//! Its own module because it answers a different question from the rest of the
//! run. Everything here is about the disk as it stands - a listing, a path, a
//! window of bytes - where a run's other fields are about what the run *did*.
//! The types are thin readings of [`files`](super::super::super::core::files),
//! which is where the fence and the caps live.

use async_graphql::{Enum, SimpleObject};

use super::super::super::core::files;
use super::super::scalars::BigInt;

/// Which question a file listing answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum FileSource {
    /// What the run recorded modifying. Free, and a claim about the run rather
    /// than about the disk: it is capped at record time, and `modifiedFilesTruncated`
    /// says when that cap was hit.
    Modified,
    /// What is in the run's working directory now, one level per request.
    Workdir,
}

impl From<FileSource> for files::FileSource {
    fn from(source: FileSource) -> Self {
        match source {
            FileSource::Modified => Self::Modified,
            FileSource::Workdir => Self::Workdir,
        }
    }
}

/// One entry of a run's file listing.
#[derive(Debug, SimpleObject)]
pub(crate) struct FileEntry {
    /// The entry's own name.
    pub(crate) name: String,
    /// Relative to the run's working directory where possible, so it can be
    /// passed straight back as `path`. Separated the way the serving host
    /// separates paths, so a Windows server answers `src\main.rs`: it is the
    /// host's own path, and it goes back to that host.
    pub(crate) path: String,
    /// Whether it is a directory. List it by passing its path back to `files`.
    pub(crate) is_dir: bool,
    /// Its size. Null when it could not be stat-ed.
    pub(crate) size: Option<BigInt>,
    /// False for a recorded path that has since been deleted.
    pub(crate) exists: bool,
    /// True for a recorded path outside the working directory, which happens
    /// when a tool was handed an absolute path. Reported rather than hidden.
    pub(crate) is_outside_workdir: bool,
    /// What the run's own mime registry makes of the name. By extension, never
    /// sniffed: a listing must not read every file. Empty for a directory.
    pub(crate) mime_type: String,
}

/// A run's files, one directory level at a time.
#[derive(Debug, SimpleObject)]
pub(crate) struct FileListing {
    /// Which question this answers.
    pub(crate) source: FileSource,
    /// The directory listed, or the working directory for a recorded listing.
    pub(crate) path: String,
    /// Where "up one level" goes. Null at the working directory's root: a client
    /// is never led above the fence.
    pub(crate) parent: Option<String>,
    /// The run's working directory, which the paths are relative to.
    pub(crate) workdir: String,
    /// The entries, directories first and then by name.
    pub(crate) entries: Vec<FileEntry>,
    /// Whether this listing stops short of the directory's real contents.
    pub(crate) truncated: bool,
    /// Whether the run hit the tracked-file cap, so its record is a prefix and
    /// the rest of the names were never stored anywhere. Read `WORKDIR` for the
    /// truth when this is true.
    pub(crate) modified_files_truncated: bool,
    /// Successful modifying tool calls, which is not a file count: a run that
    /// edits one file three times records three.
    pub(crate) modifying_tool_calls: i32,
}

impl From<files::FileListing> for FileListing {
    fn from(listed: files::FileListing) -> Self {
        Self {
            source: match listed.source {
                files::FileSource::Modified => FileSource::Modified,
                files::FileSource::Workdir => FileSource::Workdir,
            },
            path: listed.path,
            parent: listed.parent,
            workdir: listed.workdir,
            entries: listed
                .entries
                .into_iter()
                .map(|entry| FileEntry {
                    name: entry.name,
                    path: entry.path,
                    is_dir: entry.is_dir,
                    size: entry.size.map(|size| BigInt(size as i64)),
                    exists: entry.exists,
                    is_outside_workdir: entry.outside_workdir,
                    mime_type: entry.mime_type,
                })
                .collect(),
            truncated: listed.truncated,
            modified_files_truncated: listed.modified_files_truncated,
            modifying_tool_calls: i32::try_from(listed.modifying_tool_calls).unwrap_or(i32::MAX),
        }
    }
}

/// One window of one file's text.
#[derive(Debug, SimpleObject)]
pub(crate) struct FileWindow {
    /// The resolved absolute path that was read.
    pub(crate) path: String,
    /// The file's whole size, which is larger than this window when truncated.
    pub(crate) size: BigInt,
    /// Where this window starts. Not always the offset asked for: one landing
    /// mid-character is moved forward, so the windows of a file line up.
    pub(crate) offset: BigInt,
    /// Where to start the next read. Null when this window reached the end.
    pub(crate) next_offset: Option<BigInt>,
    /// This window's text.
    pub(crate) content: String,
    /// Whether the file continues past this window.
    pub(crate) truncated: bool,
}

impl From<files::FileWindow> for FileWindow {
    fn from(window: files::FileWindow) -> Self {
        Self {
            path: window.path,
            size: BigInt(window.size as i64),
            offset: BigInt(window.offset as i64),
            next_offset: window.next_offset.map(|at| BigInt(at as i64)),
            content: window.content,
            truncated: window.truncated,
        }
    }
}
