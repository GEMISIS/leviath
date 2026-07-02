//! `lev models` - Inspect available models and their capabilities.

use clap::{Args, Subcommand};
use leviath_providers::{ModelCapabilities, ModelInfo};

use super::run::build_provider_registry;
use crate::config::Config;

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
        ModelsCommand::List(a) => list_with_registry(a, build_provider_registry).await,
        ModelsCommand::Show(a) => show_with_registry(a, build_provider_registry).await,
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
            entry!(
                $provider,
                $id,
                $name,
                temp = $t,
                tools = true,
                ctx = $ctx,
                out = $out
            )
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
        entry!(
            "anthropic",
            "claude-fable-5",
            "Claude Fable 5",
            temp = false,
            ctx = 1_000_000,
            out = 128_000
        ),
        entry!(
            "anthropic",
            "claude-opus-4-8",
            "Claude Opus 4.8",
            temp = false,
            ctx = 1_000_000,
            out = 128_000
        ),
        entry!(
            "anthropic",
            "claude-opus-4-7",
            "Claude Opus 4.7",
            temp = false,
            ctx = 1_000_000,
            out = 128_000
        ),
        entry!(
            "anthropic",
            "claude-opus-4-6",
            "Claude Opus 4.6",
            temp = true,
            ctx = 1_000_000,
            out = 128_000
        ),
        entry!(
            "anthropic",
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            temp = true,
            ctx = 1_000_000,
            out = 128_000
        ),
        entry!(
            "anthropic",
            "claude-haiku-4-5-20251001",
            "Claude Haiku 4.5",
            temp = true,
            ctx = 200_000,
            out = 65_536
        ),
        // ── OpenAI ─────────────────────────────────────────────────────────────
        // GPT-5.5 — flagship (Apr 2026), 1M+ context
        entry!(
            "openai",
            "gpt-5.5",
            "GPT-5.5",
            temp = true,
            ctx = 1_050_000,
            out = 128_000
        ),
        entry!(
            "openai",
            "gpt-5.4",
            "GPT-5.4",
            temp = true,
            ctx = 1_050_000,
            out = 128_000
        ),
        entry!(
            "openai",
            "gpt-5.4-mini",
            "GPT-5.4 Mini",
            temp = true,
            ctx = 400_000,
            out = 128_000
        ),
        entry!(
            "openai",
            "gpt-5.4-nano",
            "GPT-5.4 Nano",
            temp = true,
            ctx = 400_000,
            out = 128_000
        ),
        // ── OpenRouter: Google Gemini ──────────────────────────────────────────
        entry!(
            "openrouter",
            "google/gemini-3.5-flash",
            "Gemini 3.5 Flash",
            temp = true,
            ctx = 1_048_576,
            out = 65_536
        ),
        entry!(
            "openrouter",
            "google/gemini-2.5-pro",
            "Gemini 2.5 Pro",
            temp = true,
            ctx = 1_048_576,
            out = 65_536
        ),
        entry!(
            "openrouter",
            "google/gemini-2.5-flash",
            "Gemini 2.5 Flash",
            temp = true,
            ctx = 1_048_576,
            out = 65_536
        ),
        entry!(
            "openrouter",
            "google/gemini-2.5-flash-lite",
            "Gemini 2.5 Flash Lite",
            temp = true,
            ctx = 1_048_576,
            out = 65_536
        ),
        // ── OpenRouter: Meta Llama 4 ───────────────────────────────────────────
        entry!(
            "openrouter",
            "meta-llama/llama-4-maverick",
            "Llama 4 Maverick",
            temp = true,
            ctx = 1_048_576,
            out = 32_768
        ),
        entry!(
            "openrouter",
            "meta-llama/llama-4-scout",
            "Llama 4 Scout",
            temp = true,
            ctx = 10_000_000,
            out = 32_768
        ),
        // ── OpenRouter: DeepSeek ───────────────────────────────────────────────
        entry!(
            "openrouter",
            "deepseek/deepseek-v4-pro",
            "DeepSeek V4 Pro",
            temp = true,
            ctx = 1_048_576,
            out = 393_216
        ),
        entry!(
            "openrouter",
            "deepseek/deepseek-v4-flash",
            "DeepSeek V4 Flash",
            temp = true,
            ctx = 1_048_576,
            out = 65_536
        ),
        entry!(
            "openrouter",
            "deepseek/deepseek-v3.2",
            "DeepSeek V3.2",
            temp = true,
            ctx = 131_072,
            out = 65_536
        ),
        entry!(
            "openrouter",
            "deepseek/deepseek-r1-0528",
            "DeepSeek R1 (0528)",
            temp = false,
            tools = false,
            ctx = 163_840,
            out = 32_768
        ),
        entry!(
            "openrouter",
            "deepseek/deepseek-r1",
            "DeepSeek R1",
            temp = false,
            tools = false,
            ctx = 163_840,
            out = 16_384
        ),
        // ── OpenRouter: Mistral ────────────────────────────────────────────────
        entry!(
            "openrouter",
            "mistralai/mistral-large-2512",
            "Mistral Large 3",
            temp = true,
            ctx = 262_144,
            out = 32_768
        ),
        entry!(
            "openrouter",
            "mistralai/mistral-medium-3-5",
            "Mistral Medium 3.5",
            temp = true,
            ctx = 256_000,
            out = 32_768
        ),
        entry!(
            "openrouter",
            "mistralai/mistral-small-2603",
            "Mistral Small 4",
            temp = true,
            ctx = 128_000,
            out = 32_768
        ),
        // ── OpenRouter: Qwen (Alibaba) ─────────────────────────────────────────
        entry!(
            "openrouter",
            "qwen/qwen3.6-plus",
            "Qwen 3.6 Plus",
            temp = true,
            ctx = 1_048_576,
            out = 65_536
        ),
        entry!(
            "openrouter",
            "qwen/qwen3-max",
            "Qwen3 Max",
            temp = true,
            ctx = 131_072,
            out = 32_768
        ),
        entry!(
            "openrouter",
            "qwen/qwen3-coder",
            "Qwen3 Coder 480B",
            temp = true,
            ctx = 1_048_576,
            out = 262_144
        ),
    ]
}

// ─── list ─────────────────────────────────────────────────────────────────────

/// Core of [`list`], with provider-registry construction injected so tests
/// can drive the `--remote` merge/override/error paths with a
/// [`Provider`](leviath_providers::Provider) mock instead of hitting a real
/// network endpoint (ollama) or spawning a real subprocess (claude-code) --
/// both of which [`build_provider_registry`] always registers.
async fn list_with_registry(
    args: ListArgs,
    build_registry: impl FnOnce(&Config) -> leviath_runtime::ProviderRegistry,
) -> anyhow::Result<()> {
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
        let registry = build_registry(&config);
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
                        eprintln!(
                            "Warning: could not fetch models from '{}': {}",
                            provider_name, e
                        );
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

        let temp = bool_icon(entry.capabilities.supports_temperature);
        let tools = bool_icon(entry.capabilities.supports_tools);
        let ctx = fmt_tokens(entry.capabilities.max_context_tokens);
        let out = fmt_tokens(entry.capabilities.max_output_tokens);

        println!(
            "{:<12} {:<40} {:<6} {:<7} {:<8} {:<8}",
            provider_col, entry.id, temp, tools, ctx, out
        );
    }

    if overridden
        .iter()
        .any(|id| entries.iter().any(|e| &e.id == id))
    {
        println!("\n* = capabilities overridden via [model_capabilities] in config");
    }

    Ok(())
}

// ─── show ─────────────────────────────────────────────────────────────────────

/// Core of [`show`], with provider-registry construction injected -- see
/// [`list_with_registry`] for why.
async fn show_with_registry(
    args: ShowArgs,
    build_registry: impl FnOnce(&Config) -> leviath_runtime::ProviderRegistry,
) -> anyhow::Result<()> {
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
        print_model_detail(
            model_id,
            Some(entry.display_name),
            entry.provider,
            &entry.caps,
            false,
        );
        return Ok(());
    }

    // 3. Optionally fetch from provider API if --remote and --provider are both given.
    if args.remote {
        if let Some(ref provider_name) = args.provider {
            let registry = build_registry(&config);
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
    if b {
        "✓"
    } else {
        "✗"
    }
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
    println!(
        "  System prompt:  {}",
        bool_icon(caps.supports_system_prompt)
    );
    println!(
        "  Context window: {} tokens ({})",
        caps.max_context_tokens,
        fmt_tokens(caps.max_context_tokens)
    );
    println!(
        "  Max output:     {} tokens ({})",
        caps.max_output_tokens,
        fmt_tokens(caps.max_output_tokens)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── fmt_tokens ─────────────────────────────────────────────────────────

    #[test]
    fn fmt_tokens_millions() {
        assert_eq!(fmt_tokens(1_000_000), "1M");
        assert_eq!(fmt_tokens(2_000_000), "2M");
    }

    #[test]
    fn fmt_tokens_thousands() {
        assert_eq!(fmt_tokens(128_000), "128K");
        assert_eq!(fmt_tokens(4_096), "4K");
        assert_eq!(fmt_tokens(1_000), "1K");
    }

    #[test]
    fn fmt_tokens_small() {
        assert_eq!(fmt_tokens(512), "512");
        assert_eq!(fmt_tokens(0), "0");
    }

    // ─── bool_icon ──────────────────────────────────────────────────────────

    #[test]
    fn bool_icon_values() {
        assert_eq!(bool_icon(true), "✓");
        assert_eq!(bool_icon(false), "✗");
    }

    // ─── builtin_table ──────────────────────────────────────────────────────

    #[test]
    fn builtin_table_is_not_empty() {
        let table = builtin_table();
        assert!(!table.is_empty());
    }

    #[test]
    fn builtin_table_has_anthropic_models() {
        let table = builtin_table();
        let anthropic: Vec<_> = table.iter().filter(|e| e.provider == "anthropic").collect();
        assert!(!anthropic.is_empty());
    }

    #[test]
    fn builtin_table_has_openai_models() {
        let table = builtin_table();
        let openai: Vec<_> = table.iter().filter(|e| e.provider == "openai").collect();
        assert!(!openai.is_empty());
    }

    #[test]
    fn builtin_table_has_openrouter_models() {
        let table = builtin_table();
        let openrouter: Vec<_> = table
            .iter()
            .filter(|e| e.provider == "openrouter")
            .collect();
        assert!(!openrouter.is_empty());
    }

    #[test]
    fn builtin_entries_have_valid_capabilities() {
        for entry in builtin_table() {
            #[rustfmt::skip]
            assert!(entry.caps.max_context_tokens > 0, "model {} has zero context tokens", entry.model_id);
            #[rustfmt::skip]
            assert!(entry.caps.max_output_tokens > 0, "model {} has zero output tokens", entry.model_id);
            #[rustfmt::skip]
            assert!(entry.caps.supports_streaming, "model {} doesn't support streaming", entry.model_id);
            #[rustfmt::skip]
            assert!(entry.caps.supports_system_prompt, "model {} doesn't support system prompt", entry.model_id);
        }
    }

    #[test]
    fn builtin_entries_have_unique_model_ids() {
        let table = builtin_table();
        let ids: Vec<&str> = table.iter().map(|e| e.model_id).collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        #[rustfmt::skip]
        assert_eq!(ids.len(), unique.len(), "duplicate model IDs in builtin table");
    }

    #[test]
    fn deepseek_r1_models_no_tools() {
        let table = builtin_table();
        for entry in &table {
            if entry.model_id.contains("deepseek-r1") {
                #[rustfmt::skip]
                assert!(!entry.caps.supports_tools, "DeepSeek R1 model {} should not support tools", entry.model_id);
            }
        }
    }

    // ─── print_model_detail ─────────────────────────────────────────────────

    #[test]
    fn print_model_detail_does_not_panic() {
        let caps = ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 100_000,
            max_output_tokens: 8_192,
        };
        // Should not panic
        print_model_detail("test-model", Some("Test Model"), "test", &caps, false);
        print_model_detail("test-model", None, "test", &caps, true);
    }

    // ─── fmt_tokens edge cases ──────────────────────────────────────────────

    #[test]
    fn fmt_tokens_exact_boundary() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(999_999), "999K");
    }

    #[test]
    fn fmt_tokens_large_millions() {
        assert_eq!(fmt_tokens(10_000_000), "10M");
    }

    // ─── builtin_table provider coverage ────────────────────────────────────

    #[test]
    fn builtin_table_claude_opus_no_temperature() {
        let table = builtin_table();
        for entry in &table {
            if entry.model_id == "claude-opus-4-8" || entry.model_id == "claude-opus-4-7" {
                #[rustfmt::skip]
                assert!(!entry.caps.supports_temperature, "{} should not support temperature", entry.model_id);
            }
        }
    }

    #[test]
    fn builtin_table_claude_sonnet_supports_temperature() {
        let table = builtin_table();
        let sonnet = table
            .iter()
            .find(|e| e.model_id == "claude-sonnet-4-6")
            .expect("claude-sonnet-4-6 should be in table");
        assert!(sonnet.caps.supports_temperature);
    }

    #[test]
    fn builtin_table_has_display_names() {
        let table = builtin_table();
        for entry in &table {
            #[rustfmt::skip]
            assert!(!entry.display_name.is_empty(), "model {} has empty display name", entry.model_id);
        }
    }

    #[test]
    fn builtin_table_context_larger_than_output() {
        let table = builtin_table();
        for entry in &table {
            #[rustfmt::skip]
            assert!(entry.caps.max_context_tokens >= entry.caps.max_output_tokens, "model {} has output > context", entry.model_id);
        }
    }

    // ─── bool_icon edge ─────────────────────────────────────────────────────

    #[test]
    fn bool_icon_returns_unicode() {
        assert!(!bool_icon(true).is_empty());
        assert!(!bool_icon(false).is_empty());
        assert_ne!(bool_icon(true), bool_icon(false));
    }

    // ─── builtin_table model coverage ───────────────────────────────────

    #[test]
    fn builtin_table_openai_models_support_temperature() {
        let table = builtin_table();
        for entry in &table {
            if entry.provider == "openai" {
                #[rustfmt::skip]
                assert!(entry.caps.supports_temperature, "OpenAI model {} should support temperature", entry.model_id);
            }
        }
    }

    #[test]
    fn builtin_table_gemini_flash_models_exist() {
        let table = builtin_table();
        let flash: Vec<_> = table
            .iter()
            .filter(|e| e.model_id.contains("gemini") && e.model_id.contains("flash"))
            .collect();
        #[rustfmt::skip]
        assert!(!flash.is_empty(), "Expected at least one Gemini Flash model");
    }

    #[test]
    fn builtin_table_deepseek_r1_no_temperature() {
        let table = builtin_table();
        for entry in &table {
            if entry.model_id.contains("deepseek-r1") {
                #[rustfmt::skip]
                assert!(!entry.caps.supports_temperature, "DeepSeek R1 {} should not support temperature", entry.model_id);
            }
        }
    }

    #[test]
    fn builtin_table_qwen_models_exist() {
        let table = builtin_table();
        let qwen: Vec<_> = table
            .iter()
            .filter(|e| e.model_id.contains("qwen"))
            .collect();
        assert!(!qwen.is_empty(), "Expected Qwen models in table");
    }

    #[test]
    fn builtin_table_mistral_models_exist() {
        let table = builtin_table();
        let mistral: Vec<_> = table
            .iter()
            .filter(|e| e.model_id.contains("mistral"))
            .collect();
        assert!(!mistral.is_empty(), "Expected Mistral models in table");
    }

    #[test]
    fn builtin_table_all_entries_have_provider() {
        let table = builtin_table();
        for entry in &table {
            #[rustfmt::skip]
            assert!(!entry.provider.is_empty(), "model {} has empty provider", entry.model_id);
        }
    }

    #[test]
    fn builtin_table_all_entries_have_model_id() {
        let table = builtin_table();
        for entry in &table {
            #[rustfmt::skip]
            assert!(!entry.model_id.is_empty(), "entry has empty model_id for provider {}", entry.provider);
        }
    }

    // ─── execute() / list() / show() async entry points ──────────────────

    #[tokio::test]
    async fn execute_list_command_runs_without_error() {
        let args = ModelsArgs {
            command: ModelsCommand::List(ListArgs {
                provider: None,
                remote: false,
            }),
        };
        // Should succeed: prints the builtin table
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_list_with_provider_filter_runs_without_error() {
        let args = ModelsArgs {
            command: ModelsCommand::List(ListArgs {
                provider: Some("anthropic".to_string()),
                remote: false,
            }),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_list_with_nonexistent_provider_filter() {
        let args = ModelsArgs {
            command: ModelsCommand::List(ListArgs {
                provider: Some("nonexistent_provider".to_string()),
                remote: false,
            }),
        };
        // Should succeed but print "No models found."
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_show_known_model_runs_without_error() {
        let args = ModelsArgs {
            command: ModelsCommand::Show(ShowArgs {
                model: "claude-sonnet-4-6".to_string(),
                provider: None,
                remote: false,
            }),
        };
        // Should find model in builtin table and print details
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_show_unknown_model_runs_without_error() {
        let args = ModelsArgs {
            command: ModelsCommand::Show(ShowArgs {
                model: "totally-unknown-model-xyz".to_string(),
                provider: None,
                remote: false,
            }),
        };
        // Should print "Model not found" message without error
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_show_unknown_model_with_remote_no_provider() {
        let args = ModelsArgs {
            command: ModelsCommand::Show(ShowArgs {
                model: "totally-unknown-model-xyz".to_string(),
                provider: None,
                remote: true, // remote but no provider = skips remote lookup
            }),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_show_unknown_model_with_remote_unconfigured_provider() {
        let args = ModelsArgs {
            command: ModelsCommand::Show(ShowArgs {
                model: "totally-unknown-model-xyz".to_string(),
                provider: Some("anthropic".to_string()),
                remote: true,
                // Provider won't be configured in test env (no API key)
            }),
        };
        // Should warn about unconfigured provider and then show not-found message
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    // ─── list() with builtin model having overrides in config ─────────────

    #[tokio::test]
    async fn list_with_openrouter_filter() {
        let args = ModelsArgs {
            command: ModelsCommand::List(ListArgs {
                provider: Some("openrouter".to_string()),
                remote: false,
            }),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn list_with_openai_filter() {
        let args = ModelsArgs {
            command: ModelsCommand::List(ListArgs {
                provider: Some("openai".to_string()),
                remote: false,
            }),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_builtin_anthropic_opus() {
        let args = ModelsArgs {
            command: ModelsCommand::Show(ShowArgs {
                model: "claude-opus-4-6".to_string(),
                provider: None,
                remote: false,
            }),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_builtin_openai_model() {
        let args = ModelsArgs {
            command: ModelsCommand::Show(ShowArgs {
                model: "gpt-5.5".to_string(),
                provider: None,
                remote: false,
            }),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_builtin_deepseek_r1() {
        let args = ModelsArgs {
            command: ModelsCommand::Show(ShowArgs {
                model: "deepseek/deepseek-r1".to_string(),
                provider: None,
                remote: false,
            }),
        };
        let result = execute(args).await;
        assert!(result.is_ok());
    }

    // ─── builtin_table as ModelInfo conversion ──────────────────────────

    #[test]
    fn builtin_table_to_model_info_preserves_data() {
        let table = builtin_table();
        let infos: Vec<ModelInfo> = table
            .into_iter()
            .map(|e| ModelInfo {
                id: e.model_id.to_string(),
                display_name: Some(e.display_name.to_string()),
                provider: e.provider.to_string(),
                capabilities: e.caps,
            })
            .collect();

        assert!(!infos.is_empty());
        for info in &infos {
            assert!(!info.id.is_empty());
            assert!(info.display_name.is_some());
            assert!(!info.provider.is_empty());
        }
    }

    // ─── print_model_detail coverage ────────────────────────────────────

    #[test]
    fn print_model_detail_with_no_tools_no_temp() {
        let caps = ModelCapabilities {
            supports_temperature: false,
            supports_streaming: false,
            supports_tools: false,
            supports_system_prompt: false,
            max_context_tokens: 1000,
            max_output_tokens: 500,
        };
        // Should not panic with all features disabled
        print_model_detail("test-model", Some("Test"), "test", &caps, false);
    }

    #[test]
    fn print_model_detail_user_override_source() {
        let caps = ModelCapabilities::default();
        // Should not panic with user override flag set
        print_model_detail("override-model", None, "custom", &caps, true);
    }

    // ─── fmt_tokens additional ──────────────────────────────────────────

    #[test]
    fn fmt_tokens_just_below_thousand() {
        assert_eq!(fmt_tokens(999), "999");
    }

    #[test]
    fn fmt_tokens_just_at_thousand() {
        assert_eq!(fmt_tokens(1000), "1K");
    }

    #[test]
    fn fmt_tokens_just_below_million() {
        assert_eq!(fmt_tokens(999_999), "999K");
    }

    #[test]
    fn fmt_tokens_just_at_million() {
        assert_eq!(fmt_tokens(1_000_000), "1M");
    }

    #[test]
    fn fmt_tokens_non_round_thousands() {
        // Integer division: 1500 / 1000 = 1
        assert_eq!(fmt_tokens(1500), "1K");
        assert_eq!(fmt_tokens(65_536), "65K");
    }

    // ─── list() / show() non-remote paths ──────────────────────────────
    //
    // Config::load() gracefully falls back to defaults when
    // ~/.leviath/config.toml doesn't exist, so these are safe to call
    // directly without touching the real environment. `list`/`show` are thin
    // wrappers around `list_with_registry`/`show_with_registry` (see below
    // for the --remote-path tests using a mock registry).

    #[tokio::test]
    async fn list_builtin_no_filter_succeeds() {
        let args = ListArgs {
            remote: false,
            provider: None,
        };
        let result = list_with_registry(args, build_provider_registry).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn list_builtin_with_provider_filter_succeeds() {
        let args = ListArgs {
            remote: false,
            provider: Some("anthropic".to_string()),
        };
        let result = list_with_registry(args, build_provider_registry).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn list_unknown_provider_filter_finds_nothing() {
        let args = ListArgs {
            remote: false,
            provider: Some("no-such-provider".to_string()),
        };
        // Should print "No models found." and still succeed, not error.
        let result = list_with_registry(args, build_provider_registry).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_builtin_model_succeeds() {
        // Use a model ID guaranteed to be in the builtin table.
        let known_id = builtin_table()[0].model_id.to_string();
        let args = ShowArgs {
            model: known_id,
            remote: false,
            provider: None,
        };
        let result = show_with_registry(args, build_provider_registry).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_unknown_model_without_remote_succeeds_with_warning() {
        let args = ShowArgs {
            model: "totally-unknown-model-xyz".to_string(),
            remote: false,
            provider: None,
        };
        // Falls through all lookup tiers; must not error even when not found.
        let result = show_with_registry(args, build_provider_registry).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_remote_without_provider_falls_through_gracefully() {
        // args.remote = true but no --provider given -> the remote-fetch
        // branch's inner `if let Some(ref provider_name)` is skipped.
        let args = ShowArgs {
            model: "totally-unknown-model-xyz".to_string(),
            remote: true,
            provider: None,
        };
        let result = show_with_registry(args, build_provider_registry).await;
        assert!(result.is_ok());
    }

    // ─── list()/show() --remote paths, with a mock provider ────────────────
    //
    // `build_provider_registry` always registers real `ollama`/`claude-code`
    // providers regardless of config, so these can't safely be exercised via
    // the real registry (a real network call to localhost:11434, or spawning
    // a real `claude` subprocess). `list_with_registry`/`show_with_registry`
    // take an injectable registry builder for exactly this reason: tests
    // register a `MockProvider` under a name of their choosing and filter to
    // just that provider via `--provider`, so no real ollama/claude-code
    // provider is ever touched.

    struct MockProvider {
        models: Vec<ModelInfo>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl leviath_providers::Provider for MockProvider {
        async fn infer(
            &self,
            _request: leviath_providers::InferenceRequest,
        ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
        {
            unreachable!("models command never calls infer()")
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, leviath_providers::ProviderError> {
            if self.fail {
                Err(leviath_providers::ProviderError::Other(
                    "mock provider failure".to_string(),
                ))
            } else {
                Ok(self.models.clone())
            }
        }
    }

    fn mock_registry(
        provider_name: &'static str,
        models: Vec<ModelInfo>,
        fail: bool,
    ) -> impl FnOnce(&Config) -> leviath_runtime::ProviderRegistry {
        move |_config: &Config| {
            let mut registry = leviath_runtime::ProviderRegistry::new();
            registry.register(
                provider_name.to_string(),
                std::sync::Arc::new(MockProvider { models, fail }),
            );
            registry
        }
    }

    #[tokio::test]
    async fn list_remote_merges_new_model_from_provider() {
        let args = ListArgs {
            remote: true,
            provider: Some("mock".to_string()),
        };
        let new_model = ModelInfo {
            id: "mock-brand-new-model".to_string(),
            display_name: Some("Mock Brand New Model".to_string()),
            provider: "mock".to_string(),
            capabilities: ModelCapabilities::default(),
        };
        let result = list_with_registry(args, mock_registry("mock", vec![new_model], false)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn list_remote_overrides_builtin_entry_with_same_id() {
        let known_id = builtin_table()[0].model_id.to_string();
        let args = ListArgs {
            remote: true,
            provider: Some("mock".to_string()),
        };
        let overriding_model = ModelInfo {
            id: known_id,
            display_name: Some("Overridden".to_string()),
            provider: "mock".to_string(),
            capabilities: ModelCapabilities::default(),
        };
        let result =
            list_with_registry(args, mock_registry("mock", vec![overriding_model], false)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn list_remote_provider_error_warns_and_continues() {
        let args = ListArgs {
            remote: true,
            provider: Some("mock".to_string()),
        };
        let result = list_with_registry(args, mock_registry("mock", vec![], true)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn list_remote_skips_providers_not_matching_filter() {
        // provider filter is "mock-other", but the registry only has "mock"
        // registered -> the `if filter != provider_name { continue; }`
        // branch is exercised, and the mock is never queried.
        let args = ListArgs {
            remote: true,
            provider: Some("mock-other".to_string()),
        };
        let result = list_with_registry(args, mock_registry("mock", vec![], false)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_remote_finds_model_from_provider() {
        let args = ShowArgs {
            model: "mock-remote-model".to_string(),
            remote: true,
            provider: Some("mock".to_string()),
        };
        let remote_model = ModelInfo {
            id: "mock-remote-model".to_string(),
            display_name: Some("Mock Remote Model".to_string()),
            provider: "mock".to_string(),
            capabilities: ModelCapabilities::default(),
        };
        let result =
            show_with_registry(args, mock_registry("mock", vec![remote_model], false)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_remote_model_not_found_in_provider_list_falls_through() {
        let args = ShowArgs {
            model: "totally-unknown-model-xyz".to_string(),
            remote: true,
            provider: Some("mock".to_string()),
        };
        let result = show_with_registry(args, mock_registry("mock", vec![], false)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_remote_provider_error_warns_and_falls_through() {
        let args = ShowArgs {
            model: "totally-unknown-model-xyz".to_string(),
            remote: true,
            provider: Some("mock".to_string()),
        };
        let result = show_with_registry(args, mock_registry("mock", vec![], true)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_remote_unconfigured_provider_warns_and_falls_through() {
        // provider filter names a provider that isn't in the registry at all
        // -> the `if let Some(provider) = registry.get(...)` else branch.
        let args = ShowArgs {
            model: "totally-unknown-model-xyz".to_string(),
            remote: true,
            provider: Some("nonexistent-provider".to_string()),
        };
        let result = show_with_registry(args, mock_registry("mock", vec![], false)).await;
        assert!(result.is_ok());
    }

    // ─── validate_keys() warnings + [model_capabilities] overrides ─────────
    //
    // `list_with_registry`/`show_with_registry` take an injectable registry
    // builder, so a malformed API key in the isolated test config can safely
    // exercise the `validate_keys()` warning-print branch without the
    // registry ever actually using that key (the mock registry below ignores
    // `_config` entirely).

    #[tokio::test]
    async fn list_prints_warning_and_applies_model_capabilities_override() {
        let _guard = crate::config::isolate_config_path_for_test("models-list-override");
        let known_id = builtin_table()[0].model_id.to_string();
        let mut fake_config = Config::default();
        fake_config.providers.anthropic_api_key = Some("not-a-real-key".to_string());
        fake_config.model_capabilities.insert(
            known_id,
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 1,
                max_output_tokens: 1,
            },
        );
        std::fs::write(
            Config::config_path(),
            toml::to_string(&fake_config).unwrap(),
        )
        .unwrap();

        let args = ListArgs {
            remote: false,
            provider: None,
        };
        let result = list_with_registry(args, mock_registry("mock", vec![], false)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn show_prints_warning_and_uses_model_capabilities_override() {
        let _guard = crate::config::isolate_config_path_for_test("models-show-override");
        let known_id = builtin_table()[0].model_id.to_string();
        let mut fake_config = Config::default();
        fake_config.providers.anthropic_api_key = Some("not-a-real-key".to_string());
        fake_config.model_capabilities.insert(
            known_id.clone(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 1,
                max_output_tokens: 1,
            },
        );
        std::fs::write(
            Config::config_path(),
            toml::to_string(&fake_config).unwrap(),
        )
        .unwrap();

        let args = ShowArgs {
            model: known_id,
            remote: false,
            provider: None,
        };
        let result = show_with_registry(args, mock_registry("mock", vec![], false)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn mock_provider_trivial_trait_methods() {
        // Covers the `Provider` trait methods that `list`/`show` never call
        // through this file's code paths (only `list_models()` is exercised
        // above); `infer()` is intentionally left unreachable (see its body).
        use leviath_providers::Provider;
        let provider = MockProvider {
            models: vec![],
            fail: false,
        };
        assert_eq!(provider.count_tokens("abcd", "mock-model"), 1);
        assert_eq!(provider.max_context_tokens("mock-model"), 100_000);
        assert_eq!(provider.name(), "mock");
        let _ = provider.capabilities("mock-model");
    }
}
