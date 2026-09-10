//! The mime checks the scripts routes list: where the operator's rows put
//! theirs, and the rows a table names.
//!
//! Split from `scripts.rs` by concern: a mime check is the one script kind
//! named by a registry row rather than by a manifest or a directory, so the
//! code that reads rows lives here and the routes stay about files.

use std::path::{Path, PathBuf};

use super::scripts::{ScriptItem, ScriptKind, compile_status, declared_address, status_pair};

/// The directory the operator's mime checks resolve against: the one
/// `config.toml` and `mime_types.toml` sit in, since a row's `check` is a
/// path relative to the file that names it.
pub(super) fn config_dir() -> PathBuf {
    crate::config::mime_types_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// The checks a `[mime_types]` table names, as `(row key, script)`.
///
/// Layered onto an empty registry so the keys come back normalised; a table
/// the registry refuses names nothing, which is what the manifest and config
/// routes already report about it.
pub(super) fn row_checks(rows: &toml::Table) -> Vec<(String, String)> {
    leviath_core::mime::MimeRegistry::empty()
        .layered(rows, "rows")
        .map(|reg| {
            reg.declared_checks()
                .into_iter()
                .map(|(key, script, _)| (key, script))
                .collect()
        })
        .unwrap_or_default()
}

/// List the mime checks the operator's rows name: `[mime_types]` in the
/// config and `mime_types.toml` beside it, resolved against the config's
/// directory.
///
/// Derived from the rows the way the agent's hooks are derived from its
/// manifest, because there is no directory of checks to list: a row names
/// one, and that is what makes it a check rather than a file. A row the
/// registry refuses is reported by `lev doctor`, not here.
pub(super) fn collect_mime_checks(config: &crate::config::Config, out: &mut Vec<ScriptItem>) {
    let mut rows = config.mime_types.clone();
    if let Ok(Some(file)) = crate::config::rows_in_file(&crate::config::mime_types_path()) {
        rows.extend(file);
    }
    let dir = config_dir();
    for (_, script) in row_checks(&rows) {
        let Some(addressed) = declared_address(&script) else {
            continue;
        };
        let path = addressed.path_in(&dir);
        let (compiles, error) = match std::fs::read_to_string(&path) {
            Ok(content) => status_pair(compile_status(
                ScriptKind::MimeCheck,
                &script,
                &content,
                &[],
            )),
            Err(e) => (
                false,
                Some(format!("cannot read '{}': {e}", path.display())),
            ),
        };
        out.push(ScriptItem {
            kind: ScriptKind::MimeCheck.as_str().to_string(),
            name: addressed.name,
            source: "global".to_string(),
            agent: None,
            path: path.display().to_string(),
            relative_path: Some(addressed.relative),
            declared: true,
            compiles: Some(compiles),
            error,
            provider: None,
        });
    }
}
