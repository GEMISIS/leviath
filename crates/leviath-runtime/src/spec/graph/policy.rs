//! The graph-wide settings: output shape, compaction, safety, nudging,
//! dependencies and the rest of what a run does besides its stages.

use std::collections::BTreeMap;

use leviath_core::JsonDoc;
use leviath_core::output::OnValidatorError;
use leviath_core::sandbox::{OnUnavailable, SandboxKind};
use serde::{Deserialize, Serialize};

use super::stage::CodeRef;
use crate::spec::names::{
    BlueprintName, McpServerName, MimePattern, ModelRef, RegionName, ToolName,
};

/// The shape a run's final output must take.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputDef {
    /// The format, as an opaque label the model is told: `markdown`, `json`,
    /// a house format.
    #[serde(default)]
    pub format: Option<String>,
    /// How to write it.
    #[serde(default)]
    pub instructions: Option<String>,
    /// An example answer.
    #[serde(default)]
    pub example: Option<String>,
    /// A JSON Schema the answer must meet.
    #[serde(default)]
    #[schemars(with = "Option<serde_json::Value>")]
    pub schema: Option<JsonDoc>,
    /// Code that checks the answer.
    #[serde(default)]
    pub validator: Option<CodeRef>,
    /// What happens when that code itself fails.
    #[serde(default)]
    pub on_validator_error: Option<OnValidatorError>,
    /// Whether a later answer may replace files an earlier one wrote.
    #[serde(default)]
    pub overwrite_artifacts: Option<bool>,
    /// Files the answer hands back beside its text.
    #[serde(default)]
    pub artifacts: Vec<ArtifactDef>,
}

/// A file the final output hands back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDef {
    /// The file's name.
    pub name: String,
    /// Its mime type.
    pub mime_type: MimePattern,
    /// Whether the answer must include it.
    #[serde(default)]
    pub required: bool,
    /// What it is.
    #[serde(default)]
    pub description: Option<String>,
}

/// How the conversation is summarized when the context fills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactionDef {
    /// The model that writes the summary.
    pub model: ModelRef,
    /// Its system prompt, over the default.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Its request, over the default.
    #[serde(default)]
    pub user_prompt_template: Option<String>,
    /// The longest summary, in tokens.
    pub max_summary_tokens: u32,
    /// Its sampling temperature.
    pub temperature: f32,
}

/// Where tools run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxDef {
    /// The kind of sandbox.
    #[serde(default)]
    pub kind: SandboxKind,
    /// The container image, for a container sandbox.
    #[serde(default)]
    pub image: Option<String>,
    /// The container engine, over the operator's.
    #[serde(default)]
    pub engine: Option<String>,
    /// Whether tools may reach the network.
    #[serde(default = "leviath_core::default_true")]
    pub network: bool,
    /// Extra mounts, as the engine writes them.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Whether the sandbox stays up between tool calls.
    #[serde(default)]
    pub keep_warm: bool,
    /// What happens when the sandbox cannot start.
    #[serde(default)]
    pub on_unavailable: OnUnavailable,
}

/// The nudge sent when the model answers with nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NudgeDef {
    /// Whether to nudge.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The most nudges in a row.
    #[serde(default)]
    pub max: Option<u32>,
    /// The nudge's text.
    #[serde(default)]
    pub text: Option<String>,
}

/// When repeated tool calls count as a loop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepetitionDef {
    /// Whether to watch for loops.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The same call this many times in a row is a loop.
    #[serde(default)]
    pub max_repeat_calls: Option<u32>,
    /// This many read-only calls in a row is a loop.
    #[serde(default)]
    pub max_readonly_streak: Option<u32>,
}

/// Which files the run has read and written, kept in a region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileTrackingDef {
    /// The region the list lives in.
    pub region: RegionName,
    /// Whether reads are listed.
    #[serde(default = "leviath_core::default_true")]
    pub track_reads: bool,
    /// Whether writes are listed.
    #[serde(default = "leviath_core::default_true")]
    pub track_writes: bool,
    /// The most tokens one file's entry may take.
    #[serde(default)]
    pub max_file_tokens: Option<u32>,
}

/// When the tool list is looked at again.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolRescan {
    /// Once, at spawn.
    #[default]
    AtSpawn,
    /// After a stage writes a file.
    AfterWrites,
    /// Before every batch of tool calls.
    BeforeDispatch,
}

/// Commands and tools that run without approval because they only read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SafeCommandsDef {
    /// Tools.
    #[serde(default)]
    pub tools: Vec<ToolName>,
    /// Shell command prefixes.
    #[serde(default)]
    pub shell: Vec<String>,
}

/// Something the run needs from the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyDef {
    /// What it is called in messages.
    pub name: String,
    /// What must be present.
    pub needs: Needs,
    /// Whether a spawn is refused without it. Otherwise it only warns.
    #[serde(default = "leviath_core::default_true")]
    pub required: bool,
    /// How to get it, as a person reads it.
    #[serde(default)]
    pub remedy: Option<String>,
    /// What it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// How `lev deps install` gets it.
    #[serde(default)]
    pub install: Option<InstallDef>,
}

/// What a dependency needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Needs {
    /// A configured MCP server, and the environment variables it reads.
    McpServer {
        /// The server.
        server: McpServerName,
        /// Variables it needs set.
        #[serde(default)]
        env: Vec<String>,
    },
    /// An environment variable.
    Env(String),
    /// A program on `PATH`.
    Binary(String),
    /// Code that says whether it is there.
    Check(CodeRef),
}

/// How a dependency is installed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallDef {
    /// One command for every platform.
    #[serde(default)]
    pub command: Option<String>,
    /// A command per platform (`macos`, `linux`, `windows`).
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    /// Code that installs it.
    #[serde(default)]
    pub script: Option<CodeRef>,
    /// An MCP server entry to add.
    #[serde(default)]
    pub server: Option<McpServerTemplate>,
}

/// An MCP server entry a dependency's install adds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerTemplate {
    /// `stdio` or `http`.
    #[serde(default)]
    pub transport: Option<String>,
    /// The command, for stdio.
    #[serde(default)]
    pub command: Option<String>,
    /// The URL, for http.
    #[serde(default)]
    pub url: Option<String>,
    /// The command's arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// HTTP headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// How an MCP server is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// Started as a child process, spoken to over its pipes.
    Stdio,
    /// Reached over HTTP.
    Http,
}

/// An MCP server the run brings with it, connected for the runs that need it
/// beside the operator's own servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerDef {
    /// The server's name, which its tools are prefixed with.
    pub name: McpServerName,
    /// How it is reached. Left out, `command` means stdio and `url` HTTP.
    #[serde(default)]
    pub transport: Option<McpTransport>,
    /// The command that starts it, for stdio.
    #[serde(default)]
    pub command: Option<String>,
    /// Where it is, for HTTP.
    #[serde(default)]
    pub url: Option<String>,
    /// The command's arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables the command is started with.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// HTTP headers. A value may name an environment variable as `${NAME}`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// What a script tool's host function may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScriptPermission {
    /// It may run.
    Allow,
    /// It is refused.
    Deny,
    /// It follows the run's own permission for the matching built-in tool.
    Inherit,
}

/// What the run's script tools may do, per host function. A graph can only
/// make the operator's settings stricter; a function it leaves out keeps the
/// operator's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScriptPermissionsDef {
    /// `http_get`.
    #[serde(default)]
    pub http_get: Option<ScriptPermission>,
    /// `http_post`.
    #[serde(default)]
    pub http_post: Option<ScriptPermission>,
    /// `shell`.
    #[serde(default)]
    pub shell: Option<ScriptPermission>,
    /// `read_file`.
    #[serde(default)]
    pub read_file: Option<ScriptPermission>,
    /// `write_file`.
    #[serde(default)]
    pub write_file: Option<ScriptPermission>,
    /// `env_var`.
    #[serde(default)]
    pub env_var: Option<ScriptPermission>,
}

/// How a child run's context is built from its parent's, by blueprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextTransformDef {
    /// The parent's blueprint.
    pub from: BlueprintName,
    /// The child's blueprint.
    pub to: BlueprintName,
    /// Which parent region fills which child region.
    pub mappings: Vec<RegionMappingDef>,
}

/// One region carried from a parent to a child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionMappingDef {
    /// The parent's region.
    pub from: RegionName,
    /// The child's region.
    pub to: RegionName,
    /// How the content changes on the way.
    #[serde(default)]
    pub transform: ContentTransform,
}

/// How carried content changes on the way to a child.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentTransform {
    /// As it is.
    #[default]
    Direct,
    /// Summarized.
    Summarize,
    /// Only the named fields of JSON content.
    Extract(Vec<String>),
}

/// One row of the run's mime registry: what the engine knows about a type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MimeRowDef {
    /// What providers key their encoders on.
    #[serde(default)]
    pub family: Option<String>,
    /// Whether the bytes are UTF-8 text.
    #[serde(default)]
    pub text: Option<bool>,
    /// How to estimate the part's tokens.
    #[serde(default)]
    pub tokens: Option<TokenRule>,
    /// File extensions, lowercase, without the dot.
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
    /// A hex prefix identifying the bytes.
    #[serde(default)]
    pub magic: Option<String>,
    /// The stand-in template shown to a model that cannot take the type.
    #[serde(default)]
    pub stand_in: Option<String>,
    /// Code the bytes must pass to be stored as this type.
    #[serde(default)]
    pub check: Option<CodeRef>,
}

/// How a part's tokens are estimated.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenRule {
    /// Tokens per byte.
    PerByte(f64),
    /// One token per so many pixels, up to a cap.
    PerPixel {
        /// Pixels per token.
        divisor: u32,
        /// The most tokens.
        max: u32,
    },
    /// Tokens per second of media.
    PerSecond(u32),
    /// Tokens per page.
    PerPage(u32),
    /// A fixed count.
    Fixed(u32),
}

/// The mime registry rows a run adds, by type or pattern.
pub type MimeRows = BTreeMap<MimePattern, MimeRowDef>;
