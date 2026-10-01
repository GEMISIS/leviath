//! Leviath's extension methods.
//!
//! The protocol leaves every method whose name starts with `_` to extensions.
//! A peer that does not know one answers a request for it with "method not
//! found", and ignores a notification of one. Leviath adds two requests, and
//! both take a spawn request as their params: the same JSON the HTTP API's
//! `POST /api/runs` and `lev run --request` take.
//!
//! - [`SPAWN`] starts the run and opens a session bound to it. The result is
//!   a [`SpawnResult`]. A `session/prompt` on that session streams the run;
//!   an empty prompt only follows it, and one with content is delivered to
//!   it as a message first.
//! - [`VALIDATE_SPAWN`] checks the request the whole way without starting
//!   anything. The result is a summary of the run it would be.
//!
//! A request either method refuses is answered with the JSON-RPC error
//! `invalid params` (`-32602`), whose `data` lists every problem at once:
//! each with the path in the request it is about, what was expected, what
//! arrived and how to fix it.
//!
//! An agent offering these says so in `initialize`, in
//! `agentCapabilities._meta`, the place the protocol keeps for extensions.
//! [`capability_meta`] is what Leviath puts there.

use serde::{Deserialize, Serialize};

/// Start a run from a spawn request, in a session of its own.
pub const SPAWN: &str = "_leviath/spawn";

/// Check a spawn request without starting it.
pub const VALIDATE_SPAWN: &str = "_leviath/validate_spawn";

/// The key Leviath's entry in a capability's `_meta` sits under.
pub const META_KEY: &str = "leviath";

/// What `agentCapabilities._meta` says about Leviath's extensions:
/// `{"leviath": {"methods": ["_leviath/spawn", "_leviath/validate_spawn"]}}`.
pub fn capability_meta() -> serde_json::Value {
    serde_json::json!({ META_KEY: { "methods": [SPAWN, VALIDATE_SPAWN] } })
}

/// The result of [`SPAWN`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnResult {
    /// The session bound to the new run. Prompt it to follow the run.
    pub session_id: String,
    /// The run's id, as `lev` and the HTTP API name it.
    pub run_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capability_meta_lists_both_methods_under_leviath() {
        let meta = capability_meta();
        assert_eq!(
            meta,
            serde_json::json!({"leviath": {"methods": ["_leviath/spawn", "_leviath/validate_spawn"]}})
        );
        // Both are extension methods by the protocol's rule.
        assert!(SPAWN.starts_with('_') && VALIDATE_SPAWN.starts_with('_'));
    }

    #[test]
    fn a_spawn_result_is_camel_case() {
        let result = SpawnResult {
            session_id: "s".into(),
            run_id: "r".into(),
        };
        let text = serde_json::to_string(&result).unwrap();
        assert_eq!(text, r#"{"sessionId":"s","runId":"r"}"#);
        assert_eq!(serde_json::from_str::<SpawnResult>(&text).unwrap(), result);
    }
}
