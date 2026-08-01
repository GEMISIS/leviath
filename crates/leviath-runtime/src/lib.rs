//! # Leviath Runtime
//!
//! ECS-based agent execution engine using bevy_ecs.
//!
//! The runtime manages agent lifecycle, context window management, task scheduling,
//! and inference execution through a game-loop-inspired architecture where agents
//! are entities and their behaviors are systems.
//!
//! ## Embedding
//!
//! [`AgentWorld`] is the front door for running agents inside your own
//! application - no `lev` CLI, daemon, or config file. Build a world from
//! plain values, spawn an agent, and drive the event stream:
//!
//! ```no_run
//! use leviath_runtime::{AgentWorld, BlueprintSource, ProviderCreds, SpawnSpec, WorldEvent};
//!
//! # async fn embed() -> Result<(), Box<dyn std::error::Error>> {
//! let world = AgentWorld::builder()
//!     .provider(ProviderCreds {
//!         name: "anthropic".to_string(),
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
//!         WorldEvent::StageTransition { from, to, .. } => println!("{from} -> {to}"),
//!         WorldEvent::ToolCallStarted { tool, .. } => println!("running {tool}"),
//!         WorldEvent::Interaction { request, .. } => {
//!             // The agent asked a question: answer via world.answer(...).
//!         }
//!         WorldEvent::Completed { run_id, status, .. } if run_id == run.as_ref() => break,
//!         _ => {}
//!     }
//! }
//! world.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! ## Stability layers
//!
//! - [`AgentWorld`] and the other [`embed`] types are the stable embedding
//!   surface.
//! - [`WorldHost`] and [`PipelineWorld`] are the semi-stable machinery both
//!   the daemon and `AgentWorld` are built on; use them when you need your
//!   own assembly (custom spawners, hooks, tick control).
//! - The raw ECS underneath ([`PipelineWorld::world_mut`]) is the unstable
//!   escape hatch: it tracks this crate's `bevy_ecs` version (re-exported as
//!   [`ecs`]) and carries no compatibility promise.

pub(crate) mod cancel;
pub(crate) mod compaction_bridge;
pub mod components;
pub mod context_setup;
pub(crate) mod context_tools;
pub(crate) mod context_transform;
#[cfg(feature = "control-socket")]
pub mod control_socket;
pub mod custom_region;
pub mod dynamic_interaction;
pub mod embed;
pub mod fanout;
pub(crate) mod gate_prompt;
pub mod host;
pub(crate) mod inference_bridge;
pub mod inference_pool;
pub mod interaction_hub;
pub mod interaction_points;
pub(crate) mod lane_supervisor;
pub mod persistence;
pub(crate) mod persistence_bridge;
pub mod pipeline;
pub mod provider_creds;
pub(crate) mod providers;
pub(crate) mod repetition;
pub mod restore;
pub mod script_provider;
pub mod taint;
pub mod telemetry;
pub(crate) mod tick_scope;
pub mod title;
pub(crate) mod title_bridge;
pub mod tool_bridge;
pub mod world;
// test_support.rs gates itself with an inner `#![cfg(test)]` attribute, so no
// `#[cfg(test)]` is needed here (adding one would trigger clippy's
// `duplicated_attributes` lint under `-D warnings`).
mod test_support;

pub use components::{AgentState, AgentStatus, ContextWindow, ParentRef, SubAgentChildren};
pub use embed::{
    AgentWorld, AgentWorldBuilder, BasicToolService, BlueprintSource, EmbedError, EventStream,
    RunId, SpawnSpec,
};
pub use fanout::{FanOutSpawner, FanOutSpawnerRes};
pub use host::{ControlOp, SpawnArgs, WorldEvent, WorldHost};
pub use inference_bridge::RetryPolicy;
pub use inference_pool::{InferencePoolConfig, InferencePools};
pub use interaction_hub::InteractionHub;
pub use pipeline::{ModelDefaults, ResolvedStage, ToolService};
pub use provider_creds::{ProviderCreds, build_provider_registry};
pub use providers::ProviderRegistry;
pub use taint::TaintGate;
pub use tool_bridge::BoxedToolExec;
pub use world::{PipelineWorld, TickOutcome};

/// The name issue-facing docs use for the world's event enum; the same type
/// as [`WorldEvent`].
pub type AgentEvent = WorldEvent;

/// The `bevy_ecs` version this runtime is built against, for code that
/// reaches through [`PipelineWorld::world_mut`] into the raw ECS. Depending
/// on this re-export (instead of your own `bevy_ecs`) keeps the versions
/// aligned.
pub use bevy_ecs as ecs;
