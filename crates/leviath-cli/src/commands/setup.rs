//! `lev setup` - Interactive configuration wizard

use clap::Args;
use std::io::{self, Write};

use crate::config::Config;

#[derive(Args)]
pub struct SetupArgs {
    /// Run non-interactively using only flag values (useful for scripting)
    #[arg(long)]
    pub non_interactive: bool,

    /// Anthropic API key
    #[arg(long)]
    pub anthropic_key: Option<String>,

    /// OpenAI API key
    #[arg(long)]
    pub openai_key: Option<String>,

    /// Google AI (Gemini) API key
    #[arg(long)]
    pub google_key: Option<String>,

    /// OpenRouter API key
    #[arg(long)]
    pub openrouter_key: Option<String>,

    /// Ollama base URL (default: http://localhost:11434)
    #[arg(long)]
    pub ollama_url: Option<String>,

    /// Default model override (e.g. claude-sonnet-4-6)
    #[arg(long)]
    pub default_model: Option<String>,
}

pub async fn execute(args: SetupArgs) -> anyhow::Result<()> {
    // Load existing config so we can preserve existing values as defaults
    let mut config = Config::load().unwrap_or_default();

    if args.non_interactive {
        apply_flags(&mut config, &args);
        config.save()?;
        println!("Config saved to {}", Config::config_path().display());
        return Ok(());
    }

    println!("Leviath Setup");
    println!("─────────────────────────────────────────");
    println!("Press Enter to keep the current value shown in [brackets].");
    println!("Type a value and press Enter to update it.");
    println!("Type 'clear' to remove a stored value.");
    println!();

    // Anthropic API key
    let current_anthropic = config.providers.anthropic_api_key.as_deref().map(redact);
    config.providers.anthropic_api_key = prompt_secret(
        "Anthropic API key",
        "sk-ant-...",
        config.providers.anthropic_api_key.as_deref(),
        current_anthropic.as_deref(),
    )?;

    // OpenAI API key
    let current_openai = config.providers.openai_api_key.as_deref().map(redact);
    config.providers.openai_api_key = prompt_secret(
        "OpenAI API key",
        "sk-...",
        config.providers.openai_api_key.as_deref(),
        current_openai.as_deref(),
    )?;

    // Google AI (Gemini) API key
    let current_google = config.providers.google_api_key.as_deref().map(redact);
    config.providers.google_api_key = prompt_secret(
        "Google AI (Gemini) API key",
        "AIza...",
        config.providers.google_api_key.as_deref(),
        current_google.as_deref(),
    )?;

    // OpenRouter API key
    let current_or = config.openrouter_api_key.as_deref().map(redact);
    config.openrouter_api_key = prompt_secret(
        "OpenRouter API key",
        "sk-or-...",
        config.openrouter_api_key.as_deref(),
        current_or.as_deref(),
    )?;

    // Ollama URL
    let default_ollama = "http://localhost:11434";
    let current_ollama = config.ollama_base_url.as_deref().unwrap_or(default_ollama);
    let ollama_input = prompt_plain("Ollama base URL", current_ollama)?;
    config.ollama_base_url = if ollama_input == default_ollama {
        None // store None so the default takes effect
    } else if ollama_input.is_empty() {
        config.ollama_base_url.clone()
    } else {
        Some(ollama_input)
    };

    // Default model
    let current_model = config
        .default_model
        .as_deref()
        .unwrap_or("(provider default)");
    let model_input = prompt_plain("Default model override", current_model)?;
    config.default_model = if model_input.is_empty() || model_input == "(provider default)" {
        config.default_model.clone()
    } else if model_input == "clear" {
        None
    } else {
        Some(model_input)
    };

    // Default provider
    let provider_input = prompt_plain("Default provider", &config.default_provider)?;
    if !provider_input.is_empty() {
        config.default_provider = provider_input;
    }

    println!();
    config.save()?;
    println!("Config saved to {}", Config::config_path().display());

    // Validate and warn about keys
    let warnings = config.validate_keys();
    for w in &warnings {
        println!("  Warning: {}", w);
    }

    if warnings.is_empty() {
        println!("All API keys look valid.");
    }

    Ok(())
}

fn apply_flags(config: &mut Config, args: &SetupArgs) {
    if let Some(ref k) = args.anthropic_key {
        config.providers.anthropic_api_key = Some(k.clone());
    }
    if let Some(ref k) = args.openai_key {
        config.providers.openai_api_key = Some(k.clone());
    }
    if let Some(ref k) = args.google_key {
        config.providers.google_api_key = Some(k.clone());
    }
    if let Some(ref k) = args.openrouter_key {
        config.openrouter_api_key = Some(k.clone());
    }
    if let Some(ref u) = args.ollama_url {
        config.ollama_base_url = Some(u.clone());
    }
    if let Some(ref m) = args.default_model {
        config.default_model = Some(m.clone());
    }
}

/// Prompt for a secret value. Shows a redacted hint of the stored value.
/// Returns `None` if the user clears the value; preserves the existing value on empty input.
fn prompt_secret(
    label: &str,
    hint: &str,
    current: Option<&str>,
    display: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let shown = display.unwrap_or("(not set)");
    print!("  {} [{}] ({}): ", label, shown, hint);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    Ok(if input == "clear" {
        None
    } else if input.is_empty() {
        current.map(|s| s.to_string())
    } else {
        Some(input.to_string())
    })
}

/// Prompt for a plain (non-secret) value.
fn prompt_plain(label: &str, current: &str) -> anyhow::Result<String> {
    print!("  {} [{}]: ", label, current);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Redact an API key for display: show first 8 chars + "...".
fn redact(key: &str) -> String {
    if key.len() <= 8 {
        "***".to_string()
    } else {
        format!("{}...", &key[..8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── redact ────────────────────────────────────────────────────────────

    #[test]
    fn redact_short_key() {
        assert_eq!(redact("abc"), "***");
        assert_eq!(redact("12345678"), "***");
    }

    #[test]
    fn redact_long_key() {
        assert_eq!(redact("sk-ant-api-key-12345"), "sk-ant-a...");
        assert_eq!(redact("123456789"), "12345678...");
    }

    #[test]
    fn redact_empty() {
        assert_eq!(redact(""), "***");
    }

    // ─── apply_flags ───────────────────────────────────────────────────────

    #[test]
    fn apply_flags_sets_anthropic_key() {
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: Some("sk-ant-test".to_string()),
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
        };
        apply_flags(&mut config, &args);
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-test")
        );
    }

    #[test]
    fn apply_flags_sets_all_keys() {
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: Some("ant-key".to_string()),
            openai_key: Some("oai-key".to_string()),
            google_key: Some("goog-key".to_string()),
            openrouter_key: Some("or-key".to_string()),
            ollama_url: Some("http://my-ollama:11434".to_string()),
            default_model: Some("my-model".to_string()),
        };
        apply_flags(&mut config, &args);
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("ant-key")
        );
        assert_eq!(config.providers.openai_api_key.as_deref(), Some("oai-key"));
        assert_eq!(config.providers.google_api_key.as_deref(), Some("goog-key"));
        assert_eq!(config.openrouter_api_key.as_deref(), Some("or-key"));
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://my-ollama:11434")
        );
        assert_eq!(config.default_model.as_deref(), Some("my-model"));
    }

    #[test]
    fn apply_flags_preserves_existing_on_none() {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("existing-key".to_string());
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
        };
        apply_flags(&mut config, &args);
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("existing-key")
        );
    }

    // ─── non-interactive mode save/load ────────────────────────────────────

    #[test]
    fn config_save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config {
            default_provider: "anthropic".to_string(),
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-test-key".to_string()),
                openai_api_key: None,
                google_api_key: None,
            },
            ..Config::default()
        };

        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&config_path, &content).unwrap();
        let loaded_content = std::fs::read_to_string(&config_path).unwrap();
        let loaded: Config = toml::from_str(&loaded_content).unwrap();

        assert_eq!(loaded.default_provider, "anthropic");
        assert_eq!(
            loaded.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-test-key")
        );
    }

    // ─── apply_flags partial updates ──────────────────────────────────────

    #[test]
    fn apply_flags_only_openai() {
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: Some("sk-openai".to_string()),
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
        };
        apply_flags(&mut config, &args);
        assert!(config.providers.anthropic_api_key.is_none());
        assert_eq!(
            config.providers.openai_api_key.as_deref(),
            Some("sk-openai")
        );
    }

    #[test]
    fn apply_flags_only_google() {
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: Some("AIza-test".to_string()),
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
        };
        apply_flags(&mut config, &args);
        assert_eq!(
            config.providers.google_api_key.as_deref(),
            Some("AIza-test")
        );
    }

    #[test]
    fn apply_flags_only_openrouter() {
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: Some("sk-or-key".to_string()),
            ollama_url: None,
            default_model: None,
        };
        apply_flags(&mut config, &args);
        assert_eq!(config.openrouter_api_key.as_deref(), Some("sk-or-key"));
    }

    #[test]
    fn apply_flags_only_ollama_url() {
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: Some("http://custom:1234".to_string()),
            default_model: None,
        };
        apply_flags(&mut config, &args);
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://custom:1234")
        );
    }

    #[test]
    fn apply_flags_only_default_model() {
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: Some("gpt-5".to_string()),
        };
        apply_flags(&mut config, &args);
        assert_eq!(config.default_model.as_deref(), Some("gpt-5"));
    }

    // ─── redact edge cases ────────────────────────────────────────────────

    #[test]
    fn redact_exactly_8_chars() {
        assert_eq!(redact("12345678"), "***");
    }

    #[test]
    fn redact_9_chars() {
        assert_eq!(redact("123456789"), "12345678...");
    }

    #[test]
    fn redact_typical_anthropic_key() {
        let key = "sk-ant-api03-abcdefghijklmnop";
        let redacted = redact(key);
        assert!(redacted.ends_with("..."));
        assert!(redacted.starts_with("sk-ant-a"));
    }

    // ─── apply_flags overwrites existing keys ────────────────────────────

    #[test]
    fn apply_flags_overwrites_existing_key() {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("old-key".to_string());
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: Some("new-key".to_string()),
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
        };
        apply_flags(&mut config, &args);
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("new-key")
        );
    }

    // ─── apply_flags sets multiple keys simultaneously ───────────────────

    #[test]
    fn apply_flags_partial_preserves_unrelated() {
        let mut config = Config::default();
        config.providers.anthropic_api_key = Some("ant-key".to_string());
        config.default_model = Some("old-model".to_string());
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: Some("oai-key".to_string()),
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: Some("new-model".to_string()),
        };
        apply_flags(&mut config, &args);
        // Anthropic key preserved
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("ant-key")
        );
        // OpenAI key set
        assert_eq!(config.providers.openai_api_key.as_deref(), Some("oai-key"));
        // Model updated
        assert_eq!(config.default_model.as_deref(), Some("new-model"));
    }

    // ─── redact edge case: exactly 9 chars ───────────────────────────────

    #[test]
    fn redact_special_characters() {
        let key = "!@#$%^&*()_+";
        let redacted = redact(key);
        assert!(redacted.ends_with("..."));
    }

    // ─── config roundtrip with all keys ──────────────────────────────────

    #[test]
    fn config_roundtrip_all_providers() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let config = Config {
            default_provider: "openai".to_string(),
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-key".to_string()),
                openai_api_key: Some("sk-oai-key".to_string()),
                google_api_key: Some("AIza-key".to_string()),
            },
            openrouter_api_key: Some("sk-or-key".to_string()),
            ollama_base_url: Some("http://custom:11434".to_string()),
            default_model: Some("my-model".to_string()),
            ..Config::default()
        };

        let content = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&config_path, &content).unwrap();
        let loaded_content = std::fs::read_to_string(&config_path).unwrap();
        let loaded: Config = toml::from_str(&loaded_content).unwrap();

        assert_eq!(loaded.default_provider, "openai");
        assert_eq!(
            loaded.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-key")
        );
        assert_eq!(
            loaded.providers.openai_api_key.as_deref(),
            Some("sk-oai-key")
        );
        assert_eq!(loaded.providers.google_api_key.as_deref(), Some("AIza-key"));
        assert_eq!(loaded.openrouter_api_key.as_deref(), Some("sk-or-key"));
        assert_eq!(
            loaded.ollama_base_url.as_deref(),
            Some("http://custom:11434")
        );
        assert_eq!(loaded.default_model.as_deref(), Some("my-model"));
    }
}
