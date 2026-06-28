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
    macro_rules! entry {
        // Short form — tools defaults to true
        ($provider:expr, $id:expr, $name:expr,
         temp=$t:expr, ctx=$ctx:expr, out=$out:expr) => {
            entry!($provider, $id, $name, temp=$t, tools=true, ctx=$ctx, out=$out)
        };
        // Full form — explicit tools flag
        ($provider:expr, $id:expr, $name:expr,
         temp=$t:expr, tools=$to:expr, ctx=$ctx:expr, out=$out:expr) => {
            BuiltinEntry {
                provider: $provider,
                model_id: $id,
                display_name: $name,
                caps: ModelCapabilities {
                    supports_temperature: $t,
                    supports_streaming: true,
                    supports_tools: $to,
                    supports_system_prompt: true,
                    max_context_tokens: $ctx,
                    max_output_tokens: $out,
                },
            }
        };
    }

    vec![
        // ── Anthropic ──────────────────────────────────────────────────────────
        entry!("anthropic", "claude-fable-5",            "Claude Fable 5",
               temp=false, ctx=1_000_000, out=128_000),
        entry!("anthropic", "claude-opus-4-8",           "Claude Opus 4.8",
               temp=false, ctx=1_000_000, out=128_000),
        entry!("anthropic", "claude-opus-4-7",           "Claude Opus 4.7",
               temp=false, ctx=1_000_000, out=128_000),
        entry!("anthropic", "claude-opus-4-6",           "Claude Opus 4.6",
               temp=true,  ctx=1_000_000, out=128_000),
        entry!("anthropic", "claude-sonnet-4-6",         "Claude Sonnet 4.6",
               temp=true,  ctx=1_000_000, out=128_000),
        entry!("anthropic", "claude-haiku-4-5-20251001", "Claude Haiku 4.5",
               temp=true,  ctx=200_000,   out=65_536),

        // ── OpenAI ─────────────────────────────────────────────────────────────
        // GPT-5.5 — flagship (Apr 2026), 1M+ context
        entry!("openai", "gpt-5.5",      "GPT-5.5",
               temp=true, ctx=1_050_000, out=128_000),
        entry!("openai", "gpt-5.4",      "GPT-5.4",
               temp=true, ctx=1_050_000, out=128_000),
        entry!("openai", "gpt-5.4-mini", "GPT-5.4 Mini",
               temp=true, ctx=400_000,   out=128_000),
        entry!("openai", "gpt-5.4-nano", "GPT-5.4 Nano",
               temp=true, ctx=400_000,   out=128_000),

        // ── OpenRouter: Google Gemini ──────────────────────────────────────────
        entry!("openrouter", "google/gemini-3.5-flash",      "Gemini 3.5 Flash",
               temp=true, ctx=1_048_576, out=65_536),
        entry!("openrouter", "google/gemini-2.5-pro",        "Gemini 2.5 Pro",
               temp=true, ctx=1_048_576, out=65_536),
        entry!("openrouter", "google/gemini-2.5-flash",      "Gemini 2.5 Flash",
               temp=true, ctx=1_048_576, out=65_536),
        entry!("openrouter", "google/gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite",
               temp=true, ctx=1_048_576, out=65_536),

        // ── OpenRouter: Meta Llama 4 ───────────────────────────────────────────
        entry!("openrouter", "meta-llama/llama-4-maverick", "Llama 4 Maverick",
               temp=true, ctx=1_048_576,  out=32_768),
        entry!("openrouter", "meta-llama/llama-4-scout",    "Llama 4 Scout",
               temp=true, ctx=10_000_000, out=32_768),

        // ── OpenRouter: DeepSeek ───────────────────────────────────────────────
        entry!("openrouter", "deepseek/deepseek-v4-pro",   "DeepSeek V4 Pro",
               temp=true, ctx=1_048_576, out=393_216),
        entry!("openrouter", "deepseek/deepseek-v4-flash",  "DeepSeek V4 Flash",
               temp=true, ctx=1_048_576, out=65_536),
        entry!("openrouter", "deepseek/deepseek-v3.2",      "DeepSeek V3.2",
               temp=true, ctx=131_072,   out=65_536),
        entry!("openrouter", "deepseek/deepseek-r1-0528",   "DeepSeek R1 (0528)",
               temp=false, tools=false,  ctx=163_840, out=32_768),
        entry!("openrouter", "deepseek/deepseek-r1",        "DeepSeek R1",
               temp=false, tools=false,  ctx=163_840, out=16_384),

        // ── OpenRouter: Mistral ────────────────────────────────────────────────
        entry!("openrouter", "mistralai/mistral-large-2512", "Mistral Large 3",
               temp=true, ctx=262_144, out=32_768),
        entry!("openrouter", "mistralai/mistral-medium-3-5", "Mistral Medium 3.5",
               temp=true, ctx=256_000, out=32_768),
        entry!("openrouter", "mistralai/mistral-small-2603", "Mistral Small 4",
               temp=true, ctx=128_000, out=32_768),

        // ── OpenRouter: Qwen (Alibaba) ─────────────────────────────────────────
        entry!("openrouter", "qwen/qwen3.6-plus",  "Qwen 3.6 Plus",
               temp=true, ctx=1_048_576, out=65_536),
        entry!("openrouter", "qwen/qwen3-max",     "Qwen3 Max",
               temp=true, ctx=131_072,   out=32_768),
        entry!("openrouter", "qwen/qwen3-coder",   "Qwen3 Coder 480B",
               temp=true, ctx=1_048_576, out=262_144),
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

/// Format a raw token count as a human-friendly string (e.g. 1M, 200K, 128K, 8K).
fn fmt_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
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
