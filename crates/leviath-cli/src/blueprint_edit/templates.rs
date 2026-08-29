//! Where a new agent starts: a two-stage starter, or a copy of one that
//! exists.

use super::{EditError, ManifestDoc};

/// The smallest agent that does something: `work` then `finish`, with
/// placeholder prompts. The same starter The Lair's "Start simple" gives.
pub(crate) fn empty_blueprint(name: &str) -> Result<String, EditError> {
    let mut doc = ManifestDoc::parse(EMPTY).expect("the starter is a valid manifest");
    doc.set_agent_name(name)?;
    Ok(doc.to_toml())
}

const EMPTY: &str = r#"[agent]
name = "my-agent"
version = "0.0.1"
description = "Describe what this agent does."
entry_stage = "work"

[stages.work]
mode = "autonomous"
description = "Do the task"
max_iterations = 25
system_prompt = "You are a capable, careful agent. Work on the task you were given step by step, and verify your work as you go."

[stages.work.transitions.finish]
hint = "The work is done and verified"

[stages.finish]
mode = "autonomous"
description = "Wrap up and report"
max_iterations = 5
system_prompt = "Summarize what was done, what changed, and anything left open."

[stages.finish.transitions]
"#;

/// A copy of an existing manifest under a new name: only `[agent].name`
/// changes.
pub(crate) fn clone_of(text: &str, new_name: &str) -> Result<String, EditError> {
    let mut doc = ManifestDoc::parse(text)?;
    doc.set_agent_name(new_name)?;
    Ok(doc.to_toml())
}
