//! `[media]` and `[media_types]` in `~/.leviath/config.toml`: the limits on
//! stored media parts, and the operator's additions to the media registry.

use leviath_core::media::MediaRegistry;
use leviath_core::media::registry::RegistryError;
use serde::{Deserialize, Serialize};

/// Bytes one part may be before every ingress refuses it.
pub(crate) const DEFAULT_MAX_PART_BYTES: u64 = 32 * 1024 * 1024;

/// Bytes of text a part may carry inline before it is stored like a blob.
pub(crate) const DEFAULT_INLINE_TEXT_BYTES: u64 = 1024 * 1024;

/// Stored parts one model request may carry before the oldest are dropped.
pub(crate) const DEFAULT_MAX_STORED_PER_REQUEST: usize = 100;

/// `[media]` in `~/.leviath/config.toml`.
///
/// Three ceilings on typed content. `max_part_bytes` is applied wherever a
/// part arrives: an upload, a tool result, a `read_file`, a model reply.
/// `inline_text_bytes` is where a text part stops travelling inside the entry
/// and is stored by hash like any other. `max_stored_per_request` bounds how
/// many stored parts one request carries, because every vendor has a cap of
/// its own and the oldest are the ones to drop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaConfig {
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

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            max_part_bytes: DEFAULT_MAX_PART_BYTES,
            inline_text_bytes: DEFAULT_INLINE_TEXT_BYTES,
            max_stored_per_request: DEFAULT_MAX_STORED_PER_REQUEST,
        }
    }
}

impl super::Config {
    /// The media registry this config describes: the compiled defaults with
    /// `[media_types]` layered on. A malformed row is the error, named by key,
    /// so `lev doctor` and the daemon say the same thing about it.
    pub fn media_registry(&self) -> Result<MediaRegistry, RegistryError> {
        let mut reg = MediaRegistry::builtin();
        reg.layer(&self.media_types, "config")?;
        Ok(reg)
    }

    /// [`Self::media_registry`] for a daemon that must keep running: a
    /// malformed table is logged and the defaults are used.
    pub fn media_registry_or_defaults(&self) -> MediaRegistry {
        self.media_registry().unwrap_or_else(|e| {
            tracing::warn!("[media_types] ignored: {e}");
            MediaRegistry::builtin()
        })
    }
}

#[cfg(test)]
mod registry_tests {
    use super::super::Config;

    #[test]
    fn config_rows_layer_over_the_defaults() {
        let config: Config = toml::from_str(
            "[media_types.\"model/obj\"]\ntext = false\n[media_types.\"x/y\"]\nfamily = \"custom\"\n",
        )
        .unwrap();
        let reg = config.media_registry().unwrap();
        let obj = reg.info(&"model/obj".parse().unwrap());
        assert!(!obj.text);
        assert_eq!(obj.source, "config");
        assert_eq!(reg.info(&"x/y".parse().unwrap()).family, "custom");
        assert_eq!(
            config.media_registry_or_defaults().keys().len(),
            reg.keys().len()
        );
    }

    #[test]
    fn a_malformed_row_is_named_and_the_daemon_keeps_the_defaults() {
        let config: Config =
            toml::from_str("[media_types.\"model/obj\"]\nfamilies = \"x\"\n").unwrap();
        let err = config.media_registry().unwrap_err();
        assert!(err.to_string().contains("model/obj"), "{err}");
        let reg = config.media_registry_or_defaults();
        assert_eq!(reg.info(&"model/obj".parse().unwrap()).source, "builtin");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_an_empty_table() {
        let parsed: MediaConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, MediaConfig::default());
        assert_eq!(parsed.max_part_bytes, 32 * 1024 * 1024);
        assert_eq!(parsed.inline_text_bytes, 1024 * 1024);
        assert_eq!(parsed.max_stored_per_request, 100);
    }

    #[test]
    fn each_key_is_read_on_its_own() {
        let parsed: MediaConfig = toml::from_str("max_part_bytes = 5\n").unwrap();
        assert_eq!(parsed.max_part_bytes, 5);
        assert_eq!(parsed.inline_text_bytes, DEFAULT_INLINE_TEXT_BYTES);
        let parsed: MediaConfig =
            toml::from_str("inline_text_bytes = 7\nmax_stored_per_request = 2\n").unwrap();
        assert_eq!(parsed.inline_text_bytes, 7);
        assert_eq!(parsed.max_stored_per_request, 2);
        let back = toml::to_string(&parsed).unwrap();
        assert!(back.contains("max_stored_per_request = 2"));
    }
}
