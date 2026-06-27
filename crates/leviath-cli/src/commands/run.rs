//! `lev run` - Run an agent

use clap::Args;
use leviath_core::blueprint::{ModelConfig, StageMode, ToolResultRouting};
use leviath_core::lifecycle::CompactionConfig;
use leviath_core::{Blueprint, ContextLayout, Region, RegionKind, Stage};
use leviath_core::layout::RegionDefinition;
use leviath_runtime::{AgentEngine, AgentPool, ContextWindow, ProviderRegistry};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_stream::StreamExt;

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

    // Validate API key formats
    for warning in config.validate_keys() {
        println!("Warning: {}", warning);
    }

    // Create provider registry
    let registry = build_provider_registry(&config);

    // Create engine with providers
    let mut engine = AgentEngine::with_providers(registry);

    // Create agent pool and spawn agent
    let mut pool = AgentPool::new(blueprint.clone());
    let agent_id = pool.spawn_agent(engine.world_mut());
    let entity = pool
        .get_agent(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("Failed to get spawned agent entity"))?;

    // Initialize context window regions from blueprint layout
    initialize_context_window(&mut engine, entity, &blueprint, &args.task);

    // Get compaction config from blueprint (or use default if blueprint has compacting regions)
    let compaction_config = blueprint.compaction_config.clone();
    let compaction_ref = compaction_config.as_ref();

    // Run through ALL stages sequentially
    let num_stages = blueprint.stages.len();
    for (stage_idx, stage) in blueprint.stages.iter().enumerate() {
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
            println!("\nOr set the ANTHROPIC_API_KEY environment variable.");
            return Ok(());
        }

        println!(
            "\n--- Stage {}/{}: {} ({}:{}) ---",
            stage_idx + 1,
            num_stages,
            stage.name,
            provider_name,
            model_name,
        );

        // If stage has its own context_layout, swap to it
        if let Some(ref stage_layout) = stage.context_layout {
            swap_context_layout(&mut engine, entity, stage_layout);
        }

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

        let max_iterations = stage.max_iterations.unwrap_or(10);

        // Run stage based on mode
        match &stage.mode {
            StageMode::Interactive => {
                run_interactive_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    tool_filter_ref,
                )
                .await?;
            }
            StageMode::InteractivePoints { points } => {
                run_interactive_points_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    tool_filter_ref,
                    routing_ref,
                    points,
                    compaction_ref,
                )
                .await?;
            }
            StageMode::Autonomous => {
                run_autonomous_stage(
                    &mut engine,
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                    tool_filter_ref,
                    routing_ref,
                    compaction_ref,
                )
                .await?;
            }
        }

        // Add stage transition marker if not the last stage
        if stage_idx + 1 < num_stages {
            let next_name = &blueprint.stages[stage_idx + 1].name;
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let marker = format!(
                    "[Stage complete: {}, transitioning to: {}]",
                    stage.name, next_name
                );
                let tokens = marker.len() / 4 + 1;
                let _ = window.add_to_region("conversation", marker, tokens);
            }
        }
    }

    println!("\n[All stages complete]");
    Ok(())
}

/// Run an interactive stage with streaming output.
async fn run_interactive_stage(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tool_filter: Option<&[String]>,
) -> anyhow::Result<()> {
    let mut iteration = 0;
    loop {
        if iteration >= max_iterations {
            println!("\n[Max iterations reached]");
            break;
        }

        // Use streaming for interactive mode
        let response = match stream_inference(engine, entity, provider_name, model_name, tool_filter).await {
            Ok(resp) => resp,
            Err(e) => {
                // Fall back to non-streaming
                tracing::debug!("Streaming unavailable, falling back: {}", e);
                let resp = engine
                    .run_inference_filtered(entity, provider_name, model_name, Vec::new(), tool_filter)
                    .await
                    .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;
                println!("\nAssistant: {}", resp.content);
                resp
            }
        };

        println!(
            "\n[Tokens: {} input, {} output]",
            response.tokens_used.prompt_tokens, response.tokens_used.completion_tokens
        );

        // Add response to context
        if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
            let tokens = response.content.len() / 4 + 1;
            let _ = window.add_to_region(
                "conversation",
                format!("Assistant: {}", response.content),
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
        if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
            let tokens = input.len() / 4 + 1;
            let _ = window.add_to_region(
                "conversation",
                format!("User: {}", input),
                tokens,
            );
        }

        iteration += 1;
    }
    Ok(())
}

/// Run an autonomous stage.
#[allow(clippy::too_many_arguments)]
async fn run_autonomous_stage(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tool_filter: Option<&[String]>,
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    compaction_config: Option<&CompactionConfig>,
) -> anyhow::Result<()> {
    let response = engine
        .run_inference_loop_filtered(
            entity,
            provider_name,
            model_name,
            Vec::new(),
            max_iterations,
            tool_filter,
            routing,
            compaction_config,
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
    Ok(())
}

/// Run an InteractivePoints stage: run iterations autonomously, pausing at each interaction point.
#[allow(clippy::too_many_arguments)]
async fn run_interactive_points_stage(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tool_filter: Option<&[String]>,
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    points: &[leviath_core::blueprint::InteractionPoint],
    compaction_config: Option<&CompactionConfig>,
) -> anyhow::Result<()> {
    if points.is_empty() {
        return run_autonomous_stage(engine, entity, provider_name, model_name, max_iterations, tool_filter, routing, compaction_config).await;
    }

    // Divide iterations across interaction points
    let segments = points.len() + 1;
    let iterations_per_segment = max_iterations / segments;
    let mut remaining_iterations = max_iterations;

    for point in points {
        let iters = iterations_per_segment.min(remaining_iterations);
        if iters > 0 {
            let response = engine
                .run_inference_loop_filtered(
                    entity,
                    provider_name,
                    model_name,
                    Vec::new(),
                    iters,
                    tool_filter,
                    routing,
                    compaction_config,
                    &mut |_tool_calls| async { Vec::new() },
                )
                .await;

            if let Ok(resp) = response {
                if !resp.content.is_empty() {
                    println!("{}", resp.content);
                }
            }
            remaining_iterations = remaining_iterations.saturating_sub(iters);
        }

        // Show interaction point
        println!("\n[Interaction Point: {}]", point.name);
        println!("{}", point.prompt);

        if point.required {
            print!("\n> ");
            use std::io::Write;
            std::io::stdout().flush().ok();

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let input = input.trim().to_string();

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = input.len() / 4 + 1;
                let content = format!("User [{}]: {}", point.name, input);
                let _ = window.add_to_region("conversation", content, tokens);
            }
        } else {
            println!("(Press Enter to skip or type a response)");
            print!("\n> ");
            use std::io::Write;
            std::io::stdout().flush().ok();

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            let input = input.trim().to_string();

            if !input.is_empty() {
                if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                    let tokens = input.len() / 4 + 1;
                    let content = format!("User [{}]: {}", point.name, input);
                    let _ = window.add_to_region("conversation", content, tokens);
                }
            }
        }
    }

    // Run remaining iterations
    if remaining_iterations > 0 {
        let response = engine
            .run_inference_loop_filtered(
                entity,
                provider_name,
                model_name,
                Vec::new(),
                remaining_iterations,
                tool_filter,
                routing,
                compaction_config,
                &mut |_tool_calls| async { Vec::new() },
            )
            .await;

        if let Ok(resp) = response {
            if !resp.content.is_empty() {
                println!("{}", resp.content);
            }
            println!(
                "\n[Tokens used: {} input, {} output]",
                resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
            );
        }
    }

    Ok(())
}

/// Stream inference output directly to stdout, collecting the full response.
async fn stream_inference(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    tool_filter: Option<&[String]>,
) -> anyhow::Result<leviath_providers::InferenceResponse> {
    let provider = engine
        .get_provider(provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not registered", provider_name))?;

    // Build messages from context window
    let (messages, max_tokens) = {
        let window = engine
            .world()
            .get::<ContextWindow>(entity)
            .ok_or_else(|| anyhow::anyhow!("Entity has no ContextWindow"))?;

        let messages = window.assemble_messages();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let max_tokens = remaining.min(4096);
        (messages, max_tokens)
    };

    // Build filtered tools list (empty for now — tools come from MCP in the future)
    let tools: Vec<leviath_providers::Tool> = Vec::new();
    let filtered_tools = if let Some(filter) = tool_filter {
        if filter.is_empty() {
            tools
        } else {
            tools
                .into_iter()
                .filter(|t| filter.iter().any(|f| f == &t.name))
                .collect()
        }
    } else {
        tools
    };

    let request = leviath_providers::InferenceRequest {
        messages,
        model: model_name.to_string(),
        max_tokens,
        temperature: 0.7,
        tools: filtered_tools,
        extra: serde_json::Value::Null,
    };

    let mut stream = provider.infer_stream(request).await
        .map_err(|e| anyhow::anyhow!("Stream error: {}", e))?;

    let mut full_content = String::new();
    let mut final_tokens = None;
    let mut final_finish_reason = None;
    let mut all_tool_calls: Vec<leviath_providers::ToolCall> = Vec::new();

    print!("\nAssistant: ");
    use std::io::Write;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| anyhow::anyhow!("Stream chunk error: {}", e))?;

        if !chunk.delta.is_empty() {
            print!("{}", chunk.delta);
            std::io::stdout().flush().ok();
            full_content.push_str(&chunk.delta);
        }

        if let Some(tokens) = chunk.tokens {
            final_tokens = Some(tokens);
        }
        if let Some(reason) = chunk.finish_reason {
            final_finish_reason = Some(reason);
        }

        // Accumulate tool calls from stream deltas
        for tc_delta in &chunk.tool_calls {
            while all_tool_calls.len() <= tc_delta.index {
                all_tool_calls.push(leviath_providers::ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: serde_json::Value::Null,
                });
            }
            let tc = &mut all_tool_calls[tc_delta.index];
            if let Some(ref id) = tc_delta.id {
                tc.id.clone_from(id);
            }
            if let Some(ref name) = tc_delta.name {
                tc.name.clone_from(name);
            }
            if !tc_delta.arguments_delta.is_empty() && tc.arguments.is_null() {
                if let Ok(val) = serde_json::from_str(&tc_delta.arguments_delta) {
                    tc.arguments = val;
                }
            }
        }
    }

    println!(); // newline after streaming

    // Update agent state iteration
    if let Some(mut state) = engine.world_mut().get_mut::<leviath_runtime::AgentState>(entity) {
        state.iteration += 1;
    }

    let tokens_used = final_tokens.unwrap_or(leviath_providers::TokenUsage {
        prompt_tokens: 0,
        completion_tokens: full_content.len() / 4,
        total_tokens: full_content.len() / 4,
    });

    Ok(leviath_providers::InferenceResponse {
        content: full_content,
        tool_calls: all_tool_calls,
        tokens_used,
        finish_reason: final_finish_reason.unwrap_or(leviath_providers::FinishReason::Complete),
    })
}

/// Build a ProviderRegistry from Config.
pub fn build_provider_registry(config: &Config) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();

    if let Some(ref key) = config.providers.anthropic_api_key {
        registry.register(
            "anthropic".to_string(),
            Arc::new(leviath_providers::AnthropicProvider::new(key.clone())),
        );
    }

    if let Some(ref key) = config.providers.openai_api_key {
        registry.register(
            "openai".to_string(),
            Arc::new(leviath_providers::OpenAIProvider::new(key.clone())),
        );
    }

    if let Some(ref key) = config.openrouter_api_key {
        registry.register(
            "openrouter".to_string(),
            Arc::new(leviath_providers::OpenRouterProvider::new(key.clone())),
        );
    }

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

    registry
}

/// Initialize context window regions on an entity from the blueprint.
pub fn initialize_context_window(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    blueprint: &Blueprint,
    task: &str,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        for region_def in &blueprint.context_layout.regions {
            let region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );
            window.add_region(region);
        }

        // Add a tool_results region if not present
        if window.get_region("tool_results").is_none() {
            let tool_region = Region::new(
                "tool_results".to_string(),
                RegionKind::Temporary,
                5000,
            );
            window.add_region(tool_region);
        }

        // Add a conversation region if not present
        if window.get_region("conversation").is_none() {
            let conv_region = Region::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow { max_items: 50 },
                10000,
            );
            window.add_region(conv_region);
        }

        // Add task to the first pinned/system region
        let system_region_name = blueprint
            .context_layout
            .regions
            .iter()
            .find(|r| matches!(r.kind, RegionKind::Pinned))
            .map(|r| r.name.clone());

        if let Some(region_name) = system_region_name {
            let task_tokens = task.len() / 4 + 1;
            let _ = window.add_to_region(&region_name, task.to_string(), task_tokens);
        }
    }
}

/// Swap context layout to a stage-specific layout (preserving existing content where possible).
fn swap_context_layout(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    layout: &ContextLayout,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let mut new_regions = Vec::new();
        for region_def in &layout.regions {
            let mut new_region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );

            // Copy content from existing region with same name
            if let Some(existing) = window.get_region(&region_def.name) {
                for entry in &existing.content {
                    let _ = new_region.add_entry(entry.content.clone(), entry.tokens);
                }
            }

            new_regions.push(new_region);
        }

        window.regions = new_regions;
        window.current_tokens = window.calculate_tokens();
    }
}

fn find_manifest(path: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(path);

    if path.is_file() && path.file_name() == Some(std::ffi::OsStr::new("agent.leviath")) {
        return Ok(path.to_path_buf());
    }

    if path.is_dir() {
        let manifest = path.join("agent.leviath");
        if manifest.exists() {
            return Ok(manifest);
        }
    }

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
                    "interactive" => stage.with_mode(StageMode::Interactive),
                    "interactive_points" => {
                        let mut points = Vec::new();
                        if let Some(pts_arr) = stage_value.get("interaction_points").and_then(|v| v.as_array()) {
                            for pt in pts_arr {
                                let pt_name = pt.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let pt_prompt = pt.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let pt_required = pt.get("required").and_then(|v| v.as_bool()).unwrap_or(true);
                                points.push(leviath_core::blueprint::InteractionPoint {
                                    name: pt_name,
                                    prompt: pt_prompt,
                                    required: pt_required,
                                });
                            }
                        }
                        stage.with_mode(StageMode::InteractivePoints { points })
                    }
                    _ => stage.with_mode(StageMode::Autonomous),
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

    // Parse optional [compaction] section
    if let Some(compaction_table) = parsed.get("compaction").and_then(|v| v.as_table()) {
        let mut cc = CompactionConfig::default();

        if let Some(provider) = compaction_table.get("provider").and_then(|v| v.as_str()) {
            cc.provider = provider.to_string();
        }
        if let Some(model) = compaction_table.get("model").and_then(|v| v.as_str()) {
            cc.model = model.to_string();
        }
        if let Some(sp) = compaction_table.get("system_prompt").and_then(|v| v.as_str()) {
            cc.system_prompt = Some(sp.to_string());
        }
        if let Some(mst) = compaction_table.get("max_summary_tokens").and_then(|v| v.as_integer()) {
            cc.max_summary_tokens = mst as usize;
        }
        if let Some(temp) = compaction_table.get("temperature").and_then(|v| v.as_float()) {
            cc.temperature = temp as f32;
        }

        blueprint.compaction_config = Some(cc);
    }

    Ok(blueprint)
}
