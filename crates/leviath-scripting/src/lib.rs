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

/// Apply the sandbox limits every Leviath Rhai engine shares.
///
/// One function rather than a block copied into each engine constructor. There
/// were three such copies - `ScriptEngine::new`, `build_tool_engine` (whose
/// comment read "Same hardening as `ScriptEngine::new`", which it was not
/// entirely), and the provider engine - with divergent limits and no way to add
/// a control to all of them at once. This is a security control; it should have
/// exactly one definition.
///
/// `max_operations` stays a parameter because it is a genuine policy difference:
/// a provider script driving a streaming HTTP response legitimately runs longer
/// than a validator.
pub fn harden(engine: &mut rhai::Engine, max_operations: u64) {
    // Bound runaway loops. The only wall-clock limit on pure computation.
    engine.set_max_operations(max_operations);
    engine.set_max_string_size(1_000_000);
    engine.set_max_array_size(10_000);
    engine.set_max_map_size(10_000);
    // Bound *recursion*: without a call-depth cap, a script recursing to
    // exhaustion overflows the native stack, which aborts the process rather
    // than raising a catchable Rhai error. This was previously set nowhere.
    engine.set_max_call_levels(64);
    // Generous expression nesting. Rhai's default is much lower in debug builds
    // (a stack-overflow guard for unoptimized code) and would reject legitimate
    // scripts under `cargo test`.
    engine.set_max_expr_depths(128, 128);
    // `eval` compiles a fresh string at runtime, and the default module resolver
    // lets `import` pull another `.rhai` off disk relative to the process CWD.
    // Both reach code that never passed whatever review the script itself did.
    engine.disable_symbol("eval");
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    // No print/debug: script output would otherwise leak into daemon logs.
    engine.on_print(|_| {});
    engine.on_debug(|_, _, _| {});
}

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
