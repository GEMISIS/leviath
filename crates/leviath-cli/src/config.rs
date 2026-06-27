//! CLI configuration management.

use leviath_mcp::MCPServerConfig;
use leviath_providers::ModelCapabilities;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// CLI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default provider
    pub default_provider: String,

    /// Provider API keys
    pub providers: ProviderConfig,

    /// Agent project paths
    pub agent_paths: Vec<PathBuf>,

    /// Package registries
    pub registries: Vec<String>,

    /// OpenRouter API key
    pub openrouter_api_key: Option<String>,

    /// Ollama base URL (default http://localhost:11434)
    pub ollama_base_url: Option<String>,

    /// MCP server configurations
    #[serde(default)]
    pub mcp_servers: Vec<MCPServerConfig>,

    /// Default model override
    pub default_model: Option<String>,

    /// Per-model capability overrides. Key is model ID (e.g. "my-local-llama").
    /// Takes precedence over the provider's built-in capability table.
    #[serde(default)]
    pub model_capabilities: HashMap<String, ModelCapabilities>,
}

/// Provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Anthropic API key
    pub anthropic_api_key: Option<String>,

    /// OpenAI API key
    pub openai_api_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "anthropic".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
            },
            agent_paths: Vec::new(),
            registries: vec!["https://leviath.dev/registry".to_string()],
            openrouter_api_key: None,
            ollama_base_url: None,
            mcp_servers: Vec::new(),
            default_model: None,
            model_capabilities: HashMap::new(),
        }
    }
}

impl Config {
    /// Load configuration from the default location (~/.leviath/config.toml).
    ///
    /// After loading from file (or using defaults), environment variables are
    /// checked as fallbacks. Env vars override config file values if set.
    pub fn load() -> anyhow::Result<Self> {
        // Load .env file from current directory (silently ignored if missing)
        let _ = dotenvy::dotenv();

        let path = Self::config_path();

        let mut config = if !path.exists() {
            tracing::debug!("No config file found at {}, using defaults", path.display());
            Self::default()
        } else {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("Failed to read config from '{}': {}", path.display(), e))?;

            let c: Self = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

            tracing::debug!("Loaded config from {}", path.display());
            c
        };

        // Env var fallbacks (env vars override config file if set)
        if config.providers.anthropic_api_key.is_none() {
            config.providers.anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        }
        if config.providers.openai_api_key.is_none() {
            config.providers.openai_api_key = std::env::var("OPENAI_API_KEY").ok();
        }
        if config.openrouter_api_key.is_none() {
            config.openrouter_api_key = std::env::var("OPENROUTER_API_KEY").ok();
        }
        // OLLAMA_HOST is the standard env var for Ollama
        if config.ollama_base_url.is_none() {
            config.ollama_base_url = std::env::var("OLLAMA_HOST").ok();
        }

        // Check config file permissions on Unix
        check_permissions();

        Ok(config)
    }

    /// Save configuration to the default location.
    #[allow(dead_code)] // Public API for config editing (used by init, future commands)
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            create_config_dir(parent)?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;

        std::fs::write(&path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write config to '{}': {}", path.display(), e))?;

        // Set restrictive permissions on the config file
        set_file_permissions(&path);

        tracing::debug!("Saved config to {}", path.display());
        Ok(())
    }

    /// Get the path to the config file.
    pub fn config_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".leviath")
            .join("config.toml")
    }

    /// Validate API key formats and return warnings for suspicious keys.
    pub fn validate_keys(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(ref key) = self.providers.anthropic_api_key {
            if !key.starts_with("sk-ant-") {
                warnings.push(
                    "Anthropic API key doesn't start with 'sk-ant-' — verify it's correct"
                        .to_string(),
                );
            }
        }
        if let Some(ref key) = self.providers.openai_api_key {
            if !key.starts_with("sk-") {
                warnings.push(
                    "OpenAI API key doesn't start with 'sk-' — verify it's correct".to_string(),
                );
            }
        }
        warnings
    }
}

/// Redact an API key for safe display, showing only first 4 and last 4 characters.
#[allow(dead_code)] // Public API for use by future commands and display logic
pub fn redact_key(key: &str) -> String {
    if key.len() <= 8 {
        "***".to_string()
    } else {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    }
}

/// Create the config directory with restrictive permissions.
fn create_config_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("Failed to create config directory: {}", e))?;
    set_dir_permissions(dir);
    Ok(())
}

/// Check permissions on the config file and auto-fix if too permissive (Unix only).
#[cfg(unix)]
fn check_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let path = Config::config_path();
    if !path.exists() {
        return;
    }

    if let Ok(metadata) = std::fs::metadata(&path) {
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{:o}", mode),
                "Config file has overly permissive permissions, fixing to 600"
            );
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&path, perms) {
                tracing::warn!("Failed to fix config file permissions: {}", e);
            }
        }
    }
}

#[cfg(not(unix))]
fn check_permissions() {
    // No-op on non-Unix platforms
}

/// Set restrictive permissions on the config file (Unix only).
#[cfg(unix)]
fn set_file_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    if let Err(e) = std::fs::set_permissions(path, perms) {
        tracing::warn!("Failed to set config file permissions: {}", e);
    }
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &std::path::Path) {
    // No-op on non-Unix platforms
}

/// Set restrictive permissions on the config directory (Unix only).
#[cfg(unix)]
fn set_dir_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    if let Err(e) = std::fs::set_permissions(path, perms) {
        tracing::warn!("Failed to set config directory permissions: {}", e);
    }
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &std::path::Path) {
    // No-op on non-Unix platforms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_key_short() {
        assert_eq!(redact_key("abc"), "***");
        assert_eq!(redact_key("12345678"), "***");
    }

    #[test]
    fn test_redact_key_long() {
        assert_eq!(redact_key("sk-ant-abcdef1234"), "sk-a...1234");
        assert_eq!(redact_key("123456789"), "1234...6789");
    }

    #[test]
    fn test_validate_keys_good_anthropic() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-test123".to_string()),
                openai_api_key: None,
            },
            ..Config::default()
        };
        assert!(config.validate_keys().is_empty());
    }

    #[test]
    fn test_validate_keys_bad_anthropic() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("bad-key".to_string()),
                openai_api_key: None,
            },
            ..Config::default()
        };
        let warnings = config.validate_keys();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Anthropic"));
    }

    #[test]
    fn test_validate_keys_good_openai() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: Some("sk-test123".to_string()),
            },
            ..Config::default()
        };
        assert!(config.validate_keys().is_empty());
    }

    #[test]
    fn test_validate_keys_bad_openai() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: Some("bad-key".to_string()),
            },
            ..Config::default()
        };
        let warnings = config.validate_keys();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("OpenAI"));
    }

    #[test]
    fn test_validate_keys_no_keys() {
        let config = Config::default();
        assert!(config.validate_keys().is_empty());
    }
}
