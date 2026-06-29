//! `lev bench` — benchmark cache efficiency for an agent blueprint.

use anyhow::Result;
use clap::Args;
use std::time::Instant;

/// Arguments for the `lev bench` command.
#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Agent blueprint name or path
    pub agent: String,

    /// Number of conversation turns to simulate
    #[arg(long, default_value = "10")]
    pub turns: usize,

    /// Provider to use
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use
    #[arg(long)]
    pub model: Option<String>,

    /// Dry-run mode: estimate cache efficiency from region structure without calling an LLM
    #[arg(long)]
    pub dry_run: bool,
}

/// Per-turn metrics.
#[allow(dead_code)]
struct TurnMetrics {
    prompt_tokens: usize,
    cached_tokens: usize,
    cache_write_tokens: usize,
    completion_tokens: usize,
    latency_ms: u128,
}

/// Synthetic user messages for benchmarking.
const SYNTHETIC_MESSAGES: &[&str] = &[
    "Implement a basic HTTP server with routing support",
    "Add middleware for request logging",
    "Now add tests for the routing module",
    "Refactor the error handling to use custom error types",
    "Add rate limiting middleware",
    "Implement graceful shutdown",
    "Add health check endpoint",
    "Write integration tests for the full server",
    "Optimize the router for performance",
    "Add OpenAPI documentation generation",
    "Implement authentication middleware",
    "Add database connection pooling",
    "Create migration system for schema changes",
    "Add request validation layer",
    "Implement response caching",
];

/// Hardcoded pricing per million tokens.
struct Pricing {
    input_per_m: f64,
    cache_read_per_m: f64,
    cache_write_per_m: f64,
}

fn pricing_for_provider(provider: &str) -> Pricing {
    match provider {
        "anthropic" => Pricing {
            input_per_m: 3.0,
            cache_read_per_m: 0.30,
            cache_write_per_m: 3.75,
        },
        "openai" => Pricing {
            input_per_m: 2.50,
            cache_read_per_m: 1.25,
            cache_write_per_m: 0.0,
        },
        "google" => Pricing {
            input_per_m: 0.15,
            cache_read_per_m: 0.0375,
            cache_write_per_m: 0.0,
        },
        _ => Pricing {
            input_per_m: 3.0,
            cache_read_per_m: 0.30,
            cache_write_per_m: 3.75,
        },
    }
}

/// Execute the bench command.
pub async fn execute(args: BenchArgs) -> Result<()> {
    let provider_name = args.provider.as_deref().unwrap_or("anthropic");
    let model_name = args.model.as_deref().unwrap_or("claude-sonnet-4-6");
    let total_start = Instant::now();

    println!();
    println!("Leviath Cache Efficiency Report");
    println!("{}", "═".repeat(55));
    println!(
        "Agent: {} | Provider: {} | Model: {}",
        args.agent, provider_name, model_name
    );

    if args.dry_run {
        execute_dry_run(&args, provider_name, model_name)?;
    } else {
        execute_live(&args, provider_name, model_name).await?;
    }

    let duration = total_start.elapsed();
    println!(
        "Turns: {} | Duration: {:.1}s",
        args.turns,
        duration.as_secs_f64()
    );
    println!("{}", "═".repeat(55));

    Ok(())
}

/// Dry-run: estimate cache efficiency from region structure.
fn execute_dry_run(args: &BenchArgs, provider_name: &str, model_name: &str) -> Result<()> {
    println!("Mode: dry-run (estimated from region structure)");
    println!("{}", "─".repeat(55));
    println!(
        "{:>4}  {:>8}  {:>8}  {:>6}  {:>8}  {:>8}",
        "Turn", "Prompt", "Cached", "Cache%", "Write", "Latency"
    );

    let mut all_turns: Vec<TurnMetrics> = Vec::new();

    // Estimate: system prompt is ~2000 tokens (pinned), each turn adds ~1500 tokens
    let system_tokens = 2000usize;

    for turn in 1..=args.turns {
        let new_tokens = 1500usize;
        let total_prompt = system_tokens + turn * new_tokens;

        // First turn: nothing cached; subsequent: previous context is cached
        let (cached, write) = if turn == 1 {
            (0, total_prompt)
        } else {
            let prev_total = system_tokens + (turn - 1) * new_tokens;
            // Stable prefix = 75% of previous sliding window content + all system tokens
            let stable = system_tokens + (prev_total - system_tokens) * 3 / 4;
            (stable, total_prompt - stable)
        };

        let cache_pct = if total_prompt > 0 {
            (cached as f64 / total_prompt as f64) * 100.0
        } else {
            0.0
        };

        // Estimate latency: base 1.5s, reduced by cache hit rate
        let latency_ms = (1500.0 * (1.0 - cache_pct / 200.0)) as u128;

        println!(
            "{:>4}  {:>8}  {:>8}  {:>5.1}%  {:>8}  {:>5.1}s",
            turn,
            format_num(total_prompt),
            format_num(cached),
            cache_pct,
            format_num(write),
            latency_ms as f64 / 1000.0,
        );

        all_turns.push(TurnMetrics {
            prompt_tokens: total_prompt,
            cached_tokens: cached,
            cache_write_tokens: write,
            completion_tokens: 500,
            latency_ms,
        });
    }

    print_summary(&all_turns, provider_name, model_name);
    Ok(())
}

/// Live run: actually call the LLM and measure real cache metrics.
async fn execute_live(args: &BenchArgs, provider_name: &str, model_name: &str) -> Result<()> {
    use crate::config::Config;

    let cfg = Config::load()?;
    let provider: Box<dyn leviath_providers::Provider> =
        match provider_name {
            "anthropic" => {
                let key =
                    cfg.providers.anthropic_api_key.clone().ok_or_else(|| {
                        anyhow::anyhow!("No API key for anthropic. Run `lev setup`.")
                    })?;
                Box::new(leviath_providers::AnthropicProvider::new(key))
            }
            "openai" => {
                let key =
                    cfg.providers.openai_api_key.clone().ok_or_else(|| {
                        anyhow::anyhow!("No API key for openai. Run `lev setup`.")
                    })?;
                Box::new(leviath_providers::OpenAIProvider::new(key))
            }
            "google" => {
                let key =
                    cfg.providers.google_api_key.clone().ok_or_else(|| {
                        anyhow::anyhow!("No API key for google. Run `lev setup`.")
                    })?;
                Box::new(leviath_providers::GeminiProvider::new(key))
            }
            other => anyhow::bail!("Unsupported provider for bench: {}", other),
        };

    println!("Mode: live");
    println!("{}", "─".repeat(55));
    println!(
        "{:>4}  {:>8}  {:>8}  {:>6}  {:>8}  {:>8}",
        "Turn", "Prompt", "Cached", "Cache%", "Write", "Latency"
    );

    let mut all_turns: Vec<TurnMetrics> = Vec::new();
    let mut messages: Vec<leviath_providers::Message> = vec![leviath_providers::Message {
        role: "system".to_string(),
        content: format!(
            "You are a senior software engineer working on a project called '{}'. \
             Respond concisely with implementation plans.",
            args.agent
        ),
        cache_breakpoint: true,
    }];

    for turn in 1..=args.turns {
        let msg_text = SYNTHETIC_MESSAGES[(turn - 1) % SYNTHETIC_MESSAGES.len()];
        messages.push(leviath_providers::Message {
            role: "user".to_string(),
            content: msg_text.to_string(),
            cache_breakpoint: false,
        });

        // Mark the second-to-last user message as a cache breakpoint (stable prefix)
        if messages.len() > 3 {
            let prev_user_idx = messages.len() - 2;
            messages[prev_user_idx].cache_breakpoint = true;
        }

        let request = leviath_providers::InferenceRequest {
            messages: messages.clone(),
            model: model_name.to_string(),
            max_tokens: 256,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
        };

        let start = Instant::now();
        let resp = provider.infer(request).await?;
        let latency = start.elapsed();

        let cache_pct = if resp.tokens_used.prompt_tokens > 0 {
            (resp.tokens_used.cached_tokens as f64 / resp.tokens_used.prompt_tokens as f64) * 100.0
        } else {
            0.0
        };

        println!(
            "{:>4}  {:>8}  {:>8}  {:>5.1}%  {:>8}  {:>5.1}s",
            turn,
            format_num(resp.tokens_used.prompt_tokens),
            format_num(resp.tokens_used.cached_tokens),
            cache_pct,
            format_num(resp.tokens_used.cache_write_tokens),
            latency.as_secs_f64(),
        );

        all_turns.push(TurnMetrics {
            prompt_tokens: resp.tokens_used.prompt_tokens,
            cached_tokens: resp.tokens_used.cached_tokens,
            cache_write_tokens: resp.tokens_used.cache_write_tokens,
            completion_tokens: resp.tokens_used.completion_tokens,
            latency_ms: latency.as_millis(),
        });

        // Add assistant response to conversation
        messages.push(leviath_providers::Message {
            role: "assistant".to_string(),
            content: resp.content,
            cache_breakpoint: false,
        });
    }

    print_summary(&all_turns, provider_name, model_name);
    Ok(())
}

fn print_summary(turns: &[TurnMetrics], provider_name: &str, _model_name: &str) {
    let total_prompt: usize = turns.iter().map(|t| t.prompt_tokens).sum();
    let total_cached: usize = turns.iter().map(|t| t.cached_tokens).sum();
    let total_writes: usize = turns.iter().map(|t| t.cache_write_tokens).sum();

    let cache_pct = if total_prompt > 0 {
        (total_cached as f64 / total_prompt as f64) * 100.0
    } else {
        0.0
    };

    let pricing = pricing_for_provider(provider_name);
    let cost_without = total_prompt as f64 / 1_000_000.0 * pricing.input_per_m;
    let cost_with = {
        let uncached = total_prompt.saturating_sub(total_cached) as f64;
        let cache_read = total_cached as f64;
        let cache_write = total_writes as f64;
        (uncached / 1_000_000.0 * pricing.input_per_m)
            + (cache_read / 1_000_000.0 * pricing.cache_read_per_m)
            + (cache_write / 1_000_000.0 * pricing.cache_write_per_m)
    };
    let savings_pct = if cost_without > 0.0 {
        (1.0 - cost_with / cost_without) * 100.0
    } else {
        0.0
    };

    println!("{}", "─".repeat(55));
    println!("Total prompt tokens:    {}", format_num(total_prompt));
    println!(
        "Total cached tokens:    {} ({:.1}%)",
        format_num(total_cached),
        cache_pct
    );
    println!("Total cache writes:     {}", format_num(total_writes));
    println!("Est. cost without cache: ${:.2}", cost_without);
    println!(
        "Est. cost with cache:    ${:.2} ({:.0}% savings)",
        cost_with, savings_pct
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: TestCmd,
    }

    #[derive(clap::Subcommand)]
    enum TestCmd {
        Bench(BenchArgs),
    }

    #[test]
    fn test_bench_args_defaults() {
        let cli = TestCli::parse_from(["test", "bench", "coder"]);
        match cli.cmd {
            TestCmd::Bench(args) => {
                assert_eq!(args.agent, "coder");
                assert_eq!(args.turns, 10);
                assert!(!args.dry_run);
                assert!(args.provider.is_none());
                assert!(args.model.is_none());
            }
        }
    }

    #[test]
    fn test_bench_args_with_options() {
        let cli = TestCli::parse_from([
            "test",
            "bench",
            "coder",
            "--turns",
            "5",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-6",
            "--dry-run",
        ]);
        match cli.cmd {
            TestCmd::Bench(args) => {
                assert_eq!(args.agent, "coder");
                assert_eq!(args.turns, 5);
                assert!(args.dry_run);
                assert_eq!(args.provider.as_deref(), Some("anthropic"));
                assert_eq!(args.model.as_deref(), Some("claude-sonnet-4-6"));
            }
        }
    }

    #[test]
    fn test_format_num() {
        assert_eq!(format_num(500), "500");
        assert_eq!(format_num(1500), "1.5K");
        assert_eq!(format_num(1_500_000), "1.5M");
    }

    #[test]
    fn test_pricing() {
        let p = pricing_for_provider("anthropic");
        assert!((p.input_per_m - 3.0).abs() < f64::EPSILON);
        assert!((p.cache_read_per_m - 0.30).abs() < f64::EPSILON);
    }
}

fn format_num(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
