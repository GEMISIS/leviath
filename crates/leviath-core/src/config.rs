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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_title_config_default() {
        let cfg = TitleConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.provider.is_none());
        assert!(cfg.model.is_none());
    }

    #[test]
    fn test_title_config_fields() {
        let cfg = TitleConfig {
            enabled: false,
            provider: Some("anthropic".to_string()),
            model: Some("claude-haiku-4-5-20251001".to_string()),
        };
        assert!(!cfg.enabled);
        assert_eq!(cfg.provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.model.as_deref(), Some("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn test_title_config_clone_and_debug() {
        let cfg = TitleConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cloned.enabled, cfg.enabled);
        // Debug impl is derived; exercise it so the derive is covered.
        assert!(format!("{:?}", cfg).contains("TitleConfig"));
    }

    #[test]
    fn test_title_config_serde_roundtrip() {
        let cfg = TitleConfig {
            enabled: true,
            provider: Some("openai".to_string()),
            model: Some("gpt-4o-mini".to_string()),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TitleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.enabled, cfg.enabled);
        assert_eq!(back.provider, cfg.provider);
        assert_eq!(back.model, cfg.model);
    }

    #[test]
    fn test_title_config_deserialize_defaults_enabled_true() {
        // `enabled` omitted → default_true() supplies `true`.
        let toml_str = r#"
provider = "anthropic"
model = "claude-haiku-4-5-20251001"
"#;
        let cfg: TitleConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn test_title_config_deserialize_explicit_disabled() {
        let toml_str = r#"
enabled = false
"#;
        let cfg: TitleConfig = toml::from_str(toml_str).unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.provider.is_none());
        assert!(cfg.model.is_none());
    }
}
