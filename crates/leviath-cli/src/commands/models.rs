//! `lev models` - Inspect available models and their capabilities.

use clap::{Args, Subcommand};
use leviath_providers::{ModelCapabilities, ModelInfo};

use crate::config::Config;
use super::run::build_provider_registry;

// ─── CLI types ────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct ModelsArgs {
    #[command(subcommand)]
    pub command: ModelsCommand,
}

#[derive(Subcommand)]
pub enum ModelsCommand {
    /// List available models and their capabilities
    List(ListArgs),
    /// Show capabilities for a specific model
    Show(ShowArgs),
}

#[derive(Args)]
pub struct ListArgs {
    /// Filter by provider name (anthropic, openai, ollama, openrouter)
    #[arg(short, long)]
    pub provider: Option<String>,
    /// Fetch live model list from provider APIs (slower but complete)
    #[arg(short = 'r', long)]
    pub remote: bool,
}

#[derive(Args)]
pub struct ShowArgs {
    /// Model ID to look up
    pub model: String,
    /// Provider to query (required for remote lookup)
    #[arg(short, long)]
    pub provider: Option<String>,
    /// Fetch live model list from provider APIs (slower but complete)
    #[arg(short = 'r', long)]
    pub remote: bool,
}

// ─── Entrypoint ───────────────────────────────────────────────────────────────

pub async fn execute(args: ModelsArgs) -> anyhow::Result<()> {
    match args.command {
        ModelsCommand::List(a) => list(a).await,
        ModelsCommand::Show(a) => show(a).await,
    }
}

// ─── Built-in model table ─────────────────────────────────────────────────────

/// A single row in the built-in model table.
struct BuiltinEntry {
    provider: &'static str,
    model_id: &'static str,
    display_name: &'static str,
    caps: ModelCapabilities,
}

/// Hard-coded capability table for well-known models.
///
/// This is used when the provider API is not reachable or `--remote` is not
/// specified.  Remote results override these values for identical model IDs.
fn builtin_table() -> Vec<BuiltinEntry> {
    // Helper closures keep the table compact.
    let claude4 = |model_id, display_name| BuiltinEntry {
        provider: "anthropic",
        model_id,
        display_name,
        caps: ModelCapabilities {
            supports_temperature: false,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 200_000,
            max_output_tokens: 32_768,
        },
    };

    let claude3 = |model_id, display_name| BuiltinEntry {
        provider: "anthropic",
        model_id,
        display_name,
        caps: ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 200_000,
            max_output_tokens: 8_192,
        },
    };

    let gpt4o = |model_id, display_name| BuiltinEntry {
        provider: "openai",
        model_id,
        display_name,
        caps: ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 128_000,
            max_output_tokens: 16_384,
        },
    };

    let o_series = |model_id, display_name| BuiltinEntry {
        provider: "openai",
        model_id,
        display_name,
        caps: ModelCapabilities {
            supports_temperature: false,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 200_000,
            max_output_tokens: 32_768,
        },
    };

    vec![
        // Anthropic — Claude 4.x (no temperature)
        claude4("claude-opus-4-8",              "Claude Opus 4.8"),
        claude4("claude-sonnet-4-6",            "Claude Sonnet 4.6"),
        claude4("claude-haiku-4-5-20251001",    "Claude Haiku 4.5"),
        // Anthropic — Claude 3.x (has temperature)
        claude3("claude-3-5-sonnet-20241022",   "Claude 3.5 Sonnet"),
        claude3("claude-3-5-haiku-20241022",    "Claude 3.5 Haiku"),
        // OpenAI — GPT-4o family (has temperature)
        gpt4o("gpt-4o",      "GPT-4o"),
        gpt4o("gpt-4o-mini", "GPT-4o Mini"),
        // OpenAI — o-series (no temperature)
        o_series("o1",      "o1"),
        o_series("o3-mini", "o3-mini"),
    ]
}

// ─── list ─────────────────────────────────────────────────────────────────────

async fn list(args: ListArgs) -> anyhow::Result<()> {
    let config = Config::load()?;
    for warning in config.validate_keys() {
        eprintln!("Warning: {}", warning);
    }

    // Start with the built-in table, indexed by model_id for easy overriding.
    let mut entries: Vec<ModelInfo> = builtin_table()
        .into_iter()
        .map(|e| ModelInfo {
            id: e.model_id.to_string(),
            display_name: Some(e.display_name.to_string()),
            provider: e.provider.to_string(),
            capabilities: e.caps,
        })
        .collect();

    // --remote: fetch live model lists and merge (remote wins on same ID).
    if args.remote {
        let registry = build_provider_registry(&config);
        for provider_name in registry.provider_names() {
            // If the caller filtered to a specific provider, skip others.
            if let Some(ref filter) = args.provider {
                if filter != provider_name {
                    continue;
                }
            }

            if let Some(provider) = registry.get(provider_name) {
                match provider.list_models().await {
                    Ok(remote_models) => {
                        for rm in remote_models {
                            // Override builtin entry with the same ID, or append.
                            if let Some(existing) = entries.iter_mut().find(|e| e.id == rm.id) {
                                *existing = rm;
                            } else {
                                entries.push(rm);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: could not fetch models from '{}': {}", provider_name, e);
                    }
                }
            }
        }
    }

    // Apply provider filter (after remote merge so we respect the filter).
    if let Some(ref filter) = args.provider {
        entries.retain(|e| &e.provider == filter);
    }

    // Apply user-defined capability overrides from config; track which IDs are overridden.
    let overridden: std::collections::HashSet<String> =
        config.model_capabilities.keys().cloned().collect();

    for entry in entries.iter_mut() {
        if let Some(user_caps) = config.model_capabilities.get(&entry.id) {
            entry.capabilities = user_caps.clone();
        }
    }

    if entries.is_empty() {
        println!("No models found.");
        if args.provider.is_some() {
            println!("(try removing --provider or adding --remote)");
        }
        return Ok(());
    }

    // Print table header.
    println!(
        "{:<12} {:<40} {:<6} {:<7} {:<8} {:<8}",
        "PROVIDER", "MODEL ID", "TEMP", "TOOLS", "CTX", "OUTPUT"
    );
    println!("{}", "-".repeat(85));

    for entry in &entries {
        let provider_col = if overridden.contains(&entry.id) {
            format!("*{}", entry.provider)
        } else {
            entry.provider.clone()
        };

        let temp  = bool_icon(entry.capabilities.supports_temperature);
        let tools = bool_icon(entry.capabilities.supports_tools);
        let ctx   = fmt_tokens(entry.capabilities.max_context_tokens);
        let out   = fmt_tokens(entry.capabilities.max_output_tokens);

        println!(
            "{:<12} {:<40} {:<6} {:<7} {:<8} {:<8}",
            provider_col, entry.id, temp, tools, ctx, out
        );
    }

    if overridden.iter().any(|id| entries.iter().any(|e| &e.id == id)) {
        println!("\n* = capabilities overridden via [model_capabilities] in config");
    }

    Ok(())
}

// ─── show ─────────────────────────────────────────────────────────────────────

async fn show(args: ShowArgs) -> anyhow::Result<()> {
    let config = Config::load()?;
    for warning in config.validate_keys() {
        eprintln!("Warning: {}", warning);
    }

    let model_id = &args.model;

    // 1. Check user overrides first (highest precedence).
    if let Some(user_caps) = config.model_capabilities.get(model_id) {
        print_model_detail(model_id, None, "config (user override)", user_caps, true);
        return Ok(());
    }

    // 2. Check built-in table.
    let builtin = builtin_table();
    if let Some(entry) = builtin.iter().find(|e| e.model_id == model_id) {
        print_model_detail(model_id, Some(entry.display_name), entry.provider, &entry.caps, false);
        return Ok(());
    }

    // 3. Optionally fetch from provider API if --remote and --provider are both given.
    if args.remote {
        if let Some(ref provider_name) = args.provider {
            let registry = build_provider_registry(&config);
            if let Some(provider) = registry.get(provider_name) {
                match provider.list_models().await {
                    Ok(models) => {
                        if let Some(info) = models.iter().find(|m| &m.id == model_id) {
                            print_model_detail(
                                model_id,
                                info.display_name.as_deref(),
                                &info.provider,
                                &info.capabilities,
                                false,
                            );
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: could not fetch models from '{}': {}",
                            provider_name, e
                        );
                    }
                }
            } else {
                eprintln!(
                    "Warning: provider '{}' is not configured (missing API key?)",
                    provider_name
                );
            }
        }
    }

    // 4. Not found anywhere — print a helpful message with a TOML snippet.
    println!("Model '{}' not found.", model_id);
    println!(
        "Add it to {} under [model_capabilities.'{}']",
        Config::config_path().display(),
        model_id
    );
    println!();
    println!("Example:");
    println!("[model_capabilities.'{}']", model_id);
    println!("supports_temperature  = true");
    println!("supports_streaming    = true");
    println!("supports_tools        = true");
    println!("supports_system_prompt = true");
    println!("max_context_tokens    = 8192");
    println!("max_output_tokens     = 4096");

    Ok(())
}

// ─── Display helpers ──────────────────────────────────────────────────────────

fn bool_icon(b: bool) -> &'static str {
    if b { "✓" } else { "✗" }
}

/// Format a raw token count as a human-friendly string (e.g. 200K, 128K, 8K).
fn fmt_tokens(n: usize) -> String {
    if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Print a detailed capability sheet for a single model.
fn print_model_detail(
    id: &str,
    display_name: Option<&str>,
    provider: &str,
    caps: &ModelCapabilities,
    is_user_override: bool,
) {
    println!("Model:    {}", id);
    if let Some(name) = display_name {
        println!("Name:     {}", name);
    }
    println!("Provider: {}", provider);
    if is_user_override {
        println!("Source:   user override (config)");
    }
    println!();
    println!("Capabilities");
    println!("------------");
    println!("  Temperature:    {}", bool_icon(caps.supports_temperature));
    println!("  Streaming:      {}", bool_icon(caps.supports_streaming));
    println!("  Tool calling:   {}", bool_icon(caps.supports_tools));
    println!("  System prompt:  {}", bool_icon(caps.supports_system_prompt));
    println!("  Context window: {} tokens ({})", caps.max_context_tokens, fmt_tokens(caps.max_context_tokens));
    println!("  Max output:     {} tokens ({})", caps.max_output_tokens, fmt_tokens(caps.max_output_tokens));
}
