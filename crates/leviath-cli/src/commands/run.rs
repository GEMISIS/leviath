//! `lev run` - Run an agent

use clap::Args;
use leviath_core::blueprint::{ModelConfig, ToolResultRouting};
use leviath_core::{Blueprint, ContextLayout, Region, RegionKind, Stage};
use leviath_core::layout::RegionDefinition;
use leviath_runtime::{AgentEngine, AgentPool, ProviderRegistry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;

#[derive(Args)]
pub struct RunArgs {
    /// Path to agent project or agent.leviath
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt
    #[arg(short, long)]
    pub task: String,

    /// Model override
    #[arg(short, long)]
    pub model: Option<String>,
}

pub async fn execute(args: RunArgs) -> anyhow::Result<()> {
    let path = args.path.unwrap_or_else(|| ".".to_string());
    tracing::info!(path = %path, task = %args.task, "Running agent");

    // Find agent.leviath manifest
    let manifest_path = find_manifest(&path)?;
    println!("Loading agent from: {}", manifest_path.display());

    // Parse manifest into blueprint
    let manifest_content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("Failed to read manifest: {}", e))?;
    let blueprint = parse_manifest(&manifest_content)?;

    println!("Agent: {} v{}", blueprint.name, blueprint.version);
    println!("Task: {}", args.task);

    // Load config for API keys
    let config = Config::load()?;

    // Create provider registry
    let mut registry = ProviderRegistry::new();

    // Register Anthropic provider if key available
    if let Some(ref key) = config.providers.anthropic_api_key {
        registry.register(
            "anthropic".to_string(),
            Arc::new(leviath_providers::AnthropicProvider::new(key.clone())),
        );
    }

    // Register OpenAI provider if key available
    if let Some(ref key) = config.providers.openai_api_key {
        registry.register(
            "openai".to_string(),
            Arc::new(leviath_providers::OpenAIProvider::new(key.clone())),
        );
    }

    // Register OpenRouter provider if key available
    if let Some(ref key) = config.openrouter_api_key {
        registry.register(
            "openrouter".to_string(),
            Arc::new(leviath_providers::OpenRouterProvider::new(key.clone())),
        );
    }

    // Register Ollama provider (no key needed)
    let ollama_url = config
        .ollama_base_url
        .as_deref()
        .unwrap_or("http://localhost:11434");
    registry.register(
        "ollama".to_string(),
        Arc::new(leviath_providers::OllamaProvider::with_base_url(
            ollama_url.to_string(),
        )),
    );

    // Create engine with providers
    let mut engine = AgentEngine::with_providers(registry);

    // Create agent pool and spawn agent
    let mut pool = AgentPool::new(blueprint.clone());
    let agent_id = pool.spawn_agent(engine.world_mut());
    let entity = pool
        .get_agent(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("Failed to get spawned agent entity"))?;

    // Initialize context window regions from blueprint layout
    if let Some(mut window) = engine.world_mut().get_mut::<leviath_runtime::ContextWindow>(entity) {
        for region_def in &blueprint.context_layout.regions {
            let region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );
            window.add_region(region);
        }

        // Add a tool_results region if not present (used by inference loop)
        if window.get_region("tool_results").is_none() {
            let tool_region = Region::new(
                "tool_results".to_string(),
                RegionKind::Temporary,
                5000,
            );
            window.add_region(tool_region);
        }

        // Add task to the first pinned/system region
        let system_region_name = blueprint
            .context_layout
            .regions
            .iter()
            .find(|r| matches!(r.kind, RegionKind::Pinned))
            .map(|r| r.name.clone());

        if let Some(region_name) = system_region_name {
            let task_tokens = args.task.len() / 4 + 1;
            let _ = window.add_to_region(&region_name, args.task.clone(), task_tokens);
        }
    }

    // Get model config from the first stage
    let stage = blueprint
        .stages
        .first()
        .ok_or_else(|| anyhow::anyhow!("Blueprint has no stages"))?;

    let provider_name = &stage.model.provider;
    let model_name = args.model.as_deref().unwrap_or(&stage.model.model);

    // Check if provider is available
    if !engine.providers().has(provider_name) {
        println!(
            "\nProvider '{}' is not configured. Please set an API key in ~/.leviath/config.toml",
            provider_name
        );
        println!("\nExample config:");
        println!("  [providers]");
        println!("  anthropic_api_key = \"sk-ant-...\"");
        return Ok(());
    }

    println!("Provider: {}, Model: {}", provider_name, model_name);
    println!("Running inference...\n");

    // Check if this stage is interactive
    let is_interactive = matches!(stage.mode, leviath_core::blueprint::StageMode::Interactive);

    // Build tool filter from stage's available_tools
    let tool_filter: Option<Vec<String>> = if stage.available_tools.is_empty() {
        None
    } else {
        Some(stage.available_tools.clone())
    };
    let tool_filter_ref = tool_filter.as_deref();

    // Build tool result routing config
    let routing_config = stage.tool_result_routing.as_ref().map(|r| {
        leviath_runtime::ToolResultRoutingConfig {
            default_region: r.default_region.clone(),
            tool_overrides: r.tool_overrides.clone(),
            persist: r.persist,
            max_result_tokens: r.max_result_tokens,
        }
    });
    let routing_ref = routing_config.as_ref();

    // Run the inference loop
    let max_iterations = stage.max_iterations.unwrap_or(10);

    if is_interactive {
        // Interactive mode: run inference, show response, prompt for input, repeat
        let mut iteration = 0;
        loop {
            if iteration >= max_iterations {
                println!("\n[Max iterations reached]");
                break;
            }

            let response = engine
                .run_inference_filtered(
                    entity,
                    provider_name,
                    model_name,
                    Vec::new(),
                    tool_filter_ref,
                )
                .await;

            match response {
                Ok(resp) => {
                    println!("\nAssistant: {}", resp.content);
                    println!(
                        "[Tokens: {} input, {} output]",
                        resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                    );

                    // Add response to context
                    if let Some(mut window) = engine.world_mut().get_mut::<leviath_runtime::ContextWindow>(entity) {
                        let tokens = resp.content.len() / 4 + 1;
                        let _ = window.add_to_region(
                            "conversation",
                            format!("Assistant: {}", resp.content),
                            tokens,
                        );
                    }

                    // Prompt for user input
                    print!("\nYou: ");
                    use std::io::Write;
                    std::io::stdout().flush().ok();

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let input = input.trim().to_string();

                    if input.is_empty() || input == "/quit" || input == "/exit" {
                        println!("\n[Session ended]");
                        break;
                    }

                    // Add user input to context
                    if let Some(mut window) = engine.world_mut().get_mut::<leviath_runtime::ContextWindow>(entity) {
                        let tokens = input.len() / 4 + 1;
                        let _ = window.add_to_region(
                            "conversation",
                            format!("User: {}", input),
                            tokens,
                        );
                    }
                }
                Err(e) => {
                    println!("Inference error: {}", e);
                    break;
                }
            }
            iteration += 1;
        }
    } else {
        // Autonomous mode: run the full inference loop
        let response = engine
            .run_inference_loop_filtered(
                entity,
                provider_name,
                model_name,
                Vec::new(),
                max_iterations,
                tool_filter_ref,
                routing_ref,
                &mut |_tool_calls| async { Vec::new() },
            )
            .await;

        match response {
            Ok(resp) => {
                println!("{}", resp.content);
                println!(
                    "\n[Tokens used: {} input, {} output]",
                    resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                );
            }
            Err(e) => {
                println!("Inference error: {}", e);
            }
        }
    }

    Ok(())
}

fn find_manifest(path: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(path);

    // If path is a file named agent.leviath, use it directly
    if path.is_file() && path.file_name() == Some(std::ffi::OsStr::new("agent.leviath")) {
        return Ok(path.to_path_buf());
    }

    // If path is a directory, look for agent.leviath inside it
    if path.is_dir() {
        let manifest = path.join("agent.leviath");
        if manifest.exists() {
            return Ok(manifest);
        }
    }

    // Fall back to current directory
    let current_manifest = PathBuf::from("agent.leviath");
    if current_manifest.exists() {
        return Ok(current_manifest);
    }

    anyhow::bail!(
        "Could not find agent.leviath in {} or current directory",
        path.display()
    )
}

/// Parse an agent.leviath TOML manifest into a Blueprint (public API for other commands).
pub fn parse_manifest_public(content: &str) -> anyhow::Result<Blueprint> {
    parse_manifest(content)
}

/// Parse an agent.leviath TOML manifest into a Blueprint.
fn parse_manifest(content: &str) -> anyhow::Result<Blueprint> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|e| anyhow::anyhow!("Failed to parse agent.leviath: {}", e))?;

    // Parse [agent] section
    let agent = parsed
        .get("agent")
        .ok_or_else(|| anyhow::anyhow!("Missing [agent] section"))?;

    let name = agent
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed")
        .to_string();
    let version = agent
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.1.0")
        .to_string();
    let description = agent
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Parse [stages.*] sections
    let mut stages = Vec::new();
    if let Some(stages_table) = parsed.get("stages").and_then(|v| v.as_table()) {
        for (stage_name, stage_value) in stages_table {
            let model_table = stage_value.get("model").and_then(|v| v.as_table());
            let model_config = if let Some(mt) = model_table {
                ModelConfig::new(
                    mt.get("provider")
                        .and_then(|v| v.as_str())
                        .unwrap_or("anthropic")
                        .to_string(),
                    mt.get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("claude-sonnet-4-5")
                        .to_string(),
                )
            } else {
                ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-5".to_string())
            };

            let mut stage = Stage::new(stage_name.clone(), model_config);

            // Parse mode
            if let Some(mode_str) = stage_value.get("mode").and_then(|v| v.as_str()) {
                stage = match mode_str {
                    "interactive" => stage.with_mode(leviath_core::blueprint::StageMode::Interactive),
                    _ => stage.with_mode(leviath_core::blueprint::StageMode::Autonomous),
                };
            }

            // Parse max_iterations
            if let Some(max_iter) = stage_value.get("max_iterations").and_then(|v| v.as_integer()) {
                stage.max_iterations = Some(max_iter as usize);
            }

            // Parse available_tools
            if let Some(tools_arr) = stage_value.get("available_tools").and_then(|v| v.as_array()) {
                stage.available_tools = tools_arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
            }

            // Parse tool_routing
            if let Some(routing_table) = stage_value.get("tool_routing").and_then(|v| v.as_table()) {
                let mut routing = ToolResultRouting::default();

                if let Some(dr) = routing_table.get("default_region").and_then(|v| v.as_str()) {
                    routing.default_region = dr.to_string();
                }
                if let Some(p) = routing_table.get("persist").and_then(|v| v.as_bool()) {
                    routing.persist = p;
                }
                if let Some(mt) = routing_table.get("max_result_tokens").and_then(|v| v.as_integer()) {
                    routing.max_result_tokens = Some(mt as usize);
                }
                if let Some(overrides_table) = routing_table.get("overrides").and_then(|v| v.as_table()) {
                    for (tool_name, region_val) in overrides_table {
                        if let Some(region_name) = region_val.as_str() {
                            routing.tool_overrides.insert(tool_name.clone(), region_name.to_string());
                        }
                    }
                }

                stage.tool_result_routing = Some(routing);
            }

            stages.push(stage);
        }
    }

    if stages.is_empty() {
        stages.push(Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-5".to_string()),
        ));
    }

    // Parse [context.regions] section
    let mut regions = Vec::new();
    let mut total_tokens = 0usize;

    if let Some(regions_table) = parsed
        .get("context")
        .and_then(|v| v.get("regions"))
        .and_then(|v| v.as_table())
    {
        for (region_name, region_value) in regions_table {
            let max_tokens = region_value
                .get("max_tokens")
                .and_then(|v| v.as_integer())
                .unwrap_or(5000) as usize;

            let kind_str = region_value
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("temporary");

            let kind = match kind_str {
                "pinned" => RegionKind::Pinned,
                "sliding_window" => {
                    let max_items = region_value
                        .get("max_items")
                        .and_then(|v| v.as_integer())
                        .unwrap_or(10) as usize;
                    RegionKind::SlidingWindow { max_items }
                }
                "temporary" => RegionKind::Temporary,
                "compacting" => {
                    let threshold = region_value
                        .get("threshold_tokens")
                        .and_then(|v| v.as_integer())
                        .unwrap_or((max_tokens as i64) * 8 / 10) as usize;
                    RegionKind::Compacting {
                        threshold_tokens: threshold,
                    }
                }
                "clearable" => RegionKind::Clearable,
                "compact_history" => {
                    let source = region_value
                        .get("source_region")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    RegionKind::CompactHistory {
                        source_region: source,
                    }
                }
                _ => RegionKind::Temporary,
            };

            total_tokens += max_tokens;
            regions.push(RegionDefinition::new(
                region_name.clone(),
                kind,
                max_tokens,
            ));
        }
    }

    if regions.is_empty() {
        regions.push(RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            2000,
        ));
        regions.push(RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow { max_items: 10 },
            10000,
        ));
        total_tokens = 12000;
    }

    let layout = ContextLayout::new(regions, total_tokens);

    let mut blueprint = Blueprint::new(name, description, stages, layout);
    blueprint.version = version;

    Ok(blueprint)
}
