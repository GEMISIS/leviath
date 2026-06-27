//! Rhai script engine with Leviath integration.

use rhai::{Engine, Scope};
use crate::{Error, Result};

/// Sandboxed Rhai engine for executing custom validators, transforms, and logic.
pub struct ScriptEngine {
    engine: Engine,
}

impl ScriptEngine {
    /// Create a new sandboxed script engine.
    pub fn new() -> Self {
        let mut engine = Engine::new();

        // Sandbox: disable dangerous features
        engine.set_max_operations(100_000); // Prevent infinite loops
        engine.set_max_string_size(1_000_000); // Limit string size
        engine.set_max_array_size(10_000); // Limit array size
        engine.set_max_map_size(10_000); // Limit map size

        // Disable print/debug to prevent data leakage
        engine.on_print(|_| {});
        engine.on_debug(|_, _, _| {});

        // Register Leviath functions and types
        crate::functions::register_functions(&mut engine);
        crate::types::register_types(&mut engine);

        Self { engine }
    }

    /// Validate content using a Rhai script.
    pub fn validate(&self, script: &str, content: &str) -> Result<bool> {
        let mut scope = Scope::new();
        scope.push("content", content.to_string());

        self.engine
            .eval_with_scope::<bool>(&mut scope, script)
            .map_err(|e| Error::ExecutionFailed(e.to_string()))
    }

    /// Transform content using a Rhai script.
    pub fn transform(&self, script: &str, input: rhai::Map) -> Result<String> {
        let mut scope = Scope::new();
        scope.push("input", input);

        self.engine
            .eval_with_scope::<String>(&mut scope, script)
            .map_err(|e| Error::ExecutionFailed(e.to_string()))
    }

    /// Execute a generic script with a scope.
    pub fn execute(&self, script: &str, scope: &mut Scope) -> Result<rhai::Dynamic> {
        self.engine
            .eval_with_scope(scope, script)
            .map_err(|e| Error::ExecutionFailed(e.to_string()))
    }
}

impl Default for ScriptEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_creation() {
        let engine = ScriptEngine::new();
        // Just verify the engine was created successfully
        assert!(engine.engine.max_operations() > 0);
    }

    #[test]
    fn test_simple_validation() {
        let engine = ScriptEngine::new();
        let script = r#"
            content.contains("test")
        "#;
        let result = engine.validate(script, "this is a test");
        assert!(result.is_ok());
        assert!(result.unwrap());
    }
}
