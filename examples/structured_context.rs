//! Structured context example
//!
//! Demonstrates tiered context management with all region types.

use leviath_core::{Blueprint, Stage, ContextLayout, RegionDefinition, RegionKind, blueprint::ModelConfig};

fn main() -> anyhow::Result<()> {
    println!("=== Structured Context Example ===\n");

    // Define a comprehensive context layout with all region types
    let regions = vec![
        // Pinned: Never evicted - architecture and core constraints
        RegionDefinition::new("architecture".to_string(), RegionKind::Pinned, 4000)
            .with_description("System architecture diagrams and design docs".to_string()),
        
        RegionDefinition::new("constraints".to_string(), RegionKind::Pinned, 2000)
            .with_description("Hard constraints and requirements".to_string()),

        // SlidingWindow: Last N items - conversation history
        RegionDefinition::new("conversation".to_string(), 
            RegionKind::SlidingWindow { max_items: 20 }, 15000)
            .with_description("Recent conversation turns".to_string()),

        // Temporary: First evicted - tool outputs
        RegionDefinition::new("tool_results".to_string(), RegionKind::Temporary, 40000)
            .with_description("Recent tool execution results".to_string()),

        // Compacting: Summarized when full - long-term context
        RegionDefinition::new("historical_context".to_string(), 
            RegionKind::Compacting { threshold_tokens: 10000 }, 15000)
            .with_description("Summarized historical context".to_string()),
    ];

    let layout = ContextLayout::new(regions, 80000)
        .with_eviction_order(vec![
            "tool_results".to_string(),
            "historical_context".to_string(),
            "conversation".to_string(),
        ]);

    println!("Context Layout:");
    println!("  Total budget: {} tokens", layout.total_budget_tokens);
    println!("  Regions: {}", layout.regions.len());
    println!("\nRegion Details:");
    for region in &layout.regions {
        println!("  - {}: {:?} ({} tokens)", 
            region.name, region.kind, region.max_tokens);
        if let Some(desc) = &region.description {
            println!("    {}", desc);
        }
    }

    println!("\nEviction Order:");
    for (i, region_name) in layout.eviction_order.iter().enumerate() {
        println!("  {}. {} (evicted first)", i + 1, region_name);
    }

    // Create a multi-stage blueprint
    let stages = vec![
        Stage::new("analyze".to_string(), 
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string())),
        Stage::new("implement".to_string(), 
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
        Stage::new("review".to_string(), 
            ModelConfig::new("anthropic".to_string(), "claude-opus-4".to_string())),
    ];

    let blueprint = Blueprint::new(
        "coding-agent".to_string(),
        "Multi-stage coding agent with structured context".to_string(),
        stages,
        layout,
    );

    println!("\nBlueprint: {}", blueprint.name);
    println!("  Stages: {}", blueprint.stages.len());
    for stage in &blueprint.stages {
        println!("    - {}: {} ({})", 
            stage.name, stage.model.model, stage.model.provider);
    }

    // Validate blueprint
    match blueprint.validate() {
        Ok(_) => println!("\n✓ Blueprint is valid"),
        Err(e) => println!("\n✗ Blueprint validation failed: {}", e),
    }

    Ok(())
}
