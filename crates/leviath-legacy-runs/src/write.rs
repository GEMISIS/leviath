//! Writing the run file, and moving the old files aside.

use std::path::{Path, PathBuf};

use leviath_core::files::RUN_FILE;
use leviath_runtime::runfile::codec::{self, FrameKind};
use leviath_runtime::runfile::fingerprint;
use leviath_runtime::state::{RunState, StateDelta};

use crate::ConvertError;
use crate::spec::Built;

/// Where the old files go, inside the run directory.
pub(crate) const LEGACY_DIR: &str = "legacy";

/// Where the run file is written before the old files move.
const PARTIAL: &str = "run.lvr.converting";

/// The files of an old run that a run in the new layout keeps too, under
/// the same names and in the same form: the per-stage logs and audits, the
/// answer, the stored parts, and the list of files the run uploaded to its
/// providers, which are deleted there when it ends. They stay where they
/// are, and the run file names the ones a run file names for a new run.
const KEPT: &[&str] = &[
    "stages",
    leviath_core::FINAL_OUTPUT_FILE,
    leviath_core::files::BLOBS_DIR,
    leviath_runtime::provider_files::LEDGER_FILE,
    PARTIAL,
    LEGACY_DIR,
];

/// The run file's bytes.
///
/// The spec comes first, then each piece of code once by digest (a
/// `(digest, bytes)` pair), then the state the run started in, one delta per
/// step, and the state it was last in. Stored parts stay in `blobs/`, named
/// by the last state.
pub(crate) fn encode(
    built: &Built,
    start: &RunState,
    deltas: &[StateDelta],
    last: &RunState,
) -> Vec<u8> {
    let mut out = codec::header(fingerprint());
    out.extend(frame(FrameKind::Spec, &built.spec));
    for code in &built.code {
        out.extend(frame(FrameKind::Code, code));
    }
    out.extend(frame(FrameKind::State, start));
    for delta in deltas {
        out.extend(frame(FrameKind::Delta, delta));
    }
    out.extend(frame(FrameKind::State, last));
    out
}

/// One frame. Every run-file type is plain data that postcard encodes, and
/// nothing an old run holds comes near a frame's 4 GiB limit.
fn frame<T: serde::Serialize>(kind: FrameKind, payload: &T) -> Vec<u8> {
    codec::encode(kind, payload).expect("a run-file frame always encodes")
}

/// Write `bytes` as the run file in `dir`, and move every other file in it,
/// except the ones [`KEPT`] names, into `legacy/`. Returns the run file and
/// the legacy directory.
pub(crate) fn install(dir: &Path, bytes: &[u8]) -> Result<(PathBuf, PathBuf), ConvertError> {
    let partial = dir.join(PARTIAL);
    let legacy = dir.join(LEGACY_DIR);
    let file = dir.join(RUN_FILE);
    leviath_sys::perms::write_private(&partial, bytes)
        .and_then(|()| leviath_sys::perms::create_private_dir_all(&legacy))
        .and_then(|()| std::fs::read_dir(dir))
        .and_then(|entries| {
            entries
                .flatten()
                .filter(|e| !KEPT.iter().any(|kept| e.file_name() == *kept))
                .try_for_each(|e| std::fs::rename(e.path(), legacy.join(e.file_name())))
        })
        .and_then(|()| std::fs::rename(&partial, &file))
        .map_err(ConvertError::io(dir))?;
    Ok((file, legacy))
}

/// Put the old run in `dir` back as it was before a conversion that stopped
/// part way through [`install`]: its metadata already moved into `legacy/`
/// and no run file written in `dir` yet. Every file in `legacy/` moves back
/// and the half-written run file goes. Answers whether there was a run to
/// put back; one whose files cannot all move back is an error, and is left
/// as it is.
pub(crate) fn put_back(dir: &Path) -> std::io::Result<bool> {
    let legacy = dir.join(LEGACY_DIR);
    if !legacy.join(crate::legacy::META_FILE).is_file()
        || crate::legacy::is_run_file(&dir.join(RUN_FILE))
    {
        return Ok(false);
    }
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&legacy)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    entries
        .iter()
        .filter(|e| !dir.join(e.file_name()).exists())
        .try_for_each(|e| std::fs::rename(e.path(), dir.join(e.file_name())))
        .and_then(|()| std::fs::remove_dir(&legacy))
        .map(|()| {
            let _ = std::fs::remove_file(dir.join(PARTIAL));
            true
        })
}
