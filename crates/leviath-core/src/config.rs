//! Plain configuration value types shared across crates.
//!
//! The full CLI [`Config`](../../leviath_cli/config/struct.Config.html) lives in
//! `leviath-cli`, but a few plain sub-configs are also needed by the engine in
//! `leviath-runtime` (e.g. title generation). Those live here so the runtime can
//! reference them without a CLI dependency; the CLI re-exports them for compat.

use serde::{Deserialize, Serialize};

/// Configuration for auto-generating a short human-readable run title.
///
/// Example config:
/// ```toml
/// [title]
/// enabled = true
/// provider = "anthropic"
/// model = "claude-haiku-4-5-20251001"
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleConfig {
    /// Whether to generate titles at all (default: true).
    #[serde(default = "crate::default_true")]
    pub enabled: bool,

    /// Provider to use for title generation.
    /// Falls back to the run's own first-stage provider when absent.
    pub provider: Option<String>,

    /// Model to use for title generation.
    /// Falls back to the run's own first-stage model when absent - but only if
    /// `provider` is also absent or matches the run's provider, since one
    /// provider's model name means nothing to another.
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

/// Configuration for structured observability export.
///
/// Off by default: telemetry costs a background export pipeline and most
/// interactive users have the dashboard instead. When enabled, spans, metrics
/// and log records for every run flow to the configured exporter.
///
/// Example config:
/// ```toml
/// [observability]
/// enabled = true
/// exporter = "otlp"
/// endpoint = "http://localhost:4318"
/// service_name = "leviath"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    /// Whether to export telemetry at all (default: false).
    #[serde(default)]
    pub enabled: bool,

    /// Which exporter to use (default: `otlp`).
    #[serde(default)]
    pub exporter: TelemetryExporterKind,

    /// OTLP endpoint. Falls back to `OTEL_EXPORTER_OTLP_ENDPOINT`, then to
    /// `http://localhost:4318` - the OTLP **HTTP** port. Leviath exports OTLP
    /// over HTTP/protobuf, not gRPC, so a collector's 4317 gRPC endpoint will
    /// not work here.
    pub endpoint: Option<String>,

    /// The `service.name` resource attribute. Falls back to
    /// `OTEL_SERVICE_NAME`, then to `"leviath"`.
    pub service_name: Option<String>,
}

/// Which telemetry exporter to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryExporterKind {
    /// OTLP over HTTP/protobuf to `endpoint`.
    #[default]
    Otlp,
    /// Human-readable lines through the process logger (stderr).
    Stdout,
    /// Build no exporter even when `enabled` is set.
    None,
}

impl TelemetryExporterKind {
    /// True when this kind exports nothing.
    pub fn is_none(self) -> bool {
        matches!(self, Self::None)
    }
}

/// The machine-wide toggles for the framework-authored system-prompt hints,
/// carried from the CLI's config into a spawn.
///
/// Each field is the *global* end of a stage → agent → global cascade
/// ([`crate::taint::resolve_batch_tool_hint`],
/// [`crate::taint::resolve_shell_hint`]): a blueprint's `[agent]` or
/// `[stages.<name>]` block overrides it per agent or per stage. They travel
/// together as one value so adding a hint doesn't grow the arity of every spawn
/// entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptHints {
    /// Global default for the batch-tool-calls hint.
    pub batch_tool: bool,
    /// Global default for the platform shell hint.
    pub shell: bool,
}

impl Default for PromptHints {
    /// Both hints on, matching the CLI config's `default_true` for each.
    fn default() -> Self {
        Self {
            batch_tool: true,
            shell: true,
        }
    }
}

/// One narrower scope's overrides of [`PromptHints`], as a blueprint's `[agent]`
/// or `[stages.<name>]` block writes them. `None` inherits the broader scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptHintOverrides {
    /// Override for the batch-tool-calls hint.
    pub batch_tool: Option<bool>,
    /// Override for the platform shell hint.
    pub shell: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_hints_default_on_and_overrides_default_inherit() {
        // The default has to match the CLI config's `default_true` for each
        // field, or an embedder and the daemon would disagree about the same
        // unset config.
        let hints = PromptHints::default();
        assert!(hints.batch_tool);
        assert!(hints.shell);
        assert_eq!(
            hints,
            PromptHints {
                batch_tool: true,
                shell: true
            }
        );
        assert_ne!(
            hints,
            PromptHints {
                batch_tool: true,
                shell: false
            }
        );

        // An override that names nothing inherits everything.
        let overrides = PromptHintOverrides::default();
        assert_eq!(overrides.batch_tool, None);
        assert_eq!(overrides.shell, None);
        assert_eq!(
            overrides,
            PromptHintOverrides {
                batch_tool: None,
                shell: None
            }
        );
        assert!(format!("{hints:?} {overrides:?}").contains("batch_tool"));
    }

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

    #[test]
    fn observability_config_defaults_to_disabled_otlp() {
        let cfg = ObservabilityConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.exporter, TelemetryExporterKind::Otlp);
        assert!(cfg.endpoint.is_none());
        assert!(cfg.service_name.is_none());
        // An empty TOML table and the hand-written Default must agree.
        let parsed: ObservabilityConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn observability_config_full_roundtrip() {
        let toml_str = r#"
enabled = true
exporter = "stdout"
endpoint = "http://collector:4318"
service_name = "leviath-prod"
"#;
        let cfg: ObservabilityConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.exporter, TelemetryExporterKind::Stdout);
        assert_eq!(cfg.endpoint.as_deref(), Some("http://collector:4318"));
        assert_eq!(cfg.service_name.as_deref(), Some("leviath-prod"));
        let serialized = toml::to_string(&cfg).unwrap();
        let back: ObservabilityConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(back, cfg);
        assert!(format!("{cfg:?}").contains("ObservabilityConfig"));
    }

    #[test]
    fn telemetry_exporter_kind_round_trips_through_config_syntax() {
        for (kind, text) in [
            (TelemetryExporterKind::Otlp, "otlp"),
            (TelemetryExporterKind::Stdout, "stdout"),
            (TelemetryExporterKind::None, "none"),
        ] {
            let toml_str = format!("exporter = \"{text}\"");
            let cfg: ObservabilityConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(cfg.exporter, kind);
            let serialized = toml::to_string(&cfg).unwrap();
            assert!(serialized.contains(text), "{serialized} missing {text}");
        }
    }

    #[test]
    fn telemetry_exporter_kind_is_none_helper() {
        assert!(TelemetryExporterKind::None.is_none());
        assert!(!TelemetryExporterKind::Otlp.is_none());
        assert!(!TelemetryExporterKind::Stdout.is_none());
    }

    #[test]
    fn telemetry_exporter_kind_rejects_unknown_value() {
        let err = toml::from_str::<ObservabilityConfig>("exporter = \"grpc\"");
        assert!(err.is_err());
    }
}
