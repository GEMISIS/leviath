//! Where a new agent starts: a two-stage starter, or a copy of one that
//! exists.

use super::{EditError, ManifestDoc};

/// The smallest agent that does something: `work` then `finish`, with
/// placeholder prompts, a task to work on, and a layout that scales with the
/// model's window. The same starter The Lair's "Start simple" gives.
pub(crate) fn empty_blueprint(name: &str) -> Result<String, EditError> {
    let mut doc = ManifestDoc::parse(EMPTY).expect("the starter is a valid agent.toml");
    doc.set_agent_name(name)?;
    Ok(doc.to_toml())
}

const EMPTY: &str = r#"[blueprint]
name = "my-agent"
version = "0.0.1"
description = "Describe what this agent does."

[graph]
entry = "work"

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "task" }]

[graph.layout]
total_budget_tokens = 0
regions = [
    { name = "task", kind = "pinned", budget = "5%" },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 40 }, budget = "50%" },
]

[[graph.stages]]
name = "work"
mode = "autonomous"
description = "Do the task"
max_iterations = 25
system_prompt = "You are a capable, careful agent. Work on the task you were given step by step, and verify your work as you go."

[[graph.edges]]
name = "finish"
from = "work"
to = "finish"
hint = "The work is done and verified"

[[graph.stages]]
name = "finish"
mode = "autonomous"
description = "Wrap up and report"
max_iterations = 5
system_prompt = "Summarize what was done, what changed, and anything left open."
"#;

/// A copy of an existing agent under a new name: only `[blueprint] name`
/// changes.
pub(crate) fn clone_of(text: &str, new_name: &str) -> Result<String, EditError> {
    let mut doc = ManifestDoc::parse(text)?;
    doc.set_agent_name(new_name)?;
    Ok(doc.to_toml())
}
