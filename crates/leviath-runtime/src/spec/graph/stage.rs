//! A stage of a run graph: which model runs it, which tools it has, how it
//! behaves, and the code it hooks in.

use std::collections::BTreeMap;

use leviath_core::JsonDoc;
use leviath_core::policy::ToolPolicy;
use serde::{Deserialize, Serialize};

use super::policy::{NudgeDef, SandboxDef};
use super::region::RegionLayoutDef;
use crate::spec::names::{
    BlueprintPath, BlueprintRef, McpServerName, MimePattern, ModelRef, NameError, RegionName,
    StageName, ToolName, WorkerName,
};

/// One stage.
// The run file's binary frames keep the fields this had before
// `prompt_regions` (see `spec::readable_graph`); readable formats write them
// all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct StageDef {
    /// The stage's name, unique in the graph.
    pub name: StageName,
    /// What the stage is for.
    #[serde(default)]
    pub description: Option<String>,
    /// The stage's system prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// The models that may run it.
    #[serde(default)]
    pub model: ModelChoice,
    /// The tools it may call.
    #[serde(default)]
    pub tools: Vec<ToolSelector>,
    /// Tools the stage must have; a spawn that cannot provide one is refused.
    #[serde(default)]
    pub required_tools: Vec<ToolName>,
    /// MCP servers whose tools the stage may call.
    #[serde(default)]
    pub connectors: Vec<McpServerName>,
    /// The most inference rounds the stage runs before it is cut off.
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// How the stage runs.
    #[serde(default)]
    pub mode: StageMode,
    /// A layout for this stage alone, replacing the graph's.
    #[serde(default)]
    pub layout: Option<RegionLayoutDef>,
    /// Regions this stage does not show the model.
    #[serde(default)]
    pub hide: Vec<RegionName>,
    /// The regions a media model's prompt is made of, in order. A stage whose
    /// model takes its prompt from the request's text (an image, video or
    /// speech model) sends only these regions' content; unset, it reads every
    /// region it shows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_regions: Vec<RegionName>,
    /// Regions emptied when the stage starts.
    #[serde(default)]
    pub reset: Vec<RegionName>,
    /// Per-tool approval policy for this stage.
    #[serde(default)]
    pub tool_permissions: BTreeMap<ToolName, ToolPolicy>,
    /// Whether the stage waits for the child runs it started before it ends.
    #[serde(default)]
    pub requires_children: bool,
    /// How many times the stage may be re-entered by a gate before it is let
    /// through anyway.
    #[serde(default)]
    pub max_revisits: Option<u32>,
    /// Extra guidance shown when the model picks the next edge.
    #[serde(default)]
    pub transition_prompt: Option<String>,
    /// How the call that picks the next edge is retried, and what a failure
    /// does. `None` takes the graph's.
    #[serde(default)]
    pub routing: Option<super::RoutingDef>,
    /// Whether messages sent to the run reach this stage.
    #[serde(default = "leviath_core::default_true")]
    pub accepts_messages: bool,
    /// Whether the model may end the run from this stage.
    #[serde(default)]
    pub allow_complete: bool,
    /// Whether a fan-out may run this stage as a worker.
    #[serde(default)]
    pub allow_as_worker: bool,
    /// Whether tools that wait on a person may run here.
    #[serde(default)]
    pub allow_blocking_tools: bool,
    /// Whether taint tracking is on for this stage. `None` takes the graph's.
    #[serde(default)]
    pub taint_tracking: Option<bool>,
    /// The batch-tool-calls hint. `None` takes the graph's.
    #[serde(default)]
    pub batch_tool_hint: Option<bool>,
    /// The platform shell hint. `None` takes the graph's.
    #[serde(default)]
    pub shell_hint: Option<bool>,
    /// The empty-response nudge. `None` takes the graph's.
    #[serde(default)]
    pub nudge: Option<NudgeDef>,
    /// Where tools run. `None` takes the graph's.
    #[serde(default)]
    pub sandbox: Option<SandboxDef>,
    /// Where tool results land.
    #[serde(default)]
    pub tool_routing: Option<ToolRoutingDef>,
    /// Which region each named part of the stage's output lands in.
    #[serde(default)]
    pub output_routing: BTreeMap<String, RegionName>,
    /// The stage's final-output shape, over the graph's.
    #[serde(default)]
    pub output: Option<super::OutputDef>,
    /// Whether the stage must hand back a final output before it ends.
    #[serde(default)]
    pub require_output: bool,
    /// Mime types a message or part sent to this stage may carry.
    #[serde(default)]
    pub input_accepts: Vec<MimePattern>,
    /// Mime types turned into text before this stage's model sees them.
    #[serde(default)]
    pub input_as_text: Vec<MimePattern>,
    /// Mime types each tool's results may carry.
    #[serde(default)]
    pub tool_accepts: BTreeMap<ToolName, Vec<MimePattern>>,
    /// Globs, relative to the workdir, that this stage's writes must match
    /// as well as the graph's `write_paths`. Empty adds no limit.
    #[serde(default)]
    pub write_paths: Vec<String>,
    /// Code run at points of the stage's life.
    #[serde(default)]
    pub hooks: StageHooks,
}

impl StageDef {
    /// The mime patterns the stage declares as its output: its output format
    /// when that is a mime type, and the types its output routing names.
    ///
    /// The one answer to "what does this stage expect its model to hand
    /// back": a provider reads it to ask for an audio answer, and the resolver
    /// to prefer a route whose model can give one. An `output.artifacts`
    /// entry is not part of it, because a file the answer hands back may as
    /// well be one a tool wrote.
    pub fn declared_output_mimes(&self) -> Vec<String> {
        let format = self.output.as_ref().and_then(|o| o.format.as_deref());
        format
            .into_iter()
            .map(str::to_string)
            .chain(self.output_routing.keys().cloned())
            .filter(|m| m.contains('/'))
            .collect()
    }

    /// Whether the stage has a tool that changes files the framework sees: a
    /// built-in that writes, every built-in through a group, or one of
    /// `extra`, the tools a gate counts as writing, when the stage names it.
    pub fn can_change_files(&self, extra: &[ToolName]) -> bool {
        let groups = self.tools.iter().any(|t| {
            matches!(
                t,
                ToolSelector::Group(ToolGroup::All) | ToolSelector::Group(ToolGroup::Builtin)
            )
        });
        groups
            || self.tools.iter().any(|t| match t {
                ToolSelector::Tool(name) => {
                    let canonical = leviath_tools::canonical_tool_name(name.as_str());
                    super::MODIFYING_TOOLS.contains(&canonical)
                        || extra
                            .iter()
                            .any(|x| leviath_tools::canonical_tool_name(x.as_str()) == canonical)
                }
                ToolSelector::Group(_) => false,
            })
    }
}

/// The models a stage may run on, best first.
// The run file's binary frames keep the four fields this had before
// `effort` (see `spec::readable::choice`); readable formats write them all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ModelChoice {
    /// The models, best first. Empty takes the operator's default.
    #[serde(default)]
    pub models: Vec<ModelRef>,
    /// Whether the operator's default model may stand in when none of
    /// `models` is available.
    #[serde(default = "leviath_core::default_true")]
    pub allow_user_default: bool,
    /// Sampling and output settings.
    #[serde(default)]
    pub params: ModelParams,
    /// How long one request may take, in seconds.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
    /// How hard the model should think: `low`, `medium` or `high`. Each
    /// provider sends it in its own field, at a level the model takes, and a
    /// model with no such control is sent nothing. Unset leaves each model at
    /// its own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<leviath_core::Effort>,
}

impl Default for ModelChoice {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            allow_user_default: true,
            params: ModelParams::default(),
            request_timeout_secs: None,
            effort: None,
        }
    }
}

/// Settings sent with each model request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelParams {
    /// Sampling temperature. Kept as the number written, so a `0.2` reads
    /// back as `0.2`.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// The cap on one reply's length.
    #[serde(default)]
    pub max_output_tokens: Option<OutputCap>,
    /// Provider-specific settings passed through as written (`top_p`,
    /// `reasoning_effort`, `stop`). Their meaning belongs to the provider.
    #[serde(default)]
    pub extra: BTreeMap<String, ParamScalar>,
}

/// The cap on one reply's length.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum OutputCap {
    /// A fixed number of tokens.
    Tokens(u32),
    /// A fraction (0 to 1) of the model's context window.
    WindowPercent(f64),
    /// A fraction (0 to 1) of one region's budget.
    RegionPercent {
        /// The share.
        percent: f64,
        /// The region.
        region: RegionName,
    },
}

impl OutputCap {
    /// The cap in tokens for one request.
    ///
    /// `model_window` and `model_max_output` are the model's own limits;
    /// `region_budget` answers "how many tokens may region X hold" for the
    /// window the request is built from. A relative cap is clamped to the
    /// model's maximum. A region cap naming a region the stage does not carry
    /// falls back to the model's maximum, which is the same "as much as you
    /// can" the author was reaching for. Never less than one token.
    pub fn resolve(
        &self,
        model_window: usize,
        model_max_output: usize,
        region_budget: impl Fn(&str) -> Option<usize>,
    ) -> usize {
        let share = |whole: usize, fraction: f64| (whole as f64 * fraction).round() as usize;
        match self {
            OutputCap::Tokens(t) => *t as usize,
            OutputCap::WindowPercent(p) => share(model_window, *p).min(model_max_output),
            OutputCap::RegionPercent { percent, region } => match region_budget(region.as_str()) {
                Some(budget) => share(budget, *percent).min(model_max_output),
                None => model_max_output,
            },
        }
        .max(1)
    }
}

/// A provider-specific model setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum ParamScalar {
    /// `true` or `false`.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// A number.
    Float(f64),
    /// Text.
    Text(String),
    /// A list of text, such as stop sequences.
    TextList(Vec<String>),
    /// A table of settings, such as Bedrock's
    /// `thinking = { type = "enabled", budget_tokens = 2000 }`.
    Table(BTreeMap<String, ParamScalar>),
}

impl ParamScalar {
    /// The setting as a provider's request carries it.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Int(i) => serde_json::Value::from(*i),
            Self::Float(f) => serde_json::Value::from(*f),
            Self::Text(t) => serde_json::Value::String(t.clone()),
            Self::TextList(items) => serde_json::Value::from(items.clone()),
            Self::Table(table) => serde_json::Value::Object(
                table
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect(),
            ),
        }
    }
}

/// A tool a stage may call, or a whole group of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum ToolSelector {
    /// One tool.
    Tool(ToolName),
    /// A group of tools.
    Group(ToolGroup),
}

/// A named group of tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroup {
    /// Every tool this machine offers.
    All,
    /// The built-in tools.
    Builtin,
    /// The tools that start and manage child runs.
    Subagent,
    /// Script tools.
    Scripts,
    /// Every connected MCP server's tools.
    Mcp,
}

impl ToolGroup {
    /// Every group.
    pub const ALL: [ToolGroup; 5] = [
        ToolGroup::All,
        ToolGroup::Builtin,
        ToolGroup::Subagent,
        ToolGroup::Scripts,
        ToolGroup::Mcp,
    ];

    /// The token a tool grant list writes for this group. A tool name never
    /// starts with `@`, so a token cannot be mistaken for a tool.
    pub fn token(self) -> &'static str {
        match self {
            ToolGroup::All => "@all",
            ToolGroup::Builtin => "@builtin",
            ToolGroup::Subagent => "@subagent",
            ToolGroup::Scripts => "@scripts",
            ToolGroup::Mcp => "@mcp",
        }
    }

    /// The group a grant names, or `None` for a tool name (or a token that
    /// names no group).
    pub fn parse(entry: &str) -> Option<ToolGroup> {
        ToolGroup::ALL.into_iter().find(|g| g.token() == entry)
    }

    /// Whether `entry` is spelled like a group token, whether or not it names
    /// one.
    pub fn is_token(entry: &str) -> bool {
        entry.starts_with('@')
    }

    /// The groups a grant list names, in list order, each once.
    pub fn named_in(entries: &[String]) -> Vec<ToolGroup> {
        let mut groups = Vec::new();
        for group in entries.iter().filter_map(|e| ToolGroup::parse(e)) {
            if !groups.contains(&group) {
                groups.push(group);
            }
        }
        groups
    }

    /// Whether a tool from `source` is covered by this group.
    pub fn covers(self, source: ToolGroup) -> bool {
        self == ToolGroup::All || self == source
    }
}

/// How a stage runs.
///
/// A blueprint writes a mode with no settings as its name (`"interactive"`)
/// and one with settings as a table (`{ interactive = { idle_timeout_secs =
/// 900 } }`); see `spec::readable` for both forms.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum StageMode {
    /// The model works on its own until it moves on.
    #[default]
    Autonomous,
    /// The model and a person take turns. After a reply with no tool calls
    /// the stage waits for the next message instead of moving on; it leaves
    /// when the model calls `end_conversation`, a person ends the
    /// conversation, or the idle limit runs out.
    Interactive(InteractiveDef),
    /// The model works on its own, stopping at declared points for a person.
    InteractivePoints(Vec<InteractionPointDef>),
    /// The stage splits its work over worker runs. It is given the
    /// `fan_out` tool it starts them with, whether or not its `tools` name it.
    FanOut(FanOutDef),
    /// The stage only writes the run's final output. It is given the
    /// `submit_output` tool and must call it, whether or not its `tools` name
    /// it, and it may end the run when no edge leaves it.
    Output,
}

impl StageMode {
    /// The settings of an interactive stage, or `None` for any other mode.
    pub fn interactive(&self) -> Option<&InteractiveDef> {
        match self {
            Self::Interactive(def) => Some(def),
            _ => None,
        }
    }
}

/// The settings of an interactive stage.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct InteractiveDef {
    /// How long, in seconds, the stage waits for the next message before it
    /// moves on as though the conversation had been ended. Left out, it waits
    /// for as long as it takes, unless nobody is watching the run.
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
}

/// A point where a stage stops for a person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractionPointDef {
    /// The point's name.
    pub name: String,
    /// What the person is asked.
    pub prompt: String,
    /// Whether the stage cannot end without an answer.
    #[serde(default)]
    pub required: bool,
    /// What an unattended run does here.
    #[serde(default)]
    pub unattended: UnattendedPoint,
    /// How the person answers.
    #[serde(default)]
    pub style: AnswerStyle,
    /// The choices, for a multiple-choice point.
    #[serde(default)]
    pub options: Vec<String>,
    /// What the model is told after each choice.
    #[serde(default)]
    pub directives: BTreeMap<String, String>,
    /// Choices that end the run.
    #[serde(default)]
    pub abort_options: Vec<String>,
    /// Choices that open the document for editing.
    #[serde(default)]
    pub edit_options: Vec<String>,
    /// The region holding the document the person reviews.
    #[serde(default)]
    pub document_region: Option<RegionName>,
}

/// What an unattended run does at an interaction point.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UnattendedPoint {
    /// Answer it automatically.
    #[default]
    AutoApprove,
    /// Wait for a person anyway.
    Ask,
}

/// How a person answers an interaction point.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStyle {
    /// Free text.
    #[default]
    FreeText,
    /// One of the point's options.
    MultipleChoice,
    /// Yes or no.
    Confirm,
}

/// A fan-out stage: split work over worker runs, then merge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FanOutDef {
    /// The stage this fan-out belongs to. In a blueprint it is the stage the
    /// fan-out sits under, named again so the declaration says on its own
    /// which stage hands the work out.
    pub stage: StageName,
    /// The workers this fan-out may hand items to, at least one. With more
    /// than one, the model names the worker for each item, and a name not on
    /// this list is refused before anything starts.
    pub workers_available: Vec<WorkerOption>,
    /// The stage the merged results go to.
    #[serde(default)]
    pub merge_stage: Option<StageName>,
    /// The most workers at once, at least one. Left out, every item starts
    /// at once and the inference pool paces the model calls.
    #[serde(default)]
    pub max_workers: Option<u32>,
    /// What one worker's failure does to the rest.
    #[serde(default)]
    pub on_worker_failure: WorkerFailure,
    /// The prompt that asks the model to split the work.
    #[serde(default)]
    pub split_prompt: String,
    /// The region the workers' results land in.
    #[serde(default)]
    pub results_region: Option<RegionName>,
    /// The most work items one fan-out may have, at least one. Left out,
    /// every item the split makes runs.
    #[serde(default)]
    pub max_items: Option<u32>,
    /// How many times the stage is asked again to call `fan_out` when it
    /// ends without starting any workers, before it is let through with
    /// none. `0` lets it through at once.
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

impl FanOutDef {
    /// How many times a fan-out stage that ends without calling `fan_out` is
    /// asked again before it is let through without workers, when it sets no
    /// `max_attempts`. The same bound as a gate's, for the same reason: a
    /// model that cannot do the one thing its stage is for should cost a fixed
    /// number of prompts.
    pub const DEFAULT_MAX_ATTEMPTS: usize = 3;
}

impl FanOutDef {
    /// A fan-out of `stage` that hands every item to one worker, with every
    /// other setting at its default.
    pub fn with_worker(stage: StageName, worker: WorkerOption) -> Self {
        Self {
            stage,
            workers_available: vec![worker],
            merge_stage: None,
            max_workers: None,
            on_worker_failure: WorkerFailure::Continue,
            split_prompt: String::new(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        }
    }

    /// The declared worker named `name`.
    pub fn worker(&self, name: &str) -> Option<&WorkerOption> {
        self.workers_available
            .iter()
            .find(|w| w.name.as_str() == name)
    }

    /// The one worker of a fan-out that declares exactly one, which then
    /// takes every item without the model naming it.
    pub fn sole_worker(&self) -> Option<&WorkerOption> {
        match self.workers_available.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

/// What a fan-out's only worker is for when nothing says more: it takes
/// every item. A fan-out with one worker never shows this to the model, which
/// has nothing to pick.
pub const EVERY_ITEM: &str = "Takes every item this fan-out hands out";

/// One worker a fan-out may hand items to: what the model calls it, what it
/// runs, and when to pick it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "WorkerEntry", into = "WorkerEntry")]
pub struct WorkerOption {
    /// What the model writes to hand this worker an item.
    pub name: WorkerName,
    /// What the worker runs.
    pub runs: WorkerSource,
    /// When to pick it, written for the model.
    pub use_for: String,
}

impl WorkerOption {
    /// A worker running `runs`, named after what it runs: the stage, the
    /// blueprint, the last part of a blueprint's directory, or `worker` for a
    /// query.
    pub fn named_after(runs: WorkerSource, use_for: impl Into<String>) -> Self {
        let name = match &runs {
            WorkerSource::Stage(stage) => worker_name_from(stage.as_str()),
            WorkerSource::Blueprint(blueprint) => worker_name_from(blueprint.name.as_str()),
            WorkerSource::BlueprintFile(path) => worker_name_from(
                &path
                    .path()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            WorkerSource::Query(_) => worker_name_from(""),
        };
        Self {
            name,
            runs,
            use_for: use_for.into(),
        }
    }
}

/// `text` as a worker name: each character a worker name does not allow
/// becomes `_`, a name that would start with a digit or `-` gets a `_` in
/// front, and no text at all is `worker`.
pub fn worker_name_from(text: &str) -> WorkerName {
    let mut name: String = text
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                true => c,
                false => '_',
            },
        )
        .take(63)
        .collect();
    if name.is_empty() {
        name.push_str("worker");
    }
    if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        name.insert(0, '_');
    }
    WorkerName::new(name).expect("only allowed characters, a letter or `_` first, at most 64")
}

/// A worker as a blueprint writes it: a `name`, exactly one of `stage`,
/// `blueprint` or `query`, and `use_for`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerEntry {
    /// What the model writes to hand this worker an item.
    pub name: WorkerName,
    /// A stage of this same graph, one that sets `allow_as_worker`.
    #[serde(default)]
    pub stage: Option<StageName>,
    /// A blueprint: an installed one's name (`name` or `name@digest`), or the
    /// absolute directory of one.
    #[serde(default)]
    pub blueprint: Option<String>,
    /// A description an installed blueprint is found by when the worker
    /// starts.
    #[serde(default)]
    pub query: Option<String>,
    /// When to pick this worker, written for the model.
    pub use_for: String,
}

impl TryFrom<WorkerEntry> for WorkerOption {
    type Error = String;

    fn try_from(entry: WorkerEntry) -> Result<Self, String> {
        let name = entry.name;
        let runs = match (entry.stage, entry.blueprint, entry.query) {
            (Some(stage), None, None) => WorkerSource::Stage(stage),
            (None, Some(blueprint), None) => WorkerSource::named(&blueprint)
                .map_err(|e| format!("worker '{name}': blueprint '{blueprint}': {e}"))?,
            (None, None, Some(query)) => WorkerSource::Query(query),
            _ => {
                return Err(format!(
                    "worker '{name}' needs exactly one of `stage`, `blueprint` or `query`: what it runs"
                ));
            }
        };
        Ok(Self {
            name,
            runs,
            use_for: entry.use_for,
        })
    }
}

impl From<WorkerOption> for WorkerEntry {
    fn from(option: WorkerOption) -> Self {
        let (stage, blueprint, query) = match option.runs {
            WorkerSource::Stage(stage) => (Some(stage), None, None),
            WorkerSource::Blueprint(blueprint) => (None, Some(blueprint.to_string()), None),
            WorkerSource::BlueprintFile(path) => (None, Some(path.to_string()), None),
            WorkerSource::Query(query) => (None, None, Some(query)),
        };
        Self {
            name: option.name,
            stage,
            blueprint,
            query,
            use_for: option.use_for,
        }
    }
}

/// What a fan-out worker runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSource {
    /// An installed blueprint.
    Blueprint(BlueprintRef),
    /// A stage of this same graph.
    Stage(StageName),
    /// An installed blueprint picked at run time by a query the model answers.
    Query(String),
    /// A blueprint that is not installed, read from its directory on this
    /// machine. A request from a remote caller may not name one; see
    /// [`SpawnRequest::check_remote`](crate::spec::request::SpawnRequest::check_remote).
    BlueprintFile(BlueprintPath),
}

impl WorkerSource {
    /// The worker a blueprint or a `fan_out` call names as text. A path
    /// (see [`looks_like_a_path`]) names a blueprint's directory, which must
    /// be absolute; anything else names an installed blueprint, as `name` or
    /// `name@digest`.
    pub fn named(text: &str) -> Result<Self, NameError> {
        match looks_like_a_path(text) {
            true => BlueprintPath::new(text).map(Self::BlueprintFile),
            false => BlueprintRef::parse(text).map(Self::Blueprint),
        }
    }
}

/// Whether text naming a blueprint is a path rather than a name: it holds a
/// slash or a backslash, or starts with `.` or `~`.
pub fn looks_like_a_path(text: &str) -> bool {
    text.contains(['/', '\\']) || text.starts_with(['.', '~'])
}

/// What one fan-out worker's failure does to the rest.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailure {
    /// The others carry on and the merge sees the failure.
    #[default]
    Continue,
    /// The whole fan-out fails.
    FailAll,
}

/// Where a stage's tool results land.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolRoutingDef {
    /// The region results land in unless a tool has its own.
    pub default_region: RegionName,
    /// Per-tool regions.
    #[serde(default)]
    pub tool_regions: BTreeMap<ToolName, RegionName>,
    /// Whether results stay in context after the turn that used them.
    #[serde(default)]
    pub keep_results: bool,
    /// The most tokens one result may take.
    #[serde(default)]
    pub max_result_tokens: Option<u32>,
    /// Per-tool caps on one result's tokens.
    #[serde(default)]
    pub tool_max_result_tokens: BTreeMap<ToolName, u32>,
}

/// Custom code. Where the code comes from is kept apart from what it is used
/// for, so a hook, a validator or a region script names code the same way.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CodeRef {
    /// A file beside a blueprint, by path relative to the blueprint's
    /// directory. Only a blueprint can name one.
    File(String),
    /// The code itself.
    Inline(String),
}

/// Code run at points of a stage's life.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StageHooks {
    /// When the stage starts.
    #[serde(default)]
    pub on_stage_enter: Option<CodeRef>,
    /// When the stage ends.
    #[serde(default)]
    pub on_stage_exit: Option<CodeRef>,
    /// Before each model request.
    #[serde(default)]
    pub before_inference: Option<CodeRef>,
    /// After each model reply.
    #[serde(default)]
    pub after_inference: Option<CodeRef>,
    /// Before a batch of tool calls runs.
    #[serde(default)]
    pub on_tool_call: Option<CodeRef>,
    /// When the run completes.
    #[serde(default)]
    pub on_completion: Option<CodeRef>,
    /// When the run fails.
    #[serde(default)]
    pub on_error: Option<CodeRef>,
}

impl StageHooks {
    /// Every hook that is set, with its name.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &CodeRef)> {
        [
            ("on_stage_enter", &self.on_stage_enter),
            ("on_stage_exit", &self.on_stage_exit),
            ("before_inference", &self.before_inference),
            ("after_inference", &self.after_inference),
            ("on_tool_call", &self.on_tool_call),
            ("on_completion", &self.on_completion),
            ("on_error", &self.on_error),
        ]
        .into_iter()
        .filter_map(|(name, code)| code.as_ref().map(|c| (name, c)))
    }
}

/// A tool call made at spawn to fill a region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SeedToolCall {
    /// The tool.
    pub tool: ToolName,
    /// Its arguments: JSON, as a model would write them.
    #[schemars(with = "serde_json::Value")]
    pub args: JsonDoc,
}
