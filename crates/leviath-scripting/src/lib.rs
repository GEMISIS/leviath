//! # Leviath Scripting
//!
//! Rhai scripting integration for custom validators, transforms, and dynamic logic.
//!
//! This crate provides a sandboxed Rhai engine that allows users to define custom
//! validators, context transforms, and compaction strategies without modifying
//! Leviath's core code.

pub mod engine;
pub mod functions;
pub mod sandbox;
pub mod tool;
pub mod types;

pub use engine::ScriptEngine;
pub use sandbox::SandboxConfig;
pub use tool::{
    ParamSpec, ScriptHost, ScriptTool, ScriptToolMeta, ScriptToolSet, SkippedTool,
    execute as execute_script_tool,
};

use thiserror::Error;

/// Result type alias using Scripting's Error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Error types for scripting operations.
#[derive(Error, Debug)]
pub enum Error {
    /// Script execution failed
    #[error("Script execution failed: {0}")]
    ExecutionFailed(String),

    /// Script compilation failed
    #[error("Script compilation failed: {0}")]
    CompilationFailed(String),

    /// Script validation failed
    #[error("Script validation failed: {0}")]
    ValidationFailed(String),

    /// Rhai engine error
    #[error("Rhai error: {0}")]
    RhaiError(#[from] Box<rhai::EvalAltResult>),

    /// Core error
    #[error("Core error: {0}")]
    CoreError(#[from] leviath_core::Error),
}
