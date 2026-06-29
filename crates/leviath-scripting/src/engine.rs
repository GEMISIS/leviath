//! Rhai script engine with Leviath integration.

use crate::{Error, Result};
use rhai::{Engine, Scope};

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

    #[test]
    fn test_string_operations() {
        let engine = ScriptEngine::new();

        // Test starts_with
        let script = r#"content.starts_with("Hello")"#;
        assert!(engine.validate(script, "Hello, world!").unwrap());

        // Test ends_with
        let script = r#"content.ends_with("!")"#;
        assert!(engine.validate(script, "Hello, world!").unwrap());

        // Test trim
        let script = r#"content.trim() == "hello""#;
        assert!(engine.validate(script, "  hello  ").unwrap());
    }

    #[test]
    fn test_json_validation() {
        let engine = ScriptEngine::new();
        let script = r#"is_json(content)"#;

        assert!(engine.validate(script, r#"{"key": "value"}"#).unwrap());
        assert!(!engine.validate(script, "not json").unwrap());
    }

    #[test]
    fn test_mermaid_validation() {
        let engine = ScriptEngine::new();
        let script = r#"is_mermaid(content)"#;

        assert!(engine.validate(script, "graph TD\n  A --> B").unwrap());
        assert!(engine
            .validate(script, "sequenceDiagram\n  Alice->>Bob: Hello")
            .unwrap());
        assert!(!engine.validate(script, "just text").unwrap());
    }

    #[test]
    fn test_token_counting() {
        let engine = ScriptEngine::new();
        let script = r#"count_tokens(content) > 10"#;

        let long_text = "a".repeat(50);
        assert!(engine.validate(script, &long_text).unwrap());
        assert!(!engine.validate(script, "short").unwrap());
    }

    #[test]
    fn test_split_join() {
        let engine = ScriptEngine::new();
        let script = r#"
            let parts = content.split(",");
            parts.len() == 3 && join(parts, "|") != ""
        "#;
        assert!(engine.validate(script, "a,b,c").unwrap());
    }

    #[test]
    fn test_extract_modified() {
        let engine = ScriptEngine::new();
        let script = r#"
            let modified = extract_modified(content);
            modified.len() > 0
        "#;
        let content = "file1.txt\nmodified: file2.txt\nfile3.txt";
        assert!(engine.validate(script, content).unwrap());
    }

    #[test]
    fn test_summarize() {
        let engine = ScriptEngine::new();
        let script = r#"
            let summary = summarize(content, 10);
            summary.len() <= 50
        "#;
        let long_text = "a".repeat(1000);
        assert!(engine.validate(script, &long_text).unwrap());
    }

    #[test]
    fn test_sandbox_limits() {
        let engine = ScriptEngine::new();
        // Test operation limit by creating an infinite loop
        let script = r#"
            let x = 0;
            loop {
                x = x + 1;
                if x > 200000 { break; }
            }
            true
        "#;
        // Should fail due to operation limit
        let result = engine.validate(script, "test");
        assert!(result.is_err());
    }
}
