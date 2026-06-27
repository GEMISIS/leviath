//! CLI configuration management.

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
        }
    }
}

impl Config {
    /// Load configuration from the default location.
    pub fn load() -> anyhow::Result<Self> {
        // TODO: Implement config loading from ~/.leviath/config.toml
        Ok(Self::default())
    }

    /// Save configuration to the default location.
    pub fn save(&self) -> anyhow::Result<()> {
        // TODO: Implement config saving
        Ok(())
    }
}
