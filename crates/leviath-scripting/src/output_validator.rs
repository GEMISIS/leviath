//! Script-backed validators for an agent's final output.
//!
//! A blueprint can name any format it likes, and most of them are formats this
//! codebase has never heard of. The built-in checks cover a handful that can be
//! parsed with a crate we already ship; everything else would mean owning that
//! format's parser and schema language, which is a cost the output system
//! deliberately does not take on.
//!
//! So the knowledge lives with the person who has it. An agent that produces
//! a2ui, a house report format, or a dialect of CSV nobody else uses ships a
//! `.rhai` file beside its blueprint that says what "valid" means:
//!
//! ```rhai
//! // @validator a2ui
//! fn validate(content) {
//!     let doc = parse_json(content);
//!     if doc.root == () { return "the document has no `root` node"; }
//!     ()   // fine
//! }
//! ```
//!
//! One function, one contract: **return `()` when the answer is fine, or a
//! string saying what is wrong**. The string goes back to the agent as the same
//! `[error]` refusal a schema failure produces, and it tries again. Rhai passes
//! by value, so a return value is the only thing a script can say - the same
//! shape the region hooks use.
//!
//! Execution runs on a fresh hardened engine per call: no filesystem, no
//! network, no `eval`, operation-bounded. A validator that throws, loops, or
//! returns something that is neither `()` nor a string is reported as broken
//! rather than folded into "the answer is wrong"; what happens to the
//! submission then is the consumer's policy (`on_validator_error` on the
//! output spec), not this module's.

use rhai::{AST, Dynamic, Engine, Scope};

/// Operation budget for a validator: a pure data check over one document, the
/// same policy the region hooks get rather than the far larger budget the
/// IO-driving script tools and providers need.
const VALIDATOR_MAX_OPERATIONS: u64 = 100_000;

/// A compiled output validator, ready to call.
///
/// Compiled once when the agent spawns, so a broken script is a spawn error
/// rather than a surprise at the end of a long run - the worst possible moment
/// to discover the agent cannot hand back its work.
#[derive(Debug, Clone)]
pub struct OutputValidator {
    /// The script path as written in the blueprint, for error context.
    pub path: String,
    ast: AST,
}

/// Build the hardened engine every validator call runs on.
fn build_engine() -> Engine {
    let mut engine = Engine::new();
    crate::harden(&mut engine, VALIDATOR_MAX_OPERATIONS);
    crate::functions::register_functions(&mut engine);
    crate::types::register_types(&mut engine);
    engine
}

/// Compile an output validator and check its shape.
///
/// `validate(content)` must exist and take exactly one parameter. A script that
/// defines nothing, or defines it with the wrong arity, is refused here rather
/// than silently never running.
pub fn compile(path: &str, source: &str) -> crate::Result<OutputValidator> {
    let engine = build_engine();
    let ast = engine
        .compile(source)
        .map_err(|e| crate::Error::CompilationFailed(format!("{path}: {e}")))?;

    let arity = ast
        .iter_functions()
        .find(|f| f.name == "validate")
        .map(|f| f.params.len());
    match arity {
        Some(1) => Ok(OutputValidator {
            path: path.to_string(),
            ast,
        }),
        Some(n) => Err(crate::Error::ValidationFailed(format!(
            "{path}: fn validate must take exactly one parameter (content), found {n}"
        ))),
        None => Err(crate::Error::ValidationFailed(format!(
            "{path}: script must define fn validate(content)"
        ))),
    }
}

/// What a validator said about one submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The answer is fine.
    Valid,
    /// The answer is not, for this reason. Goes back to the agent verbatim.
    Invalid(String),
    /// The validator itself is broken: it threw, ran out of operations, or
    /// returned something that is neither `()` nor a string. Carries the error
    /// text, script path included, so a consumer that refuses the submission
    /// can hand the model something actionable.
    ///
    /// Kept apart from [`Invalid`](Self::Invalid) on purpose: a broken script
    /// is flagged on the run as an authoring problem, where a rejection is the
    /// answer's problem. Whether the submission is then refused or recorded
    /// unchecked is the consumer's `on_validator_error` policy, decided where
    /// the verdict is applied rather than here.
    Unusable(String),
}

/// Run `validator` over `content`.
pub fn validate(validator: &OutputValidator, content: &str) -> Verdict {
    let engine = build_engine();
    let result: Result<Dynamic, _> = engine.call_fn(
        &mut Scope::new(),
        &validator.ast,
        "validate",
        (content.to_string(),),
    );
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            return Verdict::Unusable(format!("{}: validate: {e}", validator.path));
        }
    };
    if value.is_unit() {
        return Verdict::Valid;
    }
    match value.into_string() {
        // A script may also say "fine" by returning an empty string, which is
        // easy to write by accident and unambiguous in meaning.
        Ok(reason) if reason.trim().is_empty() => Verdict::Valid,
        Ok(reason) => Verdict::Invalid(reason),
        Err(actual) => Verdict::Unusable(format!(
            "{}: validate must return () or a string, got {actual}",
            validator.path
        )),
    }
}

#[cfg(test)]
mod tests;
