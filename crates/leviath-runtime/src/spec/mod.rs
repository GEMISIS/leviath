//! The run graph: stages, the edges between them, and the context layout.
//!
//! This is the vocabulary a spawn is written in. It lives beside the ECS world
//! that runs it, above the leaf values in `leviath-core` that tools and
//! providers share.

pub mod blueprint;
pub mod env;
pub mod graph;
pub mod inputs;
pub mod issues;
pub mod launch;
pub mod layout;
pub mod manifest;
pub mod names;
pub mod request;
pub mod run_spec;

pub use blueprint::{
    Blueprint, ContextTransform, EdgeTransform, FileTrackingConfig, NudgeConfig, ReadPathsConfig,
    RepetitionDetectionConfig, ResolvedNudge, Stage, StuckConfig, ToolResultRouting,
    TransitionCondition, TransitionEdge, resolve_nudge,
};
pub use layout::{BudgetSpec, ContextLayout, RegionDefinition};
