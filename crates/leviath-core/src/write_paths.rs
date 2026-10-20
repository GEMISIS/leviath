//! Write paths: where inside the workdir a stage's write tools may write.
//!
//! A graph, a stage, a launch request and every run above a child or a
//! fan-out worker can each name a list of globs. Each list is a *layer*, and
//! a write must match at least one glob of every layer. A layer can only take
//! paths away, so a stage narrows its graph, and a worker can never write
//! where the run that started it could not.
//!
//! Globs are relative to the workdir and use `/` between components. `*`
//! stays inside one component and `**` crosses any number of them, so
//! `docs/**` is everything under `docs`. Matching is case-sensitive on every
//! platform: a path the globs do not spell is refused, never guessed at.
//!
//! A path is judged twice, and must pass both times:
//!
//! 1. **As written**, with `.` and `..` folded out lexically.
//! 2. **As the filesystem resolves it**: the deepest existing ancestor is
//!    canonicalized, so a symlink inside an allowed directory that points
//!    somewhere else is judged by where it lands.
//!
//! A path that leaves the workdir either way, or that cannot be resolved at
//! all, is refused.

use std::path::{Path, PathBuf};

/// Why `text` cannot be a write-path glob, or `Ok` when it can.
///
/// A glob must stay inside the workdir: no absolute path, no drive or UNC
/// prefix, no `~`, and no `.` or `..` component (a leading `./` is allowed and
/// dropped). It must also compile. A backslash is read as a separator, as in
/// `[read_paths]`, so a pattern written on Windows means the same thing
/// everywhere.
pub fn check_glob(text: &str) -> Result<(), String> {
    let normalized = normalize(text);
    let why = if normalized.is_empty() {
        Some("it is empty")
    } else if normalized.starts_with('/') || normalized.contains(':') {
        Some("it is an absolute path or names a drive; write paths are relative to the workdir")
    } else if normalized.starts_with('~') {
        Some("it names the home directory; write paths are relative to the workdir")
    } else if normalized
        .split('/')
        .any(|c| c == ".." || c == "." || c.is_empty())
    {
        Some("it has a `..`, `.` or empty component; write paths must stay inside the workdir")
    } else {
        None
    };
    if let Some(why) = why {
        return Err(format!("write path '{text}' is refused: {why}"));
    }
    glob::Pattern::new(&normalized)
        .map(drop)
        .map_err(|e| format!("write path '{text}' is not a valid glob: {e}"))
}

/// The glob as it is matched: backslashes as `/`, any leading `./` dropped.
fn normalize(text: &str) -> String {
    let mut text = text.trim().replace('\\', "/");
    while let Some(rest) = text.strip_prefix("./") {
        text = rest.to_string();
    }
    text
}

/// One layer: the globs as written, and the ones that compiled.
#[derive(Debug, Clone)]
struct Layer {
    written: Vec<String>,
    patterns: Vec<glob::Pattern>,
}

/// A compiled set of write-path layers. With no layers, every path inside the
/// workdir may be written.
#[derive(Debug, Clone, Default)]
pub struct WriteFence {
    layers: Vec<Layer>,
}

impl WriteFence {
    /// A fence over `layers`, outermost first. An empty layer is no limit and
    /// is skipped. A glob that does not pass [`check_glob`] matches nothing,
    /// so a layer made only of such globs refuses every write: the spec is
    /// validated before a run starts, and anything that got past that fails
    /// closed rather than open.
    pub fn new<L: AsRef<[String]>>(layers: &[L]) -> Self {
        let layers = layers
            .iter()
            .map(AsRef::as_ref)
            .filter(|globs| !globs.is_empty())
            .map(|globs| Layer {
                written: globs.to_vec(),
                patterns: globs
                    .iter()
                    .filter(|g| check_glob(g).is_ok())
                    .filter_map(|g| glob::Pattern::new(&normalize(g)).ok())
                    .collect(),
            })
            .collect();
        Self { layers }
    }

    /// Whether this fence limits nothing.
    pub fn is_open(&self) -> bool {
        self.layers.is_empty()
    }

    /// Whether `relative` (components joined by `/`) matches every layer.
    pub fn allows(&self, relative: &str) -> bool {
        let options = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        self.layers.iter().all(|layer| {
            layer
                .patterns
                .iter()
                .any(|p| p.matches_with(relative, options))
        })
    }

    /// The globs a write must match, as the refusal names them: one layer as
    /// `a, b`, several as `(a, b) and (c)`.
    pub fn describe(&self) -> String {
        let layers: Vec<String> = self.layers.iter().map(|l| l.written.join(", ")).collect();
        match layers.as_slice() {
            [one] => one.clone(),
            many => many
                .iter()
                .map(|l| format!("({l})"))
                .collect::<Vec<_>>()
                .join(" and "),
        }
    }

    /// What a call to `tool` that would write `shown` is answered with: the
    /// tool, the path, and the globs, so the model can correct course. Starts
    /// with `[blocked]`, so the run's events record the call as blocked.
    pub fn refusal(&self, tool: &str, shown: &str) -> String {
        format!(
            "[blocked] {tool} refused: {shown} is outside this stage's write paths ({}). Write \
             inside them instead.",
            self.describe()
        )
    }

    /// Judge a write to `requested` (relative to `workdir`, or absolute) and
    /// return `Err` with the path as the refusal should show it when the
    /// fence refuses it. An open fence allows everything; whether the path
    /// stays inside the workdir at all is the write tool's own check.
    pub fn check(&self, requested: &str, workdir: &Path) -> Result<(), String> {
        if self.is_open() {
            return Ok(());
        }
        let raw = match Path::new(requested).is_absolute() {
            true => PathBuf::from(requested),
            false => workdir.join(requested),
        };
        let lexical = crate::paths::fold_dot_dot(&raw);
        let root = std::fs::canonicalize(workdir).unwrap_or(workdir.to_path_buf());
        let written = lexical.as_deref().and_then(|p| under(p, workdir, &root));
        let shown = written.clone().unwrap_or(requested.to_string());
        let real = lexical
            .as_deref()
            .and_then(crate::paths::canonicalize_for_match)
            .and_then(|p| relative(&p, &root));
        match (written, real) {
            (Some(w), Some(r)) if self.allows(&w) && self.allows(&r) => Ok(()),
            _ => Err(shown),
        }
    }
}

/// `path` under the workdir as [`relative`] reads it, spelled either the way
/// `workdir` is or through any other name for it (`/tmp` for `/private/tmp`
/// on macOS): an ancestor of `path` that resolves to `root`, the workdir's
/// real path, is the workdir however it is spelled.
fn under(path: &Path, workdir: &Path, root: &Path) -> Option<String> {
    relative(path, workdir).or_else(|| {
        path.ancestors()
            .find(|a| std::fs::canonicalize(a).is_ok_and(|real| real == root))
            .and_then(|alias| relative(path, alias))
    })
}

/// `path` under `root`, its components joined by `/`. `None` when it is not
/// under `root`, or is `root` itself.
fn relative(path: &Path, root: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rest
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
#[path = "write_paths_tests.rs"]
mod tests;
