//! What upgrading a blueprint dropped, kept beside it until its owner has
//! seen it.
//!
//! Upgrading an `agent.leviath` leaves out every key Leviath 0.6.4 and
//! earlier accepted but never read. Those keys never changed a run, but
//! whoever wrote one may have believed it did, so each is a warning, and a
//! warning shown once in a start-up summary is easy to miss. So the upgrade
//! also writes them to `legacy/upgrade-warnings.json` in the blueprint's
//! directory, with a digest of the `agent.toml` it wrote, and `lev list` and
//! `lev validate` show them beside the blueprint for as long as that file is
//! the one the upgrade wrote. Editing the blueprint, or deleting the note,
//! dismisses them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The note's file name, inside the blueprint's `legacy/`.
pub(crate) const FILE: &str = "upgrade-warnings.json";

/// What the note holds.
#[derive(Debug, Serialize, Deserialize)]
struct Note {
    /// The digest of the `agent.toml` the upgrade wrote.
    blueprint: String,
    /// The warnings, a line each.
    warnings: Vec<String>,
}

/// Where the note of the blueprint in `dir` is kept.
pub(crate) fn path(dir: &Path) -> PathBuf {
    dir.join(crate::blueprint_upgrade::LEGACY_DIR).join(FILE)
}

/// The digest of the blueprint file in `dir`, or `None` when it cannot be
/// read.
fn digest_of(dir: &Path) -> Option<String> {
    std::fs::read(dir.join(leviath_blueprint::FILE_NAME))
        .ok()
        .map(|bytes| {
            leviath_runtime::spec::names::Digest::of(&bytes)
                .as_str()
                .to_string()
        })
}

/// Keep `warnings` beside the blueprint in `dir`, against the `agent.toml`
/// it holds now.
#[cfg(feature = "legacy-runs")]
pub(crate) fn record(dir: &Path, warnings: &[String]) -> std::io::Result<()> {
    let note = Note {
        blueprint: digest_of(dir).unwrap_or_default(),
        warnings: warnings.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&note).expect("a note of plain strings serializes");
    leviath_sys::write_atomic(&path(dir), &bytes, None)
}

/// The warnings kept beside the blueprint in `dir`, while its `agent.toml`
/// is still the one they were written against; none once it has changed.
pub(crate) fn read(dir: &Path) -> Vec<String> {
    std::fs::read(path(dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Note>(&bytes).ok())
        .filter(|note| Some(&note.blueprint) == digest_of(dir).as_ref())
        .map(|note| note.warnings)
        .unwrap_or_default()
}

/// How to dismiss the warnings of the blueprint in `dir`.
pub(crate) fn dismiss_hint(dir: &Path) -> String {
    format!(
        "these were dropped when the blueprint was upgraded from agent.leviath; edit {} or \
         delete {} once you have checked them",
        dir.join(leviath_blueprint::FILE_NAME).display(),
        path(dir).display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_are_shown_until_the_blueprint_changes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_empty(), "no note, nothing to show");
        std::fs::create_dir_all(dir.path().join(crate::blueprint_upgrade::LEGACY_DIR)).unwrap();
        std::fs::write(
            dir.path().join(leviath_blueprint::FILE_NAME),
            "[blueprint]\n",
        )
        .unwrap();
        let lines = vec!["blueprint 'a': `x = 1` was dropped".to_string()];
        record(dir.path(), &lines).unwrap();
        assert_eq!(read(dir.path()), lines);
        std::fs::write(
            dir.path().join(leviath_blueprint::FILE_NAME),
            "[blueprint]\n# checked\n",
        )
        .unwrap();
        assert!(read(dir.path()).is_empty(), "an edit dismisses them");
        let hint = dismiss_hint(dir.path());
        assert!(hint.contains(FILE), "{hint}");
    }

    #[test]
    fn a_note_that_does_not_read_or_has_no_blueprint_shows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(crate::blueprint_upgrade::LEGACY_DIR)).unwrap();
        std::fs::write(path(dir.path()), "not json").unwrap();
        assert!(read(dir.path()).is_empty());
        // Written with no blueprint beside it: nothing it could be shown for.
        record(dir.path(), &["w".to_string()]).unwrap();
        assert!(read(dir.path()).is_empty());
    }
}
