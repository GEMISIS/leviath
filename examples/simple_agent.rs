//! Simple agent example
//!
//! Demonstrates creating a minimal agent with basic context regions.

use leviath_core::{Blueprint, Stage, ContextLayout, RegionDefinition, RegionKind, blueprint::ModelConfig};
use leviath_runtime::{AgentEngine, AgentPool};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Define context layout with a simple pinned region
    let regions = vec![
        RegionDefinition::new("identity".to_string(), RegionKind::Pinned, 2000)
            .with_description("Agent identity and constraints".to_string()),
        RegionDefinition::new("conversation".to_string(), 
            RegionKind::SlidingWindow { max_items: 10 }, 8000)
            .with_description("Recent conversation history".to_string()),
    ];

    let layout = ContextLayout::new(regions, 10000)
        .with_eviction_order(vec!["conversation".to_string()]);

    // Define a single-stage agent
    let stages = vec![
        Stage::new("respond".to_string(), 
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string())),
    ];

    // Create blueprint
    let blueprint = Blueprint::new(
        "simple-assistant".to_string(),
        "A simple conversational assistant".to_string(),
        stages,
        layout,
    );

    // Create engine and pool
    let mut engine = AgentEngine::new();
    let mut pool = AgentPool::new(blueprint);

    // Spawn an agent
    let agent_id = pool.spawn_agent(engine.world_mut());
    println!("Spawned agent: {}", agent_id);

    // Run one tick
    engine.tick();
    println!("Agent executed successfully!");

    Ok(())
}
