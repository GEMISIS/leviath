//! Upgrading a run file in binary layout 2 to this build's layout, in place.
//!
//! The upgraded file is the old one with each spec frame read as layout 2
//! wrote it and written again as this build writes it, and every other frame
//! copied byte for byte: no other frame changed shape (see
//! [`crate::layout2`]). A frame torn by a crash at the end of the old file is
//! left out, as any reader of the file would cut it off. The old file is kept
//! in the run's `legacy/` directory first, and the new one then replaces it in
//! one step, so the run directory always holds one whole run file or the
//! other.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use leviath_core::files::RUN_FILE;
use leviath_runtime::runfile::codec::{self, FrameKind, HEADER_LEN};
use leviath_runtime::spec::names::RunId;
use leviath_runtime::spec::run_spec::RunSpec;

use crate::ConvertError;
use crate::layout2;
use crate::write::LEGACY_DIR;

/// What upgrading a run file did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeReport {
    /// The run.
    pub run_id: RunId,
    /// The run file, now in this build's layout.
    pub run_file: PathBuf,
    /// The run file as it was, kept in the run's `legacy/` directory.
    pub original: PathBuf,
    /// The bytes of a frame a crash left half written at the end of the old
    /// file, which the upgraded file leaves out. The kept original still
    /// holds them.
    pub cut: usize,
}

/// The name the old file is kept under in `legacy/`, the `n`th time one
/// with other bytes is already there.
fn kept_name(n: u32) -> String {
    match n {
        0 => "run.v2.lvr".to_string(),
        n => format!("run.v2.{n}.lvr"),
    }
}

/// The first bytes of the file at `path`, as far as a run file's header goes.
fn header_of(path: &Path) -> Option<[u8; HEADER_LEN]> {
    let mut head = [0u8; HEADER_LEN];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .ok()
        .map(|()| head)
}

pub(crate) fn needs_upgrade(run_dir: &Path) -> bool {
    header_of(&run_dir.join(RUN_FILE))
        .is_some_and(|head| codec::check_header(&head, &layout2::FINGERPRINT).is_ok())
}

pub(crate) fn upgrade(run_dir: &Path) -> Result<UpgradeReport, ConvertError> {
    let path = run_dir.join(RUN_FILE);
    if !needs_upgrade(run_dir) {
        return Err(ConvertError::NotLayout2 { path });
    }
    // A file that went between the two reads reads as one with no frames,
    // which is refused below.
    let bytes = std::fs::read(&path).unwrap_or_default();
    let (upgraded, run_id, cut) = rewrite(&path, &bytes)?;
    let legacy = run_dir.join(LEGACY_DIR);
    let original = leviath_sys::perms::create_private_dir_all(&legacy)
        .and_then(|()| keep(&legacy, &bytes))
        .and_then(|kept| leviath_sys::perms::write_private(&path, &upgraded).map(|()| kept))
        .map_err(ConvertError::io(run_dir))?;
    Ok(UpgradeReport {
        run_id,
        run_file: path,
        original,
        cut,
    })
}

/// The layout-2 run file `bytes`, read from `path`, in this build's layout,
/// with the run's id and how many bytes of torn tail were left out. A file
/// whose spec does not read as layout 2 wrote it is refused.
fn rewrite(path: &Path, bytes: &[u8]) -> Result<(Vec<u8>, RunId, usize), ConvertError> {
    let unreadable = |why: String| ConvertError::Unreadable {
        path: path.to_path_buf(),
        why,
    };
    let (frames, end) = codec::frames(bytes);
    if frames.first().is_none_or(|f| f.kind != FrameKind::Spec) {
        return Err(unreadable("it does not start with the run's spec".into()));
    }
    let mut run_id = None;
    let mut out = codec::header(leviath_runtime::runfile::fingerprint());
    for frame in &frames {
        match frame.kind {
            FrameKind::Spec => {
                let spec: RunSpec = frame
                    .decode::<layout2::RunSpec>(bytes)
                    .map_err(|e| unreadable(e.to_string()))?
                    .into();
                run_id.get_or_insert_with(|| spec.run_id.clone());
                out.extend(crate::write::frame(FrameKind::Spec, &spec));
            }
            _ => out.extend_from_slice(&bytes[frame.offset..frame.end()]),
        }
    }
    let run_id = run_id.expect("the first frame is a spec");
    Ok((out, run_id, bytes.len() - end))
}

/// Keep `bytes`, the run file as it was, in the directory `legacy`, and
/// answer where. A file an earlier upgrade kept there is never written over:
/// one with the same bytes is this copy already, and one with other bytes
/// keeps its name while this one takes the next.
fn keep(legacy: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let free_or_same = |path: &PathBuf| std::fs::read(path).ok().is_none_or(|held| held == bytes);
    let path = (0..)
        .map(|n| legacy.join(kept_name(n)))
        .find(free_or_same)
        .expect("there is always a name not yet taken");
    match path.is_file() {
        true => Ok(path),
        false => leviath_sys::perms::write_private(&path, bytes).map(|()| path),
    }
}
