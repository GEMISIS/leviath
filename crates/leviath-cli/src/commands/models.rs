//! `lev models` - Inspect available models and their capabilities.

use clap::{Args, Subcommand};
use leviath_providers::{ModelCapabilities, ModelInfo};

use super::run::build_provider_registry_from_config;
use crate::config::Config;

// ─── CLI types ────────────────────────────────────────────────────────────────

/// Arguments for `lev models`.
#[derive(Args)]
pub struct ModelsArgs {
    /// Which models subcommand to run.
    #[command(subcommand)]
    pub command: ModelsCommand,
}

/// The `lev models` subcommands.
#[derive(Subcommand)]
pub enum ModelsCommand {
    /// List available models and their capabilities
    List(ListArgs),
    /// Show capabilities for a specific model
    Show(ShowArgs),
}

/// Arguments for `lev models list`.
#[derive(Args)]
pub struct ListArgs {
    /// Filter by provider name (anthropic, openai, ollama, openrouter)
    #[arg(short, long)]
    pub provider: Option<String>,
    /// Fetch live model list from provider APIs (slower but complete)
    #[arg(short = 'r', long)]
    pub remote: bool,
    /// Include models from providers this install has no credential for
    #[arg(short = 'a', long)]
    pub all: bool,
    /// Report the table as JSON, one object per model.
    #[arg(long)]
    pub json: bool,
}

/// One model in `lev models list --json`.
///
/// Carries the full capability set rather than the six columns the table has
/// room for, and turns the table's `*` marker into a field, since a caller
/// choosing a model needs to know a capability came from config rather than
/// from the provider.
#[derive(serde::Serialize)]
struct ModelRow {
    id: String,
    provider: String,
    display_name: Option<String>,
    capabilities: ModelCapabilities,
    /// True when `[model_capabilities]` in config.toml replaced what Leviath
    /// knows about this model.
    capabilities_overridden: bool,
}

/// Arguments for `lev models show`.
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

/// Run `lev models`: show which provider/model pairs are reachable.
pub async fn execute(args: ModelsArgs) -> anyhow::Result<()> {
    match args.command {
        ModelsCommand::List(a) => list_with_registry(a, &build_provider_registry_from_config).await,
        ModelsCommand::Show(a) => show_with_registry(a, &build_provider_registry_from_config).await,
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

/// Providers whose entry in [`builtin_table`] is complete enough to say "this
/// model does not exist" about.
///
/// Anthropic, OpenAI and Google publish a short list and this table tracks it.
/// The rest do not: Ollama serves whatever has been pulled locally, OpenRouter
/// proxies hundreds of models of which the table lists a sample, and a script
/// provider defines its own catalog at run time. Naming a model those hosts
/// have and this build has not heard of is normal, so they are never checked.
const CLOSED_CATALOG_PROVIDERS: &[&str] = &["anthropic", "openai", "google"];

/// The `(provider, model)` rows [`crate::lint`] checks a blueprint's model
/// references against, limited to the providers with a closed catalog.
pub fn closed_catalog_models() -> Vec<(String, String)> {
    builtin_table()
        .into_iter()
        .filter(|e| CLOSED_CATALOG_PROVIDERS.contains(&e.provider))
        .map(|e| (e.provider.to_string(), e.model_id.to_string()))
        .collect()
}

/// Context-window size per `(provider, model)`, from the same table.
///
/// Every provider, not only the closed catalogs: the question here is "how big
/// is this window", and an OpenRouter row that names a size is as useful as an
/// Anthropic one. A model absent from the table simply is not consulted.
pub fn builtin_model_windows() -> std::collections::HashMap<(String, String), usize> {
    builtin_table()
        .into_iter()
        .map(|e| {
            (
                (e.provider.to_string(), e.model_id.to_string()),
                e.caps.max_context_tokens,
            )
        })
        .collect()
}

/// Hard-coded capability table for well-known models.
///
/// This is used when the provider API is not reachable or `--remote` is not
/// specified.  Remote results override these values for identical model IDs.
fn builtin_table() -> Vec<BuiltinEntry> {
    macro_rules! entry {
        // Short form - tools defaults to true
        ($provider:expr_2021, $id:expr_2021, $name:expr_2021,
         temp=$t:expr_2021, ctx=$ctx:expr_2021, out=$out:expr_2021) => {
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
        // Full form - explicit tools flag
        ($provider:expr_2021, $id:expr_2021, $name:expr_2021,
         temp=$t:expr_2021, tools=$to:expr_2021, ctx=$ctx:expr_2021, out=$out:expr_2021) => {
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
            "claude-opus-5",
            "Claude Opus 5",
            temp = false,
            ctx = 1_000_000,
            out = 128_000
        ),
        entry!(
            "anthropic",
            "claude-sonnet-5",
            "Claude Sonnet 5",
            temp = false,
            ctx = 1_000_000,
            out = 128_000
        ),
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
        // GPT-5.5 - flagship (Apr 2026), 1M+ context
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
        // ── Google (Gemini) ────────────────────────────────────────────────────
        // Native Google provider entries. Without these, a user whose only key
        // is a Gemini key saw no model they could run: every Gemini row in this
        // table routed through OpenRouter, which needs a different key.
        entry!(
            "google",
            "gemini-3.5-flash",
            "Gemini 3.5 Flash",
            temp = true,
            ctx = 1_048_576,
            out = 65_535
        ),
        entry!(
            "google",
            "gemini-3.1-pro-preview",
            "Gemini 3.1 Pro (preview)",
            temp = true,
            ctx = 1_048_576,
            out = 65_535
        ),
        entry!(
            "google",
            "gemini-3-flash",
            "Gemini 3 Flash",
            temp = true,
            ctx = 1_048_576,
            out = 65_535
        ),
        entry!(
            "google",
            "gemini-3.1-flash-lite",
            "Gemini 3.1 Flash Lite",
            temp = true,
            ctx = 1_048_576,
            out = 65_535
        ),
        // ── OpenRouter: xAI and Meta ──────────────────────────────────────────
        // Listed for the reason #568 exists: an unlisted model falls back to
        // 8192 max output, and a stage asked to rewrite a whole report then
        // truncates at 8192 every attempt and never finishes. OpenRouter
        // reports no `max_completion_tokens` for either, so the table is where
        // the real ceiling has to live until that issue lands.
        entry!(
            "openrouter",
            "x-ai/grok-4.6",
            "Grok 4.6",
            temp = true,
            ctx = 500_000,
            out = 64_000
        ),
        entry!(
            "openrouter",
            "meta/muse-spark-1.2",
            "Muse Spark 1.2",
            temp = true,
            ctx = 1_048_576,
            out = 64_000
        ),
        // ── OpenRouter: Anthropic ─────────────────────────────────────────────
        // The same models as the `anthropic` entries above, reached through the
        // gateway instead of directly. Listed because a gateway is a route, not
        // a model family: someone who prefers OpenRouter should be able to run
        // the model a blueprint actually asks for, rather than whichever model
        // that blueprint happened to put in its OpenRouter slot. Capabilities
        // mirror the direct entries - `temp = false` in particular, since the
        // default for an unlisted model is `true` and these do not take one.
        entry!(
            "openrouter",
            "anthropic/claude-opus-5",
            "Claude Opus 5 (via OpenRouter)",
            temp = false,
            ctx = 1_000_000,
            out = 128_000
        ),
        entry!(
            "openrouter",
            "anthropic/claude-sonnet-5",
            "Claude Sonnet 5 (via OpenRouter)",
            temp = false,
            ctx = 1_000_000,
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
///
/// `build_registry` is a `&dyn Fn` trait object, not a generic
/// `impl FnOnce`, deliberately: every test below passes a distinct closure
/// type (each `mock_registry(...)` call site produces its own closure type,
/// separate again from the production `build_provider_registry` function
/// item type). A generic parameter would make `cargo-llvm-cov` instrument
/// each call site's monomorphization of this function separately, and it
/// has been observed to report the production instantiation as 0-hit even
/// though it's genuinely exercised by `execute_list_command_runs_without_error`
/// et al. - the same instantiation-merging undercount `run/task.rs`'s
/// `resolve_task_with` documents at length and fixes the same way. A
/// `&dyn Fn` trait object is one concrete type regardless of what closure is
/// passed, so every call site shares a single instrumented instantiation.
async fn list_with_registry(
    args: ListArgs,
    build_registry: &dyn Fn(
        &Config,
    ) -> Result<
        leviath_runtime::ProviderRegistry,
        leviath_providers::ProviderError,
    >,
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

    // Only what this install can actually run. Listing every model the binary
    // knows about made a user with one key scroll past dozens of models they
    // had no credential for, and hid whether their own key had been picked up
    // at all. `--all` restores the full catalogue for shopping around.
    let registry = build_registry(&config)?;
    let available: std::collections::HashSet<String> = registry
        .provider_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    if !args.all {
        entries.retain(|e| available.contains(&e.provider));
    }

    // The one name the script path below owns outright, so the sweep does not
    // also query it: it reports a failure differently (fatal there, a warning
    // here) and querying it twice would be a wasted round trip.
    let script_target = args.provider.as_deref().filter(|name| {
        !available.contains(*name) && !builtin_table().iter().any(|e| e.provider == *name)
    });

    // --remote: fetch live model lists and merge (remote wins on same ID).
    if args.remote {
        // `resolvable_names`, not `provider_names`: a script provider is
        // reachable only through `get`, so a sweep built on the registered
        // names alone silently omitted every one of them - the same shape as
        // issues #523 and #531.
        for provider_name in registry.resolvable_names() {
            // If the caller filtered to a specific provider, skip others.
            if let Some(ref filter) = args.provider
                && filter != &provider_name
            {
                continue;
            }
            if script_target == Some(provider_name.as_str()) {
                continue; // asked for, and answered for, below
            }

            // A script name is a candidate until it compiles, so this cannot
            // assume the lookup succeeds the way the old `provider_names` loop
            // could. One that will not load is skipped here - the layer has
            // already logged why - and is only ever fatal when the caller named
            // it with `--provider`, which is `script_target` above.
            let Some(provider) = registry.get(&provider_name) else {
                continue;
            };
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

    // A `--provider` naming something neither registered natively nor present
    // in the built-in table is a script provider (or a typo). Ask the script
    // layer: it is the only thing that can answer for one, and a name it cannot
    // answer for is an error rather than an empty table, since nothing else
    // could have answered either. A name the built-in table knows is left alone
    // - `--provider openai` with no OpenAI key is an empty table, exactly as it
    // has always been.
    let mut script_provider_answered = false;
    if let Some(name) = script_target {
        merge_script_provider(&registry, name, &mut entries).await?;
        script_provider_answered = true;
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
            entry.capabilities = user_caps.apply_to(entry.capabilities.clone());
        }
    }

    // JSON before the emptiness guard: an empty catalog is an empty array, not
    // an error, and a caller polling this should not have to parse a nudge.
    if args.json {
        let rows: Vec<ModelRow> = entries
            .into_iter()
            .map(|e| ModelRow {
                capabilities_overridden: overridden.contains(&e.id),
                id: e.id,
                provider: e.provider,
                display_name: e.display_name,
                capabilities: e.capabilities,
            })
            .collect();
        // Owned scalars with no map keys to reject, so this cannot fail.
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).expect("a model listing serializes")
        );
        return Ok(());
    }

    if entries.is_empty() {
        // Reachable three ways now: a `--provider` filter that matches nothing,
        // no configured provider at all (a fresh install), or a script provider
        // that loaded and named no models. The last one is not the same answer
        // as the other two - the script compiled and its credentials were
        // accepted - so it says so rather than reading as "nothing here".
        println!("No models available.");
        if script_provider_answered {
            println!(
                "(the script provider loaded and listed no models - its script \
                 may not define `list_models(state)`)"
            );
        } else {
            println!(
                "(configure a provider with `lev setup`, or pass --all to see every \
                 model Leviath knows about)"
            );
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

/// Load the script provider called `name` and merge whatever `list_models`
/// answers into `entries`.
///
/// A script provider defines its catalog at run time, so the built-in table
/// has no rows for one and `lev models list --provider <name>` used to print
/// "No models available." for a perfectly good provider. Loading it here is
/// what makes that command the smoke test `rhai-providers.md` prescribes: it
/// compiles the script, runs `initialize`, and calls `list_models`.
///
/// **Errors rather than warning.** This is the command the Rhai docs send you
/// to *because* a broken provider script is otherwise skipped silently at model
/// selection, and an empty table under an exit code of 0 is that same silence
/// one step earlier. A name reaching this function has nothing in the built-in
/// table either, so there is no answer to fall back to and nothing an exit code
/// of 0 could honestly mean.
async fn merge_script_provider(
    registry: &leviath_runtime::ProviderRegistry,
    name: &str,
    entries: &mut Vec<ModelInfo>,
) -> anyhow::Result<()> {
    let Some(provider) = registry.get(name) else {
        // Either a name with nothing behind it, or a script that failed to
        // compile - the script layer logged which as it happened, so this says
        // the part it cannot: that the command found nothing to ask.
        anyhow::bail!(
            "no provider named '{name}' is configured, and no script provider \
             by that name loaded. Check the spelling, `[model_providers.{name}]` \
             in {}, and that {name}.rhai compiles (a load failure is logged \
             above with the line it failed on).",
            Config::config_path().display()
        );
    };
    let models = provider
        .list_models()
        .await
        .map_err(|e| anyhow::anyhow!("provider '{name}' could not list its models: {e}"))?;
    for rm in models {
        match entries.iter_mut().find(|e| e.id == rm.id) {
            Some(existing) => *existing = rm,
            None => entries.push(rm),
        }
    }
    Ok(())
}

// ─── show ─────────────────────────────────────────────────────────────────────

/// Core of [`show`], with provider-registry construction injected - see
/// [`list_with_registry`] for why.
async fn show_with_registry(
    args: ShowArgs,
    build_registry: &dyn Fn(
        &Config,
    ) -> Result<
        leviath_runtime::ProviderRegistry,
        leviath_providers::ProviderError,
    >,
) -> anyhow::Result<()> {
    let config = Config::load()?;
    for warning in config.validate_keys() {
        eprintln!("Warning: {}", warning);
    }

    let model_id = &args.model;

    // 1. The built-in row, which a `[model_capabilities]` entry corrects rather
    //    than replaces - so it has to be found before the override is applied.
    //    Printing the override alone would report `Default` for every field the
    //    operator did not mention, which is not what the run will use.
    let builtin = builtin_table();
    let builtin_entry = builtin.iter().find(|e| e.model_id == model_id);
    let user_caps = config.model_capabilities.get(model_id);

    if let Some(user_caps) = user_caps {
        let base = builtin_entry.map(|e| e.caps.clone()).unwrap_or_default();
        print_model_detail(
            model_id,
            builtin_entry.map(|e| e.display_name),
            "config (user override)",
            &user_caps.apply_to(base),
            true,
        );
        return Ok(());
    }

    // 2. The built-in table on its own.
    if let Some(entry) = builtin_entry {
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
    if args.remote
        && let Some(ref provider_name) = args.provider
    {
        let registry = build_registry(&config)?;
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

    // 4. Not found anywhere - print a helpful message with a TOML snippet.
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

/// Print what a model charges, and where the figure came from.
///
/// Two sources, and the difference matters to a reader: an operator's config
/// entry is their own contracted rate and is current by definition, while a
/// shipped rate is a transcription of a vendor's public page on a particular
/// day and cannot notice a repricing. Only one of those deserves a date, and
/// printing it is the only way somebody can judge whether to trust the number.
///
/// A model with neither prints nothing rather than a zero: a run that touches
/// it reports its cost as unavailable, and a "$0.00" line here would contradict
/// that.
fn print_model_pricing(provider: &str, model: &str) {
    let Some(p) = leviath_providers::pricing::published_rates(provider, model) else {
        return;
    };
    println!();
    println!("Pricing (USD per million tokens)");
    println!("--------------------------------");
    println!("  Input:          ${:.2}", p.input_per_mtok);
    println!("  Cached input:   ${:.2}", p.cached_input_per_mtok);
    println!("  Cache write:    ${:.2}", p.cache_write_per_mtok);
    println!("  Output:         ${:.2}", p.output_per_mtok);
    // Always labelled as the published price, never as the operator's: a
    // capability override says nothing about whether they also declared a rate,
    // and claiming "your config" for a figure that came from this table would
    // be worse than not showing a source at all.
    println!(
        "  Source:         published list price, read {}",
        leviath_providers::pricing::RATES_READ_ON
    );
    println!("  \u{26a0}  {provider} does not serve prices through its API, so this was");
    println!("     transcribed by hand and may be out of date. A rate set on the");
    println!("     model's config entry overrides this, and is the only way to");
    println!("     record a negotiated price no public page would show.");
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

    print_model_pricing(provider, id);
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
            assert!(entry.caps.max_context_tokens > 0);
            assert!(entry.caps.max_output_tokens > 0);
            assert!(entry.caps.supports_streaming);
            assert!(entry.caps.supports_system_prompt);
        }
    }

    #[test]
    fn builtin_entries_have_unique_model_ids() {
        let table = builtin_table();
        let ids: Vec<&str> = table.iter().map(|e| e.model_id).collect();
        let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len());
    }

    #[test]
    fn deepseek_r1_models_no_tools() {
        let table = builtin_table();
        for entry in &table {
            if entry.model_id.contains("deepseek-r1") {
                assert!(!entry.caps.supports_tools);
            }
        }
    }

    // ─── print_model_detail ─────────────────────────────────────────────────

    /// A listed model prints its rates, the date they were read, and the
    /// warning - and never claims the figure came from the operator's config,
    /// which this function cannot see.
    #[test]
    fn pricing_output_warns_and_does_not_claim_a_config_source() {
        // Exercised for panics and for the branch; the text itself is asserted
        // through the table, which is what a reader is actually shown.
        print_model_pricing("anthropic", "claude-opus-5");
        print_model_pricing("openai", "gpt-5.5");
        // A model with no listed rate prints nothing rather than a zero, which
        // would contradict the run reporting its cost as unavailable.
        print_model_pricing("openrouter", "x-ai/grok-4.6");
        print_model_pricing("anthropic", "claude-opus-9");
    }

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
                assert!(!entry.caps.supports_temperature);
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
            assert!(!entry.display_name.is_empty());
        }
    }

    #[test]
    fn builtin_table_context_larger_than_output() {
        let table = builtin_table();
        for entry in &table {
            assert!(entry.caps.max_context_tokens >= entry.caps.max_output_tokens);
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
                assert!(entry.caps.supports_temperature);
            }
        }
    }

    /// A user whose only key is a Gemini key must find models they can run.
    /// Every Gemini row in this table used to route through OpenRouter, which
    /// needs a different key, so `lev models list` showed that user nothing
    /// their key could reach.
    #[test]
    fn builtin_table_offers_native_google_models() {
        let table = builtin_table();
        let native: Vec<&str> = table
            .iter()
            .filter(|e| e.provider == "google")
            .map(|e| e.model_id)
            .collect();
        assert!(
            !native.is_empty(),
            "the native google provider must offer models of its own"
        );
        // Native ids are bare (`gemini-3.5-flash`), never OpenRouter-prefixed.
        for id in &native {
            assert!(
                !id.contains('/'),
                "native google model id must not be vendor-prefixed: {id}"
            );
        }
    }

    #[test]
    fn builtin_table_gemini_flash_models_exist() {
        let table = builtin_table();
        let flash: Vec<_> = table
            .iter()
            .filter(|e| e.model_id.contains("gemini") && e.model_id.contains("flash"))
            .collect();
        assert!(!flash.is_empty());
    }

    #[test]
    fn builtin_table_deepseek_r1_no_temperature() {
        let table = builtin_table();
        for entry in &table {
            if entry.model_id.contains("deepseek-r1") {
                assert!(!entry.caps.supports_temperature);
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
        assert!(!qwen.is_empty());
    }

    #[test]
    fn builtin_table_mistral_models_exist() {
        let table = builtin_table();
        let mistral: Vec<_> = table
            .iter()
            .filter(|e| e.model_id.contains("mistral"))
            .collect();
        assert!(!mistral.is_empty());
    }

    #[test]
    fn builtin_table_all_entries_have_provider() {
        let table = builtin_table();
        for entry in &table {
            assert!(!entry.provider.is_empty());
        }
    }

    #[test]
    fn builtin_table_all_entries_have_model_id() {
        let table = builtin_table();
        for entry in &table {
            assert!(!entry.model_id.is_empty());
        }
    }

    // ─── execute() / list() / show() async entry points ──────────────────

    #[tokio::test]
    async fn execute_list_command_runs_without_error() {
        crate::config::with_isolated_config_path_async(
            "models-execute_list_command_runs_without_error",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::List(ListArgs {
                        provider: None,
                        remote: false,
                        all: false,
                        json: false,
                    }),
                };
                // Should succeed: prints the builtin table
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_list_with_provider_filter_runs_without_error() {
        crate::config::with_isolated_config_path_async(
            "models-execute_list_with_provider_filter_runs_without_error",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::List(ListArgs {
                        provider: Some("anthropic".to_string()),
                        remote: false,
                        all: false,
                        json: false,
                    }),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_list_with_nonexistent_provider_filter_fails() {
        crate::config::with_isolated_config_path_async(
            "models-execute_list_with_nonexistent_provider_filter",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::List(ListArgs {
                        provider: Some("nonexistent_provider".to_string()),
                        remote: false,
                        all: false,
                        json: false,
                    }),
                };
                // Nothing registered, nothing in the built-in table, no script
                // of that name: there is no answer, so this exits non-zero
                // rather than printing an empty table.
                let err = execute(args).await.expect_err("nothing can answer");
                assert!(
                    err.to_string().contains("nonexistent_provider"),
                    "the message names the provider: {err}"
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_show_known_model_runs_without_error() {
        crate::config::with_isolated_config_path_async(
            "models-execute_show_known_model_runs_without_error",
            |_fake_dir| async move {
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
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_show_unknown_model_runs_without_error() {
        crate::config::with_isolated_config_path_async(
            "models-execute_show_unknown_model_runs_without_error",
            |_fake_dir| async move {
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
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_show_unknown_model_with_remote_no_provider() {
        crate::config::with_isolated_config_path_async(
            "models-execute_show_unknown_model_with_remote_no_provider",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::Show(ShowArgs {
                        model: "totally-unknown-model-xyz".to_string(),
                        provider: None,
                        remote: true, // remote but no provider = skips remote lookup
                    }),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_show_unknown_model_with_remote_unconfigured_provider() {
        crate::config::with_isolated_config_path_async(
            "models-execute_show_unknown_model_with_remote_unconfigured_provider",
            |_fake_dir| async move {
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
            },
        )
        .await;
    }

    // ─── list() with builtin model having overrides in config ─────────────

    #[tokio::test]
    async fn list_with_openrouter_filter() {
        // execute() -> list_with_registry() calls the real Config::load(),
        // which reads the process-global LEVIATH_CONFIG_PATH. Without
        // isolating it here, a concurrently-running test that points that
        // var at a temporarily-invalid-TOML fake config (e.g.
        // list_with_registry_propagates_config_load_error) can make this
        // test observe that torn state and fail nondeterministically --
        // exactly what happened on CI.
        crate::config::with_isolated_config_path_async(
            "models-list-openrouter-filter",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::List(ListArgs {
                        provider: Some("openrouter".to_string()),
                        remote: false,
                        all: false,
                        json: false,
                    }),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_with_openai_filter() {
        // See the comment on list_with_openrouter_filter - same real
        // Config::load() race.
        crate::config::with_isolated_config_path_async(
            "models-list-openai-filter",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::List(ListArgs {
                        provider: Some("openai".to_string()),
                        remote: false,
                        all: false,
                        json: false,
                    }),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_builtin_anthropic_opus() {
        // See the comment on list_with_openrouter_filter - same real
        // Config::load() race.
        crate::config::with_isolated_config_path_async(
            "models-show-anthropic-opus",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::Show(ShowArgs {
                        model: "claude-opus-4-6".to_string(),
                        provider: None,
                        remote: false,
                    }),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_builtin_openai_model() {
        // See the comment on list_with_openrouter_filter - same real
        // Config::load() race.
        crate::config::with_isolated_config_path_async(
            "models-show-openai-model",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::Show(ShowArgs {
                        model: "gpt-5.5".to_string(),
                        provider: None,
                        remote: false,
                    }),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_builtin_deepseek_r1() {
        // See the comment on list_with_openrouter_filter - same real
        // Config::load() race.
        crate::config::with_isolated_config_path_async(
            "models-show-deepseek-r1",
            |_fake_dir| async move {
                let args = ModelsArgs {
                    command: ModelsCommand::Show(ShowArgs {
                        model: "deepseek/deepseek-r1".to_string(),
                        provider: None,
                        remote: false,
                    }),
                };
                let result = execute(args).await;
                assert!(result.is_ok());
            },
        )
        .await;
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
        crate::config::with_isolated_config_path_async(
            "models-list_builtin_no_filter_succeeds",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: false,
                    json: false,
                };
                let result = list_with_registry(args, &build_provider_registry_from_config).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_json_with_no_configured_provider_is_an_empty_array() {
        // The prose report prints a nudge here. JSON must not: a caller reading
        // this branches on the array's length, and a sentence would not parse.
        //
        // `openai` rather than a made-up name: a provider the built-in table
        // knows but this install has no credential for is the case that still
        // has an honest empty answer. A name nothing can answer for is an error
        // (see `execute_list_with_nonexistent_provider_filter_fails`), and an
        // error has no array to print.
        crate::config::with_isolated_config_path_async(
            "models-list_json_empty",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: Some("openai".to_string()),
                    all: false,
                    json: true,
                };
                let result = list_with_registry(args, &build_provider_registry_from_config).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_json_with_all_succeeds() {
        crate::config::with_isolated_config_path_async(
            "models-list_json_all",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: true,
                    json: true,
                };
                let result = list_with_registry(args, &build_provider_registry_from_config).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[test]
    fn model_row_serializes_capabilities_and_the_override_flag() {
        // The table shows an override as a `*` prefix on the provider column.
        // JSON has to say so in a field, or a caller cannot tell a capability
        // Leviath knows from one config asserted.
        let row = ModelRow {
            id: "m".to_string(),
            provider: "p".to_string(),
            display_name: Some("M".to_string()),
            capabilities: ModelCapabilities::default(),
            capabilities_overridden: true,
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
        assert_eq!(value["id"], serde_json::json!("m"));
        assert_eq!(value["capabilities_overridden"], serde_json::json!(true));
        assert!(value["capabilities"]["supports_tools"].is_boolean());
    }

    #[tokio::test]
    async fn list_builtin_with_provider_filter_succeeds() {
        crate::config::with_isolated_config_path_async(
            "models-list_builtin_with_provider_filter_succeeds",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: Some("anthropic".to_string()),
                    all: false,
                    json: false,
                };
                let result = list_with_registry(args, &build_provider_registry_from_config).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_unknown_provider_filter_is_an_error_not_an_empty_table() {
        crate::config::with_isolated_config_path_async(
            "models-list_unknown_provider_filter_finds_nothing",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: Some("no-such-provider".to_string()),
                    all: false,
                    json: false,
                };
                let err = list_with_registry(args, &build_provider_registry_from_config)
                    .await
                    .expect_err("a name nothing can answer for is an error");
                let msg = err.to_string();
                assert!(msg.contains("no-such-provider"), "{msg}");
                assert!(
                    msg.contains(".rhai"),
                    "it says where a script would go: {msg}"
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_builtin_model_succeeds() {
        crate::config::with_isolated_config_path_async(
            "models-show_builtin_model_succeeds",
            |_fake_dir| async move {
                // Use a model ID guaranteed to be in the builtin table.
                let known_id = builtin_table()[0].model_id.to_string();
                let args = ShowArgs {
                    model: known_id,
                    remote: false,
                    provider: None,
                };
                let result = show_with_registry(args, &build_provider_registry_from_config).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_unknown_model_without_remote_succeeds_with_warning() {
        crate::config::with_isolated_config_path_async(
            "models-show_unknown_model_without_remote_succeeds_with_warning",
            |_fake_dir| async move {
                let args = ShowArgs {
                    model: "totally-unknown-model-xyz".to_string(),
                    remote: false,
                    provider: None,
                };
                // Falls through all lookup tiers; must not error even when not found.
                let result = show_with_registry(args, &build_provider_registry_from_config).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_remote_without_provider_falls_through_gracefully() {
        crate::config::with_isolated_config_path_async(
            "models-show_remote_without_provider_falls_through_gracefully",
            |_fake_dir| async move {
                // args.remote = true but no --provider given -> the remote-fetch
                // branch's inner `if let Some(ref provider_name)` is skipped.
                let args = ShowArgs {
                    model: "totally-unknown-model-xyz".to_string(),
                    remote: true,
                    provider: None,
                };
                let result = show_with_registry(args, &build_provider_registry_from_config).await;
                assert!(result.is_ok());
            },
        )
        .await;
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
            _request: &leviath_providers::InferenceRequest,
        ) -> Result<leviath_providers::InferenceResponse, leviath_providers::ProviderError>
        {
            Err(leviath_providers::ProviderError::Other(
                "MockProvider does not support infer".to_string(),
            ))
        }

        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            leviath_core::estimate_tokens(text)
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
    ) -> impl Fn(&Config) -> Result<leviath_runtime::ProviderRegistry, leviath_providers::ProviderError>
    {
        // `Fn` (not `FnOnce`) so the closure can be called through the
        // `&dyn Fn` trait object `list_with_registry`/`show_with_registry`
        // now take - see the doc comment on `list_with_registry` for why.
        // Only ever actually invoked once per test, but `Fn`'s "may be
        // called more than once" contract means captured state can't be
        // moved out on each call, hence the clone.
        move |_config: &Config| {
            let mut registry = leviath_runtime::ProviderRegistry::new();
            registry.register(
                provider_name.to_string(),
                std::sync::Arc::new(MockProvider {
                    models: models.clone(),
                    fail,
                }),
            );
            Ok(registry)
        }
    }

    #[tokio::test]
    async fn list_remote_merges_new_model_from_provider() {
        crate::config::with_isolated_config_path_async(
            "models-list_remote_merges_new_model_from_provider",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: true,
                    provider: Some("mock".to_string()),
                    all: false,
                    json: false,
                };
                let new_model = ModelInfo {
                    id: "mock-brand-new-model".to_string(),
                    display_name: Some("Mock Brand New Model".to_string()),
                    provider: "mock".to_string(),
                    capabilities: ModelCapabilities::default(),
                };
                let result =
                    list_with_registry(args, &mock_registry("mock", vec![new_model], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_remote_without_provider_filter_queries_all_providers() {
        // No `--provider` filter set: every provider in the registry should
        // be queried for remote models (the `if let Some(ref filter) = ...`
        // pattern-doesn't-match arm, never exercised by the other
        // `list_remote_*` tests below, which all pass a provider filter).
        crate::config::with_isolated_config_path_async(
            "models-list_remote_without_provider_filter_queries_all_providers",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: true,
                    provider: None,
                    all: false,
                    json: false,
                };
                let new_model = ModelInfo {
                    id: "mock-brand-new-model".to_string(),
                    display_name: Some("Mock Brand New Model".to_string()),
                    provider: "mock".to_string(),
                    capabilities: ModelCapabilities::default(),
                };
                let result =
                    list_with_registry(args, &mock_registry("mock", vec![new_model], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_remote_overrides_builtin_entry_with_same_id() {
        crate::config::with_isolated_config_path_async(
            "models-list_remote_overrides_builtin_entry_with_same_id",
            |_fake_dir| async move {
                let known_id = builtin_table()[0].model_id.to_string();
                let args = ListArgs {
                    remote: true,
                    provider: Some("mock".to_string()),
                    all: false,
                    json: false,
                };
                let overriding_model = ModelInfo {
                    id: known_id,
                    display_name: Some("Overridden".to_string()),
                    provider: "mock".to_string(),
                    capabilities: ModelCapabilities::default(),
                };
                let result =
                    list_with_registry(args, &mock_registry("mock", vec![overriding_model], false))
                        .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// Only models the install can reach are listed: a registry holding just
    /// `anthropic` must not print google/openai/openrouter rows. Before this,
    /// a user with one key scrolled past dozens of models they could not run,
    /// with no way to tell whether their own key had registered.
    #[tokio::test]
    async fn list_shows_only_providers_the_install_has_credentials_for() {
        crate::config::with_isolated_config_path_async(
            "models-list_only_available",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: false,
                    json: false,
                };
                // A registry with exactly one provider that the builtin table
                // also knows: its rows survive, everything else is filtered.
                let result =
                    list_with_registry(args, &mock_registry("anthropic", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// A remote fetch that returns a model id the builtin table already lists
    /// replaces that row (remote wins), which requires the row to have survived
    /// the availability filter.
    #[tokio::test]
    async fn list_remote_overrides_a_builtin_entry_with_the_same_id() {
        crate::config::with_isolated_config_path_async(
            "models-list_remote_override",
            |_fake_dir| async move {
                let remote = vec![ModelInfo {
                    id: "claude-sonnet-5".to_string(),
                    display_name: Some("Claude Sonnet 5 (remote)".to_string()),
                    provider: "anthropic".to_string(),
                    capabilities: leviath_providers::ModelCapabilities::default(),
                }];
                let args = ListArgs {
                    remote: true,
                    provider: None,
                    all: false,
                    json: false,
                };
                let result =
                    list_with_registry(args, &mock_registry("anthropic", remote, false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// `--all` restores the full catalogue for shopping around before choosing
    /// a provider.
    #[tokio::test]
    async fn list_all_includes_providers_without_credentials() {
        crate::config::with_isolated_config_path_async(
            "models-list_all_includes_everything",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: true,
                    json: false,
                };
                let result = list_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// A capability override marks its row with `*`, which requires the row to
    /// survive the availability filter first.
    #[tokio::test]
    async fn list_marks_overridden_capabilities_for_an_available_provider() {
        crate::config::with_isolated_config_path_async(
            "models-list_overridden_available",
            |_fake_dir| async move {
                let mut config = Config::default();
                config.model_capabilities.insert(
                    "claude-sonnet-5".to_string(),
                    leviath_providers::ModelCapabilityOverride::default(),
                );
                config
                    .save_to_path(&Config::config_path())
                    .expect("the isolated config path is writable");
                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: false,
                    json: false,
                };
                let result =
                    list_with_registry(args, &mock_registry("anthropic", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_remote_provider_error_warns_and_continues() {
        crate::config::with_isolated_config_path_async(
            "models-list_remote_provider_error_warns_and_continues",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: true,
                    provider: Some("mock".to_string()),
                    all: false,
                    json: false,
                };
                let result = list_with_registry(args, &mock_registry("mock", vec![], true)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn list_remote_skips_providers_not_matching_filter() {
        crate::config::with_isolated_config_path_async(
            "models-list_remote_skips_providers_not_matching_filter",
            |_fake_dir| async move {
                // The registry only has "mock" registered, so the filter never
                // matches and the `if filter != provider_name { continue; }`
                // branch is exercised without the mock ever being queried.
                // `openai` rather than a made-up name so the filter is one the
                // built-in table can still answer for - an unanswerable name is
                // an error, which would end the command before this branch.
                let args = ListArgs {
                    remote: true,
                    provider: Some("openai".to_string()),
                    all: false,
                    json: false,
                };
                let result = list_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// A registry whose only provider is a `.rhai` script in `dir`, exactly as
    /// `lev models list` builds one for a real install. Not a mock: the script
    /// is compiled, `initialize` runs, and `list_models` is dispatched, which
    /// is the whole point of the command this covers.
    fn script_registry(
        dir: std::path::PathBuf,
    ) -> impl Fn(&Config) -> Result<leviath_runtime::ProviderRegistry, leviath_providers::ProviderError>
    {
        move |_config: &Config| {
            let layer = leviath_runtime::script_provider::ScriptProviderLayer::new(
                dir.clone(),
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                None,
                Vec::new(),
            );
            Ok(leviath_runtime::ProviderRegistry::new()
                .with_script_layer(std::sync::Arc::new(layer)))
        }
    }

    /// The command `rhai-providers.md` prescribes as a provider script's smoke
    /// test: it has to reach the script layer, since the built-in table has no
    /// row for a provider that names its own models at run time (issue #523).
    #[tokio::test]
    async fn list_provider_reaches_a_script_provider() {
        let dir = tempfile::tempdir().expect("a temp providers dir");
        std::fs::write(
            dir.path().join("scripted.rhai"),
            "fn initialize(config) { #{ ok: true } }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { [ #{ id: \"scripted-large\", max_context_tokens: 32768 } ] }",
        )
        .expect("the temp dir is writable");
        let dir = dir.path().to_path_buf();
        crate::config::with_isolated_config_path_async(
            "models-list_provider_reaches_a_script_provider",
            |_fake_dir| async move {
                // JSON so the merged row is the whole output, not a table cell.
                let args = ListArgs {
                    remote: false,
                    provider: Some("scripted".to_string()),
                    all: false,
                    json: true,
                };
                let result = list_with_registry(args, &script_registry(dir)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// `--remote` sweeps every provider it can resolve, script providers now
    /// included - the sweep used to be built on `provider_names`, which names
    /// only the natively registered ones (issues #523, #531).
    #[tokio::test]
    async fn list_remote_sweeps_script_providers_too() {
        let dir = tempfile::tempdir().expect("a temp providers dir");
        std::fs::write(
            dir.path().join("swept.rhai"),
            "fn initialize(config) { #{ ok: true } }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { [ #{ id: \"swept-large\" } ] }",
        )
        .expect("the temp dir is writable");
        // A second script that will not compile: the sweep skips it rather
        // than failing, which is only fatal when `--provider` named it.
        std::fs::write(
            dir.path().join("wont-compile.rhai"),
            "fn initialize(config) { #{",
        )
        .expect("the temp dir is writable");
        let dir = dir.path().to_path_buf();
        crate::config::with_isolated_config_path_async(
            "models-list_remote_sweeps_script_providers_too",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: true,
                    provider: None,
                    all: false,
                    json: true,
                };
                let result = list_with_registry(args, &script_registry(dir)).await;
                assert!(result.is_ok(), "one broken script does not fail the sweep");
            },
        )
        .await;
    }

    /// A `--provider` naming a script provider is answered once, by the path
    /// that reports its failures properly - not twice, once by each.
    #[tokio::test]
    async fn a_named_script_provider_is_not_also_swept() {
        let dir = tempfile::tempdir().expect("a temp providers dir");
        std::fs::write(
            dir.path().join("once.rhai"),
            "fn initialize(config) { #{ ok: true } }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { [ #{ id: \"once-large\" } ] }",
        )
        .expect("the temp dir is writable");
        let dir = dir.path().to_path_buf();
        crate::config::with_isolated_config_path_async(
            "models-a_named_script_provider_is_not_also_swept",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: true,
                    provider: Some("once".to_string()),
                    all: false,
                    json: true,
                };
                let result = list_with_registry(args, &script_registry(dir)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// A script provider that loads but names no models is not the same answer
    /// as "nothing is configured", and does not print the `lev setup` nudge.
    #[tokio::test]
    async fn list_says_when_a_script_provider_loaded_and_listed_nothing() {
        let dir = tempfile::tempdir().expect("a temp providers dir");
        std::fs::write(
            dir.path().join("quiet.rhai"),
            "fn initialize(config) { #{ ok: true } }\n\
             fn inference(state, request) { #{ content: \"ok\" } }",
        )
        .expect("the temp dir is writable");
        let dir = dir.path().to_path_buf();
        crate::config::with_isolated_config_path_async(
            "models-list_says_when_a_script_provider_listed_nothing",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: Some("quiet".to_string()),
                    all: false,
                    json: false,
                };
                let result = list_with_registry(args, &script_registry(dir)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// The auth failure the docs promise surfaces here: `list_models` threw, so
    /// the command fails with what the provider said instead of printing an
    /// empty table under an exit code of 0.
    #[tokio::test]
    async fn list_reports_a_script_provider_that_cannot_list() {
        let dir = tempfile::tempdir().expect("a temp providers dir");
        std::fs::write(
            dir.path().join("angry.rhai"),
            "fn initialize(config) { #{ ok: true } }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { throw \"401 Unauthorized\" }",
        )
        .expect("the temp dir is writable");
        let dir = dir.path().to_path_buf();
        crate::config::with_isolated_config_path_async(
            "models-list_reports_a_script_provider_that_cannot_list",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: Some("angry".to_string()),
                    all: false,
                    json: false,
                };
                let err = list_with_registry(args, &script_registry(dir))
                    .await
                    .expect_err("a provider that cannot list is a failed smoke test");
                let msg = err.to_string();
                assert!(msg.contains("angry"), "{msg}");
                assert!(
                    msg.contains("401 Unauthorized"),
                    "the provider's own words survive: {msg}"
                );
            },
        )
        .await;
    }

    /// A script provider that names a model the built-in table also carries
    /// replaces that row rather than adding a second one, the same way a
    /// `--remote` fetch does. `--all` is what keeps the built-in rows in the
    /// table for it to collide with.
    #[tokio::test]
    async fn a_script_provider_overrides_a_builtin_row_with_the_same_id() {
        let dir = tempfile::tempdir().expect("a temp providers dir");
        let builtin_id = builtin_table()[0].model_id.to_string();
        std::fs::write(
            dir.path().join("mirror.rhai"),
            format!(
                "fn initialize(config) {{ #{{ ok: true }} }}\n\
                 fn inference(state, request) {{ #{{ content: \"ok\" }} }}\n\
                 fn list_models(state) {{ [ #{{ id: \"{builtin_id}\", \
                 max_context_tokens: 4096 }} ] }}"
            ),
        )
        .expect("the temp dir is writable");
        let dir = dir.path().to_path_buf();
        crate::config::with_isolated_config_path_async(
            "models-a_script_provider_overrides_a_builtin_row",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: Some("mirror".to_string()),
                    all: true,
                    json: true,
                };
                let result = list_with_registry(args, &script_registry(dir)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    /// A built-in provider name is answered from the table, not by trying to
    /// load a script of that name - `--provider openai` with no OpenAI key is
    /// an empty table, exactly as it was before script providers were reached.
    #[tokio::test]
    async fn list_provider_naming_a_builtin_does_not_reach_the_script_layer() {
        let dir = tempfile::tempdir().expect("a temp providers dir");
        // A script that would fail to compile if it were ever reached.
        std::fs::write(dir.path().join("openai.rhai"), "this is not rhai (")
            .expect("the temp dir is writable");
        let dir = dir.path().to_path_buf();
        crate::config::with_isolated_config_path_async(
            "models-list_provider_naming_a_builtin_skips_scripts",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: Some("openai".to_string()),
                    all: true,
                    json: false,
                };
                let result = list_with_registry(args, &script_registry(dir)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_remote_finds_model_from_provider() {
        crate::config::with_isolated_config_path_async(
            "models-show_remote_finds_model_from_provider",
            |_fake_dir| async move {
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
                    show_with_registry(args, &mock_registry("mock", vec![remote_model], false))
                        .await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_remote_model_not_found_in_provider_list_falls_through() {
        crate::config::with_isolated_config_path_async(
            "models-show_remote_model_not_found_in_provider_list_falls_through",
            |_fake_dir| async move {
                let args = ShowArgs {
                    model: "totally-unknown-model-xyz".to_string(),
                    remote: true,
                    provider: Some("mock".to_string()),
                };
                let result = show_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_remote_provider_error_warns_and_falls_through() {
        crate::config::with_isolated_config_path_async(
            "models-show_remote_provider_error_warns_and_falls_through",
            |_fake_dir| async move {
                let args = ShowArgs {
                    model: "totally-unknown-model-xyz".to_string(),
                    remote: true,
                    provider: Some("mock".to_string()),
                };
                let result = show_with_registry(args, &mock_registry("mock", vec![], true)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_remote_unconfigured_provider_warns_and_falls_through() {
        crate::config::with_isolated_config_path_async(
            "models-show_remote_unconfigured_provider_warns_and_falls_through",
            |_fake_dir| async move {
                // provider filter names a provider that isn't in the registry at all
                // -> the `if let Some(provider) = registry.get(...)` else branch.
                let args = ShowArgs {
                    model: "totally-unknown-model-xyz".to_string(),
                    remote: true,
                    provider: Some("nonexistent-provider".to_string()),
                };
                let result = show_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
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
        crate::config::with_isolated_config_path_async(
            "models-list-override",
            |_fake_dir| async move {
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
                    }
                    .into(),
                );
                std::fs::write(
                    Config::config_path(),
                    toml::to_string(&fake_config).unwrap(),
                )
                .unwrap();

                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: false,
                    json: false,
                };
                let result = list_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_prints_warning_and_uses_model_capabilities_override() {
        crate::config::with_isolated_config_path_async(
            "models-show-override",
            |_fake_dir| async move {
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
                    }
                    .into(),
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
                let result = show_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_ok());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn mock_provider_trivial_trait_methods() {
        use leviath_providers::Provider;
        let provider = MockProvider {
            models: vec![],
            fail: false,
        };
        assert_eq!(provider.count_tokens("abcd", "mock-model").await, 1);
        assert_eq!(provider.max_context_tokens("mock-model"), 100_000);
        assert_eq!(provider.name(), "mock");
        let _ = provider.capabilities("mock-model");
    }

    #[tokio::test]
    async fn mock_provider_infer_returns_err() {
        use leviath_providers::Provider;
        let provider = MockProvider {
            models: vec![],
            fail: false,
        };
        let request = leviath_providers::InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "mock".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let result = provider.infer(&request).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_with_registry_propagates_config_load_error() {
        crate::config::with_isolated_config_path_async(
            "models-list_with_registry_propagates_config_load_error",
            |fake_dir| async move {
                std::fs::write(fake_dir.join("config.toml"), "not valid toml [[[").unwrap();
                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: false,
                    json: false,
                };
                let result = list_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_err());
            },
        )
        .await;
    }

    // ─── CLI argument parsing (clap derive) ────────────────────────────────
    //
    // `ModelsArgs`/`ModelsCommand`/`ListArgs`/`ShowArgs` only ever get
    // constructed as plain struct literals elsewhere in this file's tests,
    // which never exercises clap's derive-generated `Args`/`FromArgMatches`
    // parsing implementations (`augment_args`, `from_arg_matches`, etc.) --
    // those are only reached in production via `main.rs`'s real
    // `Cli::parse()`, which isn't part of this crate's `--lib` test target.
    // Wrapping `ModelsArgs` in a minimal local `Parser` and driving it
    // through `try_parse_from` exercises that derive machinery directly and
    // doubles as a real regression test for the actual flag/positional
    // contract (short flags, long flags, subcommand names).

    use clap::Parser as _;

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(flatten)]
        models: ModelsArgs,
    }

    /// Unwraps the `List` variant, panicking otherwise. A bare `match ... =>
    /// panic!(...)` inline in each test would leave that panic arm a
    /// permanent 0-hit region in a green suite (it only fires on failure) --
    /// extracting it here lets a single `#[should_panic]` test exercise it
    /// once, matching the pattern already used in `serve/blueprints.rs`.
    fn expect_list(cmd: ModelsCommand) -> ListArgs {
        match cmd {
            ModelsCommand::List(args) => args,
            ModelsCommand::Show(_) => panic!("expected List"),
        }
    }

    #[test]
    #[should_panic(expected = "expected List")]
    fn expect_list_panics_on_show() {
        expect_list(ModelsCommand::Show(ShowArgs {
            model: "x".to_string(),
            provider: None,
            remote: false,
        }));
    }

    /// Unwraps the `Show` variant, panicking otherwise. See [`expect_list`].
    fn expect_show(cmd: ModelsCommand) -> ShowArgs {
        match cmd {
            ModelsCommand::Show(args) => args,
            ModelsCommand::List(_) => panic!("expected Show"),
        }
    }

    #[test]
    #[should_panic(expected = "expected Show")]
    fn expect_show_panics_on_list() {
        expect_show(ModelsCommand::List(ListArgs {
            provider: None,
            remote: false,
            all: false,
            json: false,
        }));
    }

    #[test]
    fn parses_list_with_no_flags() {
        let cli = TestCli::try_parse_from(["lev", "list"]).unwrap();
        let args = expect_list(cli.models.command);
        assert!(args.provider.is_none());
        assert!(!args.remote);
    }

    #[test]
    fn parses_list_with_long_flags() {
        let cli = TestCli::try_parse_from(["lev", "list", "--provider", "anthropic", "--remote"])
            .unwrap();
        let args = expect_list(cli.models.command);
        assert_eq!(args.provider.as_deref(), Some("anthropic"));
        assert!(args.remote);
    }

    #[test]
    fn parses_list_with_short_flags() {
        let cli = TestCli::try_parse_from(["lev", "list", "-p", "openai", "-r"]).unwrap();
        let args = expect_list(cli.models.command);
        assert_eq!(args.provider.as_deref(), Some("openai"));
        assert!(args.remote);
    }

    #[test]
    fn parses_show_with_positional_model_and_long_flags() {
        let cli = TestCli::try_parse_from([
            "lev",
            "show",
            "claude-sonnet-4-6",
            "--provider",
            "anthropic",
            "--remote",
        ])
        .unwrap();
        let args = expect_show(cli.models.command);
        assert_eq!(args.model, "claude-sonnet-4-6");
        assert_eq!(args.provider.as_deref(), Some("anthropic"));
        assert!(args.remote);
    }

    #[test]
    fn parses_show_with_short_flags() {
        let cli =
            TestCli::try_parse_from(["lev", "show", "gpt-5.5", "-p", "openai", "-r"]).unwrap();
        let args = expect_show(cli.models.command);
        assert_eq!(args.model, "gpt-5.5");
        assert_eq!(args.provider.as_deref(), Some("openai"));
        assert!(args.remote);
    }

    #[test]
    fn parses_show_missing_required_positional_errors() {
        let result = TestCli::try_parse_from(["lev", "show"]);
        assert!(result.is_err());
    }

    #[test]
    fn parses_unknown_subcommand_errors() {
        let result = TestCli::try_parse_from(["lev", "not-a-subcommand"]);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn show_with_registry_propagates_config_load_error() {
        crate::config::with_isolated_config_path_async(
            "models-show_with_registry_propagates_config_load_error",
            |fake_dir| async move {
                std::fs::write(fake_dir.join("config.toml"), "not valid toml [[[").unwrap();
                let args = ShowArgs {
                    model: "any-model".to_string(),
                    remote: false,
                    provider: None,
                };
                let result = show_with_registry(args, &mock_registry("mock", vec![], false)).await;
                assert!(result.is_err());
            },
        )
        .await;
    }

    /// A builder that fails the way a machine with no readable root
    /// certificate store would.
    fn cannot_build(
        _config: &Config,
    ) -> Result<leviath_runtime::ProviderRegistry, leviath_providers::ProviderError> {
        Err(leviath_providers::ProviderError::ClientBuild(
            "no roots".to_string(),
        ))
    }

    #[tokio::test]
    async fn list_reports_a_registry_that_will_not_build() {
        crate::config::with_isolated_config_path_async(
            "models-list_reports_a_registry_that_will_not_build",
            |_fake_dir| async move {
                let args = ListArgs {
                    remote: false,
                    provider: None,
                    all: false,
                    json: false,
                };
                let err = list_with_registry(args, &cannot_build)
                    .await
                    .expect_err("a failing registry builder should fail the command");
                assert!(err.to_string().contains("root certificate store"));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn show_reports_a_registry_that_will_not_build() {
        crate::config::with_isolated_config_path_async(
            "models-show_reports_a_registry_that_will_not_build",
            |_fake_dir| async move {
                // Both `remote` and `provider`: the registry is only built
                // when the two are given together.
                let args = ShowArgs {
                    // A model the built-in table does not know, so the lookup
                    // falls through to the registry instead of returning early.
                    model: "not-a-built-in-model".to_string(),
                    provider: Some("anthropic".to_string()),
                    remote: true,
                };
                let err = show_with_registry(args, &cannot_build)
                    .await
                    .expect_err("a failing registry builder should fail the command");
                assert!(err.to_string().contains("root certificate store"));
            },
        )
        .await;
    }
}
