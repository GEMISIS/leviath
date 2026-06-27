//! `lev spawn` - Spawn an agent from a blueprint

use clap::Args;
use std::sync::Arc;

use crate::config::Config;

#[derive(Args)]
pub struct SpawnArgs {
    /// Blueprint name
    #[arg(value_name = "BLUEPRINT")]
    pub blueprint: String,

    /// Number of agents to spawn
    #[arg(short, long, default_value = "1")]
    pub count: usize,
}

pub async fn execute(args: SpawnArgs) -> anyhow::Result<()> {
    tracing::info!(blueprint = %args.blueprint, count = args.count, "Spawning agents");

    // Look up blueprint in ~/.leviath/agents/{name}/agent.leviath
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let manifest_path = home
        .join(".leviath")
        .join("agents")
        .join(&args.blueprint)
        .join("agent.leviath");

    if !manifest_path.exists() {
        anyhow::bail!(
            "Blueprint '{}' not found. Expected at: {}\nRun `lev list` to see installed agents.",
            args.blueprint,
            manifest_path.display()
        );
    }

    // Parse the manifest
    let content = std::fs::read_to_string(&manifest_path)?;
    let blueprint = super::run::parse_manifest_public(&content)?;

    println!(
        "Spawning {} agent(s) from blueprint: {} v{}",
        args.count, blueprint.name, blueprint.version
    );

    // Create engine with providers
    let config = Config::load()?;
    let mut registry = leviath_runtime::ProviderRegistry::new();

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

    let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
    let mut pool = leviath_runtime::AgentPool::new(blueprint);

    let mut agent_ids = Vec::new();
    for _ in 0..args.count {
        let agent_id = pool.spawn_agent(engine.world_mut());
        agent_ids.push(agent_id);
    }

    println!("Spawned agents:");
    for id in &agent_ids {
        println!("  - {}", id);
    }

    Ok(())
}
