//! Structural validation of tool-call arguments against declared schemas.
//!
//! Every tool advertises a JSON Schema for its parameters (built-ins in
//! `defs.rs`, Rhai script tools via their compiled `@param` annotations, MCP
//! tools via the server's `inputSchema`). Until issue #155 nothing checked a
//! model's arguments against that schema: handlers did ad-hoc presence checks
//! that could not tell `{"path": 42}` from a missing `path`, and extra or
//! misspelled properties passed through silently. This module is the one
//! validator dispatch consults before a call is executed.
//!
//! Two properties are load-bearing:
//!
//! - **A schema that does not compile skips validation instead of refusing
//!   calls.** Garbage schemas are reachable in normal operation - a typo'd
//!   Rhai `@param n strng required` compiles to `{"type": "strng"}`, and MCP
//!   servers may send fragments this crate cannot interpret. Refusing those
//!   calls would break working tools; [`ArgValidation::SchemaUnusable`] lets
//!   the caller log and dispatch anyway.
//! - **External `$ref`s never resolve.** The `jsonschema` dependency is built
//!   with `default-features = false`, so a server-supplied schema referencing
//!   an external URI fails to compile (and is skipped, per the point above)
//!   rather than fetching over the network or filesystem at validation time.

use serde_json::Value;

/// How many individual schema violations a refusal message reports before
/// summarising the rest. The message goes back to the model as a tool result;
/// three concrete violations are enough to self-correct on, and a pathological
/// call (say, a giant object where a string was expected) should not turn into
/// a pathological refusal.
const MAX_REPORTED_ERRORS: usize = 3;

/// Byte cap on each rendered violation. Validator messages embed the offending
/// instance value, which the model already has; a huge argument does not need
/// to be echoed back in full.
const MAX_ERROR_LEN: usize = 256;

/// The outcome of checking one tool call's arguments against its schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgValidation {
    /// The arguments satisfy the schema; dispatch the call.
    Valid,
    /// The arguments violate the schema. Carries the complete refusal text
    /// (`[error] invalid arguments for '<tool>': ...`), ready to return as the
    /// tool result. The `[error]` prefix is deliberate: it is already in the
    /// dispatch layer's no-effect prefix list, so a refused call is not
    /// counted as work the agent did.
    Invalid(String),
    /// The schema itself would not compile, so nothing was checked. Carries
    /// the compile error for the caller to log; the call must still dispatch.
    SchemaUnusable(String),
}

/// Validate `args` against `schema`, the exact parameter schema advertised to
/// the model for `tool_name`.
///
/// `Value::Null` arguments are treated as `{}`: providers substitute an empty
/// object when a model omits tool input entirely, and a null reaching here
/// means the same "no arguments" - not a JSON null argument object.
pub fn validate_tool_args(tool_name: &str, schema: &Value, args: &Value) -> ArgValidation {
    let validator = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        Err(e) => return ArgValidation::SchemaUnusable(e.to_string()),
    };
    let empty_object;
    let instance = match args.is_null() {
        true => {
            empty_object = Value::Object(serde_json::Map::new());
            &empty_object
        }
        false => args,
    };
    let violations: Vec<String> = validator.iter_errors(instance).map(render_error).collect();
    if violations.is_empty() {
        return ArgValidation::Valid;
    }
    let reported = violations
        .iter()
        .take(MAX_REPORTED_ERRORS)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    let suffix = match violations.len() > MAX_REPORTED_ERRORS {
        true => format!("; (and {} more)", violations.len() - MAX_REPORTED_ERRORS),
        false => String::new(),
    };
    ArgValidation::Invalid(format!(
        "[error] invalid arguments for '{tool_name}': {reported}{suffix}"
    ))
}

/// One violation as the model will read it: the validator's own message,
/// length-capped, prefixed with the offending argument's path when the
/// violation is not at the root.
fn render_error(error: jsonschema::ValidationError<'_>) -> String {
    let message = error.to_string();
    let message = leviath_core::truncate_at_boundary(&message, MAX_ERROR_LEN);
    let path = error.instance_path().to_string();
    match path.is_empty() {
        true => message.to_string(),
        false => format!("at {path}: {message}"),
    }
}

#[cfg(test)]
mod tests;
