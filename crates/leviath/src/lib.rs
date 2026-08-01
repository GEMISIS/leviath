//! Leviath as a library: one dependency that pulls in the whole agent
//! runtime, re-exported under a single namespace.
//!
//! Leviath is a structured agent runtime for LLMs: context memory laid out in
//! regions with token budgets, multi-stage workflows described by blueprints,
//! and an ECS-based execution engine. The `lev` binary (the `leviath-cli`
//! crate) is the packaged product; this crate is for embedding the same
//! machinery in your own application.
//!
//! Build a world, spawn an agent, watch the events:
//!
//! ```no_run
//! use leviath::prelude::*;
//!
//! # async fn embed() -> std::result::Result<(), Box<dyn std::error::Error>> {
//! let world = AgentWorld::builder()
//!     .provider(ProviderCreds {
//!         api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
//!         ..ProviderCreds::simple("anthropic")
//!     })
//!     .build()?;
//!
//! let mut events = world.events();
//! let run = world
//!     .spawn(SpawnSpec::new(
//!         BlueprintSource::Path("coder.leviath".into()),
//!         "Build a CSV parser",
//!         std::env::current_dir()?,
//!     ))
//!     .await?;
//!
//! while let Some(event) = events.next().await {
//!     match event {
//!         AgentEvent::StageTransition { from, to, .. } => println!("{from} -> {to}"),
//!         AgentEvent::ToolCallStarted { tool, .. } => println!("running {tool}"),
//!         AgentEvent::Interaction { request, .. } => {
//!             // The agent asked a question: answer it via world.answer(...).
//!         }
//!         AgentEvent::Completed { run_id, status, .. } if run_id == run.as_ref() => break,
//!         _ => {}
//!     }
//! }
//! world.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! Each module below is a re-export of one of the underlying workspace
//! crates, so `leviath::core` is `leviath-core`, `leviath::runtime` is
//! `leviath-runtime`, and so on. Apps that only need one layer can depend on
//! that crate directly and skip the rest.
//!
//! This crate contains no code of its own, only re-exports, and CI enforces
//! that (see `guard-facade` in the repository's ci.yml). Behavior lives in,
//! and is tested in, the crates re-exported here.

/// Core types and traits: context regions, memory layouts, blueprint
/// manifests, policies, and lifecycle configuration (`leviath-core`).
pub use leviath_core as core;

/// The ECS-based execution engine: agent state, the stage pipeline,
/// persistence, and provider wiring (`leviath-runtime`).
pub use leviath_runtime as runtime;

/// LLM provider integrations: Anthropic, OpenAI, Gemini, Ollama, OpenRouter,
/// and Rhai-scripted providers (`leviath-providers`).
pub use leviath_providers as providers;

/// Native built-in tools available to agents (`leviath-tools`).
pub use leviath_tools as tools;

/// Model Context Protocol client support: discovery, execution, and OAuth
/// (`leviath-mcp`).
pub use leviath_mcp as mcp;

/// Rhai scripting integration: custom validators, transforms, script tools,
/// and dynamic logic (`leviath-scripting`).
pub use leviath_scripting as scripting;

/// OpenTelemetry export for the telemetry event stream (`leviath-telemetry`).
pub use leviath_telemetry as telemetry;

/// Agent packaging, sharing, and installation (`leviath-package`).
pub use leviath_package as package;

/// Agent Client Protocol (JSON-RPC over stdio) wire types and mappings
/// (`leviath-agent-client`).
pub use leviath_agent_client as agent_client;

/// The types most embeddings touch first, importable in one line.
pub mod prelude {
    pub use leviath_core::interaction::{InteractionRequest, InteractionResponse};
    pub use leviath_core::{
        Blueprint, BudgetSpec, ContextLayout, Error, PolicyConfig, RegionDefinition, Result,
    };
    pub use leviath_runtime::{
        AgentEvent, AgentState, AgentStatus, AgentWorld, AgentWorldBuilder, BasicToolService,
        BlueprintSource, ContextWindow, EmbedError, EventStream, ProviderCreds, ProviderRegistry,
        RunId, SpawnSpec, ToolService, WorldEvent, build_provider_registry,
    };
}
