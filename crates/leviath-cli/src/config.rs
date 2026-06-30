//! CLI configuration management.

use leviath_mcp::MCPServerConfig;
use leviath_providers::ModelCapabilities;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Whether a tool call should execute automatically or require user approval.
///
/// The effective policy for a tool is resolved by narrowest scope first:
/// launch-flag > stage > agent > global config > built-in default.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Execute without prompting.
    Allow,
    /// Ask the user before each call (or once per session with `allow_session`).
    #[default]
    Ask,
    /// Never execute — return a denied error to the model.
    Deny,
}

/// Configuration for auto-generating a short title from the task prompt.
///
/// The title is generated once, at worker startup, by a cheap/fast model.
/// Set `enabled = false` in `[title]` to disable title generation entirely.
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

fn default_true() -> bool {
    true
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

    /// Global tool permission overrides.
    ///
    /// Keys are tool names (e.g. `"bash"`, `"write_file"`).  Values override
    /// the built-in Claude Code-style defaults.  Narrower scopes (agent,
    /// stage, launch flags) take precedence over these.
    #[serde(default)]
    pub tool_permissions: HashMap<String, ToolPolicy>,

    /// Title-generation configuration.
    ///
    /// Controls whether a short human-readable title is auto-generated from
    /// the task prompt at worker startup.
    #[serde(default)]
    pub title: TitleConfig,
}

/// Provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Anthropic API key
    pub anthropic_api_key: Option<String>,

    /// OpenAI API key
    pub openai_api_key: Option<String>,

    /// Google AI (Gemini) API key
    pub google_api_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: "anthropic".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
                google_api_key: None,
            },
            agent_paths: Vec::new(),
            registries: vec!["https://leviath.dev/registry".to_string()],
            openrouter_api_key: None,
            ollama_base_url: None,
            mcp_servers: Vec::new(),
            default_model: None,
            model_capabilities: HashMap::new(),
            tool_permissions: HashMap::new(),
            title: TitleConfig::default(),
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
            let content = std::fs::read_to_string(&path).map_err(|e| {
                anyhow::anyhow!("Failed to read config from '{}': {}", path.display(), e)
            })?;

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
        if config.providers.google_api_key.is_none() {
            config.providers.google_api_key = std::env::var("GOOGLE_API_KEY").ok();
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

        std::fs::write(&path, content).map_err(|e| {
            anyhow::anyhow!("Failed to write config to '{}': {}", path.display(), e)
        })?;

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
                google_api_key: None,
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
                google_api_key: None,
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
                google_api_key: None,
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
                google_api_key: None,
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

    // ─── Config defaults ───────────────────────────────────────────────────

    #[test]
    fn config_default_values() {
        let config = Config::default();
        assert_eq!(config.default_provider, "anthropic");
        assert!(config.providers.anthropic_api_key.is_none());
        assert!(config.providers.openai_api_key.is_none());
        assert!(config.providers.google_api_key.is_none());
        assert!(config.openrouter_api_key.is_none());
        assert!(config.ollama_base_url.is_none());
        assert!(config.mcp_servers.is_empty());
        assert!(config.default_model.is_none());
        assert!(config.model_capabilities.is_empty());
        assert!(config.tool_permissions.is_empty());
        assert!(!config.registries.is_empty());
    }

    // ─── TitleConfig ───────────────────────────────────────────────────────

    #[test]
    fn title_config_default() {
        let tc = TitleConfig::default();
        assert!(tc.enabled);
        assert!(tc.provider.is_none());
        assert!(tc.model.is_none());
    }

    #[test]
    fn title_config_serde_roundtrip() {
        let tc = TitleConfig {
            enabled: false,
            provider: Some("openai".to_string()),
            model: Some("gpt-5.4-mini".to_string()),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let back: TitleConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.enabled);
        assert_eq!(back.provider.as_deref(), Some("openai"));
        assert_eq!(back.model.as_deref(), Some("gpt-5.4-mini"));
    }

    // ─── ToolPolicy ────────────────────────────────────────────────────────

    #[test]
    fn tool_policy_default_is_ask() {
        let policy = ToolPolicy::default();
        assert_eq!(policy, ToolPolicy::Ask);
    }

    #[test]
    fn tool_policy_serde_roundtrip() {
        for policy in [ToolPolicy::Allow, ToolPolicy::Ask, ToolPolicy::Deny] {
            let json = serde_json::to_string(&policy).unwrap();
            let back: ToolPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(policy, back);
        }
    }

    #[test]
    fn tool_policy_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&ToolPolicy::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(serde_json::to_string(&ToolPolicy::Ask).unwrap(), "\"ask\"");
        assert_eq!(
            serde_json::to_string(&ToolPolicy::Deny).unwrap(),
            "\"deny\""
        );
    }

    // ─── Config TOML parsing ───────────────────────────────────────────────

    #[test]
    fn config_from_toml_with_all_fields() {
        let toml_content = r#"
default_provider = "openai"
openrouter_api_key = "sk-or-test"
ollama_base_url = "http://my-ollama:11434"
default_model = "gpt-5"
registries = ["https://example.com/registry"]
agent_paths = []

[providers]
anthropic_api_key = "sk-ant-test"
openai_api_key = "sk-test"
google_api_key = "AIza-test"

[tool_permissions]
bash = "deny"
read_file = "allow"

[title]
enabled = false
provider = "anthropic"
model = "claude-haiku-4-5"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.default_provider, "openai");
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-test")
        );
        assert_eq!(config.providers.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            config.providers.google_api_key.as_deref(),
            Some("AIza-test")
        );
        assert_eq!(config.openrouter_api_key.as_deref(), Some("sk-or-test"));
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://my-ollama:11434")
        );
        assert_eq!(config.default_model.as_deref(), Some("gpt-5"));
        assert!(!config.title.enabled);
        assert_eq!(config.tool_permissions.get("bash"), Some(&ToolPolicy::Deny));
        assert_eq!(
            config.tool_permissions.get("read_file"),
            Some(&ToolPolicy::Allow)
        );
    }

    #[test]
    fn config_from_minimal_toml() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.default_provider, "anthropic");
        assert!(config.providers.anthropic_api_key.is_none());
    }

    #[test]
    fn config_from_toml_with_mcp_servers() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[[mcp_servers]]
name = "test-server"
command = "echo"
args = ["hello"]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "test-server");
    }

    #[test]
    fn config_from_toml_with_model_capabilities() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[model_capabilities."my-custom-model"]
supports_temperature = true
supports_streaming = false
supports_tools = true
supports_system_prompt = true
max_context_tokens = 4096
max_output_tokens = 2048
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        let caps = config.model_capabilities.get("my-custom-model").unwrap();
        assert!(caps.supports_temperature);
        assert!(!caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 4096);
        assert_eq!(caps.max_output_tokens, 2048);
    }

    // ─── validate_keys with both keys ──────────────────────────────────────

    #[test]
    fn validate_keys_both_bad() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("bad".to_string()),
                openai_api_key: Some("bad".to_string()),
                google_api_key: None,
            },
            ..Config::default()
        };
        let warnings = config.validate_keys();
        assert_eq!(warnings.len(), 2);
    }

    // ─── redact_key edge cases ─────────────────────────────────────────────

    #[test]
    fn redact_key_exactly_9_chars() {
        // 9 chars: should show first 4 + ... + last 4
        assert_eq!(redact_key("123456789"), "1234...6789");
    }

    #[test]
    fn redact_key_empty() {
        assert_eq!(redact_key(""), "***");
    }

    // ─── config_path ───────────────────────────────────────────────────────

    #[test]
    fn config_path_contains_leviath() {
        let path = Config::config_path();
        assert!(path.to_str().unwrap().contains(".leviath"));
        assert!(path.to_str().unwrap().ends_with("config.toml"));
    }

    // ─── Config save/load roundtrip ────────────────────────────────────────

    #[test]
    fn config_toml_roundtrip() {
        let config = Config {
            default_provider: "openai".to_string(),
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-key".to_string()),
                openai_api_key: None,
                google_api_key: None,
            },
            tool_permissions: {
                let mut m = HashMap::new();
                m.insert("bash".to_string(), ToolPolicy::Deny);
                m
            },
            ..Config::default()
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.default_provider, "openai");
        assert_eq!(
            deserialized.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-key")
        );
        assert_eq!(
            deserialized.tool_permissions.get("bash"),
            Some(&ToolPolicy::Deny)
        );
    }

    // ─── validate_keys: both keys valid ──────────────────────────────────

    #[test]
    fn validate_keys_both_valid() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: Some("sk-ant-good-key".to_string()),
                openai_api_key: Some("sk-good-key".to_string()),
                google_api_key: None,
            },
            ..Config::default()
        };
        assert!(config.validate_keys().is_empty());
    }

    // ─── validate_keys: google key has no validation ─────────────────────

    #[test]
    fn validate_keys_google_key_not_validated() {
        let config = Config {
            providers: ProviderConfig {
                anthropic_api_key: None,
                openai_api_key: None,
                google_api_key: Some("anything-goes".to_string()),
            },
            ..Config::default()
        };
        // Google key has no prefix validation
        assert!(config.validate_keys().is_empty());
    }

    // ─── redact_key additional ───────────────────────────────────────────

    #[test]
    fn redact_key_typical_openai() {
        let key = "sk-proj-abcdef12345678";
        let redacted = redact_key(key);
        assert!(redacted.starts_with("sk-p"));
        assert!(redacted.ends_with("5678"));
        assert!(redacted.contains("..."));
    }

    #[test]
    fn redact_key_typical_anthropic() {
        let key = "sk-ant-api03-abc123xyz";
        let redacted = redact_key(key);
        assert!(redacted.starts_with("sk-a"));
        assert!(redacted.contains("..."));
    }

    // ─── Config TOML parsing: registries ─────────────────────────────────

    #[test]
    fn config_from_toml_custom_registries() {
        let toml_content = r#"
default_provider = "anthropic"
registries = ["https://my-registry.example.com", "https://backup.example.com"]
agent_paths = ["/my/agents"]

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.registries.len(), 2);
        assert_eq!(config.registries[0], "https://my-registry.example.com");
        assert_eq!(config.agent_paths.len(), 1);
    }

    // ─── Config save writes file ─────────────────────────────────────────

    #[test]
    fn config_save_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("subdir").join("config.toml");
        // We can't easily test Config::save() because it uses a fixed path,
        // but we can test the serialization and write manually
        let config = Config::default();
        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, &content).unwrap();
        assert!(config_path.exists());
        let loaded_content = std::fs::read_to_string(&config_path).unwrap();
        let loaded: Config = toml::from_str(&loaded_content).unwrap();
        assert_eq!(loaded.default_provider, "anthropic");
    }

    // ─── TitleConfig serde from TOML ─────────────────────────────────────

    #[test]
    fn title_config_from_toml_defaults() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(config.title.enabled);
        assert!(config.title.provider.is_none());
        assert!(config.title.model.is_none());
    }

    #[test]
    fn title_config_from_toml_disabled() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[title]
enabled = false
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.title.enabled);
    }

    // ─── ToolPolicy in tool_permissions ───────────────────────────────────

    #[test]
    fn config_tool_permissions_allow() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[tool_permissions]
read_file = "allow"
write_file = "ask"
bash = "deny"
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(
            config.tool_permissions.get("read_file"),
            Some(&ToolPolicy::Allow)
        );
        assert_eq!(
            config.tool_permissions.get("write_file"),
            Some(&ToolPolicy::Ask)
        );
        assert_eq!(config.tool_permissions.get("bash"), Some(&ToolPolicy::Deny));
    }

    // ─── Config with agent_paths ─────────────────────────────────────────

    #[test]
    fn config_with_agent_paths() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = ["/home/user/agents", "/opt/agents"]

[providers]
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.agent_paths.len(), 2);
    }

    // ─── Config with multiple model_capabilities ─────────────────────────

    #[test]
    fn config_multiple_model_capabilities() {
        let toml_content = r#"
default_provider = "anthropic"
registries = []
agent_paths = []

[providers]

[model_capabilities."model-a"]
supports_temperature = true
supports_streaming = true
supports_tools = true
supports_system_prompt = true
max_context_tokens = 8192
max_output_tokens = 4096

[model_capabilities."model-b"]
supports_temperature = false
supports_streaming = false
supports_tools = false
supports_system_prompt = false
max_context_tokens = 2048
max_output_tokens = 1024
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert_eq!(config.model_capabilities.len(), 2);
        let caps_a = config.model_capabilities.get("model-a").unwrap();
        assert!(caps_a.supports_temperature);
        assert_eq!(caps_a.max_context_tokens, 8192);
        let caps_b = config.model_capabilities.get("model-b").unwrap();
        assert!(!caps_b.supports_temperature);
        assert_eq!(caps_b.max_context_tokens, 2048);
    }
}
