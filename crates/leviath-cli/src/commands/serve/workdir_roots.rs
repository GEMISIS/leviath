//! The directories a `lev serve` lets a caller point a run at.

use std::path::{Path, PathBuf};

/// The `--workdir-root` directories, each resolved once, when the server
/// starts.
///
/// Resolved once and never again. A root resolved afresh on every request
/// follows whatever its path names *now*, so a run working in an outer root
/// could swap an inner root for a symlink to `/` and open the whole
/// filesystem. Held to the directory each root named at startup, there is
/// nothing to swap. A requested path is still resolved on every request
/// (its deepest existing ancestor canonicalized), so a `..` or a symlink
/// inside a root that leads out of it is outside.
#[derive(Debug, Clone, Default)]
pub(super) struct WorkdirRoots(Vec<PathBuf>);

impl WorkdirRoots {
    /// Resolve each root once. A root that is not an existing directory is
    /// refused, so a server never starts fenced to a path somebody could
    /// create later.
    pub(super) fn resolve(roots: &[PathBuf]) -> Result<Self, String> {
        roots
            .iter()
            .map(|root| resolve_one(root))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    /// Whether no root is set, which fences nothing.
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The roots, resolved, in the order the operator gave them.
    pub(super) fn as_slice(&self) -> &[PathBuf] {
        &self.0
    }

    /// The deepest root `path` resolves inside, or `None` when it is under
    /// none of them (and always when no root is set). The deepest, so a
    /// directory under a root nested in another reports the inner one.
    pub(super) fn holding(&self, path: &Path) -> Option<&Path> {
        let real = leviath_core::canonicalize_for_match(path)?;
        self.0
            .iter()
            .filter(|root| real.starts_with(root))
            .max_by_key(|root| root.components().count())
            .map(PathBuf::as_path)
    }
}

/// One root as the directory it names now, or why it cannot be one.
fn resolve_one(root: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(root)
        .ok()
        .filter(|real| real.is_dir())
        .ok_or(format!(
            "--workdir-root '{}' is not a directory that exists; create it first",
            root.display()
        ))
}

#[cfg(test)]
#[path = "workdir_roots_tests.rs"]
mod tests;
