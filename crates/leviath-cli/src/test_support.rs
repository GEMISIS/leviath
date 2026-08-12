//! Shared test-only helpers for this crate's `#[cfg(test)]` code.
//!
//! The tracing subscriber comes from `leviath-testkit` (one workspace-wide
//! copy); the helpers below are CLI-specific fixtures.

pub(crate) use leviath_testkit::with_tracing;

/// A value whose `Serialize` impl always returns `Err`, so tests can drive the
/// `?` error arm of the crate's `serde_json::to_string_pretty(...)?` helpers
/// (which serialize trivially-serializable structs that never fail on real input).
pub(crate) struct PoisonSerialize;

impl serde::Serialize for PoisonSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("PoisonSerialize always fails"))
    }
}

/// Write an `agent.leviath` manifest into `dir` and return its path.
///
/// Consolidates the `std::fs::write(dir.join("agent.leviath"), ...).unwrap()`
/// idiom repeated across the CLI command test modules. `contents` accepts
/// anything byte-like (`&str`, `String`, byte slices) so both manifest text
/// and deliberately-malformed byte payloads route through the same helper.
pub(crate) fn write_test_agent(
    dir: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::path::PathBuf {
    let path = dir.as_ref().join("agent.leviath");
    std::fs::write(&path, contents).unwrap();
    path
}

/// A self-contained, coder-shaped blueprint for daemon spawn/recovery/setup
/// tests. Deliberately does NOT read the shipped `agents/coder/agent.leviath`:
/// those tests exercise spawn/reload *logic*, not the shipped blueprint, so they
/// must stay isolated from blueprint edits. Budgets are absolute (window-
/// independent) so a fake small-context test model can't starve the region the
/// stage system prompt is injected into. The `task` region is load-bearing:
/// every caller spawns this with a task, and a blueprint with nowhere to put one
/// is refused at spawn.
#[cfg(test)]
pub(crate) fn inline_coder_manifest() -> String {
    r#"[agent]
name = "coder"
version = "0.0.0"
description = "Inline test blueprint (coder-shaped); self-contained."
entry_stage = "analyze"

[tool_permissions]
read_file = "allow"
list_dir = "allow"
write_file = "ask"
bash = "ask"

[stages.analyze]
mode = "autonomous"
model = { provider = "anthropic", model = "m" }
description = "Understand the task"
available_tools = ["read_file", "list_dir"]
system_prompt = "Analyze the task and outline a short plan."
[stages.analyze.transitions.implement]
transform = "direct"

[stages.implement]
mode = "autonomous"
model = { provider = "anthropic", model = "m" }
description = "Write the code"
available_tools = ["write_file", "read_file", "list_dir", "bash"]
system_prompt = "Implement the plan."
[stages.implement.transitions.review]
transform = "compact"

[stages.review]
mode = "autonomous"
model = { provider = "anthropic", model = "m" }
description = "Review the code"
available_tools = ["read_file", "list_dir"]
allow_complete = true
system_prompt = "Review the implementation."

[context.regions]
system = { kind = "pinned", max_tokens = 8000 }
task = { kind = "pinned", max_tokens = 2000 }
codebase = { kind = "temporary", max_tokens = 20000 }
conversation = { kind = "sliding_window", max_items = 40, max_tokens = 20000 }
"#
    .to_string()
}

/// A self-contained blueprint whose stage 0 (`plan`) is an `interactive_points`
/// stage with a `plan_approval` interaction point - for recovery tests that
/// resume a run parked at an interaction point. Self-contained for the same
/// isolation reason as [`inline_coder_manifest`].
#[cfg(test)]
pub(crate) fn inline_interactive_manifest() -> String {
    r#"[agent]
name = "planning-agent"
version = "0.0.0"
description = "Inline test blueprint (interactive plan); self-contained."
entry_stage = "plan"

[tool_permissions]
read_file = "allow"

[stages.plan]
mode = "interactive_points"
model = { provider = "anthropic", model = "m" }
description = "Plan"
available_tools = ["read_file", "ask_user_text", "edit_document"]
allow_complete = true
system_prompt = "Produce a plan and ask for approval."
[stages.plan.transitions.implement]
hint = "approved"

[[stages.plan.interaction_points]]
name = "plan_approval"
prompt = "Approve the plan?"
required = true
style = "multiple_choice"
options = ["Approve", "Abort"]
document_region = "plan"
abort_options = ["Abort"]

[stages.implement]
mode = "autonomous"
model = { provider = "anthropic", model = "m" }
description = "Implement"
available_tools = ["write_file"]
system_prompt = "Implement the approved plan."

[context.regions]
system = { kind = "pinned", max_tokens = 8000 }
plan = { kind = "pinned", max_tokens = 6000 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_test_agent_creates_manifest_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_agent(dir.path(), "version = \"1.0\"\n");
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version = \"1.0\"\n"
        );
    }

    #[test]
    fn poison_serialize_always_errs() {
        let err = serde_json::to_string(&PoisonSerialize).unwrap_err();
        assert!(err.to_string().contains("PoisonSerialize always fails"));
    }
}
