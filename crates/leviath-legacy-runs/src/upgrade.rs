//! Upgrading a run file in binary layout 2, 3, 4 or 5 to this build's layout,
//! in place.
//!
//! The upgraded file is the old one with each spec, state and delta frame read
//! as its layout wrote it and written again as this build writes it, and the
//! code and owner frames copied byte for byte, since they did not change shape
//! (see [`crate::layout2`], [`crate::layout3`], [`crate::layout4`] and
//! [`crate::layout5`]). A state and a delta in layout 4 or 5 already have this
//! build's shape, so they are copied byte for byte too. A frame torn by a
//! crash at the end of the old file is left out, as any reader of the file would cut it
//! off. The old file is kept in the run's `legacy/` directory first, and the
//! new one then replaces it in one step, so the run directory always holds one
//! whole run file or the other.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use leviath_core::files::RUN_FILE;
use leviath_runtime::runfile::codec::{self, FrameKind, HEADER_LEN};
use leviath_runtime::spec::names::RunId;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::{RunState, StateDelta};

use crate::ConvertError;
use crate::write::LEGACY_DIR;
use crate::{layout2, layout3, layout4, layout5};

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

/// The older layouts a run file is upgraded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Alpha builds before stage plans kept one chosen model.
    Two,
    /// Builds before a fan-out declared the workers it may hand items to.
    Three,
    /// Builds before a graph's compaction models were resolved as a stage's
    /// are.
    Four,
    /// Builds before a spawn's idempotency key and a graph's routing were
    /// kept.
    Five,
}

impl Layout {
    /// The layout whose fingerprint `head` carries, if it is an older one.
    fn of(head: &[u8; HEADER_LEN]) -> Option<Self> {
        [
            (Self::Two, &layout2::FINGERPRINT),
            (Self::Three, &layout3::FINGERPRINT),
            (Self::Four, &layout4::FINGERPRINT),
            (Self::Five, &layout5::FINGERPRINT),
        ]
        .into_iter()
        .find(|(_, fingerprint)| codec::check_header(head, fingerprint).is_ok())
        .map(|(layout, _)| layout)
    }

    /// Its number, as the name of a kept file carries it.
    fn number(self) -> u32 {
        match self {
            Self::Two => 2,
            Self::Three => 3,
            Self::Four => 4,
            Self::Five => 5,
        }
    }
}

/// The name the old file in `layout` is kept under in `legacy/`, the `n`th
/// time one with other bytes is already there.
fn kept_name(layout: Layout, n: u32) -> String {
    let v = layout.number();
    match n {
        0 => format!("run.v{v}.lvr"),
        n => format!("run.v{v}.{n}.lvr"),
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

/// The older layout of the run file in `run_dir`, if it is in one.
fn layout_of(run_dir: &Path) -> Option<Layout> {
    header_of(&run_dir.join(RUN_FILE)).and_then(|head| Layout::of(&head))
}

pub(crate) fn needs_upgrade(run_dir: &Path) -> bool {
    layout_of(run_dir).is_some()
}

pub(crate) fn upgrade(run_dir: &Path) -> Result<UpgradeReport, ConvertError> {
    let path = run_dir.join(RUN_FILE);
    let Some(layout) = layout_of(run_dir) else {
        return Err(ConvertError::NotOldLayout { path });
    };
    // A file that went between the two reads reads as one with no frames,
    // which is refused below.
    let bytes = std::fs::read(&path).unwrap_or_default();
    let (upgraded, run_id, cut) = rewrite(&path, &bytes, layout)?;
    let legacy = run_dir.join(LEGACY_DIR);
    let original = leviath_sys::perms::create_private_dir_all(&legacy)
        .and_then(|()| keep(&legacy, &bytes, layout))
        .and_then(|kept| leviath_sys::perms::write_private(&path, &upgraded).map(|()| kept))
        .map_err(ConvertError::io(run_dir))?;
    Ok(UpgradeReport {
        run_id,
        run_file: path,
        original,
        cut,
    })
}

/// The run file `bytes` in `layout`, read from `path`, in this build's
/// layout, with the run's id and how many bytes of torn tail were left out. A
/// file with a frame that does not read as its layout wrote it is refused, and
/// so is one with a damaged frame that whole frames follow: leaving it out
/// would drop them too.
fn rewrite(
    path: &Path,
    bytes: &[u8],
    layout: Layout,
) -> Result<(Vec<u8>, RunId, usize), ConvertError> {
    let unreadable = |why: String| ConvertError::Unreadable {
        path: path.to_path_buf(),
        why,
    };
    let scan = codec::scan(bytes);
    if let Some(damage) = scan.stop.damage() {
        return Err(unreadable(damage.to_string()));
    }
    let (frames, end) = (scan.frames, scan.end);
    if frames.first().is_none_or(|f| f.kind != FrameKind::Spec) {
        return Err(unreadable("it does not start with the run's spec".into()));
    }
    let mut run_id = None;
    let mut out = codec::header(leviath_runtime::runfile::fingerprint());
    for frame in &frames {
        let read = |e: codec::CodecError| unreadable(format!("{e}"));
        match frame.kind {
            FrameKind::Spec => {
                let spec: RunSpec = match layout {
                    Layout::Two => frame.decode::<layout2::RunSpec>(bytes).map(Into::into),
                    Layout::Three => frame.decode::<layout3::RunSpec>(bytes).map(Into::into),
                    Layout::Four => frame.decode::<layout4::RunSpec>(bytes).map(Into::into),
                    Layout::Five => frame.decode::<layout5::RunSpec>(bytes).map(Into::into),
                }
                .map_err(read)?;
                run_id.get_or_insert_with(|| spec.run_id.clone());
                out.extend(crate::write::frame(FrameKind::Spec, &spec));
            }
            // A state and a delta in layout 4 or 5 are this build's.
            FrameKind::State | FrameKind::Delta
                if matches!(layout, Layout::Four | Layout::Five) =>
            {
                out.extend_from_slice(&bytes[frame.offset..frame.end()])
            }
            // A state and a delta are the same shape in layouts 2 and 3.
            FrameKind::State => {
                let state: RunState = frame
                    .decode::<layout3::RunState>(bytes)
                    .map_err(read)?
                    .into();
                out.extend(crate::write::frame(FrameKind::State, &state));
            }
            FrameKind::Delta => {
                let delta: StateDelta = frame
                    .decode::<layout3::StateDelta>(bytes)
                    .map_err(read)?
                    .into();
                out.extend(crate::write::frame(FrameKind::Delta, &delta));
            }
            FrameKind::Code | FrameKind::Owner => {
                out.extend_from_slice(&bytes[frame.offset..frame.end()])
            }
        }
    }
    let run_id = run_id.expect("the first frame is a spec");
    Ok((out, run_id, bytes.len() - end))
}

/// Keep `bytes`, the run file as it was, in the directory `legacy`, and
/// answer where. A file an earlier upgrade kept there is never written over:
/// one with the same bytes is this copy already, and one with other bytes
/// keeps its name while this one takes the next.
fn keep(legacy: &Path, bytes: &[u8], layout: Layout) -> std::io::Result<PathBuf> {
    let free_or_same = |path: &PathBuf| std::fs::read(path).ok().is_none_or(|held| held == bytes);
    let path = (0..)
        .map(|n| legacy.join(kept_name(layout, n)))
        .find(free_or_same)
        .expect("there is always a name not yet taken");
    match path.is_file() {
        true => Ok(path),
        false => leviath_sys::perms::write_private(&path, bytes).map(|()| path),
    }
}
