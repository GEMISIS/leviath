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

    /// Enable the Claude Code CLI transport (runs on your Claude subscription
    /// instead of an API key). Off unless set: the CLI adds its own context to
    /// every call, including your account email address.
    #[arg(long)]
    pub claude_code: Option<bool>,

    /// Reasoning effort for the Claude Code transport
    /// (low, medium, high, xhigh, max)
    #[arg(long)]
    pub claude_code_effort: Option<String>,
}

/// `lev setup`: load the config, then either apply flags non-interactively or
/// run the interactive prompt sequence, saving to `Config::config_path()`.
///
/// The reader is a lazily-constructed factory so the interactive arm can take
/// the process's real `stdin().lock()` (the un-unit-testable leaf, supplied by
/// the binary) while the non-interactive arm never builds it. Tests drive both
/// arms with an isolated config path and an in-memory `Cursor`.
pub fn execute_with<R: io::BufRead>(
    args: &SetupArgs,
    make_reader: impl FnOnce() -> R,
) -> anyhow::Result<()> {
    let mut config = Config::load().unwrap_or_default();
    let save_path = Config::config_path();
    if args.non_interactive {
        return run_non_interactive_setup(&mut config, args, &save_path);
    }
    run_interactive_setup(&mut config, &mut make_reader(), &save_path)
}

/// Core of the `--non-interactive` path, with an explicit `save_path` so tests
/// can target a tempfile instead of the real config.
fn run_non_interactive_setup(
    config: &mut Config,
    args: &SetupArgs,
    save_path: &std::path::Path,
) -> anyhow::Result<()> {
    apply_flags(config, args);
    config.save_to_path(save_path)?;
    println!("Config saved to {}", save_path.display());
    Ok(())
}

/// The interactive prompt sequence, reading from any [`io::BufRead`] and saving
/// to an explicit `save_path` so tests can drive it with an in-memory reader and
/// a tempfile instead of blocking on real stdin and writing the real config.
fn run_interactive_setup<R: io::BufRead>(
    config: &mut Config,
    reader: &mut R,
    save_path: &std::path::Path,
) -> anyhow::Result<()> {
    println!("Leviath Setup");
    println!("─────────────────────────────────────────");
    println!("Press Enter to keep the current value shown in [brackets].");
    println!("Type a value and press Enter to update it.");
    println!("Type 'clear' to remove a stored value.");
    println!();

    // Anthropic API key
    let current_anthropic = config.providers.anthropic_api_key.as_deref().map(redact);
    config.providers.anthropic_api_key = prompt_secret(
        reader,
        "Anthropic API key",
        "sk-ant-...",
        config.providers.anthropic_api_key.as_deref(),
        current_anthropic.as_deref(),
    );

    // OpenAI API key
    let current_openai = config.providers.openai_api_key.as_deref().map(redact);
    config.providers.openai_api_key = prompt_secret(
        reader,
        "OpenAI API key",
        "sk-...",
        config.providers.openai_api_key.as_deref(),
        current_openai.as_deref(),
    );

    // Google AI (Gemini) API key
    let current_google = config.providers.google_api_key.as_deref().map(redact);
    config.providers.google_api_key = prompt_secret(
        reader,
        "Google AI (Gemini) API key",
        "AIza...",
        config.providers.google_api_key.as_deref(),
        current_google.as_deref(),
    );

    // OpenRouter API key
    let current_or = config.openrouter_api_key.as_deref().map(redact);
    config.openrouter_api_key = prompt_secret(
        reader,
        "OpenRouter API key",
        "sk-or-...",
        config.openrouter_api_key.as_deref(),
        current_or.as_deref(),
    );

    // Ollama URL
    let default_ollama = "http://localhost:11434";
    let current_ollama = config.ollama_base_url.as_deref().unwrap_or(default_ollama);
    let ollama_input = prompt_plain(reader, "Ollama base URL", current_ollama);
    config.ollama_base_url = if ollama_input == default_ollama {
        None // store None so the default takes effect
    } else if ollama_input.is_empty() {
        config.ollama_base_url.clone()
    } else {
        Some(ollama_input)
    };

    // Claude Code transport — offered, never selected for the user. The shown
    // default is the stored value, so on a fresh install this reads `[no]` and
    // pressing Enter through the whole wizard leaves it disabled.
    println!();
    println!("  Claude Code transport: run Leviath on your Claude subscription, no API key.");
    println!("  Caveat: the CLI adds ~130 tokens of its own context to every call, including");
    println!("  your account email address and the current date. This cannot be disabled.");
    let enabled_before = config.providers.claude_code_enabled;
    let cc_input = prompt_plain(
        reader,
        "Enable Claude Code provider",
        yes_no(enabled_before),
    );
    config.providers.claude_code_enabled = parse_yes_no(&cc_input, enabled_before);

    if config.providers.claude_code_enabled {
        let current_effort = config
            .providers
            .claude_code_effort
            .as_deref()
            .unwrap_or(leviath_providers::claude_code::DEFAULT_EFFORT);
        let effort_input = prompt_plain(
            reader,
            "  Claude Code effort (low/medium/high/xhigh/max)",
            current_effort,
        );
        if !effort_input.is_empty() {
            config.providers.claude_code_effort = Some(effort_input);
        }
        if !enabled_before {
            println!("  Enabled. Sign in with `claude` if you haven't already.");
        }
    }

    // Default model
    let current_model = config
        .default_model
        .as_deref()
        .unwrap_or("(provider default)");
    let model_input = prompt_plain(reader, "Default model override", current_model);
    config.default_model = if model_input.is_empty() || model_input == "(provider default)" {
        config.default_model.clone()
    } else if model_input == "clear" {
        None
    } else {
        Some(model_input)
    };

    // Default provider
    let provider_input = prompt_plain(reader, "Default provider", &config.default_provider);
    if !provider_input.is_empty() {
        config.default_provider = provider_input;
    }

    println!();
    config.save_to_path(save_path)?;
    println!("Config saved to {}", save_path.display());

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
    if let Some(enabled) = args.claude_code {
        config.providers.claude_code_enabled = enabled;
    }
    if let Some(ref e) = args.claude_code_effort {
        config.providers.claude_code_effort = Some(e.clone());
    }
}

/// Prompt for a secret value. Returns `None` if the user clears the value;
/// preserves the existing value on empty input. I/O errors are swallowed and
/// treated as empty input so the caller only needs to handle save failure.
fn prompt_secret<R: io::BufRead>(
    reader: &mut R,
    label: &str,
    hint: &str,
    current: Option<&str>,
    display: Option<&str>,
) -> Option<String> {
    let shown = display.unwrap_or("(not set)");
    print!("  {} [{}] ({}): ", label, shown, hint);
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = reader.read_line(&mut input);
    let input = input.trim();
    if input == "clear" {
        None
    } else if input.is_empty() {
        current.map(|s| s.to_string())
    } else {
        Some(input.to_string())
    }
}

/// Prompt for a plain (non-secret) value. I/O errors are swallowed and
/// treated as empty input so the caller only needs to handle save failure.
fn prompt_plain<R: io::BufRead>(reader: &mut R, label: &str, current: &str) -> String {
    print!("  {} [{}]: ", label, current);
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = reader.read_line(&mut input);
    input.trim().to_string()
}

/// The bracketed default shown for a yes/no prompt.
fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// Interpret a yes/no answer, keeping `current` on empty or unrecognized input.
///
/// Unrecognized input keeps the current value rather than guessing: for a prompt
/// whose default is "off", a typo must never read as consent.
fn parse_yes_no(input: &str, current: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" | "on" => true,
        "n" | "no" | "false" | "0" | "off" => false,
        _ => current,
    }
}

/// Redact an API key for display: show the first 8 characters + "...".
///
/// Counts *characters*, not bytes. A byte-based version both panicked on a key
/// containing a multi-byte character straddling byte 8 (the shape of issue #115)
/// and, worse, leaked: a 3-character 9-byte key is longer than 8 bytes, so the
/// byte branch would print every character of it followed by an "I truncated
/// this" marker.
fn redact(key: &str) -> String {
    if key.chars().count() <= 8 {
        "***".to_string()
    } else {
        format!("{}...", key.chars().take(8).collect::<String>())
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

    #[test]
    fn redact_multibyte_key() {
        // Issue #115: byte 8 falls inside the third '日' (bytes 6..9), which used
        // to panic.
        assert_eq!(redact("日本語日本語日本語"), "日本語日本語日本...");
        // 3 characters but 9 bytes — the byte-length guard would have classified
        // this as "long" and printed the whole key. Character counting redacts it.
        assert_eq!(redact("日本語"), "***");
        // Exactly 8 characters is still fully redacted.
        assert_eq!(redact("日本語日本語日本"), "***");
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
            claude_code: None,
            claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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
    fn apply_flags_sets_claude_code_enable_and_effort() {
        // The non-interactive `--claude-code`/`--claude-code-effort` arms.
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
            claude_code: Some(true),
            claude_code_effort: Some("xhigh".to_string()),
        };
        apply_flags(&mut config, &args);
        assert!(config.providers.claude_code_enabled);
        assert_eq!(
            config.providers.claude_code_effort.as_deref(),
            Some("xhigh")
        );
    }

    #[test]
    fn apply_flags_can_disable_claude_code() {
        // `--claude-code false` turns an enabled provider back off.
        let mut config = Config::default();
        config.providers.claude_code_enabled = true;
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
            claude_code: Some(false),
            claude_code_effort: None,
        };
        apply_flags(&mut config, &args);
        assert!(!config.providers.claude_code_enabled);
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
            claude_code: None,
            claude_code_effort: None,
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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
            claude_code: None,
            claude_code_effort: None,
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

    #[test]
    fn redact_special_characters() {
        let key = "!@#$%^&*()_+";
        let redacted = redact(key);
        assert!(redacted.ends_with("..."));
    }

    // ─── prompt_secret / prompt_plain (mocked stdin) ───────────────────────

    use std::io::Cursor;

    fn reader_from(input: &str) -> Cursor<Vec<u8>> {
        Cursor::new(input.as_bytes().to_vec())
    }

    #[test]
    fn prompt_secret_clear_returns_none() {
        let mut reader = reader_from("clear\n");
        let result = prompt_secret(
            &mut reader,
            "Anthropic API key",
            "sk-ant-...",
            Some("existing-key"),
            Some("existin..."),
        );
        assert_eq!(result, None);
    }

    #[test]
    fn prompt_secret_empty_input_preserves_current() {
        let mut reader = reader_from("\n");
        let result = prompt_secret(
            &mut reader,
            "Anthropic API key",
            "sk-ant-...",
            Some("existing-key"),
            Some("existin..."),
        );
        assert_eq!(result, Some("existing-key".to_string()));
    }

    #[test]
    fn prompt_secret_empty_input_with_no_current_stays_none() {
        let mut reader = reader_from("\n");
        let result = prompt_secret(&mut reader, "Anthropic API key", "sk-ant-...", None, None);
        assert_eq!(result, None);
    }

    #[test]
    fn prompt_secret_new_value_overwrites() {
        let mut reader = reader_from("sk-ant-brand-new\n");
        let result = prompt_secret(
            &mut reader,
            "Anthropic API key",
            "sk-ant-...",
            Some("old-key"),
            Some("old-k..."),
        );
        assert_eq!(result, Some("sk-ant-brand-new".to_string()));
    }

    #[test]
    fn prompt_secret_eof_behaves_like_empty_input() {
        // Closed/exhausted stdin: read_line leaves the buffer empty, which
        // is handled the same as a plain empty-input Enter press.
        let mut reader = reader_from("");
        let result = prompt_secret(
            &mut reader,
            "Anthropic API key",
            "sk-ant-...",
            Some("existing-key"),
            Some("existin..."),
        );
        assert_eq!(result, Some("existing-key".to_string()));
    }

    #[test]
    fn prompt_plain_returns_trimmed_new_value() {
        let mut reader = reader_from("  http://custom:1234  \n");
        let result = prompt_plain(&mut reader, "Ollama base URL", "http://localhost:11434");
        assert_eq!(result, "http://custom:1234");
    }

    #[test]
    fn prompt_plain_empty_input_returns_empty_string() {
        let mut reader = reader_from("\n");
        let result = prompt_plain(&mut reader, "Default provider", "anthropic");
        assert_eq!(result, "");
    }

    #[test]
    fn prompt_plain_eof_returns_empty_string() {
        let mut reader = reader_from("");
        let result = prompt_plain(&mut reader, "Default provider", "anthropic");
        assert_eq!(result, "");
    }

    // ─── run_interactive_setup (mocked stdin + tempfile save path) ─────────

    /// One line per prompt, in order: anthropic, openai, google, openrouter,
    /// ollama URL, enable-claude-code, default model, default provider.
    /// (Answering yes to claude-code adds an effort prompt after it.)
    fn all_prompts_input(lines: &[&str]) -> Cursor<Vec<u8>> {
        reader_from(&lines.join("\n"))
    }

    #[test]
    fn run_interactive_setup_all_defaults_kept() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config::default();

        // 7 prompts: anthropic, openai, google, openrouter, ollama, model, provider.
        // Empty answers to all of them.
        let mut reader = all_prompts_input(&["", "", "", "", "", "", "", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        assert!(save_path.exists());
        assert!(config.providers.anthropic_api_key.is_none());
        assert!(config.ollama_base_url.is_none());
        assert_eq!(config.default_provider, "anthropic");
        // The requirement that motivated the prompt's shape: pressing Enter
        // through the whole wizard must never turn Claude Code on, because
        // doing so starts sending the user's account email to Anthropic.
        assert!(!config.providers.claude_code_enabled);
    }

    // ─── Claude Code opt-in prompt ──────────────────────────────────────────

    /// Run the wizard answering only the claude-code prompt (index 5); everything
    /// else is left at its default. Returns the resulting config.
    fn setup_answering_claude_code(answers: &[&str]) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config::default();
        let mut lines = vec!["", "", "", "", ""];
        lines.extend_from_slice(answers);
        lines.extend_from_slice(&["", ""]); // model, provider
        let mut reader = all_prompts_input(&lines);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();
        config
    }

    #[test]
    fn answering_yes_enables_claude_code_with_the_default_effort() {
        // "yes" then Enter at the effort prompt.
        let config = setup_answering_claude_code(&["yes", ""]);
        assert!(config.providers.claude_code_enabled);
        // Enter keeps the shown default rather than storing it explicitly.
        assert!(config.providers.claude_code_effort.is_none());
        // Enabling the transport must not hijack the default provider.
        assert_eq!(config.default_provider, "anthropic");
    }

    #[test]
    fn answering_yes_can_set_an_effort_level() {
        let config = setup_answering_claude_code(&["y", "xhigh"]);
        assert!(config.providers.claude_code_enabled);
        assert_eq!(
            config.providers.claude_code_effort.as_deref(),
            Some("xhigh")
        );
    }

    #[test]
    fn declining_leaves_claude_code_off_and_skips_the_effort_prompt() {
        // Only one answer consumed: no effort prompt is shown when declined.
        let config = setup_answering_claude_code(&["no"]);
        assert!(!config.providers.claude_code_enabled);
        assert!(config.providers.claude_code_effort.is_none());
    }

    #[test]
    fn an_unrecognized_answer_keeps_the_current_setting() {
        // A typo at a prompt whose default is "off" must not read as consent.
        let config = setup_answering_claude_code(&["mabye"]);
        assert!(!config.providers.claude_code_enabled);
    }

    #[test]
    fn an_enabled_provider_can_be_turned_back_off() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config {
            providers: crate::config::ProviderConfig {
                claude_code_enabled: true,
                claude_code_effort: Some("high".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        let mut reader = all_prompts_input(&["", "", "", "", "", "n", "", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();
        assert!(!config.providers.claude_code_enabled);
    }

    #[test]
    fn an_enabled_provider_keeps_its_settings_on_enter() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config {
            providers: crate::config::ProviderConfig {
                claude_code_enabled: true,
                claude_code_effort: Some("high".to_string()),
                ..Config::default().providers
            },
            ..Config::default()
        };
        // Enter at both the enable prompt and the effort prompt.
        let mut reader = all_prompts_input(&["", "", "", "", "", "", "", "", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();
        assert!(config.providers.claude_code_enabled);
        assert_eq!(config.providers.claude_code_effort.as_deref(), Some("high"));
    }

    #[test]
    fn yes_no_rendering_and_parsing_round_trip() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
        for affirmative in ["y", "YES", "true", "1", "on", " Yes "] {
            assert!(parse_yes_no(affirmative, false), "{affirmative}");
        }
        for negative in ["n", "NO", "false", "0", "off"] {
            assert!(!parse_yes_no(negative, true), "{negative}");
        }
        // Empty and unrecognized both keep the current value.
        assert!(parse_yes_no("", true));
        assert!(!parse_yes_no("", false));
        assert!(parse_yes_no("wat", true));
        assert!(!parse_yes_no("wat", false));
    }

    #[test]
    fn run_interactive_setup_sets_new_values() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config::default();

        let mut reader = all_prompts_input(&[
            "sk-ant-new",
            "sk-oai-new",
            "AIza-new",
            "sk-or-new",
            "http://custom-ollama:9999",
            "", // claude-code: declined
            "gpt-5",
            "openai",
        ]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-new")
        );
        assert_eq!(
            config.providers.openai_api_key.as_deref(),
            Some("sk-oai-new")
        );
        assert_eq!(config.providers.google_api_key.as_deref(), Some("AIza-new"));
        assert_eq!(config.openrouter_api_key.as_deref(), Some("sk-or-new"));
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://custom-ollama:9999")
        );
        assert_eq!(config.default_model.as_deref(), Some("gpt-5"));
        assert_eq!(config.default_provider, "openai");

        // Verify it was actually persisted to the injected path.
        let saved = std::fs::read_to_string(&save_path).unwrap();
        assert!(saved.contains("sk-ant-new"));
    }

    #[test]
    fn run_interactive_setup_clear_removes_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("old-ant".to_string()),
                openai_api_key: Some("old-oai".to_string()),
                google_api_key: Some("old-goog".to_string()),
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            openrouter_api_key: Some("old-or".to_string()),
            ..Config::default()
        };

        let mut reader = all_prompts_input(&["clear", "clear", "clear", "clear", "", "", "", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        assert!(config.providers.anthropic_api_key.is_none());
        assert!(config.providers.openai_api_key.is_none());
        assert!(config.providers.google_api_key.is_none());
        assert!(config.openrouter_api_key.is_none());
    }

    #[test]
    fn run_interactive_setup_ollama_url_matching_default_stores_none() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config {
            ollama_base_url: Some("http://something-else:1111".to_string()),
            ..Config::default()
        };

        let mut reader = all_prompts_input(&["", "", "", "", "http://localhost:11434", "", "", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        assert!(config.ollama_base_url.is_none());
    }

    #[test]
    fn run_interactive_setup_default_model_clear() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config {
            default_model: Some("old-model".to_string()),
            ..Config::default()
        };

        let mut reader = all_prompts_input(&["", "", "", "", "", "", "clear", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        assert!(config.default_model.is_none());
    }

    #[test]
    fn run_interactive_setup_default_model_provider_default_string_preserves() {
        // Exercises the right-hand side of `is_empty() || == "(provider default)"`.
        // When the user types the literal string "(provider default)" it is treated
        // the same as pressing Enter — the existing model is preserved.
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config {
            default_model: Some("existing-model".to_string()),
            ..Config::default()
        };

        let mut reader = all_prompts_input(&["", "", "", "", "", "", "(provider default)", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        assert_eq!(config.default_model.as_deref(), Some("existing-model"));
    }

    #[test]
    fn run_interactive_setup_warns_on_invalid_key_format() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config::default();

        // Anthropic key that doesn't start with "sk-ant-" triggers a warning.
        let mut reader =
            all_prompts_input(&["not-a-valid-anthropic-key", "", "", "", "", "", "", ""]);
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        let warnings = config.validate_keys();
        assert!(!warnings.is_empty());
    }

    #[test]
    fn run_interactive_setup_eof_treated_as_all_empty() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config::default();

        // Closed stdin: every read_line call returns 0 bytes immediately.
        let mut reader = reader_from("");
        run_interactive_setup(&mut config, &mut reader, &save_path).unwrap();

        assert!(save_path.exists());
        assert!(config.providers.anthropic_api_key.is_none());
    }

    #[test]
    fn run_interactive_setup_save_failure_returns_error() {
        // Make save_to_path fail by putting a file where the parent dir must be.
        let dir = tempfile::tempdir().unwrap();
        let blocking = dir.path().join("not-a-dir");
        std::fs::write(&blocking, "").unwrap();
        let bad_save = blocking.join("config.toml");
        let mut config = Config::default();
        let mut reader = all_prompts_input(&["", "", "", "", "", "", "", ""]);
        let result = run_interactive_setup(&mut config, &mut reader, &bad_save);
        assert!(result.is_err());
    }

    // ─── run_non_interactive_setup (tempfile save path) ────────────────────

    #[test]
    fn run_non_interactive_setup_applies_flags_and_saves() {
        let dir = tempfile::tempdir().unwrap();
        let save_path = dir.path().join("config.toml");
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: Some("sk-ant-cli".to_string()),
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
            claude_code: None,
            claude_code_effort: None,
        };

        run_non_interactive_setup(&mut config, &args, &save_path).unwrap();

        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-cli")
        );
        let saved = std::fs::read_to_string(&save_path).unwrap();
        assert!(saved.contains("sk-ant-cli"));
    }

    #[test]
    fn run_non_interactive_setup_save_failure_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocking = dir.path().join("not-a-dir");
        std::fs::write(&blocking, "").unwrap();
        let bad_save = blocking.join("config.toml");
        let mut config = Config::default();
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
            claude_code: None,
            claude_code_effort: None,
        };
        let result = run_non_interactive_setup(&mut config, &args, &bad_save);
        assert!(result.is_err());
    }

    // ─── redact edge ──────────────────────────────────────────────────────

    #[test]
    fn redact_exactly_8_chars_is_redacted() {
        // "12345678" has len == 8, which is <= 8, so should return "***"
        let result = redact("12345678");
        assert_eq!(result, "***");
    }

    #[test]
    fn redact_very_long_key() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345";
        let redacted = redact(key);
        assert!(redacted.ends_with("..."));
        assert!(redacted.starts_with("sk-ant-a"));
        assert!(!redacted.contains("xyz12345"));
    }

    // ─── non-interactive execute path (file-based) ─────────────────────────

    #[test]
    fn apply_flags_all_none_does_not_change_config() {
        let mut config = Config {
            default_provider: "openai".to_string(),
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("existing".to_string()),
                openai_api_key: None,
                google_api_key: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
            },
            ..Config::default()
        };
        let args = SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
            claude_code: None,
            claude_code_effort: None,
        };
        apply_flags(&mut config, &args);
        assert_eq!(
            config.providers.anthropic_api_key.as_deref(),
            Some("existing")
        );
        assert_eq!(config.default_provider, "openai");
    }

    #[test]
    fn redact_unicode_chars() {
        // Unicode characters where len() gives bytes
        let key = "abcdefghi"; // 9 chars, > 8
        let redacted = redact(key);
        assert_eq!(redacted, "abcdefgh...");
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
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
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

    // ─── config-path write-through ───────────────────────────────────────
    //
    // The `execute()` entrypoint (branch on `non_interactive`, and the real
    // stdin read for the interactive path) lives in the binary's `real_setup`;
    // the library keeps the two fully-tested cores. This test pins the
    // guarantee that `run_non_interactive_setup` writes through
    // `Config::config_path()`, exercised directly against the core with an
    // isolated config path.

    #[test]
    fn run_non_interactive_setup_writes_through_config_path() {
        crate::config::with_isolated_config_path("setup-non-interactive-configpath", |_fake_dir| {
            let mut config = Config::load().unwrap_or_default();
            let args = SetupArgs {
                non_interactive: true,
                anthropic_key: Some("sk-ant-execute-test".to_string()),
                openai_key: None,
                google_key: None,
                openrouter_key: None,
                ollama_url: None,
                default_model: None,
                claude_code: None,
                claude_code_effort: None,
            };

            run_non_interactive_setup(&mut config, &args, &Config::config_path()).unwrap();

            // Config::load() re-reads via the same (isolated) LEVIATH_CONFIG_PATH,
            // proving the setup core wrote through Config::config_path().
            let reloaded = Config::load().unwrap();
            assert_eq!(
                reloaded.providers.anthropic_api_key.as_deref(),
                Some("sk-ant-execute-test")
            );
        });
    }

    /// Reader factory shared by both `execute_with` tests below: 7 empty lines,
    /// one per interactive prompt (so every value keeps its default). Using one
    /// named `fn` in both call sites gives `execute_with` a single
    /// monomorphization whose two arms are covered across the two tests, and
    /// keeps this factory's body covered (the interactive test calls it).
    fn seven_blank_lines() -> Cursor<Vec<u8>> {
        Cursor::new(b"\n\n\n\n\n\n\n".to_vec())
    }

    #[test]
    fn execute_with_non_interactive_applies_flags_and_never_builds_the_reader() {
        crate::config::with_isolated_config_path(
            "setup-execute-with-noninteractive",
            |_fake_dir| {
                let args = SetupArgs {
                    non_interactive: true,
                    anthropic_key: Some("sk-ant-ew".to_string()),
                    openai_key: None,
                    google_key: None,
                    openrouter_key: None,
                    ollama_url: None,
                    default_model: None,
                    claude_code: None,
                    claude_code_effort: None,
                };
                // `--non-interactive` short-circuits before the reader factory runs.
                execute_with(&args, seven_blank_lines).unwrap();
                assert_eq!(
                    Config::load()
                        .unwrap()
                        .providers
                        .anthropic_api_key
                        .as_deref(),
                    Some("sk-ant-ew")
                );
            },
        );
    }

    #[test]
    fn execute_with_interactive_reads_from_the_injected_factory() {
        crate::config::with_isolated_config_path("setup-execute-with-interactive", |_fake_dir| {
            let args = SetupArgs {
                non_interactive: false,
                anthropic_key: None,
                openai_key: None,
                google_key: None,
                openrouter_key: None,
                ollama_url: None,
                default_model: None,
                claude_code: None,
                claude_code_effort: None,
            };
            execute_with(&args, seven_blank_lines).unwrap();
        });
    }
}
