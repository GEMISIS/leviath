//! `[mime]` in `~/.leviath/config.toml`, the limits on stored mime parts,
//! and `mime_types.toml` beside it, the operator's additions to the mime
//! registry (a `[mime_types]` table in the config still loads, under the
//! file).
//!
//! A row in either place may name a `check`, a Rhai script relative to the
//! config's directory whose `check(bytes, mime_type)` refuses bytes that
//! are not what they claim. The scripts are compiled here, when the
//! registry is built, so a broken one is a load error the daemon, `lev
//! doctor` and `lev mime` all report the same way.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leviath_core::mime::MimeRegistry;
use leviath_core::mime::registry::RegistryError;
use serde::{Deserialize, Serialize};

/// The file beside `config.toml` that holds the operator's registry rows.
pub(crate) const MIME_TYPES_FILE: &str = "mime_types.toml";

/// The example `lev mime init` writes; the published copy the docs link is
/// held equal to it by a test.
pub(crate) const MIME_TYPES_EXAMPLE: &str = include_str!("mime_types.example.toml");

/// Where the rows live: beside the config, wherever that is.
pub(crate) fn mime_types_path() -> PathBuf {
    let mut path = super::Config::config_path();
    path.set_file_name(MIME_TYPES_FILE);
    path
}

/// Why the registry could not be built from the config and the file.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MimeTypesError {
    /// A row under `[mime_types]` in `config.toml`.
    #[error("[mime_types] in config.toml: {0}")]
    Config(RegistryError),
    /// `mime_types.toml` could not be read, is not TOML, or holds a row the
    /// registry refuses.
    #[error("{path}: {message}")]
    File {
        /// The file.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
    },
    /// A row's `check` script could not be loaded.
    #[error("mime check for {key} ({script}): {message}")]
    Check {
        /// The row's type or pattern.
        key: String,
        /// The script as the row wrote it.
        script: String,
        /// What was wrong with it.
        message: String,
    },
}

/// Compile every check the operator's rows name and attach it to its row.
///
/// A script path is relative to the config's directory and has to resolve
/// inside it, the fence a blueprint's scripts get against the blueprint's
/// directory: a row is configuration, and configuration that could point
/// the daemon at any file on the machine and run it is not.
pub(crate) fn attach_checks(
    reg: &mut MimeRegistry,
    config_dir: &Path,
) -> Result<(), MimeTypesError> {
    for (key, script, _) in reg.declared_checks() {
        let fail = |message: String| MimeTypesError::Check {
            key: key.clone(),
            script: script.clone(),
            message,
        };
        let path = config_dir.join(&script);
        if !leviath_core::resolves_within(&path, config_dir) {
            return Err(fail(format!(
                "resolves outside {}; a check lives beside the config that names it",
                config_dir.display()
            )));
        }
        let source = std::fs::read_to_string(&path)
            .map_err(|e| fail(format!("cannot read {}: {e}", path.display())))?;
        let compiled = leviath_scripting::mime_check::compile(&script, &source)
            .map_err(|e| fail(e.to_string()))?;
        // `declared_checks` hands back the registry's own normalised keys,
        // which are the one thing `attach_check` can refuse.
        reg.attach_check(&key, Arc::new(compiled))
            .expect("a key the registry itself listed");
    }
    Ok(())
}

/// The rows `path` holds: its top-level tables, plus any under a
/// `[mime_types]` wrapper, so a block cut out of `config.toml` loads as it
/// was. `None` when there is no file.
pub(crate) fn rows_in_file(path: &Path) -> Result<Option<toml::Table>, MimeTypesError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(MimeTypesError::File {
                path: path.to_path_buf(),
                message: e.to_string(),
            });
        }
    };
    let mut table: toml::Table = toml::from_str(&text).map_err(|e| MimeTypesError::File {
        path: path.to_path_buf(),
        message: e.message().to_string(),
    })?;
    if let Some(toml::Value::Table(wrapped)) = table.remove("mime_types") {
        table.extend(wrapped);
    }
    Ok(Some(table))
}

/// Bytes one part may be before every ingress refuses it.
pub(crate) const DEFAULT_MAX_PART_BYTES: u64 = 32 * 1024 * 1024;

/// Bytes of text a part may carry inline before it is stored like a blob.
pub(crate) const DEFAULT_INLINE_TEXT_BYTES: u64 = 1024 * 1024;

/// Stored parts one model request may carry before the oldest are dropped.
pub(crate) const DEFAULT_MAX_STORED_PER_REQUEST: usize = 100;

/// `[mime]` in `~/.leviath/config.toml`.
///
/// Three ceilings on typed content. `max_part_bytes` is applied wherever a
/// part arrives: an upload, a tool result, a `read_file`, a model reply.
/// `inline_text_bytes` is where a text part stops travelling inside the entry
/// and is stored by hash like any other. `max_stored_per_request` bounds how
/// many stored parts one request carries, because every vendor has a cap of
/// its own and the oldest are the ones to drop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MimeConfig {
    /// Bytes one part may be. Larger is refused where it arrives.
    #[serde(default = "default_max_part_bytes")]
    pub max_part_bytes: u64,

    /// Bytes of text kept inline in an entry before the part is stored.
    #[serde(default = "default_inline_text_bytes")]
    pub inline_text_bytes: u64,

    /// Stored parts one model request carries; the oldest beyond it are
    /// dropped with a warning.
    #[serde(default = "default_max_stored_per_request")]
    pub max_stored_per_request: usize,
}

fn default_max_part_bytes() -> u64 {
    DEFAULT_MAX_PART_BYTES
}

fn default_inline_text_bytes() -> u64 {
    DEFAULT_INLINE_TEXT_BYTES
}

fn default_max_stored_per_request() -> usize {
    DEFAULT_MAX_STORED_PER_REQUEST
}

impl Default for MimeConfig {
    fn default() -> Self {
        Self {
            max_part_bytes: DEFAULT_MAX_PART_BYTES,
            inline_text_bytes: DEFAULT_INLINE_TEXT_BYTES,
            max_stored_per_request: DEFAULT_MAX_STORED_PER_REQUEST,
        }
    }
}

impl super::Config {
    /// The mime registry this install describes: the compiled defaults, a
    /// `[mime_types]` table in the config, then `mime_types.toml` beside
    /// it, later rows winning, with every check the rows name compiled and
    /// attached. A malformed row or a check that will not load is the
    /// error, named by key and by where it lives, so `lev doctor`, `lev
    /// mime` and the daemon say the same thing about it.
    pub fn mime_registry(&self) -> Result<MimeRegistry, MimeTypesError> {
        let mut reg = MimeRegistry::builtin();
        reg.layer(&self.mime_types, "config")
            .map_err(MimeTypesError::Config)?;
        let path = mime_types_path();
        if let Some(rows) = rows_in_file(&path)? {
            reg.layer(&rows, MIME_TYPES_FILE)
                .map_err(|e| MimeTypesError::File {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
        }
        let config_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        attach_checks(&mut reg, &config_dir)?;
        Ok(reg)
    }

    /// [`Self::mime_registry`] for a daemon that must keep running: a
    /// malformed row is logged and the defaults are used.
    pub fn mime_registry_or_defaults(&self) -> MimeRegistry {
        self.mime_registry().unwrap_or_else(|e| {
            tracing::warn!("mime types ignored: {e}");
            MimeRegistry::builtin()
        })
    }
}

#[cfg(test)]
mod registry_tests {
    use super::super::Config;

    #[test]
    // Isolated: the registry reads `mime_types.toml` beside whatever
    // `config.toml` the environment names, and another test's fake config
    // directory must not become this one's.
    fn config_rows_layer_over_the_defaults() {
        crate::config::with_isolated_config_path("mime-config-rows", |_| {
            let config: Config = toml::from_str(
                "[mime_types.\"model/obj\"]\ntext = false\n[mime_types.\"x/y\"]\nfamily = \"custom\"\n",
            )
            .unwrap();
            let reg = config.mime_registry().unwrap();
            let obj = reg.info(&"model/obj".parse().unwrap());
            assert!(!obj.text);
            assert_eq!(obj.source, "config");
            assert_eq!(reg.info(&"x/y".parse().unwrap()).family, "custom");
            assert_eq!(
                config.mime_registry_or_defaults().keys().len(),
                reg.keys().len()
            );
        });
    }

    #[test]
    fn a_malformed_row_is_named_and_the_daemon_keeps_the_defaults() {
        crate::config::with_isolated_config_path("mime-config-bad-row", |_| {
            let config: Config =
                toml::from_str("[mime_types.\"model/obj\"]\nfamilies = \"x\"\n").unwrap();
            let err = config.mime_registry().unwrap_err();
            assert!(
                err.to_string().starts_with("[mime_types] in config.toml:"),
                "{err}"
            );
            assert!(err.to_string().contains("model/obj"), "{err}");
            let reg = config.mime_registry_or_defaults();
            assert_eq!(reg.info(&"model/obj".parse().unwrap()).source, "builtin");
        });
    }

    /// `mime_types.toml` beside the config layers over both the defaults
    /// and the config's own table, in either of its two shapes, and every
    /// way it can be wrong is named with its path.
    #[test]
    fn the_file_beside_the_config_layers_last_and_is_named_when_wrong() {
        crate::config::with_isolated_config_path("mime-types-file", |dir| {
            let path = dir.join(super::MIME_TYPES_FILE);
            assert_eq!(super::mime_types_path(), path);
            // A run that failed part-way leaves its file or directory behind.
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_dir_all(&path);
            let config: Config =
                toml::from_str("[mime_types.\"model/obj\"]\ntext = false\n").unwrap();
            // No file: the config's row stands.
            assert!(
                !config
                    .mime_registry()
                    .unwrap()
                    .info(&"model/obj".parse().unwrap())
                    .text
            );
            // Bare rows, and a wrapped block moved out of the config.
            std::fs::write(
                &path,
                "[\"model/obj\"]\ntext = true\n[mime_types.\"x/y\"]\nfamily = \"custom\"\n",
            )
            .unwrap();
            let reg = config.mime_registry().unwrap();
            let obj = reg.info(&"model/obj".parse().unwrap());
            assert!(obj.text);
            assert_eq!(obj.source, "mime_types.toml");
            assert_eq!(reg.info(&"x/y".parse().unwrap()).family, "custom");
            // Not TOML.
            std::fs::write(&path, "= = =\n").unwrap();
            // The file variant names the path first.
            let err = config.mime_registry().unwrap_err().to_string();
            assert!(err.starts_with(&path.display().to_string()));
            // A row the registry refuses.
            std::fs::write(&path, "[\"model/obj\"]\nfamilies = \"x\"\n").unwrap();
            let err = config.mime_registry().unwrap_err().to_string();
            assert!(err.contains("model/obj"), "{err}");
            // The daemon keeps the compiled defaults, the config's rows
            // included: one bad row anywhere is one registry it cannot build.
            assert_eq!(
                config
                    .mime_registry_or_defaults()
                    .info(&"model/obj".parse().unwrap())
                    .source,
                "builtin"
            );
            // Unreadable: a directory where the file should be.
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            // The file variant names the path first.
            let unreadable = config.mime_registry().unwrap_err().to_string();
            assert!(unreadable.starts_with(&path.display().to_string()));
        });
    }

    /// A row's `check` is compiled from beside the config and refuses bytes
    /// through every store; a script that is missing, escapes the config's
    /// directory or does not compile is named with its row.
    #[test]
    fn a_rows_check_is_compiled_from_beside_the_config() {
        crate::config::with_isolated_config_path("mime-types-check", |dir| {
            use leviath_core::mime::{Blob, BlobStore, MemoryBlobStore};
            let path = dir.join(super::MIME_TYPES_FILE);
            let _ = std::fs::remove_file(&path);
            std::fs::create_dir_all(dir.join("checks")).unwrap();
            std::fs::write(
                dir.join("checks/scene.rhai"),
                "fn check(bytes, mime_type) { if bytes.len() < 4 { return \"too short\"; } () }",
            )
            .unwrap();
            std::fs::write(
                &path,
                "[\"application/x-acme-scene\"]\nfamily = \"model\"\ncheck = \"checks/scene.rhai\"\n",
            )
            .unwrap();
            let config = Config::default();
            let reg = config.mime_registry().unwrap();
            let scene: leviath_core::mime::MimeType = "application/x-acme-scene".parse().unwrap();
            assert_eq!(reg.info(&scene).check.as_deref(), Some("checks/scene.rhai"));
            let store = MemoryBlobStore::new();
            let err = store
                .put(
                    "r",
                    &Blob::new(scene.clone(), b"ab".to_vec()).named("a.scene"),
                    &reg,
                )
                .unwrap_err();
            assert!(err.to_string().contains("too short"), "{err}");
            assert!(
                store
                    .put("r", &Blob::new(scene, b"ACME1".to_vec()), &reg)
                    .is_ok()
            );

            let named = |rows: &str| {
                std::fs::write(&path, rows).unwrap();
                let err = config.mime_registry().unwrap_err();
                let text = err.to_string();
                assert!(text.starts_with("mime check for x/y ("), "{text}");
                text
            };
            let missing = named("[\"x/y\"]\ncheck = \"checks/gone.rhai\"\n");
            assert!(missing.contains("cannot read"), "{missing}");
            let escaping = named("[\"x/y\"]\ncheck = \"../outside.rhai\"\n");
            assert!(escaping.contains("resolves outside"), "{escaping}");
            std::fs::write(dir.join("checks/broken.rhai"), "fn check(a) { () }").unwrap();
            let broken = named("[\"x/y\"]\ncheck = \"checks/broken.rhai\"\n");
            assert!(broken.contains("exactly two parameters"), "{broken}");
            // The daemon keeps the defaults rather than a registry with a
            // check it could not compile.
            assert_eq!(
                config
                    .mime_registry_or_defaults()
                    .info(&"x/y".parse().unwrap())
                    .source,
                "builtin"
            );
        });
    }

    /// The published copy is the embedded one: the docs link the live file
    /// and `lev mime init` writes the embedded text.
    #[test]
    fn the_published_example_is_the_embedded_one_and_loads() {
        let published = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/schema/mime_types.example.toml"
        ))
        .expect("docs/schema/mime_types.example.toml");
        assert_eq!(published, super::MIME_TYPES_EXAMPLE);
        let rows: toml::Table = toml::from_str(super::MIME_TYPES_EXAMPLE).unwrap();
        let mut reg = leviath_core::mime::MimeRegistry::builtin();
        reg.layer(&rows, "example").unwrap();
        assert!(reg.info(&"model/obj".parse().unwrap()).text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_an_empty_table() {
        let parsed: MimeConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, MimeConfig::default());
        assert_eq!(parsed.max_part_bytes, 32 * 1024 * 1024);
        assert_eq!(parsed.inline_text_bytes, 1024 * 1024);
        assert_eq!(parsed.max_stored_per_request, 100);
    }

    #[test]
    fn each_key_is_read_on_its_own() {
        let parsed: MimeConfig = toml::from_str("max_part_bytes = 5\n").unwrap();
        assert_eq!(parsed.max_part_bytes, 5);
        assert_eq!(parsed.inline_text_bytes, DEFAULT_INLINE_TEXT_BYTES);
        let parsed: MimeConfig =
            toml::from_str("inline_text_bytes = 7\nmax_stored_per_request = 2\n").unwrap();
        assert_eq!(parsed.inline_text_bytes, 7);
        assert_eq!(parsed.max_stored_per_request, 2);
        let back = toml::to_string(&parsed).unwrap();
        assert!(back.contains("max_stored_per_request = 2"));
    }
}
