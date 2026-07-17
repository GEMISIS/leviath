//! Plain configuration value types shared across crates.
//!
//! The full CLI [`Config`](../../leviath_cli/config/struct.Config.html) lives in
//! `leviath-cli`, but a few plain sub-configs are also needed by the engine in
//! `leviath-runtime` (e.g. title generation). Those live here so the runtime can
//! reference them without a CLI dependency; the CLI re-exports them for compat.

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// Configuration for auto-generating a short human-readable run title.
///
/// Example config:
/// ```toml
/// [title]
/// enabled = true
/// provider = "anthropic"
/// model = "claude-haiku-4-5-20251001"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleConfig {
    /// Whether to generate titles at all (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Provider to use for title generation.
    /// Defaults to the global `default_provider` when absent.
    pub provider: Option<String>,

    /// Model to use for title generation.
    /// Defaults to a cheap fast model for the resolved provider when absent.
    pub model: Option<String>,
}

impl Default for TitleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            model: None,
        }
    }
}
