//! Rhai script engine with Leviath integration.

use crate::{Error, Result};
use rhai::{Engine, Scope};

/// Sandboxed Rhai engine for executing custom validators, transforms, and logic.
pub struct ScriptEngine {
    engine: Engine,
}

/// Operation budget for a seed or transform script: enough for any
/// text-shaping a region seed does, and a ceiling on a script that loops.
/// Tool scripts get their own, larger budget (`SCRIPT_TOOL_MAX_OPERATIONS`).
const TRANSFORM_MAX_OPERATIONS: u64 = 100_000;

impl ScriptEngine {
    /// Create a new sandboxed script engine.
    pub fn new() -> Self {
        let mut engine = Engine::new();
        crate::harden(&mut engine, TRANSFORM_MAX_OPERATIONS);

        crate::functions::register_functions(&mut engine);
        crate::types::register_types(&mut engine);

        Self { engine }
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

    /// Evaluate a taint gate check script.
    ///
    /// The script is evaluated with `context` in scope and must produce a
    /// bool: an expression over `context`, or a body that ends by calling a
    /// `check(context)` it defines. `context` holds `tool`, `target` and
    /// `taint_level`.
    pub fn check_gate_rule(
        &self,
        script: &str,
        tool: &str,
        target: Option<&str>,
        taint_level: &str,
    ) -> Result<bool> {
        let mut context = rhai::Map::new();
        context.insert("tool".into(), rhai::Dynamic::from(tool.to_string()));
        context.insert(
            "target".into(),
            rhai::Dynamic::from(target.unwrap_or("").to_string()),
        );
        context.insert(
            "taint_level".into(),
            rhai::Dynamic::from(taint_level.to_string()),
        );

        let mut scope = Scope::new();
        scope.push("context", context);

        self.engine
            .eval_with_scope::<bool>(&mut scope, script)
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

    /// `Default` is what clippy asks of a zero-argument `new`, and it has to
    /// build the same engine.
    #[test]
    fn default_builds_an_engine() {
        let engine = ScriptEngine::default();
        assert_eq!(engine.transform(r#""ok""#, rhai::Map::new()).unwrap(), "ok");
    }

    #[test]
    fn test_engine_creation() {
        let engine = ScriptEngine::new();
        assert!(engine.engine.max_operations() > 0);
    }

    #[test]
    fn test_transform_success() {
        let engine = ScriptEngine::new();
        let mut input = rhai::Map::new();
        input.insert("name".into(), rhai::Dynamic::from("world".to_string()));

        let script = r#"("hello " + input["name"])"#;
        let result = engine.transform(script, input);
        assert_eq!(result.unwrap(), "hello world");
    }

    #[test]
    fn test_transform_script_error_returns_execution_failed() {
        let engine = ScriptEngine::new();
        let input = rhai::Map::new();

        // Syntax error in the script.
        let script = "this is not valid rhai {{{";
        let result = engine.transform(script, input);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("Script execution failed:")
        );
    }

    #[test]
    fn test_transform_wrong_return_type_returns_execution_failed() {
        let engine = ScriptEngine::new();
        let input = rhai::Map::new();

        // Returns an integer, not a String - eval_with_scope::<String> should error.
        let script = "42";
        let result = engine.transform(script, input);
        assert!(result.is_err());
    }

    // ─── execute() ──────────────────────────────────────────────────────────

    #[test]
    fn test_execute_returns_dynamic_value() {
        let engine = ScriptEngine::new();
        let mut scope = Scope::new();
        scope.push("x", 10_i64);

        let result = engine.execute("x * 2", &mut scope);
        let value = result.unwrap();
        assert_eq!(value.as_int().unwrap(), 20);
    }

    #[test]
    fn test_execute_script_error_returns_execution_failed() {
        let engine = ScriptEngine::new();
        let mut scope = Scope::new();

        let result = engine.execute("undefined_function_call()", &mut scope);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("Script execution failed:")
        );
    }

    #[test]
    fn test_print_and_debug_statements_invoke_noop_callbacks() {
        // `print`/`debug` are wired to no-op closures in `new()` to prevent
        // data leakage from sandboxed scripts; a script that never calls
        // them leaves those closures registered but never invoked. This
        // exercises both, proving the sandbox tolerates (and silently
        // discards) print/debug output instead of erroring.
        let engine = ScriptEngine::new();
        let mut scope = Scope::new();
        let result = engine.execute(r#"print("hello"); debug("world"); true"#, &mut scope);
        assert!(result.unwrap().as_bool().unwrap());
    }

    // ─── Default ────────────────────────────────────────────────────────────

    #[test]
    fn test_gate_rule_allows_matching_tool() {
        let engine = ScriptEngine::new();
        let script = r#"
            context["tool"] == "send_email"
            && context["target"].ends_with("@mycompany.com")
            && (context["taint_level"] == "public" || context["taint_level"] == "internal")
        "#;
        let result = engine
            .check_gate_rule(
                script,
                "send_email",
                Some("alice@mycompany.com"),
                "internal",
            )
            .unwrap();
        assert!(result);
    }

    #[test]
    fn test_gate_rule_blocks_external_email() {
        let engine = ScriptEngine::new();
        let script = r#"
            context["tool"] == "send_email"
            && context["target"].ends_with("@mycompany.com")
        "#;
        let result = engine
            .check_gate_rule(script, "send_email", Some("bob@external.com"), "internal")
            .unwrap();
        assert!(!result);
    }

    #[test]
    fn test_gate_rule_blocks_wrong_tool() {
        let engine = ScriptEngine::new();
        let script = r#"context["tool"] == "send_email""#;
        let result = engine
            .check_gate_rule(script, "post_to_slack", None, "public")
            .unwrap();
        assert!(!result);
    }

    #[test]
    fn test_gate_rule_no_target_uses_empty_string() {
        let engine = ScriptEngine::new();
        let script = r#"context["target"] == """#;
        let result = engine
            .check_gate_rule(script, "shell", None, "public")
            .unwrap();
        assert!(result);
    }

    #[test]
    fn test_gate_rule_script_error() {
        let engine = ScriptEngine::new();
        let result = engine.check_gate_rule("invalid {{ syntax", "shell", None, "public");
        assert!(result.is_err());
    }
}
