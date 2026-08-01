//! Embedding the runtime as a library: the batteries-included assembly a
//! Rust application uses to run agents in-process, without the `lev` CLI,
//! the daemon, or a config file.
//!
//! The daemon remains the primary interface for the CLI; this module is the
//! same machinery ([`PipelineWorld`](crate::world::PipelineWorld) +
//! [`WorldHost`](crate::host::WorldHost)) assembled from plain values.

mod error;
mod spawner;
mod stream;
mod tool_service;
mod world;

pub use error::EmbedError;
pub use stream::EventStream;
pub use tool_service::BasicToolService;
pub use world::{AgentWorld, AgentWorldBuilder, BlueprintSource, RunId, SpawnSpec};
