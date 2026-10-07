//! Reading a blueprint from disk, and finding an installed one by name.

use std::path::{Path, PathBuf};

use leviath_runtime::spec::env::LoadedBlueprint;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::names::{BlueprintRef, Digest};

use crate::file::{BlueprintFile, FILE_NAME};

/// Why a blueprint could not be read.
#[derive(Debug, thiserror::Error)]
pub enum BlueprintError {
    /// The file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// What the filesystem said.
        source: std::io::Error,
    },
    /// The file is not a blueprint: bad TOML, an unknown key, a missing one.
    #[error("{} is not a valid blueprint: {message}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// What the TOML reader said, with the line.
        message: String,
    },
    /// The file reads, but its graph does not hold together.
    #[error("{} has a graph that does not hold together: {issues}", path.display())]
    Graph {
        /// The file.
        path: PathBuf,
        /// Every problem found.
        issues: SpawnIssues,
    },
}

/// Read the blueprint at `path`: an `agent.toml`, or the directory holding
/// one. Its files (code, prompts) are read relative to that directory, and it
/// is pinned to the digest of the file's bytes.
pub fn load(path: &Path) -> Result<LoadedBlueprint, BlueprintError> {
    let path = file_path(path);
    let bytes = std::fs::read(&path).map_err(|source| BlueprintError::Read {
        path: path.clone(),
        source,
    })?;
    let file = std::str::from_utf8(&bytes)
        .map_err(|e| e.to_string())
        .and_then(BlueprintFile::parse)
        .map_err(|message| BlueprintError::Parse {
            path: path.clone(),
            message,
        })?;
    Ok(LoadedBlueprint {
        graph: file.run_graph(),
        reference: BlueprintRef {
            name: file.blueprint.name,
            digest: Some(Digest::of(&bytes)),
        },
        version: file.blueprint.version,
        base_dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
    })
}

/// Read the blueprint at `path` and check that its graph holds together:
/// every stage, region and input it names is declared, and every setting is
/// in range. This is what `lev validate` runs before its lint.
pub fn validate(path: &Path) -> Result<LoadedBlueprint, BlueprintError> {
    let loaded = load(path)?;
    match loaded.graph.validate(&SpecPath::root().field("graph")) {
        Ok(()) => Ok(loaded),
        Err(issues) => Err(BlueprintError::Graph {
            path: file_path(path),
            issues,
        }),
    }
}

/// Find the installed blueprint `reference` names, looking for
/// `<dir>/<name>/agent.toml` in each of `dirs` in turn. A reference pinned to
/// a digest only matches a file with those exact bytes.
///
/// This is what a host's `ResolveEnv::blueprint` answers with, so every
/// failure is a [`SpawnIssue`] placed relative to the reference: resolving
/// puts `source.blueprint` in front of it. Boxed, since an issue is large
/// beside a result's `Ok`.
pub fn find(
    dirs: &[PathBuf],
    reference: &BlueprintRef,
) -> Result<LoadedBlueprint, Box<SpawnIssue>> {
    let at = SpecPath::root();
    let Some(dir) = dirs
        .iter()
        .map(|d| d.join(reference.name.as_str()))
        .find(|d| d.join(FILE_NAME).is_file())
    else {
        return Err(Box::new(
            SpawnIssue::new(
                at,
                IssueCode::Unresolvable,
                format!("no blueprint named \"{}\" is installed", reference.name),
            )
            .known(installed(dirs)),
        ));
    };
    let loaded = load(&dir).map_err(|e| {
        Box::new(
            SpawnIssue::new(at.clone(), IssueCode::Invalid, e.to_string())
                .hint("run `lev validate` on it to see every problem"),
        )
    })?;
    if loaded.reference.name != reference.name {
        return Err(Box::new(
            SpawnIssue::new(
                at,
                IssueCode::Invalid,
                format!(
                    "the blueprint in {} calls itself \"{}\"",
                    dir.display(),
                    loaded.reference.name
                ),
            )
            .expected(reference.name.to_string())
            .got(loaded.reference.name.to_string()),
        ));
    }
    match &reference.digest {
        Some(pin) if Some(pin) != loaded.reference.digest.as_ref() => Err(Box::new(
            SpawnIssue::new(
                at.field("digest"),
                IssueCode::Changed,
                format!(
                    "the installed \"{}\" is a different revision",
                    reference.name
                ),
            )
            .expected(pin.to_string())
            .got(loaded.reference.to_string()),
        )),
        _ => Ok(loaded),
    }
}

/// The names of the blueprints installed in `dirs`, sorted, for an issue's
/// list of what could have been meant.
pub fn installed(dirs: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .filter(|e| e.path().join(FILE_NAME).is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn file_path(path: &Path) -> PathBuf {
    match path.is_dir() {
        true => path.join(FILE_NAME),
        false => path.to_path_buf(),
    }
}
