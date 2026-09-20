//! A run's files: what it recorded changing, what is in its working directory,
//! and one window of one file's text.
//!
//! Two genuinely different questions, and neither substitutes for the other.
//! What the run recorded is free, because it is already in `meta.json`, but it
//! is a claim about the run rather than about the disk and it is capped at
//! record time. What is in the working directory is the truth, read one
//! directory level per request: that bound is the answer to a repository with a
//! `node_modules` in it, where one request trying to enumerate everything is no
//! answer at all.

use std::path::{Path, PathBuf};

use leviath_core::mime::MimeRegistry;

use super::error::ServeError;
use crate::runstate::RunMeta;

/// Most entries one directory listing returns.
///
/// A single `node_modules/.pnpm` really does hold six figures of entries, and
/// the answer is built in memory.
pub(crate) const MAX_LISTING_ENTRIES: usize = 1000;

/// The most bytes one file read returns.
pub(crate) const MAX_FILE_READ_BYTES: u64 = 1024 * 1024;

/// Which question a listing answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileSource {
    /// What the run recorded modifying.
    Modified,
    /// What is in the run's working directory now.
    Workdir,
}

impl FileSource {
    /// The word this source goes on the wire as.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Workdir => "workdir",
        }
    }

    /// Read the word, or report the bad one.
    pub(crate) fn parse(word: Option<&str>) -> Result<Self, ServeError> {
        match word {
            None | Some("modified") => Ok(Self::Modified),
            Some("workdir") => Ok(Self::Workdir),
            Some(other) => Err(ServeError::BadRequest(format!(
                "Invalid source '{other}': expected 'modified' or 'workdir'"
            ))),
        }
    }
}

/// One entry of a listing.
#[derive(Debug)]
pub(crate) struct FileEntry {
    /// The entry's own name.
    pub(crate) name: String,
    /// Relative to the run's working directory where possible, so a client can
    /// pass it straight back.
    pub(crate) path: String,
    /// Whether it is a directory.
    pub(crate) is_dir: bool,
    /// Its size, or nothing when it could not be stat-ed.
    pub(crate) size: Option<u64>,
    /// False for a recorded path that has since been deleted.
    pub(crate) exists: bool,
    /// True for a recorded path that resolves outside the working directory,
    /// which happens when a tool was handed an absolute path. Reported rather
    /// than hidden.
    pub(crate) outside_workdir: bool,
    /// What the run's own mime registry makes of the name, so a client can
    /// decide whether to render the file without a request per row. By
    /// extension, never sniffed: a listing must not read every file.
    pub(crate) mime_type: String,
}

/// A run's files, one level at a time.
#[derive(Debug)]
pub(crate) struct FileListing {
    /// Which question this answers.
    pub(crate) source: FileSource,
    /// The directory listed, or the working directory for a recorded listing.
    pub(crate) path: String,
    /// Where "up one level" goes. Nothing at the working directory's root: a
    /// client is never led above the fence.
    pub(crate) parent: Option<String>,
    /// The run's working directory, which the paths are relative to.
    pub(crate) workdir: String,
    /// The entries themselves.
    pub(crate) entries: Vec<FileEntry>,
    /// Whether the listing stops short of the directory's real contents.
    pub(crate) truncated: bool,
    /// Whether the run hit the tracked-file cap, so its recorded list is a
    /// prefix and the rest of the names were never stored anywhere.
    pub(crate) modified_files_truncated: bool,
    /// Successful modifying tool calls, which is not a file count: a run that
    /// edits one file three times records three.
    pub(crate) modifying_tool_calls: usize,
}

/// One window of one file's text.
#[derive(Debug)]
pub(crate) struct FileWindow {
    /// The resolved absolute path that was read.
    pub(crate) path: String,
    /// The file's whole size, which is larger than the window when truncated.
    pub(crate) size: u64,
    /// Where this window starts. Not always the offset that was asked for: one
    /// landing mid-character is moved forward to the next character boundary, so
    /// the windows of a file line up and concatenate back into it.
    pub(crate) offset: u64,
    /// Where to start the next read. Nothing when this window reached the end.
    pub(crate) next_offset: Option<u64>,
    /// This window's bytes as text.
    pub(crate) content: String,
    /// Whether the file continues past this window.
    pub(crate) truncated: bool,
}

/// What reading a path gave: a file's window, or the directory's listing.
///
/// A directory is the natural way to ask "what is in here", so it lists rather
/// than refusing.
#[derive(Debug)]
pub(crate) enum FileRead {
    /// One window of one file.
    Window(FileWindow),
    /// A directory's contents.
    Listing(Box<FileListing>),
}

/// List what a run touched, or what is in its working directory.
///
/// A relative directory resolves against the run's working directory, and an
/// absolute one is accepted only where it lands inside it: the same fence the
/// file tools answer to, so this lists exactly what the run could reach.
pub(crate) fn listing(
    meta: &RunMeta,
    source: FileSource,
    dir: Option<&Path>,
    hidden: bool,
    registry: &MimeRegistry,
) -> Result<FileListing, ServeError> {
    let workdir = PathBuf::from(&meta.workdir);
    match source {
        FileSource::Modified => Ok(modified_listing(meta, &workdir, registry)),
        FileSource::Workdir => {
            let resolved = match dir {
                None => None,
                Some(dir) => {
                    let resolved = match dir.is_absolute() {
                        true => dir.to_path_buf(),
                        false => workdir.join(dir),
                    };
                    if !leviath_core::resolves_within(&resolved, &workdir) {
                        return Err(ServeError::Forbidden(format!(
                            "path '{}' is outside the run's working directory",
                            dir.display()
                        )));
                    }
                    Some(resolved)
                }
            };
            workdir_listing(meta, &workdir, resolved.as_deref(), hidden, registry)
        }
    }
}

/// Read one window of one of the run's files, or list the directory it names.
///
/// The path resolves against the run's working directory, and an absolute one is
/// accepted only where it lands inside it: a run's files are the run's, and a
/// path that walks out of the fence is refused rather than followed.
pub(crate) fn read(
    meta: &RunMeta,
    requested_path: &str,
    offset: u64,
    hidden: bool,
    registry: &MimeRegistry,
) -> Result<FileRead, ServeError> {
    let workdir = PathBuf::from(&meta.workdir);
    let requested = PathBuf::from(requested_path);
    let resolved = match requested.is_absolute() {
        true => requested,
        false => workdir.join(&requested),
    };
    if !leviath_core::resolves_within(&resolved, &workdir) {
        return Err(ServeError::Forbidden(format!(
            "path '{requested_path}' is outside the run's working directory"
        )));
    }

    let size = match std::fs::metadata(&resolved) {
        Ok(stat) if stat.is_dir() => {
            return workdir_listing(meta, &workdir, Some(&resolved), hidden, registry)
                .map(|listing| FileRead::Listing(Box::new(listing)));
        }
        Ok(stat) => stat.len(),
        Err(_) => {
            return Err(ServeError::NotFound(format!(
                "file '{requested_path}' not found"
            )));
        }
    };

    // A run's artifact can be far larger than one answer, so a caller pages
    // through it rather than being stuck with the first megabyte.
    if offset > size {
        return Err(ServeError::RangeNotSatisfiable(format!(
            "offset {offset} is past the end of '{requested_path}' ({size} bytes)"
        )));
    }

    let mut bytes = Vec::new();
    if let Err(e) = std::fs::File::open(&resolved)
        // Chained rather than `?`: the offset is already bounded by the file's
        // size above, so a seek into it has no reachable failure of its own.
        .and_then(|mut f| std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset)).map(|_| f))
        .map(|f| std::io::Read::take(f, MAX_FILE_READ_BYTES))
        .and_then(|mut f| std::io::Read::read_to_end(&mut f, &mut bytes))
    {
        return Err(ServeError::NotFound(format!(
            "could not read '{requested_path}': {e}"
        )));
    }
    let read_len = bytes.len();
    let next_offset = offset + read_len as u64;
    let truncated = next_offset < size;

    // A byte offset can land mid-character at either end. Trimming a partial
    // character off the front keeps the window aligned so the *next* page starts
    // on a boundary, which is what makes concatenating pages give back the file.
    let leading_partial = bytes
        .iter()
        .take(4)
        .position(|b| !is_utf8_continuation(*b))
        .filter(|_| offset > 0)
        .unwrap_or(0);
    let bytes = bytes.split_off(leading_partial);
    let read_len = read_len - leading_partial;

    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        // The cap can land mid-character in a file that is otherwise valid
        // UTF-8. That is the cap's doing, not the file's: drop the split
        // character's leading bytes rather than calling a text file binary.
        // (`valid_up_to` is at most 3 bytes short of the end when the only
        // problem is the cut.)
        Err(e) if truncated && e.utf8_error().valid_up_to() + 4 > read_len => {
            let valid = e.utf8_error().valid_up_to();
            let mut prefix = e.into_bytes();
            prefix.truncate(valid);
            String::from_utf8_lossy(&prefix).into_owned()
        }
        Err(_) => {
            return Err(ServeError::UnsupportedMedia(format!(
                "'{requested_path}' is not a text file"
            )));
        }
    };

    Ok(FileRead::Window(FileWindow {
        path: resolved.to_string_lossy().into_owned(),
        size,
        // Where this window actually started and ended, so a caller can ask for
        // the next one without guessing what the UTF-8 trim did.
        offset: offset + leading_partial as u64,
        next_offset: match truncated {
            true => Some(offset + leading_partial as u64 + content.len() as u64),
            false => None,
        },
        content,
        truncated,
    }))
}

/// Whether `b` is a UTF-8 continuation byte (`10xxxxxx`), so the middle of a
/// character rather than the start of one.
fn is_utf8_continuation(b: u8) -> bool {
    (b & 0b1100_0000) == 0b1000_0000
}

/// The type the registry gives a listing row from its name alone, empty for a
/// directory.
fn entry_mime(name: &str, is_dir: bool, registry: &MimeRegistry) -> String {
    match is_dir {
        true => String::new(),
        false => registry.resolve(None, Some(name), &[]).to_string(),
    }
}

/// Whether a run's recorded list of files stops short of what it changed.
fn record_truncated(meta: &RunMeta) -> bool {
    meta.flags.modified_files.len() >= leviath_core::run_meta::MAX_TRACKED_MODIFIED_FILES
}

/// The paths the run recorded modifying, stat-ed against the working directory.
fn modified_listing(meta: &RunMeta, workdir: &Path, registry: &MimeRegistry) -> FileListing {
    let entries = meta
        .flags
        .modified_files
        .iter()
        .map(|rel| {
            let resolved = match Path::new(rel).is_absolute() {
                true => PathBuf::from(rel),
                false => workdir.join(rel),
            };
            let stat = std::fs::metadata(&resolved).ok();
            let name = Path::new(rel)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel.clone());
            let is_dir = stat.as_ref().is_some_and(|m| m.is_dir());
            FileEntry {
                mime_type: entry_mime(&name, is_dir, registry),
                name,
                path: rel.clone(),
                is_dir,
                size: stat.as_ref().map(|m| m.len()),
                // A recorded path can name a file since deleted, or, for a tool
                // given an absolute path, one outside the working directory.
                // Reported rather than filtered away, so the list stays a
                // faithful account of what the run did.
                exists: stat.is_some(),
                outside_workdir: !leviath_core::resolves_within(&resolved, workdir),
            }
        })
        .collect();

    FileListing {
        source: FileSource::Modified,
        path: meta.workdir.clone(),
        parent: None,
        workdir: meta.workdir.clone(),
        entries,
        truncated: false,
        // Both of the ways this list misleads, made visible in the answer rather
        // than left in the documentation.
        modified_files_truncated: record_truncated(meta),
        modifying_tool_calls: meta.flags.modified_file_count,
    }
}

/// One directory level of the run's working directory.
fn workdir_listing(
    meta: &RunMeta,
    workdir: &Path,
    dir: Option<&Path>,
    hidden: bool,
    registry: &MimeRegistry,
) -> Result<FileListing, ServeError> {
    let target = dir
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.to_path_buf());
    if !target.is_dir() {
        // Told apart from an ordinary miss because a lost workspace is a known
        // run outcome, and an empty listing would read as "this run touched
        // nothing".
        return Err(ServeError::NotFound(format!(
            "the run's working directory '{}' no longer exists",
            target.display()
        )));
    }

    let mut entries: Vec<FileEntry> = Vec::new();
    let mut truncated = false;
    for child in std::fs::read_dir(&target).into_iter().flatten().flatten() {
        if entries.len() >= MAX_LISTING_ENTRIES {
            truncated = true;
            break;
        }
        let name = child.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
            continue;
        }
        let path = child.path();
        // Per entry, not just for the directory asked for: a symlinked child can
        // point outside the fence.
        if !leviath_core::resolves_within(&path, workdir) {
            continue;
        }
        let stat = child.metadata().ok();
        let is_dir = stat.as_ref().is_some_and(|m| m.is_dir());
        entries.push(FileEntry {
            path: path
                .strip_prefix(workdir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
            mime_type: entry_mime(&name, is_dir, registry),
            name,
            is_dir,
            size: stat.as_ref().map(|m| m.len()),
            exists: true,
            outside_workdir: false,
        });
    }
    // Directories first, then by name: the grouping a file tree wants, done once
    // here rather than in every client.
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));

    Ok(FileListing {
        source: FileSource::Workdir,
        path: target.to_string_lossy().into_owned(),
        parent: (target != workdir)
            .then(|| target.parent().map(|p| p.to_string_lossy().into_owned()))
            .flatten(),
        workdir: meta.workdir.clone(),
        entries,
        truncated,
        modified_files_truncated: record_truncated(meta),
        modifying_tool_calls: meta.flags.modified_file_count,
    })
}
