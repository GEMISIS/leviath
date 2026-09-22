//! How a `{name}` off a route and a path a manifest declares become the same
//! three answers: what to call the file, where it sits, and what to write into
//! the row or the manifest that names it.
//!
//! One module because the two callers have to agree. A listing reports a script
//! by the spelling a manifest would use, and a write reports that same spelling
//! for the file it just wrote. Working it out in two places is how they end up
//! saying different things about one file, so it is worked out here.

use std::path::{Path, PathBuf};

use super::scripts::ScriptKind;

/// A `.rhai` file addressed relative to some directory.
///
/// One shape for the two places a relative script path is read: a `{name}` off
/// the URL, and a path a manifest declares. Both have to end up as the same
/// three answers - what to call it, where it sits, and what to write into a
/// manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Addressed {
    /// The `{name}` the routes address it by: `/`-separated, `.rhai` stripped.
    pub(super) name: String,
    /// The same file relative to the base directory, extension included, always
    /// with `/` separators because that is what goes into a manifest.
    pub(super) relative: String,
    /// The directory components between the base and the file, in order.
    pub(super) dirs: Vec<String>,
    /// The file name, `.rhai` included.
    pub(super) file: String,
}

impl Addressed {
    /// Where the file sits under `base`.
    pub(super) fn dir_in(&self, base: &Path) -> PathBuf {
        self.dirs
            .iter()
            .fold(base.to_path_buf(), |acc, d| acc.join(d))
    }

    /// The file itself under `base`.
    pub(super) fn path_in(&self, base: &Path) -> PathBuf {
        self.dir_in(base).join(&self.file)
    }
}

/// Read a `{name}` as a path relative to a script directory, or `None` when it
/// is not one these routes can address.
///
/// A name may be a single component (`check`) or a `/`-separated relative path
/// (`validators/a2ui`), because that is the shape a manifest declares a hook or
/// a validator in and the shape the listing has to report back. Every component
/// goes through [`leviath_core::is_safe_path_component`], which is what keeps
/// `..`, an absolute path, a Windows `\` separator and an empty segment out:
/// `Path::join` normalizes none of them. Containment is still checked
/// afterwards by the script routes' own guard, because a component that is safe
/// to spell can still be a symlink pointing elsewhere.
///
/// The `.rhai` extension is fixed here rather than taken from the caller, so no
/// request can ask for a `.toml`, a manifest, or anything else in the agent's
/// directory. A name that already carries the extension means the same file,
/// since that is how a manifest and this listing both spell one.
pub(super) fn addressed_path(name: &str) -> Option<Addressed> {
    let stem = name.strip_suffix(".rhai").unwrap_or(name);
    let mut dirs: Vec<String> = Vec::new();
    let mut file = String::new();
    for part in stem.split('/') {
        if !leviath_core::is_safe_path_component(part) {
            return None;
        }
        // Which component is the file is only known once the walk ends, so
        // whatever was holding that place becomes a directory as soon as
        // another component arrives. An empty `file` is the first turn.
        if !file.is_empty() {
            dirs.push(std::mem::take(&mut file));
        }
        file = part.to_string();
    }
    Some(Addressed {
        name: stem.to_string(),
        relative: format!("{stem}.rhai"),
        file: format!("{file}.rhai"),
        dirs,
    })
}

/// How a manifest-declared path is addressed, or `None` when the manifest
/// declared something these routes cannot address.
///
/// A manifest may name any path inside the agent's directory, a subdirectory
/// included, and [`addressed_path`] handles those. What it does not handle is a
/// declaration that is not a `.rhai` file at all: `addressed_path` appends the
/// extension, so a declared `notes.txt` would be reported as `notes.txt.rhai`,
/// a different file. Requiring the suffix here keeps the listing off it.
pub(super) fn declared_address(declared: &str) -> Option<Addressed> {
    let stem = declared.strip_suffix(".rhai")?;
    addressed_path(stem)
}

/// The spelling of a file that goes in the row or the manifest naming it.
///
/// The one place a write works this out, and it gives the answer the listing
/// gives: nothing names a global tool or a provider by path, so those have
/// none, and a tool's own directory is part of the spelling because an agent's
/// manifest reads it from beside the manifest rather than from inside `tools/`.
pub(super) fn relative_of(
    kind: ScriptKind,
    agent: Option<&str>,
    addressed: &Addressed,
) -> Option<String> {
    match (agent, kind) {
        (None, ScriptKind::Tool | ScriptKind::Provider) => None,
        (Some(_), ScriptKind::Tool) => Some(format!("tools/{}", addressed.relative)),
        _ => Some(addressed.relative.clone()),
    }
}
