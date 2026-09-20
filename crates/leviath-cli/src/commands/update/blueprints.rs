//! Step three of `lev update`: respell renamed keys in the blueprints the user
//! has installed.
//!
//! The bundled blueprints are replaced wholesale by the step before this one,
//! so they arrive spelled the way the binary that shipped them spells things.
//! A blueprint the user wrote, or installed from somewhere else, is nobody's to
//! replace - and it keeps working either way, because the parser still reads
//! both names. This is the step that offers to make the file say what the docs
//! say, with the user watching.
//!
//! The rewrite is a splice of the exact bytes each key occupies, found from the
//! parsed document's spans. Everything else about the file - comment, spacing,
//! key order, the quotes around a value, whether the table is inline - is
//! untouched, because nothing else is rewritten.

use std::path::{Path, PathBuf};

use leviath_core::manifest::renamed::{RENAMED_KEYS, RenamedKey};
use toml_edit::{Document, TableLike};

/// One installed blueprint that still spells a key the old way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlueprintRewrite {
    /// The manifest file.
    pub path: PathBuf,
    /// The blueprint's directory name, which is what the user calls it.
    pub name: String,
    /// The file with every old key respelled.
    pub rewritten: String,
    /// One line per key, for the report.
    pub changes: Vec<String>,
}

/// The manifest inside an installed blueprint's directory.
///
/// One name, because `lev add` writes exactly this and nothing reads any
/// other: a directory without it is not a blueprint, which is the right
/// answer for the several things that also live in an agents directory.
const MANIFEST: &str = "agent.leviath";

/// Every blueprint under `agents_dir` that would be rewritten, in name order.
///
/// A directory that cannot be read, a manifest that will not parse and a
/// manifest with nothing to change are all "no rewrite here". None of them is
/// this step's business to report: `lev validate` is where a blueprint is
/// judged, and an update that started listing parse errors in files it was not
/// asked about would bury the three lines it does have to say.
pub(crate) fn plan_blueprints(agents_dir: &Path) -> Vec<BlueprintRewrite> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    let mut found: Vec<BlueprintRewrite> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path().join(MANIFEST);
            let text = std::fs::read_to_string(&path).ok()?;
            let (rewritten, changes) = rewrite(&text)?;
            Some(BlueprintRewrite {
                name: entry.file_name().to_string_lossy().into_owned(),
                path,
                rewritten,
                changes,
            })
        })
        .collect();
    // Sorted because a directory listing is in whatever order the filesystem
    // hands back, and a plan printed twice should read the same twice.
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// The manifest text with every old key respelled, and a line describing each,
/// or `None` when there is nothing to change.
///
/// A key written under both names is left alone: the new one is what the
/// parser reads, so respelling the old one would put two of the same key in
/// one table and turn a file that loads into one that does not. `lev validate`
/// reports that case, where the fix is to delete a line rather than move it.
pub(crate) fn rewrite(text: &str) -> Option<(String, Vec<String>)> {
    let document: Document<&str> = Document::parse(text).ok()?;
    let mut edits = Vec::new();
    visit(document.as_table(), &mut Vec::new(), &mut edits);
    if edits.is_empty() {
        return None;
    }

    // Applied back to front so every span still indexes the text it was found
    // in: a replacement ahead of one not yet applied would shift it.
    edits.sort_by_key(|edit| edit.at.start);
    let changes = edits
        .iter()
        .map(|edit| {
            format!(
                "`[{path}] {old}` becomes `{new}`. {note}",
                path = edit.path,
                old = edit.key.old,
                new = edit.key.new,
                note = edit.key.note,
            )
        })
        .collect();
    let mut out = text.to_string();
    for edit in edits.iter().rev() {
        out.replace_range(edit.at.clone(), edit.key.new);
    }
    Some((out, changes))
}

/// One key to respell: the bytes it occupies, and which rename it is.
struct Edit {
    /// The key's own span in the original text, name only.
    at: std::ops::Range<usize>,
    /// The table it sits in, dotted, for the report.
    path: String,
    /// Which rename.
    key: &'static RenamedKey,
}

/// Collect the edits in `table` and every table under it.
fn visit<'a>(table: &'a dyn TableLike, path: &mut Vec<&'a str>, edits: &mut Vec<Edit>) {
    for key in RENAMED_KEYS {
        if !key.site.holds(path) || table.contains_key(key.new) {
            continue;
        }
        let Some((found, _)) = table.get_key_value(key.old) else {
            continue;
        };
        // Infallible, and said with `expect` rather than a branch nothing can
        // reach: a span is `None` only for a key built in memory or one whose
        // document has been mutated, and this document was parsed from the
        // text above and is never touched again.
        let at = found.span().expect("a parsed key carries its span");
        edits.push(Edit {
            at,
            path: path.join("."),
            key,
        });
    }
    for (name, item) in table.iter() {
        let Some(child) = item.as_table_like() else {
            continue;
        };
        path.push(name);
        visit(child, path, edits);
        path.pop();
    }
}

#[cfg(test)]
mod tests;
