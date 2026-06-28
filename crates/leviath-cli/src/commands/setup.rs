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
    let ollama_input = prompt_plain(
        "Ollama base URL",
        current_ollama,
    )?;
    config.ollama_base_url = if ollama_input == default_ollama {
        None // store None so the default takes effect
    } else if ollama_input.is_empty() {
        config.ollama_base_url.clone()
    } else {
        Some(ollama_input)
    };

    // Default model
    let current_model = config.default_model.as_deref().unwrap_or("(provider default)");
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
