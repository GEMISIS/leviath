//! CLI configuration management.

use leviath_mcp::MCPServerConfig;
use serde::{Deserialize, Serialize};
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
        }
    }
}

impl Config {
    /// Load configuration from the default location (~/.leviath/config.toml).
    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path();

        if !path.exists() {
            tracing::debug!("No config file found at {}, using defaults", path.display());
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read config from '{}': {}", path.display(), e))?;

        let config: Self = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?;

        tracing::debug!("Loaded config from {}", path.display());
        Ok(config)
    }

    /// Save configuration to the default location.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create config directory: {}", e))?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;

        std::fs::write(&path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write config to '{}': {}", path.display(), e))?;

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
}
