//! # Leviath Scripting
//!
//! Rhai scripting integration for custom validators, transforms, and dynamic logic.
//!
//! This crate provides a sandboxed Rhai engine that allows users to define custom
//! validators, context transforms, and compaction strategies without modifying
//! Leviath's core code.

pub mod dependency_check;
pub mod engine;
pub mod functions;
pub mod mime_check;
pub mod output_validator;
pub mod parts;
pub mod region_hook;
mod script_check;
pub mod stage_hook;
pub mod tool;
pub mod types;

/// The largest blob a script may hold, in bytes: the same 32 MiB the host
/// caps a fetched body at, so a body the host accepts is one the script can
/// take.
pub const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024;

/// A Rhai engine with the sandbox limits every Leviath engine shares: the only
/// way any crate in this workspace makes one, which `every_engine_is_sandboxed`
/// holds every source file to.
///
/// One constructor rather than limits applied after each `Engine::new`. This is
/// a security control: a call site that forgets a step gets an engine with no
/// limits at all, and that includes engines that only compile. A ban such as
/// `eval`'s is a parse-time rule, so an AST compiled by an unhardened engine
/// carries a forbidden call straight past the engine that later runs it.
///
/// `max_operations` stays a parameter because it is a genuine policy difference:
/// a provider script driving a streaming HTTP response legitimately runs longer
/// than a validator. An engine that only compiles never runs anything, so any
/// budget does.
pub fn sandboxed(max_operations: u64) -> rhai::Engine {
    let mut engine = rhai::Engine::new();
    harden(&mut engine, max_operations);
    engine
}

fn harden(engine: &mut rhai::Engine, max_operations: u64) {
    // Bound runaway loops. The only wall-clock limit on pure computation.
    engine.set_max_operations(max_operations);
    engine.set_max_string_size(1_000_000);
    // Rhai applies the array ceiling to blobs as well, and a blob is what
    // `http_get_bytes` hands a script: a PDF or an image, on its way to
    // `write_part`. The ceiling therefore matches the largest body the host
    // will fetch, or every real file fails inside the call. Arrays of values
    // are still bounded, by the operation budget: a script builds one an
    // element at a time, and no host function returns one this large.
    engine.set_max_array_size(MAX_BLOB_BYTES);
    engine.set_max_map_size(10_000);
    // Bound *recursion*: without a call-depth cap, a script recursing to
    // exhaustion overflows the native stack, which aborts the process rather
    // than raising a catchable Rhai error. Rhai does not cap this by default.
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
pub use tool::{
    ParamSpec, ScriptHost, ScriptToolMeta, ScriptToolSet, SkippedTool,
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
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Every `.rs` file under `dir`, recursively; none when `dir` is not a
    /// directory.
    fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            match path.is_dir() {
                true => sources(&path, out),
                false if path.extension().is_some_and(|e| e == "rs") => out.push(path),
                false => {}
            }
        }
    }

    /// Each `file:line` in `files` that makes a Rhai engine itself, leaving
    /// out this file, which holds the one constructor allowed to.
    fn offenders(files: &[std::path::PathBuf]) -> Vec<String> {
        let this = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs")
            .canonicalize()
            .unwrap();
        files
            .iter()
            .filter(|f| f.canonicalize().unwrap() != this)
            .flat_map(|f| {
                let text = std::fs::read_to_string(f).unwrap();
                text.lines()
                    .enumerate()
                    .filter(|(_, line)| {
                        let line = line.replace("ScriptEngine::new", "");
                        line.contains("Engine::new(") || line.contains("Engine::new_raw(")
                    })
                    .map(|(i, _)| format!("{}:{}", f.display(), i + 1))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// No crate in the workspace makes a Rhai engine except through
    /// [`sandboxed`](super::sandboxed), tests included: an engine made any
    /// other way has no limits and no bans, and one that only compiles lets a
    /// banned call through to whichever engine later runs the AST.
    #[test]
    fn every_engine_is_sandboxed() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for krate in std::fs::read_dir(&crates).unwrap().flatten() {
            sources(&krate.path().join("src"), &mut files);
        }
        let found = files.len();
        assert!(found > 100, "found only {found} sources");
        let offenders = offenders(&files);
        assert!(
            offenders.is_empty(),
            "make the engine with leviath_scripting::sandboxed: {offenders:#?}"
        );
    }

    /// The check finds an engine made directly, either way, and passes over
    /// a `ScriptEngine`, which is made through the constructor.
    #[test]
    fn the_check_finds_an_engine_made_directly() {
        let dir = tempfile::tempdir().unwrap();
        let planted = dir.path().join("planted.rs");
        std::fs::write(
            &planted,
            "let a = rhai::Engine::new();\nlet b = ScriptEngine::new();\nlet c = Engine::new_raw();\n",
        )
        .unwrap();
        let found = offenders(std::slice::from_ref(&planted));
        let at = |line: usize| format!("{}:{line}", planted.display());
        assert_eq!(found, [at(1), at(3)]);
    }
}
