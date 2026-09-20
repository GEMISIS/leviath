//! Arguments for the tools that ask a person something, end a stage, or run
//! another agent.
//!
//! Three small groups in one file rather than three files of five types: they
//! are all plain mirrors of a declared schema, and splitting them further would
//! only add places to look.

use async_graphql::SimpleObject;
use serde::Deserialize;

use super::super::super::scalars::Json;

/// Arguments for the `present_for_review` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct PresentForReviewArgs {
    /// The short title shown above the review prompt.
    pub(crate) title: String,
    /// The document presented, as markdown.
    pub(crate) markdown: String,
}

/// Arguments for the `ask_user_text` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct AskUserTextArgs {
    /// The question asked.
    pub(crate) prompt: String,
}

/// Arguments for the `ask_user_choice` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct AskUserChoiceArgs {
    /// The question asked.
    pub(crate) prompt: String,
    /// The options offered. The tool refuses fewer than two, so a call recorded
    /// with one is a call that never ran.
    pub(crate) options: Vec<String>,
}

/// Arguments for the `ask_user_confirm` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct AskUserConfirmArgs {
    /// The yes or no question asked.
    pub(crate) prompt: String,
}

/// Arguments for the `edit_document` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct EditDocumentArgs {
    /// The document handed over for editing.
    pub(crate) content: String,
    /// The instruction shown above the editable field.
    #[serde(default)]
    pub(crate) prompt: Option<String>,
}

/// Arguments for the `submit_output` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SubmitOutputArgs {
    /// The final answer, in full.
    pub(crate) content: String,
    /// Files produced alongside it. Each entry is raw JSON because the tool
    /// takes either a bare path or an object, and a debugger showing what was
    /// submitted must show whichever of the two the model chose.
    #[serde(default)]
    pub(crate) artifacts: Option<Vec<Json>>,
}

/// One unit of work handed to a fan-out worker.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct FanOutItem {
    /// The item's own id, which names its child run.
    pub(crate) id: String,
    /// Everything the worker gets, which the blueprint's author defines. Raw
    /// JSON because nothing here knows its shape.
    pub(crate) context: Json,
}

/// Arguments for the `fan_out` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct FanOutArgs {
    /// The blueprint each item runs. Left out inside a fan-out stage, which
    /// names its worker itself.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// One entry per unit of work. An empty list is a valid answer: it says
    /// there was nothing to hand out.
    pub(crate) items: Vec<FanOutItem>,
    /// How many run at once, where the model capped it.
    #[serde(default)]
    pub(crate) max_workers: Option<i32>,
}

/// Arguments for the `spawn_agent` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SpawnAgentArgs {
    /// The blueprint to run, by name.
    pub(crate) blueprint: String,
    /// The task handed to it.
    pub(crate) task: String,
    /// Whether the caller blocked until it finished. Left out means no, which is
    /// also what the tool's own default says.
    #[serde(default)]
    pub(crate) wait: Option<bool>,
    /// Context put in the child's first pinned region.
    #[serde(default)]
    pub(crate) seed_context: Option<String>,
    /// Stored parts of this run handed to the child, each by name or by the
    /// start of its sha256.
    #[serde(default)]
    pub(crate) parts: Option<Vec<String>>,
    /// A depth limit for the child's own children.
    #[serde(default)]
    pub(crate) max_child_depth: Option<i32>,
    /// The shape the child was asked to answer in, overriding its blueprint's.
    #[serde(default)]
    pub(crate) output_format: Option<String>,
    /// Extra guidance about that shape.
    #[serde(default)]
    pub(crate) output_instructions: Option<String>,
}

/// Arguments for the `check_agent` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct CheckAgentArgs {
    /// The agent asked about, by its live id.
    pub(crate) agent_id: String,
}

/// Arguments for the `wait_for_agent` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct WaitForAgentArgs {
    /// The agent waited for, by its live id.
    pub(crate) agent_id: String,
}

/// Arguments for the `send_to_agent` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SendToAgentArgs {
    /// The agent written to, by its live id.
    pub(crate) agent_id: String,
    /// What was sent.
    pub(crate) message: String,
    /// The region it was delivered to. Left out means the conversation.
    #[serde(default)]
    pub(crate) target_region: Option<String>,
}

/// Arguments for the `kill_agent` tool.
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct KillAgentArgs {
    /// The agent killed, by its live id.
    pub(crate) agent_id: String,
}
