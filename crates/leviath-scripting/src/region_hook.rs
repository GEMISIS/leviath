//! Script-backed custom region hooks (`RegionKind::Custom`).
//!
//! A custom region's `.rhai` script owns the region's behavior through up to
//! three functions, each a **return-value contract** (Rhai passes arguments by
//! value, so mutating the `ctx` argument in place has no effect - the script
//! must return its result):
//!
//! - `render(ctx)` - **required**. Shapes the region's contribution to the
//!   assembled context. Returns a string (one system block) or a map
//!   `#{ system: ..., messages: [...] }`.
//! - `on_write(ctx)` - optional. Sees each incoming entry; returns a string
//!   (replace the content), `true`/`()` (accept unchanged), or `false` (drop).
//! - `on_overflow(ctx)` - optional. Chooses what to evict under budget
//!   pressure; returns an array of entry indices to drop.
//!
//! This module owns compilation (once, at agent spawn - the CLI resolves the
//! blueprint-dir-relative path and reads the file) and the three call
//! entry points. Interpretation of the returned values - block/message
//! construction, index validation, fallbacks - lives with the caller
//! (`leviath-runtime`), which receives plain [`serde_json::Value`]s so it
//! needs no `rhai` dependency of its own.
//!
//! Execution runs on a fresh hardened engine per call (see [`crate::harden`])
//! over the precompiled AST: no filesystem, no network, no `eval`, operation-
//! bounded. The registered helpers are the same pure function/type sets every
//! Leviath script engine gets.

use rhai::{AST, Dynamic, Engine, Scope};

/// Operation budget for region hooks: pure data transforms, same policy as
/// `ScriptEngine` validators/transforms (not the 500k the IO-driving script
/// tools/providers get).
const REGION_HOOK_MAX_OPERATIONS: u64 = 100_000;

/// A compiled custom-region script, ready to call. Compiled once at spawn and
/// shared via `Arc` on the runtime's context window, keyed by `path`.
#[derive(Debug, Clone)]
pub struct RegionScript {
    /// The script path as written in the blueprint - log/error context only.
    pub path: String,
    ast: AST,
    has_on_write: bool,
    has_on_overflow: bool,
}

impl RegionScript {
    /// Whether the script defines `on_write(ctx)`.
    pub fn has_on_write(&self) -> bool {
        self.has_on_write
    }

    /// Whether the script defines `on_overflow(ctx)`.
    pub fn has_on_overflow(&self) -> bool {
        self.has_on_overflow
    }
}

/// Build the hardened engine every region-hook call runs on.
fn build_engine() -> Engine {
    let mut engine = Engine::new();
    crate::harden(&mut engine, REGION_HOOK_MAX_OPERATIONS);
    crate::functions::register_functions(&mut engine);
    crate::types::register_types(&mut engine);
    engine
}

/// Compile a custom-region script and verify its shape: `render(ctx)` must
/// exist with exactly one parameter; `on_write`/`on_overflow` are recorded
/// when present (also arity 1). Used by the CLI at spawn (fail-fast: a
/// missing or broken script is a spawn error, not a silent runtime fallback)
/// and by `lev validate`.
pub fn compile(path: &str, source: &str) -> crate::Result<RegionScript> {
    let engine = build_engine();
    let ast = engine
        .compile(source)
        .map_err(|e| crate::Error::CompilationFailed(format!("{path}: {e}")))?;

    let arity_of = |name: &str| -> Option<usize> {
        ast.iter_functions()
            .find(|f| f.name == name)
            .map(|f| f.params.len())
    };

    match arity_of("render") {
        Some(1) => {}
        Some(n) => {
            return Err(crate::Error::ValidationFailed(format!(
                "{path}: fn render must take exactly one parameter (ctx), found {n}"
            )));
        }
        None => {
            return Err(crate::Error::ValidationFailed(format!(
                "{path}: script must define fn render(ctx)"
            )));
        }
    }
    // Optional hooks: present-but-wrong-arity is an authoring error worth
    // failing on at spawn, not a silent "hook never fires".
    for optional in ["on_write", "on_overflow"] {
        if let Some(n) = arity_of(optional)
            && n != 1
        {
            return Err(crate::Error::ValidationFailed(format!(
                "{path}: fn {optional} must take exactly one parameter (ctx), found {n}"
            )));
        }
    }

    Ok(RegionScript {
        path: path.to_string(),
        has_on_write: arity_of("on_write").is_some(),
        has_on_overflow: arity_of("on_overflow").is_some(),
        ast,
    })
}

/// Call one of the script's functions with `ctx` and hand back the raw result
/// as JSON. The caller interprets the shape; a value that cannot be
/// represented as JSON (a function pointer, say) is a validation error.
fn call(
    script: &RegionScript,
    fn_name: &str,
    ctx: serde_json::Value,
) -> crate::Result<serde_json::Value> {
    let engine = build_engine();
    let ctx_dyn = rhai::serde::to_dynamic(ctx).map_err(|e| {
        crate::Error::ExecutionFailed(format!("{}: building {fn_name} ctx: {e}", script.path))
    })?;
    let result: Dynamic = engine
        .call_fn(&mut Scope::new(), &script.ast, fn_name, (ctx_dyn,))
        .map_err(|e| crate::Error::ExecutionFailed(format!("{}: {fn_name}: {e}", script.path)))?;
    rhai::serde::from_dynamic::<serde_json::Value>(&result).map_err(|e| {
        crate::Error::ValidationFailed(format!(
            "{}: {fn_name} returned a value that is not plain data: {e}",
            script.path
        ))
    })
}

/// Run `render(ctx)`. The result is a JSON string (one system block) or an
/// object with optional `system` / `messages` fields - shape validation is the
/// caller's job.
pub fn run_render(
    script: &RegionScript,
    ctx: serde_json::Value,
) -> crate::Result<serde_json::Value> {
    call(script, "render", ctx)
}

/// Run `on_write(ctx)`. Callers must check [`RegionScript::has_on_write`]
/// first - calling a missing function is an execution error.
pub fn run_on_write(
    script: &RegionScript,
    ctx: serde_json::Value,
) -> crate::Result<serde_json::Value> {
    call(script, "on_write", ctx)
}

/// Run `on_overflow(ctx)`. Callers must check
/// [`RegionScript::has_on_overflow`] first.
pub fn run_on_overflow(
    script: &RegionScript,
    ctx: serde_json::Value,
) -> crate::Result<serde_json::Value> {
    call(script, "on_overflow", ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> serde_json::Value {
        json!({
            "region": { "name": "brain", "budget": 1000, "current_tokens": 10, "entry_count": 1 },
            "entries": [ { "content": "hello", "tokens": 2, "kind": "text" } ],
            "stage_name": "plan",
            "stage_iterations": 3,
            "model": "test-model",
            "window": { "total_tokens": 10, "max_tokens": 2000 },
        })
    }

    // ─── compile ─────────────────────────────────────────────────────────

    #[test]
    fn compile_accepts_render_only_script() {
        let s = compile("r.rhai", "fn render(ctx) { \"out\" }").unwrap();
        assert_eq!(s.path, "r.rhai");
        assert!(!s.has_on_write());
        assert!(!s.has_on_overflow());
    }

    #[test]
    fn compile_detects_optional_hooks() {
        let src = r#"
            fn render(ctx) { "out" }
            fn on_write(ctx) { true }
            fn on_overflow(ctx) { [] }
        "#;
        let s = compile("r.rhai", src).unwrap();
        assert!(s.has_on_write());
        assert!(s.has_on_overflow());
    }

    #[test]
    fn compile_rejects_syntax_errors() {
        let err = compile("bad.rhai", "fn render(ctx) {").unwrap_err();
        assert!(matches!(err, crate::Error::CompilationFailed(_)), "{err}");
        assert!(err.to_string().contains("bad.rhai"));
    }

    #[test]
    fn compile_rejects_missing_render() {
        let err = compile("no.rhai", "fn on_write(ctx) { true }").unwrap_err();
        assert!(matches!(err, crate::Error::ValidationFailed(_)));
        assert!(err.to_string().contains("must define fn render"));
    }

    #[test]
    fn compile_rejects_wrong_render_arity() {
        let err = compile("a.rhai", "fn render(a, b) { \"x\" }").unwrap_err();
        assert!(err.to_string().contains("exactly one parameter"), "{err}");
    }

    #[test]
    fn compile_rejects_wrong_optional_hook_arity() {
        let src = "fn render(ctx) { \"x\" }\nfn on_write() { true }";
        let err = compile("w.rhai", src).unwrap_err();
        assert!(err.to_string().contains("on_write"), "{err}");
        let src = "fn render(ctx) { \"x\" }\nfn on_overflow(a, b) { [] }";
        let err = compile("o.rhai", src).unwrap_err();
        assert!(err.to_string().contains("on_overflow"), "{err}");
    }

    // ─── render ──────────────────────────────────────────────────────────

    #[test]
    fn render_returns_string() {
        let s = compile("r.rhai", "fn render(ctx) { `[${ctx.region.name}] ok` }").unwrap();
        let out = run_render(&s, ctx()).unwrap();
        assert_eq!(out, json!("[brain] ok"));
    }

    #[test]
    fn render_sees_full_ctx() {
        // The script reads every advertised ctx field and echoes them back -
        // locks the ctx schema from the script's point of view.
        let src = r#"
            fn render(ctx) {
                #{
                    system: `${ctx.stage_name}/${ctx.stage_iterations}/${ctx.model}`,
                    messages: [
                        #{ role: "user", content: ctx.entries[0].content },
                    ],
                    budget: ctx.region.budget,
                    window_max: ctx.window.max_tokens,
                }
            }
        "#;
        let s = compile("r.rhai", src).unwrap();
        let out = run_render(&s, ctx()).unwrap();
        assert_eq!(out["system"], json!("plan/3/test-model"));
        assert_eq!(out["messages"][0]["content"], json!("hello"));
        assert_eq!(out["budget"], json!(1000));
        assert_eq!(out["window_max"], json!(2000));
    }

    #[test]
    fn render_returns_unit_as_null() {
        // The caller treats non-string/non-map as a hook error; the scripting
        // layer just reports it faithfully.
        let s = compile("r.rhai", "fn render(ctx) { }").unwrap();
        let out = run_render(&s, ctx()).unwrap();
        assert_eq!(out, serde_json::Value::Null);
    }

    #[test]
    fn render_runtime_throw_is_execution_error() {
        let s = compile("r.rhai", "fn render(ctx) { throw \"boom\" }").unwrap();
        let err = run_render(&s, ctx()).unwrap_err();
        assert!(matches!(err, crate::Error::ExecutionFailed(_)));
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn render_infinite_loop_hits_operation_limit() {
        let s = compile("r.rhai", "fn render(ctx) { loop { } }").unwrap();
        let err = run_render(&s, ctx()).unwrap_err();
        assert!(matches!(err, crate::Error::ExecutionFailed(_)), "{err}");
    }

    #[test]
    fn render_unrepresentable_return_is_validation_error() {
        // A function pointer cannot cross the JSON boundary.
        let s = compile("r.rhai", "fn render(ctx) { Fn(\"render\") }").unwrap();
        let err = run_render(&s, ctx()).unwrap_err();
        assert!(matches!(err, crate::Error::ValidationFailed(_)), "{err}");
    }

    #[test]
    fn hooks_cannot_reach_disabled_engine_surface() {
        // The sandbox holds for hooks: `eval` is disabled outright, so a
        // script trying to use it never even compiles.
        let err = compile("r.rhai", "fn render(ctx) { eval(\"1+1\") }").unwrap_err();
        assert!(matches!(err, crate::Error::CompilationFailed(_)), "{err}");
        assert!(err.to_string().contains("eval"), "{err}");
    }

    // ─── on_write / on_overflow ──────────────────────────────────────────

    #[test]
    fn on_write_returns_replacement_string() {
        let src = r#"
            fn render(ctx) { "" }
            fn on_write(ctx) { ctx.entry.content.to_upper() }
        "#;
        let s = compile("w.rhai", src).unwrap();
        let out = run_on_write(
            &s,
            json!({ "region": { "name": "brain" }, "entry": { "content": "hi", "kind": "text", "tokens": 1 }, "stage_name": "plan" }),
        )
        .unwrap();
        assert_eq!(out, json!("HI"));
    }

    #[test]
    fn on_write_returns_booleans_and_unit() {
        for (body, expected) in [
            ("true", json!(true)),
            ("false", json!(false)),
            ("", serde_json::Value::Null),
        ] {
            let src = format!("fn render(ctx) {{ \"\" }}\nfn on_write(ctx) {{ {body} }}");
            let s = compile("w.rhai", &src).unwrap();
            let out = run_on_write(&s, json!({"entry": {"content": "x"}})).unwrap();
            assert_eq!(out, expected, "body: {body}");
        }
    }

    #[test]
    fn on_overflow_returns_index_array() {
        let src = r#"
            fn render(ctx) { "" }
            fn on_overflow(ctx) {
                let drops = [];
                for (entry, i) in ctx.entries {
                    if entry.kind != "tool_result" { drops.push(i); }
                }
                drops
            }
        "#;
        let s = compile("o.rhai", src).unwrap();
        let out = run_on_overflow(
            &s,
            json!({
                "region": { "name": "brain" },
                "entries": [
                    { "content": "a", "kind": "text" },
                    { "content": "b", "kind": "tool_result" },
                    { "content": "c", "kind": "text" },
                ],
                "needed_tokens": 10,
            }),
        )
        .unwrap();
        assert_eq!(out, json!([0, 2]));
    }

    #[test]
    fn calling_a_missing_hook_is_an_execution_error() {
        // Callers gate on has_on_write/has_on_overflow; if they get it wrong
        // the failure is an ordinary error, not a panic.
        let s = compile("r.rhai", "fn render(ctx) { \"\" }").unwrap();
        assert!(!s.has_on_write());
        let err = run_on_write(&s, json!({})).unwrap_err();
        assert!(matches!(err, crate::Error::ExecutionFailed(_)));
    }
}
