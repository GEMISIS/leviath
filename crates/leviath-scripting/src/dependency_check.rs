//! Script-backed checks for a blueprint dependency.
//!
//! A `[[dependencies]]` entry of `kind = "script"` names a `check`, a `.rhai`
//! file beside the blueprint that decides whether the thing the agent needs is
//! in place:
//!
//! ```rhai
//! // deps/check.rhai: the agent needs a config file and an API key.
//! fn check() {
//!     if !has_env("ACME_API_KEY") { return "set ACME_API_KEY in your env"; }
//!     if !path_exists("/opt/acme/config.toml") { return "run acme-setup first"; }
//!     ()   // satisfied
//! }
//! ```
//!
//! One function, one contract: **`fn check()` returns `()` when the dependency
//! is satisfied, or a string remedy saying how to satisfy it**. The remedy is
//! shown by `lev deps`, `lev validate` and the spawn gate.
//!
//! The engine is hardened - no `eval`, no `import`, operation-bounded - and
//! gets three read-only probes and nothing else: `has_env(name)`, `env(name)`
//! and `path_exists(path)`. A check reads the machine, it never changes it;
//! anything that installs is a separate, explicit `lev deps install`.

use rhai::{AST, Dynamic, Engine, Scope};

/// Operation budget for a check: a handful of probes and some string work.
const CHECK_MAX_OPERATIONS: u64 = 1_000_000;

/// A compiled dependency check, ready to run.
#[derive(Debug, Clone)]
pub struct DependencyCheck {
    /// The script path as written in the manifest, for error context.
    pub path: String,
    ast: AST,
}

/// Build the hardened engine every check runs on, with the read-only probes.
fn build_engine() -> Engine {
    let mut engine = Engine::new();
    crate::harden(&mut engine, CHECK_MAX_OPERATIONS);
    crate::functions::register_functions(&mut engine);
    crate::types::register_types(&mut engine);
    // Read-only probes. A check inspects the machine and never mutates it, so
    // there is deliberately no shell, no write and no network here.
    engine.register_fn("has_env", |name: &str| std::env::var_os(name).is_some());
    engine.register_fn("env", |name: &str| std::env::var(name).unwrap_or_default());
    engine.register_fn("path_exists", |path: &str| {
        std::path::Path::new(path).exists()
    });
    engine
}

/// Compile a dependency check and check its shape.
///
/// `check()` must exist and take no parameters. A script that defines nothing,
/// or defines it with the wrong arity, is refused here rather than silently
/// never running.
pub fn compile(path: &str, source: &str) -> crate::Result<DependencyCheck> {
    let engine = build_engine();
    let ast = engine
        .compile(source)
        .map_err(|e| crate::Error::CompilationFailed(format!("{path}: {e}")))?;
    let arity = ast
        .iter_functions()
        .find(|f| f.name == "check")
        .map(|f| f.params.len());
    match arity {
        Some(0) => Ok(DependencyCheck {
            path: path.to_string(),
            ast,
        }),
        Some(n) => Err(crate::Error::ValidationFailed(format!(
            "{path}: fn check must take no parameters, found {n}"
        ))),
        None => Err(crate::Error::ValidationFailed(format!(
            "{path}: script must define fn check()"
        ))),
    }
}

/// What a check said about a dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The dependency is in place.
    Satisfied,
    /// It is not, with this remedy. Shown to the user verbatim.
    Unmet(String),
    /// The check itself is broken: it threw, ran out of operations, or returned
    /// something that is neither `()` nor a string. Carries the error text.
    Unusable(String),
}

/// Run a compiled `check()`.
pub fn run(check: &DependencyCheck) -> Verdict {
    let engine = build_engine();
    let result: Result<Dynamic, _> = engine.call_fn(&mut Scope::new(), &check.ast, "check", ());
    let value = match result {
        Ok(v) => v,
        Err(e) => return Verdict::Unusable(format!("{}: check: {e}", check.path)),
    };
    if value.is_unit() {
        return Verdict::Satisfied;
    }
    match value.into_string() {
        // An empty string is easy to write by accident and unambiguous, so it
        // reads as "satisfied" rather than as a blank remedy.
        Ok(remedy) if remedy.trim().is_empty() => Verdict::Satisfied,
        Ok(remedy) => Verdict::Unmet(remedy),
        Err(actual) => Verdict::Unusable(format!(
            "{}: check must return () or a string, got {actual}",
            check.path
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(source: &str) -> DependencyCheck {
        compile("deps/check.rhai", source).expect("fixture compiles")
    }

    #[test]
    fn a_satisfied_check_returns_unit_or_empty() {
        assert_eq!(run(&compiled("fn check() { () }")), Verdict::Satisfied);
        assert_eq!(run(&compiled(r#"fn check() { "" }"#)), Verdict::Satisfied);
        assert_eq!(
            run(&compiled(r#"fn check() { "   " }"#)),
            Verdict::Satisfied
        );
    }

    #[test]
    fn an_unmet_check_returns_its_remedy() {
        assert_eq!(
            run(&compiled(r#"fn check() { "install acme" }"#)),
            Verdict::Unmet("install acme".to_string())
        );
    }

    #[test]
    fn probes_read_the_machine_read_only() {
        // PATH is set on every platform CI runs on; a random name is not.
        assert_eq!(
            run(&compiled(
                r#"fn check() { if has_env("PATH") { () } else { "no PATH" } }"#
            )),
            Verdict::Satisfied
        );
        assert_eq!(
            run(&compiled(
                r#"fn check() { if has_env("LEVIATH_DEP_UNSET_XYZ") { "set" } else { "missing" } }"#
            )),
            Verdict::Unmet("missing".to_string())
        );
        assert_eq!(
            run(&compiled(
                r#"fn check() { if env("PATH") == "" { "empty" } else { () } }"#
            )),
            Verdict::Satisfied
        );
        assert_eq!(
            run(&compiled(
                r#"fn check() { if env("LEVIATH_DEP_UNSET_XYZ") == "" { "empty" } else { () } }"#
            )),
            Verdict::Unmet("empty".to_string())
        );
    }

    #[test]
    fn path_exists_probe_answers_both_ways() {
        let dir = std::env::temp_dir();
        let present = format!(
            r#"fn check() {{ if path_exists("{}") {{ () }} else {{ "missing" }} }}"#,
            dir.display().to_string().replace('\\', "\\\\")
        );
        assert_eq!(run(&compiled(&present)), Verdict::Satisfied);
        assert_eq!(
            run(&compiled(
                r#"fn check() { if path_exists("/leviath/nope/xyz") { "here" } else { "gone" } }"#
            )),
            Verdict::Unmet("gone".to_string())
        );
    }

    #[test]
    fn a_broken_check_is_unusable() {
        let unusable = |source: &str| {
            let v = format!("{:?}", run(&compiled(source)));
            assert!(v.starts_with("Unusable("), "{v}");
            v
        };
        assert!(unusable(r#"fn check() { throw "boom" }"#).contains("boom"));
        assert!(unusable("fn check() { 42 }").contains("() or a string"));
        assert!(unusable("fn check() { loop {} }").contains("deps/check.rhai"));
    }

    #[test]
    fn the_shape_is_checked_at_compile_time() {
        let none = compile("c.rhai", "fn other() { () }").unwrap_err();
        assert!(
            none.to_string().contains("must define fn check()"),
            "{none}"
        );
        let two = compile("c.rhai", "fn check(a) { () }").unwrap_err();
        assert!(two.to_string().contains("no parameters"), "{two}");
        let syntax = compile("c.rhai", "fn check() {").unwrap_err();
        assert!(syntax.to_string().contains("c.rhai"), "{syntax}");
    }
}
