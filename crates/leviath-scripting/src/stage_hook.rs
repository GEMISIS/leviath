//! Script-backed stage lifecycle hooks (`[stages.<name>.hooks]`, issue #260).
//!
//! Where a custom region's script owns one region's behaviour, these let a
//! blueprint observe and steer the agent's own lifecycle: what the context
//! holds as a stage opens, what happens as it closes, and (in later hooks) what
//! is about to be inferred or called.
//!
//! Same shape as [`crate::region_hook`], deliberately - a blueprint author who
//! has written one has written the other:
//!
//! - **A return-value contract.** Rhai passes arguments by value, so mutating
//!   `ctx` in place does nothing; the script returns its decision.
//! - **Compiled once**, at agent spawn, by the CLI. A missing or malformed
//!   script is a spawn error, not a runtime surprise.
//! - **A fresh hardened engine per call** ([`crate::harden`]): no filesystem,
//!   no network, no `eval`, operation-bounded.
//! - **JSON at the boundary**, so `leviath-runtime` interprets the outcome
//!   without depending on `rhai`.
//!
//! # The outcome contract
//!
//! Every hook returns one of four things, and the same four everywhere, so a
//! reader does not have to learn a vocabulary per hook:
//!
//! | script returns | meaning |
//! |---|---|
//! | `()`, `true` | [`HookOutcome::Allow`] - proceed unchanged |
//! | `false` | [`HookOutcome::Cancel`] with no reason given |
//! | `#{ action: "allow" }` | as above, written out |
//! | `#{ action: "modify", value: ... }` | [`HookOutcome::Modify`] - proceed with `value` |
//! | `#{ action: "cancel", reason: "..." }` | [`HookOutcome::Cancel`] |
//! | `#{ action: "retry" }` | [`HookOutcome::Retry`] |
//!
//! What `Modify` and `Retry` *mean* is the calling hook's business - the shape
//! of `value` differs between "the regions to write" and "the request to send",
//! and not every hook can honour `Retry`. This module decides only that the
//! script returned a well-formed decision; the caller decides whether it is one
//! it can act on. That split is why an unknown `action` is an error here (a
//! typo'd `"modfiy"` must not read as `Allow`) while an unhonourable one is
//! reported by the caller.

use rhai::{AST, Dynamic, Engine, Scope};

/// Operation budget for stage hooks.
///
/// The same 100k a region hook gets, and for the same reason: these are pure
/// data transforms over a context snapshot, not the IO-driving script tools and
/// providers that are given 500k.
const STAGE_HOOK_MAX_OPERATIONS: u64 = 100_000;

/// The hooks this build implements, as the function names a script defines.
///
/// The blueprint field and the Rhai function share a name on purpose: a
/// blueprint saying `on_stage_enter = "hooks.rhai"` means "call
/// `fn on_stage_enter(ctx)` in that file", with nothing in between to look up.
pub(crate) const HOOK_NAMES: &[&str] = &[
    "on_stage_enter",
    "on_stage_exit",
    "before_inference",
    "after_inference",
    "on_tool_call",
    "on_completion",
    "on_error",
];

/// What a hook decided.
///
/// Deliberately not `Option<Value>`: "proceed unchanged" and "proceed with
/// this" are different answers, and so is "do not proceed". Collapsing them
/// would make a cancelling hook indistinguishable from one that returned
/// nothing, which is the failure mode the taint gate's own history warns about.
#[derive(Debug, Clone, PartialEq)]
pub enum HookOutcome {
    /// Proceed unchanged.
    Allow,
    /// Proceed, using this value instead. Its shape is the caller's contract.
    Modify(serde_json::Value),
    /// Do not proceed. The reason is shown to the operator and, where the
    /// caller can, written into the agent's context so the model learns why.
    Cancel(Option<String>),
    /// Do the thing again. Not every caller can honour this; one that cannot
    /// says so rather than silently treating it as `Allow`.
    Retry,
}

/// A compiled stage-hook script, ready to call.
///
/// Compiled once at spawn and shared via `Arc`, keyed by the path as written in
/// the blueprint - the same lifecycle a [`crate::region_hook::RegionScript`]
/// has, so one file backing several hooks is compiled once.
#[derive(Debug, Clone)]
pub struct HookScript {
    /// The script path as written in the blueprint - log/error context only.
    pub path: String,
    ast: AST,
    defined: Vec<String>,
}

impl HookScript {
    /// Whether this script defines the named hook.
    pub fn defines(&self, hook: &str) -> bool {
        self.defined.iter().any(|d| d == hook)
    }

    /// Every hook this script defines, in `HOOK_NAMES` order.
    pub fn defined(&self) -> &[String] {
        &self.defined
    }
}

/// Build the hardened engine every stage-hook call runs on.
fn build_engine() -> Engine {
    let mut engine = Engine::new();
    crate::harden(&mut engine, STAGE_HOOK_MAX_OPERATIONS);
    crate::functions::register_functions(&mut engine);
    crate::types::register_types(&mut engine);
    engine
}

/// Compile a stage-hook script and record which hooks it defines.
///
/// `wanted` is what the blueprint asked this file for. A file that does not
/// define a hook it was named for is a compile error: the blueprint asked for
/// behaviour that would otherwise never run, and a hook that never runs looks
/// exactly like one that ran and allowed everything.
///
/// Every hook takes exactly one parameter (`ctx`); a different arity is
/// rejected here rather than failing at the first call, mid-run.
pub fn compile(path: &str, source: &str, wanted: &[&str]) -> crate::Result<HookScript> {
    let engine = build_engine();
    let ast = engine
        .compile(source)
        .map_err(|e| crate::Error::CompilationFailed(format!("{path}: {e}")))?;

    let arity_of = |name: &str| -> Option<usize> {
        ast.iter_functions()
            .find(|f| f.name == name)
            .map(|f| f.params.len())
    };

    let mut defined = Vec::new();
    for hook in HOOK_NAMES {
        match arity_of(hook) {
            Some(1) => defined.push((*hook).to_string()),
            Some(n) => {
                return Err(crate::Error::ValidationFailed(format!(
                    "{path}: fn {hook} must take exactly one parameter (ctx), found {n}"
                )));
            }
            None => {}
        }
    }

    for hook in wanted {
        if !defined.iter().any(|d| d == hook) {
            return Err(crate::Error::ValidationFailed(format!(
                "{path}: the blueprint names this file for '{hook}', but it defines no \
                 fn {hook}(ctx)"
            )));
        }
    }

    Ok(HookScript {
        path: path.to_string(),
        ast,
        defined,
    })
}

/// Call `hook(ctx)` and interpret the decision.
///
/// The caller supplies `ctx` as JSON and gets a [`HookOutcome`]; nothing about
/// `rhai` crosses this boundary.
pub fn run(script: &HookScript, hook: &str, ctx: serde_json::Value) -> crate::Result<HookOutcome> {
    let engine = build_engine();
    // Total conversion: every JSON value has a Dynamic representation, so a
    // failure here is a programmer error, not a script error (same stance as
    // the region hooks and the provider layer).
    let ctx_dyn = rhai::serde::to_dynamic(ctx).expect("JSON always converts to Dynamic");
    let result: Dynamic = engine
        .call_fn(&mut Scope::new(), &script.ast, hook, (ctx_dyn,))
        .map_err(|e| crate::Error::ExecutionFailed(format!("{}: {hook}: {e}", script.path)))?;

    if result.is_unit() {
        return Ok(HookOutcome::Allow);
    }
    if let Ok(b) = result.as_bool() {
        return Ok(match b {
            true => HookOutcome::Allow,
            false => HookOutcome::Cancel(None),
        });
    }

    let value = rhai::serde::from_dynamic::<serde_json::Value>(&result).map_err(|e| {
        crate::Error::ValidationFailed(format!(
            "{}: {hook} returned a value that is not plain data: {e}",
            script.path
        ))
    })?;
    outcome_from(&script.path, hook, value)
}

/// Read a returned map into an outcome.
///
/// Split out so every arm is reachable from a plain value in tests, without
/// standing up an engine to produce each shape.
fn outcome_from(path: &str, hook: &str, value: serde_json::Value) -> crate::Result<HookOutcome> {
    let bad = |what: String| crate::Error::ValidationFailed(format!("{path}: {hook}: {what}"));

    let Some(obj) = value.as_object() else {
        return Err(bad(format!(
            "expected (), a bool, or a map with an 'action', got: {value}"
        )));
    };
    let Some(action) = obj.get("action").and_then(|a| a.as_str()) else {
        return Err(bad(
            "the returned map has no 'action' (expected allow, modify, cancel, or retry)"
                .to_string(),
        ));
    };
    match action {
        "allow" => Ok(HookOutcome::Allow),
        "retry" => Ok(HookOutcome::Retry),
        "cancel" => Ok(HookOutcome::Cancel(
            obj.get("reason")
                .and_then(|r| r.as_str())
                .map(str::to_string),
        )),
        // A `modify` with no `value` is rejected rather than read as `allow`:
        // the script asked to change something and naming nothing is a bug in
        // it, not an instruction to proceed.
        "modify" => match obj.get("value") {
            Some(v) => Ok(HookOutcome::Modify(v.clone())),
            None => Err(bad(
                "action 'modify' needs a 'value' saying what to proceed with".to_string(),
            )),
        },
        other => Err(bad(format!(
            "unknown action '{other}' (expected allow, modify, cancel, or retry)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(src: &str) -> HookScript {
        compile("hooks.rhai", src, &[]).expect("compiles")
    }

    // ─── compile ──────────────────────────────────────────────────────────

    #[test]
    fn a_script_records_every_hook_it_defines() {
        let s = script("fn on_stage_enter(ctx) { () } fn on_stage_exit(ctx) { () }");
        assert!(s.defines("on_stage_enter"));
        assert!(s.defines("on_stage_exit"));
        assert_eq!(s.defined(), ["on_stage_enter", "on_stage_exit"]);
    }

    #[test]
    fn a_hook_the_script_does_not_define_is_not_claimed() {
        let s = script("fn on_stage_enter(ctx) { () }");
        assert!(s.defines("on_stage_enter"));
        assert!(!s.defines("on_stage_exit"));
    }

    #[test]
    fn a_syntax_error_names_the_file() {
        let err = compile("hooks.rhai", "fn on_stage_enter(ctx) {", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("hooks.rhai"), "{err}");
    }

    /// Arity is checked at compile rather than at the first call: a hook that
    /// takes the wrong number of arguments fails on entry to some stage,
    /// possibly minutes in, and the blueprint author is not there to see it.
    #[test]
    fn a_hook_with_the_wrong_arity_is_rejected_at_compile() {
        let err = compile("hooks.rhai", "fn on_stage_enter(a, b) { () }", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("exactly one parameter"), "{err}");
        assert!(err.contains("found 2"), "{err}");
    }

    /// The case that matters most: the blueprint asked this file for a hook and
    /// the file does not implement it. Silently accepting would make a
    /// never-called hook indistinguishable from one that allowed everything.
    #[test]
    fn a_file_named_for_a_hook_it_does_not_define_is_rejected() {
        let err = compile(
            "hooks.rhai",
            "fn on_stage_exit(ctx) { () }",
            &["on_stage_enter"],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("on_stage_enter"), "{err}");
        assert!(err.contains("defines no"), "{err}");
    }

    #[test]
    fn a_file_that_defines_what_was_asked_for_compiles() {
        assert!(
            compile(
                "hooks.rhai",
                "fn on_stage_enter(ctx) { () }",
                &["on_stage_enter"]
            )
            .is_ok()
        );
    }

    // ─── the outcome contract, through a real engine ──────────────────────

    fn run_returning(body: &str) -> crate::Result<HookOutcome> {
        let s = script(&format!("fn on_stage_enter(ctx) {{ {body} }}"));
        run(&s, "on_stage_enter", serde_json::json!({"stage": "main"}))
    }

    #[test]
    fn unit_and_true_both_allow() {
        assert_eq!(run_returning("()").unwrap(), HookOutcome::Allow);
        assert_eq!(run_returning("true").unwrap(), HookOutcome::Allow);
    }

    /// `false` cancels rather than allowing. Reading a bare `false` as "no
    /// opinion" is how a hook that meant to stop something silently does not.
    #[test]
    fn false_cancels_with_no_reason() {
        assert_eq!(run_returning("false").unwrap(), HookOutcome::Cancel(None));
    }

    #[test]
    fn a_written_out_allow_is_the_same_as_unit() {
        assert_eq!(
            run_returning(r#"#{ action: "allow" }"#).unwrap(),
            HookOutcome::Allow
        );
    }

    #[test]
    fn modify_carries_its_value() {
        let got = run_returning(r#"#{ action: "modify", value: #{ notes: "seeded" } }"#).unwrap();
        assert_eq!(
            got,
            HookOutcome::Modify(serde_json::json!({"notes": "seeded"}))
        );
    }

    #[test]
    fn cancel_carries_its_reason() {
        assert_eq!(
            run_returning(r#"#{ action: "cancel", reason: "over budget" }"#).unwrap(),
            HookOutcome::Cancel(Some("over budget".to_string()))
        );
        assert_eq!(
            run_returning(r#"#{ action: "cancel" }"#).unwrap(),
            HookOutcome::Cancel(None)
        );
    }

    #[test]
    fn retry_is_its_own_outcome() {
        assert_eq!(
            run_returning(r#"#{ action: "retry" }"#).unwrap(),
            HookOutcome::Retry
        );
    }

    #[test]
    fn the_ctx_reaches_the_script() {
        let s = script(r#"fn on_stage_enter(ctx) { #{ action: "modify", value: ctx.stage } }"#);
        let got = run(&s, "on_stage_enter", serde_json::json!({"stage": "review"})).unwrap();
        assert_eq!(got, HookOutcome::Modify(serde_json::json!("review")));
    }

    // ─── malformed decisions ──────────────────────────────────────────────

    /// A typo must not read as `Allow`. This is the whole reason an unknown
    /// action is an error rather than a default.
    #[test]
    fn an_unknown_action_is_an_error_not_an_allow() {
        let err = run_returning(r#"#{ action: "modfiy" }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown action 'modfiy'"), "{err}");
    }

    #[test]
    fn a_map_without_an_action_is_an_error() {
        let err = run_returning(r#"#{ value: 1 }"#).unwrap_err().to_string();
        assert!(err.contains("no 'action'"), "{err}");
    }

    /// `modify` naming nothing is a bug in the script, not an instruction to
    /// proceed unchanged - it asked to change something and said what to.
    #[test]
    fn modify_without_a_value_is_an_error() {
        let err = run_returning(r#"#{ action: "modify" }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a 'value'"), "{err}");
    }

    #[test]
    fn a_bare_scalar_is_an_error() {
        let err = run_returning("42").unwrap_err().to_string();
        assert!(err.contains("expected (), a bool, or a map"), "{err}");
    }

    #[test]
    fn a_script_that_throws_reports_the_hook_and_file() {
        let err = run_returning(r#"throw "nope""#).unwrap_err().to_string();
        assert!(err.contains("hooks.rhai"), "{err}");
        assert!(err.contains("on_stage_enter"), "{err}");
    }

    #[test]
    fn calling_a_hook_the_script_lacks_is_an_execution_error() {
        let s = script("fn on_stage_enter(ctx) { () }");
        assert!(run(&s, "on_stage_exit", serde_json::json!({})).is_err());
    }

    /// A value with no JSON representation cannot cross the boundary. The
    /// caller gets a validation error naming the file, not a panic.
    #[test]
    fn a_return_that_is_not_plain_data_is_rejected() {
        let err = run_returning("|| 1").unwrap_err().to_string();
        assert!(err.contains("hooks.rhai"), "{err}");
    }

    // ─── the sandbox actually applies ─────────────────────────────────────

    /// The hardening is not decoration: a hook is agent-adjacent code and must
    /// not be able to reach the filesystem or spin forever.
    #[test]
    fn a_hook_cannot_reach_the_host() {
        let s = script(r#"fn on_stage_enter(ctx) { open_file("/etc/passwd") }"#);
        assert!(run(&s, "on_stage_enter", serde_json::json!({})).is_err());
    }

    #[test]
    fn a_runaway_hook_is_stopped_by_the_operation_budget() {
        let s = script("fn on_stage_enter(ctx) { let i = 0; while true { i += 1; } }");
        let err = run(&s, "on_stage_enter", serde_json::json!({}))
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty(), "a runaway must fail, not hang");
    }
}
