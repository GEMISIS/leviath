//! Arguments for the tools that ask a person something, end a stage, or run
//! another blueprint.
//!
//! Three small groups in one file rather than three files of five types: they
//! are all plain mirrors of a declared schema, and splitting them further would
//! only add places to look.

use async_graphql::{ID, SimpleObject};
use leviath_graphql_derive::mirror;
use serde::Deserialize;

use super::super::super::scalars::Json;

/// Arguments for the `present_for_review` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct PresentForReviewArgs {
    /// The short title shown above the review prompt.
    pub(crate) title: String,
    /// The document presented, as markdown.
    pub(crate) markdown: String,
}

/// Arguments for the `ask_user_text` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct AskUserTextArgs {
    /// The question asked.
    pub(crate) prompt: String,
}

/// Arguments for the `ask_user_choice` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct AskUserChoiceArgs {
    /// The question asked.
    pub(crate) prompt: String,
    /// The options offered. The tool refuses fewer than two, so a call recorded
    /// with one is a call that never ran.
    pub(crate) options: Vec<String>,
}

/// Arguments for the `ask_user_confirm` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct AskUserConfirmArgs {
    /// The yes or no question asked.
    pub(crate) prompt: String,
}

/// Arguments for the `edit_document` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct EditDocumentArgs {
    /// The document handed over for editing.
    pub(crate) content: String,
    /// The instruction shown above the editable field.
    #[serde(default)]
    pub(crate) prompt: Option<String>,
}

/// Arguments for the `submit_output` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SubmitOutputArgs {
    /// The final answer, in full.
    pub(crate) content: String,
    /// Files produced alongside it, in whichever of the tool's two shapes the
    /// model wrote each one. Absent when the answer named no files.
    #[serde(default)]
    pub(crate) artifacts: Option<Vec<SubmittedArtifact>>,
}

/// One entry of `submit_output`'s `artifacts` argument.
///
/// The tool takes a bare path or an object, and which one the model chose is
/// part of what was submitted, so it is a type here rather than a reading that
/// flattens the two. The run's own record of what it produced is `artifacts` on
/// the run, which is resolved and typed either way.
#[derive(Debug, Deserialize, async_graphql::Union)]
#[serde(from = "ArtifactWire")]
pub(crate) enum SubmittedArtifact {
    /// A path on its own.
    Path(ArtifactByPath),
    /// A path with what to call it, or what it is.
    Described(ArtifactDescribed),
}

/// An artifact the model named by path alone.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ArtifactByPath {
    /// The file, relative to the run's working directory.
    pub(crate) path: String,
}

/// An artifact the model named and described.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ArtifactDescribed {
    /// The file, relative to the run's working directory.
    pub(crate) path: String,
    /// What to call it. Null when the model left it to the file name.
    pub(crate) name: Option<String>,
    /// The `type` key the model wrote, which is a mime type. Null when it left
    /// the type to the registry.
    pub(crate) mime_type: Option<String>,
}

/// The two shapes the tool's schema accepts, before either is a type.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ArtifactWire {
    /// `"out/report.md"`.
    Path(String),
    /// `{ "path": "out/report.md", "name": "Report", "type": "text/markdown" }`.
    Described {
        /// Where the file is.
        path: String,
        /// What to call it.
        #[serde(default)]
        name: Option<String>,
        /// What it is.
        #[serde(default, rename = "type")]
        mime_type: Option<String>,
    },
}

impl From<ArtifactWire> for SubmittedArtifact {
    fn from(wire: ArtifactWire) -> Self {
        match wire {
            ArtifactWire::Path(path) => Self::Path(ArtifactByPath { path }),
            ArtifactWire::Described {
                path,
                name,
                mime_type,
            } => Self::Described(ArtifactDescribed {
                path,
                name,
                mime_type,
            }),
        }
    }
}

/// One unit of work handed to a fan-out worker.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct FanOutItem {
    /// The item's own id, which names its child run.
    pub(crate) id: ID,
    /// Everything the worker gets, which the blueprint's author defines.
    ///
    /// Raw JSON because the tool declares it as an object and nothing more: the
    /// worker is seeded with whatever this holds, so its shape is a contract
    /// between one blueprint's stages and nothing this schema can name.
    pub(crate) context: Json,
}

/// Arguments for the `fan_out` tool.
#[mirror(no_filter)]
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
#[mirror(no_filter)]
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
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct CheckAgentArgs {
    /// The agent asked about, by its live id.
    pub(crate) agent_id: ID,
}

/// Arguments for the `wait_for_agent` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct WaitForAgentArgs {
    /// The agent waited for, by its live id.
    pub(crate) agent_id: ID,
}

/// Arguments for the `send_to_agent` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SendToAgentArgs {
    /// The agent written to, by its live id.
    pub(crate) agent_id: ID,
    /// What was sent.
    pub(crate) message: String,
    /// The region it was delivered to. Left out means the conversation.
    #[serde(default)]
    pub(crate) target_region: Option<String>,
}

/// Arguments for the `kill_agent` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct KillAgentArgs {
    /// The agent killed, by its live id.
    pub(crate) agent_id: ID,
}
