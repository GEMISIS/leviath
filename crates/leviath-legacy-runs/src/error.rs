//! Why a run directory could not be converted.

use std::path::PathBuf;

use leviath_runtime::spec::issues::SpawnIssues;
use leviath_runtime::spec::names::NameError;

/// Why a run directory could not be converted. Nothing in the directory is
/// moved or written unless the conversion succeeds.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// The directory already holds a run file in the new format.
    #[error("{} is already a run file; there is nothing to convert", path.display())]
    AlreadyConverted {
        /// The run file.
        path: PathBuf,
    },
    /// The directory holds no run file in binary layout 2 to upgrade: none at
    /// all, or one in another layout, this build's among them.
    #[error("{} is not a run file in layout 2; there is nothing to upgrade", path.display())]
    NotLayout2 {
        /// The run file.
        path: PathBuf,
    },
    /// The directory is not a run in the old layout.
    #[error("{} is not an old run directory: {why}", path.display())]
    NotARun {
        /// The directory.
        path: PathBuf,
        /// What is missing.
        why: String,
    },
    /// A file could not be read, written or moved.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
    /// A file is there but does not parse.
    #[error("{} does not parse: {why}", path.display())]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// The parser's complaint.
        why: String,
    },
    /// The run's blueprint is in neither the run nor the installed agents.
    #[error(
        "the blueprint this run ran is not in the run and not installed; looked at {}",
        show(tried)
    )]
    NoBlueprint {
        /// Every place looked.
        tried: Vec<PathBuf>,
    },
    /// The blueprint parsed but is not a valid run graph.
    #[error("the run's blueprint is not a valid run graph:\n{0}")]
    Graph(SpawnIssues),
    /// A name the run file needs checked is not a valid one.
    #[error("{field} is not valid: {error}")]
    Name {
        /// Where the name came from.
        field: String,
        /// Why it is not valid.
        error: NameError,
    },
}

fn show(paths: &[PathBuf]) -> String {
    let shown: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    shown.join(", ")
}

impl ConvertError {
    pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Self::Io { path, source }
    }

    pub(crate) fn name(field: impl Into<String>) -> impl FnOnce(NameError) -> Self {
        let field = field.into();
        move |error| Self::Name { field, error }
    }
}
