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
    /// Values for the worker graph's declared inputs, by name, as the model
    /// wrote them. Checked against those declarations before any worker
    /// starts. Null when the model gave the item none.
    ///
    /// JSON because the names and types are the worker graph's own, declared
    /// by whoever wrote it, and this schema cannot name them.
    #[serde(default, deserialize_with = "object")]
    pub(crate) inputs: Option<Json>,
}

/// A JSON object, as the tools declare `inputs`: anything else does not fit.
fn object<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Json>, D::Error> {
    serde_json::Map::<String, serde_json::Value>::deserialize(d)
        .map(|map| Some(Json(serde_json::Value::Object(map))))
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

/// Arguments for the `spawn_agent` tool, and for `validate_spawn`, which takes
/// exactly the same.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SpawnAgentArgs {
    /// What the child runs: an installed blueprint, or a whole graph.
    pub(crate) source: SpawnAgentSource,
    /// Values for the child graph's declared inputs, by name, as the model
    /// wrote them. JSON because the names and types are that graph's own.
    #[serde(default, deserialize_with = "object")]
    pub(crate) inputs: Option<Json>,
    /// Whether the caller blocked until it finished. Left out means no, which is
    /// also what the tool's own default says.
    #[serde(default)]
    pub(crate) wait: Option<bool>,
    /// A depth limit for the child's own children.
    #[serde(default)]
    pub(crate) max_child_depth: Option<i32>,
    /// The output shape the child was asked for, over its graph's own.
    #[serde(default)]
    pub(crate) output: Option<AskedOutput>,
    /// Stored parts of this run handed to the child, each by name or by the
    /// start of its sha256.
    #[serde(default)]
    pub(crate) parts: Option<Vec<String>>,
}

/// The output shape a `spawn_agent` call asked its child for.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct AskedOutput {
    /// The format label: markdown, json, a mime type, a house format.
    #[serde(default)]
    pub(crate) format: Option<String>,
    /// Guidance about that shape.
    #[serde(default)]
    pub(crate) instructions: Option<String>,
    /// An example answer.
    #[serde(default)]
    pub(crate) example: Option<String>,
    /// A JSON Schema the answer must meet.
    #[serde(default)]
    pub(crate) schema: Option<Json>,
}

/// Arguments for the `spawn_schema` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SpawnSchemaArgs {
    /// The part of the spawn request's schema asked for. Null asked for the
    /// top level.
    #[serde(default)]
    pub(crate) part: Option<String>,
}

/// Arguments for the `describe_blueprint` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct DescribeBlueprintArgs {
    /// The installed blueprint asked about.
    pub(crate) blueprint: SpawnAgentBlueprintSource,
}

/// What a `run_history` call asked to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, async_graphql::Enum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunHistoryView {
    /// The run in brief.
    Summary,
    /// Its whole state, now or at a step.
    State,
    /// Each edge it took.
    Transitions,
}

/// Arguments for the `run_history` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct RunHistoryArgs {
    /// The run read.
    pub(crate) run_id: ID,
    /// What it asked to see. Null asked for the summary.
    #[serde(default)]
    pub(crate) view: Option<RunHistoryView>,
    /// The step the state was asked at. Null asked for now.
    #[serde(default)]
    pub(crate) at: Option<i32>,
}

/// What a `spawn_agent` call asked the child to run.
///
/// The tool takes either an installed blueprint or a whole graph, and which
/// one the model chose is part of what it asked for, so it is a union here
/// rather than a reading that flattens the two.
#[derive(Debug, Deserialize, async_graphql::Union)]
#[serde(from = "SourceWire")]
pub(crate) enum SpawnAgentSource {
    /// An installed blueprint.
    Blueprint(SpawnAgentBlueprintSource),
    /// A graph the model wrote.
    Graph(SpawnAgentGraphSource),
}

/// An installed blueprint a `spawn_agent` or `describe_blueprint` call named.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
#[serde(from = "BlueprintWire")]
pub(crate) struct SpawnAgentBlueprintSource {
    /// The blueprint's name.
    pub(crate) name: String,
    /// The revision the model pinned it to. Null when it named the blueprint
    /// alone.
    pub(crate) digest: Option<String>,
}

/// A whole graph a `spawn_agent` call wrote for its child.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct SpawnAgentGraphSource {
    /// The graph, as the model wrote it, in the JSON form a spawn request's
    /// raw graph takes.
    pub(crate) graph: Json,
}

impl From<BlueprintWire> for SpawnAgentBlueprintSource {
    fn from(wire: BlueprintWire) -> Self {
        match wire {
            BlueprintWire::Name(name) => Self { name, digest: None },
            BlueprintWire::Pinned { name, digest } => Self { name, digest },
        }
    }
}

/// The two shapes the tool's `source` takes, before either is a type.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum SourceWire {
    /// `{ "blueprint": "coder" }` or `{ "blueprint": { "name": "coder" } }`.
    Blueprint {
        /// The blueprint, by name or by name and digest.
        blueprint: SpawnAgentBlueprintSource,
    },
    /// `{ "graph": { ... } }`.
    Graph {
        /// The graph.
        graph: serde_json::Value,
    },
}

/// A blueprint as the tool takes it: a name, or a name and a digest.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum BlueprintWire {
    /// `"coder"`, or the absolute directory of a blueprint that is not
    /// installed.
    Name(String),
    /// `{ "name": "coder", "digest": "..." }`.
    Pinned {
        /// The name.
        name: String,
        /// The revision.
        #[serde(default)]
        digest: Option<String>,
    },
}

impl From<SourceWire> for SpawnAgentSource {
    fn from(wire: SourceWire) -> Self {
        match wire {
            SourceWire::Blueprint { blueprint } => Self::Blueprint(blueprint),
            SourceWire::Graph { graph } => {
                Self::Graph(SpawnAgentGraphSource { graph: Json(graph) })
            }
        }
    }
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
