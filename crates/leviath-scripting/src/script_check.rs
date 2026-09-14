//! The shape every script-backed check shares.
//!
//! A mime check and a dependency check are one contract with different
//! arguments: a `fn check` in a hardened engine that returns `()` when all is
//! well, or a string saying what is not. What differs between them is the
//! engine's extra functions, the operation budget, the arity, and the words
//! each verdict uses, so those are parameters here and the compile-and-run
//! machinery is written once.

use rhai::{AST, Dynamic, Engine, Scope};

/// A compiled `fn check`, ready to call.
#[derive(Debug, Clone)]
pub(crate) struct ScriptCheck {
    /// The script path as written where it was declared, for error context.
    pub(crate) path: String,
    /// The compiled script.
    pub(crate) ast: AST,
}

/// What a check said.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The check returned `()`, or an empty string: all is well.
    Fine,
    /// The check returned this string, saying what is wrong.
    Complaint(String),
    /// The check itself is broken: it threw, ran out of operations, or
    /// returned something that is neither `()` nor a string. Carries the
    /// error text, script path included.
    Unusable(String),
}

/// The engine a check compiles and runs on: hardened to `max_operations`,
/// with the crate's functions and types, plus whatever `extras` registers.
pub(crate) fn build_engine(max_operations: u64, extras: fn(&mut Engine)) -> Engine {
    let mut engine = Engine::new();
    crate::harden(&mut engine, max_operations);
    crate::functions::register_functions(&mut engine);
    crate::types::register_types(&mut engine);
    extras(&mut engine);
    engine
}

/// Compile `source` from `path` and check it defines `fn check` taking
/// `arity` parameters. `expected` says how many in an error ("no
/// parameters"), `signature` how the function reads ("check()"). A script
/// that defines nothing, or defines it with the wrong arity, is refused here
/// rather than silently never running.
pub(crate) fn compile(
    engine: &Engine,
    path: &str,
    source: &str,
    arity: usize,
    expected: &str,
    signature: &str,
) -> crate::Result<ScriptCheck> {
    let ast = engine
        .compile(source)
        .map_err(|e| crate::Error::CompilationFailed(format!("{path}: {e}")))?;
    let found = ast
        .iter_functions()
        .find(|f| f.name == "check")
        .map(|f| f.params.len());
    match found {
        Some(n) if n == arity => Ok(ScriptCheck {
            path: path.to_string(),
            ast,
        }),
        Some(n) => Err(crate::Error::ValidationFailed(format!(
            "{path}: fn check must take {expected}, found {n}"
        ))),
        None => Err(crate::Error::ValidationFailed(format!(
            "{path}: script must define fn {signature}"
        ))),
    }
}

/// Call the script's `check` with `args` and read its answer by the shared
/// contract: `()` or an empty string is fine, any other string is the
/// complaint, anything else means the check cannot be trusted.
pub(crate) fn run(engine: &Engine, path: &str, ast: &AST, args: impl rhai::FuncArgs) -> Outcome {
    let result: Result<Dynamic, _> = engine.call_fn(&mut Scope::new(), ast, "check", args);
    let value = match result {
        Ok(v) => v,
        Err(e) => return Outcome::Unusable(format!("{path}: check: {e}")),
    };
    if value.is_unit() {
        return Outcome::Fine;
    }
    match value.into_string() {
        // An empty string is easy to write by accident and unambiguous in
        // meaning, so it reads as fine rather than as a blank complaint.
        Ok(text) if text.trim().is_empty() => Outcome::Fine,
        Ok(text) => Outcome::Complaint(text),
        Err(actual) => Outcome::Unusable(format!(
            "{path}: check must return () or a string, got {actual}"
        )),
    }
}
